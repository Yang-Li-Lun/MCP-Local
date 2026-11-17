pub mod environment;
mod native;
#[cfg(all(test, feature = "desktop"))]
mod stdio_tests;
#[cfg(test)]
mod tests;

use crate::{
    filesystem::Access,
    security::{
        SharedRuntime,
        channel::Mode,
        host::{Permit, SessionAuthority},
    },
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

const UNKNOWN: &str = "SESSION_UNKNOWN：找不到此工作階段。";
const STOP: &str = "HOST_STOP_UNCONFIRMED：無法確認主機 Job 已停止。";
const MAX_OUTPUT: usize = 1024 * 1024;
const MAX_SESSIONS: usize = 16;

type Launcher = fn(&str, &std::path::Path) -> Result<native::Process>;

pub struct Sessions {
    records: Mutex<Vec<Arc<Record>>>,
    security: SharedRuntime,
    cancel: CancellationToken,
    launch: Launcher,
}

pub struct Start {
    pub command: Zeroizing<String>,
    pub cwd: PathBuf,
    pub pin: Access,
    pub root_id: String,
    pub lifetime: Duration,
}

#[derive(Default)]
struct State {
    pid: Option<u32>,
    finished: Option<Instant>,
    stopped: bool,
    output: Vec<u8>,
    exit_code: Option<i32>,
    reason: Option<&'static str>,
    client_terminated: bool,
    stdin_closed: bool,
}

struct Record {
    id: String,
    state: Mutex<State>,
    stop: CancellationToken,
    ops: mpsc::SyncSender<Input>,
    cwd: PathBuf,
    root_id: String,
    _pin: Access,
}

struct Input {
    permit: Permit,
    text: Zeroizing<String>,
    eof: bool,
    cancel: CancellationToken,
    reply: tokio::sync::oneshot::Sender<Result<usize>>,
}

pub struct Output {
    pub value: Value,
    pub revision: Option<String>,
}

impl Sessions {
    pub fn new(security: SharedRuntime) -> Result<Self> {
        security
            .lock()
            .map_err(|_| anyhow::anyhow!(STOP))?
            .require_mode(Mode::HostControl)?;
        Ok(Self {
            records: Mutex::new(Vec::new()),
            security,
            cancel: CancellationToken::new(),
            launch: native::Process::prepare,
        })
    }

    pub fn authorize(&self) -> Result<()> {
        let result = self
            .security
            .lock()
            .map_err(|_| anyhow::anyhow!(STOP))?
            .require_mode(Mode::HostControl);
        if result.is_err() {
            self.cancel.cancel();
        }
        result
    }

    fn lookup(&self, id: &str) -> Result<Arc<Record>> {
        self.records
            .lock()
            .map_err(|_| anyhow::anyhow!(STOP))?
            .iter()
            .find(|record| record.id == id)
            .cloned()
            .context(UNKNOWN)
    }

    pub async fn start(
        &self,
        request: Start,
        observe: Duration,
        cancel: CancellationToken,
    ) -> Result<Output> {
        ensure!(
            !request.command.trim().is_empty()
                && request.command.chars().count() <= 8192
                && !request.command.contains('\0'),
            "COMMAND_LIMIT：命令必須為 1 至 8192 字元。"
        );
        ensure!(
            (1..=3600).contains(&request.lifetime.as_secs()) && observe <= Duration::from_secs(3),
            "HOST_LIFETIME_LIMIT"
        );
        self.authorize()?;
        let permit = self
            .security
            .lock()
            .map_err(|_| anyhow::anyhow!(STOP))?
            .prepare_host_action()?;
        let mut random = [0; 16];
        getrandom::fill(&mut random)?;
        let (ops, inbox) = mpsc::sync_channel(1);
        let record = Arc::new(Record {
            id: crate::listing::hex(&random),
            state: Mutex::new(State::default()),
            stop: CancellationToken::new(),
            ops,
            cwd: request.cwd,
            root_id: request.root_id,
            _pin: request.pin,
        });
        let pending = record.stop.clone().drop_guard();
        {
            let mut records = self.records.lock().map_err(|_| anyhow::anyhow!(STOP))?;
            records.retain(|record| {
                let state = record
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                !state.stopped
                    || state
                        .finished
                        .is_none_or(|at| at.elapsed() < Duration::from_secs(900))
            });
            ensure!(records.len() < MAX_SESSIONS, "SESSION_LIMIT");
            ensure!(
                records.iter().all(|record| record
                    .state
                    .lock()
                    .is_ok_and(|s| s.finished.is_some() && s.stopped)),
                "SESSION_LIMIT：同時只允許一個主機工作階段，且必須確認前一個 Job 停止。"
            );
            records.push(record.clone());
        }
        let (ready, prepared) = tokio::sync::oneshot::channel();
        let worker_record = record.clone();
        let security = self.security.clone();
        let revoked = self.cancel.clone();
        let launch = self.launch;
        let spawned = thread::Builder::new()
            .name("host-session".into())
            .spawn(move || {
                run(
                    worker_record,
                    security,
                    revoked,
                    request.command,
                    request.lifetime,
                    permit,
                    inbox,
                    ready,
                    launch,
                );
            });
        if spawned.is_err() {
            self.records
                .lock()
                .map_err(|_| anyhow::anyhow!(STOP))?
                .retain(|r| !Arc::ptr_eq(r, &record));
            anyhow::bail!("HOST_WORKER_FAILED");
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("OPERATION_CANCELLED"),
            result = prepared => result.context(STOP)??,
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("OPERATION_CANCELLED"),
            _ = wait(&record, observe) => {},
        }
        let result = self.view(&record, 0, Some(24_000))?;
        pending.disarm();
        Ok(result)
    }

    fn view(&self, record: &Record, offset: usize, length: Option<usize>) -> Result<Output> {
        let state = record.state.lock().map_err(|_| anyhow::anyhow!(STOP))?;
        let bytes = if state.finished.is_none() {
            match std::str::from_utf8(&state.output) {
                Err(error) if error.error_len().is_none() => &state.output[..error.valid_up_to()],
                _ => &state.output,
            }
        } else {
            &state.output
        };
        let raw = String::from_utf8_lossy(bytes);
        let decision = if length.is_some() {
            Some(
                self.security
                    .lock()
                    .map_err(|_| anyhow::anyhow!(STOP))?
                    .session_output(&raw, state.finished.is_some()),
            )
        } else {
            None
        };
        let blocked = matches!(
            state.reason,
            Some("HOST_OUTPUT_LIMIT" | "HOST_OUTPUT_FAILED" | "HOST_STOP_UNCONFIRMED")
        );
        let text = if blocked {
            crate::security::guard::BLOCK_TEXT
        } else {
            decision
                .as_ref()
                .and_then(|d| d.text.as_deref())
                .unwrap_or(&raw)
        };
        let offset = offset.min(text.chars().count());
        let output: Option<String> =
            length.map(|length| text.chars().skip(offset).take(length).collect());
        let next = offset + output.as_ref().map_or(0, |s| s.chars().count());
        let mut value = json!({"session_id":record.id, "pid":state.pid, "completed":state.finished.is_some(),
            "exit_code":state.exit_code, "reason":state.reason, "offset":offset, "next_offset":next,
            "dropped_chars":0, "truncated":blocked, "sandbox":"host_process", "host_files_access":"current_user_trusted_commands",
            "job_stopped":state.stopped, "stdin_closed":state.stdin_closed});
        if let Some(output) = output {
            value["output"] = output.into();
        }
        if blocked {
            value["security_output"] = if state.reason == Some("HOST_OUTPUT_LIMIT") {
                "blocked_output_limit"
            } else {
                "blocked_incomplete_output"
            }
            .into();
        } else if let Some(status) = decision.as_ref().and_then(|d| d.status) {
            value["security_output"] = status.into();
        }
        Ok(Output {
            value,
            revision: decision.map(|d| d.revision),
        })
    }

    pub async fn read(
        &self,
        id: &str,
        offset: usize,
        length: usize,
        duration: Duration,
    ) -> Result<Output> {
        ensure!(
            (1..=32000).contains(&length) && duration <= Duration::from_secs(3),
            "SESSION_WAIT_LIMIT"
        );
        let record = self.lookup(id)?;
        wait(&record, duration).await;
        self.view(&record, offset, Some(length))
    }

    pub fn list(&self) -> Result<Value> {
        let records = self.records.lock().map_err(|_| anyhow::anyhow!(STOP))?;
        let sessions = records
            .iter()
            .map(|r| self.view(r, 0, None).map(|v| v.value))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({"sessions":sessions,"max_sessions":MAX_SESSIONS,"max_active":1}))
    }

    pub async fn interact(
        &self,
        id: &str,
        text: Zeroizing<String>,
        eof: bool,
        cancel: CancellationToken,
    ) -> Result<Value> {
        ensure!(text.len() <= 4096 && !text.contains('\0'), "STDIN_LIMIT");
        let permit = self
            .security
            .lock()
            .map_err(|_| anyhow::anyhow!(STOP))?
            .prepare_host_action()?;
        let record = self.lookup(id)?;
        let (reply, response) = tokio::sync::oneshot::channel();
        let queued = cancel.child_token();
        let _on_drop = queued.clone().drop_guard();
        record
            .ops
            .try_send(Input {
                permit,
                text,
                eof,
                cancel: queued,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("SESSION_INPUT_BUSY"))?;
        let count = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("OPERATION_CANCELLED"),
            result = tokio::time::timeout(Duration::from_secs(3), response) => result.context("HOST_INPUT_TIMEOUT")?.context(STOP)??,
        };
        Ok(json!({"session_id":id,"bytes_sent":count,"stdin_closed":eof}))
    }

    pub async fn terminate(&self, id: &str) -> Result<Output> {
        let record = self.lookup(id)?;
        {
            let mut state = record.state.lock().map_err(|_| anyhow::anyhow!(STOP))?;
            state.client_terminated = true;
            if state.finished.is_some()
                && state.stopped
                && matches!(
                    state.reason,
                    None | Some("lifetime_limit" | "stdin_timeout" | "terminated_by_client")
                )
            {
                state.reason = Some("terminated_by_client");
            }
        }
        record.stop.cancel();
        wait(&record, Duration::from_secs(7)).await;
        ensure!(
            record
                .state
                .lock()
                .is_ok_and(|s| s.finished.is_some() && s.stopped),
            "{STOP}"
        );
        self.view(&record, 0, Some(24_000))
    }

    pub async fn close(&self, id: &str) -> Result<Value> {
        self.terminate(id).await?;
        self.records
            .lock()
            .map_err(|_| anyhow::anyhow!(STOP))?
            .retain(|r| r.id != id);
        Ok(json!({"session_id":id,"closed":true,"profile_removed":false}))
    }

    pub fn export_location(&self, id: &str) -> Result<(String, PathBuf)> {
        let record = self.lookup(id)?;
        let state = record.state.lock().map_err(|_| anyhow::anyhow!(STOP))?;
        ensure!(
            state.finished.is_some() && state.stopped && state.pid.is_some(),
            "SESSION_RUNNING"
        );
        record._pin.verify_host_directory()?;
        Ok((record.root_id.clone(), record.cwd.clone()))
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.cancel.cancel();
        let records = self
            .records
            .lock()
            .map_err(|_| anyhow::anyhow!(STOP))?
            .clone();
        for record in records {
            wait(&record, Duration::from_secs(7)).await;
            ensure!(
                record
                    .state
                    .lock()
                    .is_ok_and(|s| s.finished.is_some() && s.stopped),
                "{STOP}"
            );
        }
        Ok(())
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn wait(record: &Record, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline && record.state.lock().is_ok_and(|s| s.finished.is_none()) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    record: Arc<Record>,
    security: SharedRuntime,
    revoked: CancellationToken,
    command: Zeroizing<String>,
    lifetime: Duration,
    permit: Permit,
    inbox: mpsc::Receiver<Input>,
    ready: tokio::sync::oneshot::Sender<Result<()>>,
    launch: Launcher,
) {
    let mut process = None;
    let mut authority: Option<Arc<SessionAuthority>> = None;
    let preparation = (|| {
        record._pin.verify_host_directory()?;
        ensure!(
            !record.stop.is_cancelled() && !revoked.is_cancelled(),
            "OPERATION_CANCELLED"
        );
        security
            .lock()
            .map_err(|_| anyhow::anyhow!(STOP))?
            .check_host_action(&permit)?;
        process = Some(launch(&command, &record.cwd)?);
        let process = process.as_ref().unwrap();
        // The process is suspended and already inside its Job at the final lease check.
        let mut runtime = security.lock().map_err(|_| anyhow::anyhow!(STOP))?;
        authority = Some(runtime.begin_host_session(permit)?);
        ensure!(
            !record.stop.is_cancelled() && !revoked.is_cancelled(),
            "OPERATION_CANCELLED"
        );
        record._pin.verify_host_directory()?;
        process.resume()?;
        drop(runtime);
        record.state.lock().map_err(|_| anyhow::anyhow!(STOP))?.pid = Some(process.pid);
        Ok(())
    })();
    drop(command);
    let prepared = preparation.is_ok();
    let received = ready.send(preparation).is_ok();
    let mut reason = if prepared && received {
        None
    } else {
        Some("HOST_START_FAILED")
    };
    let mut output = Vec::new();
    let mut exit_code = None;
    if prepared && received {
        let process = process.as_mut().unwrap();
        let authority = authority.as_ref().unwrap();
        let deadline = Instant::now() + lifetime;
        while reason.is_none() {
            if let Ok(mut runtime) = security.lock() {
                runtime.heartbeat();
            } else {
                revoked.cancel();
            }
            if record.stop.is_cancelled()
                || revoked.is_cancelled()
                || authority.cancellation().is_cancelled()
            {
                reason = Some(
                    if !revoked.is_cancelled()
                        && !authority.cancellation().is_cancelled()
                        && record.state.lock().is_ok_and(|s| s.client_terminated)
                    {
                        "terminated_by_client"
                    } else {
                        "HOST_CANCELLED"
                    },
                );
                break;
            }
            if Instant::now() >= deadline {
                reason = Some("lifetime_limit");
                break;
            }
            if let Err(error) = process.drain(&mut output, MAX_OUTPUT) {
                reason = Some(if error.to_string().contains("HOST_OUTPUT_LIMIT") {
                    "HOST_OUTPUT_LIMIT"
                } else {
                    "HOST_OUTPUT_FAILED"
                });
                break;
            }
            record
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .output
                .clone_from(&output);
            match process.poll() {
                Ok(Some(code)) => {
                    exit_code = Some(code);
                    break;
                }
                Err(_) => {
                    reason = Some("HOST_PROCESS_FAILED");
                    break;
                }
                _ => {}
            }
            if let Ok(input) = inbox.try_recv() {
                let result = (|| {
                    ensure!(
                        !input.cancel.is_cancelled() && !input.reply.is_closed(),
                        "OPERATION_CANCELLED"
                    );
                    ensure!(process.input.is_some(), "SESSION_FINISHED");
                    let file = process.input.as_ref().unwrap().try_clone()?;
                    let (send, response) = mpsc::sync_channel(1);
                    let count = input.text.len();
                    let text = input.text;
                    let cancel = input.cancel.clone();
                    let writer_security = security.clone();
                    let writer_authority = authority.clone();
                    let writer_stop = record.stop.clone();
                    let writer =
                        thread::Builder::new()
                            .name("host-stdin".into())
                            .spawn(move || {
                                let result = (|| {
                                    ensure!(!cancel.is_cancelled(), "OPERATION_CANCELLED");
                                    writer_security
                                        .lock()
                                        .map_err(|_| anyhow::anyhow!(STOP))?
                                        .authorize_host_input(input.permit, &writer_authority)?;
                                    let result = native::Process::write(file, text.as_bytes());
                                    if result.is_err() {
                                        writer_stop.cancel();
                                    }
                                    result
                                })();
                                let _ = send.send(result);
                            })?;
                    let write_deadline = Instant::now() + Duration::from_secs(1);
                    let write_result = loop {
                        if let Ok(result) = response.try_recv() {
                            break result;
                        }
                        if let Ok(mut runtime) = security.lock() {
                            runtime.heartbeat();
                        } else {
                            revoked.cancel();
                        }
                        if Instant::now() >= write_deadline
                            || Instant::now() >= deadline
                            || input.cancel.is_cancelled()
                            || record.stop.is_cancelled()
                            || revoked.is_cancelled()
                            || authority.cancellation().is_cancelled()
                        {
                            if Instant::now() >= write_deadline {
                                reason = Some("stdin_timeout");
                            }
                            record.stop.cancel();
                            process.job.terminate()?;
                            break Err(anyhow::anyhow!("STDIN_TIMEOUT"));
                        }
                        thread::sleep(Duration::from_millis(10));
                    };
                    if writer.join().is_err() {
                        record.stop.cancel();
                        anyhow::bail!("HOST_INPUT_FAILED");
                    }
                    write_result?;
                    if input.eof {
                        process.input.take();
                        record
                            .state
                            .lock()
                            .map_err(|_| anyhow::anyhow!(STOP))?
                            .stdin_closed = true;
                    }
                    Ok(count)
                })();
                let _ = input.reply.send(result);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    let stopped = process.as_mut().is_none_or(|process| {
        process.input.take();
        let stopped = process.job.terminate().is_ok();
        if stopped && reason != Some("HOST_OUTPUT_LIMIT") {
            loop {
                let before = output.len();
                if let Err(error) = process.drain(&mut output, MAX_OUTPUT) {
                    reason = Some(if error.to_string().contains("HOST_OUTPUT_LIMIT") {
                        "HOST_OUTPUT_LIMIT"
                    } else {
                        "HOST_OUTPUT_FAILED"
                    });
                    break;
                }
                if output.len() == before {
                    break;
                }
            }
        }
        stopped
    });
    if !stopped {
        if let Ok(mut runtime) = security.lock() {
            runtime.fail();
        }
        revoked.cancel();
        reason = Some("HOST_STOP_UNCONFIRMED");
    }
    if reason == Some("HOST_OUTPUT_LIMIT") {
        output.clear();
    }
    let mut state = record
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    state.output = output;
    state.exit_code = exit_code;
    state.reason = reason;
    state.stopped = stopped;
    state.stdin_closed = true;
    state.finished = Some(Instant::now());
}
