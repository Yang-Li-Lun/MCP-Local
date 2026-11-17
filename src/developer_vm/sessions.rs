use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use encoding_rs::{CoderResult, UTF_8};
use serde::Serialize;
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use uuid::{Builder, Uuid};
use zeroize::Zeroizing;

use crate::{
    security::SharedRuntime,
    settings::{ReaderSettings, WorkspaceSettings},
};

use super::{
    STOP_UNCONFIRMED,
    guard::{GuardClient, Launch, Mappings},
    session::INVALID_PAGE,
    spool::{GuestError, MAX_STDIN, validate_command},
};

const MAX_SESSIONS: usize = 16;
const SESSION_TTL: Duration = Duration::from_secs(900);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const UNKNOWN: &str = "SESSION_UNKNOWN：找不到此工作階段。";
const CLOSED_INPUT: &str = "SESSION_FINISHED：此工作階段已無法接收輸入。";
const GUEST_PENDING: &str = "SESSION_STARTING：正在啟動 Windows Sandbox。";

pub struct Start {
    pub command: Zeroizing<String>,
    pub root_id: Option<String>,
    pub working_directory: Option<String>,
    pub lifetime_seconds: u32,
}

#[derive(Serialize)]
pub struct View {
    pub session_id: String,
    pub pid: Option<u32>,
    pub completed: bool,
    pub exit_code: Option<i32>,
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub offset: usize,
    pub next_offset: usize,
    pub dropped_chars: usize,
    pub truncated: bool,
    pub sandbox: &'static str,
    pub host_files_access: &'static str,
    pub vm_id: Option<String>,
    pub vm_stopped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_output: Option<&'static str>,
    #[serde(skip)]
    pub policy_revision: Option<String>,
}

#[derive(Serialize)]
pub struct InputReceipt {
    pub session_id: String,
    pub bytes_sent: usize,
    pub stdin_closed: bool,
}

#[derive(Serialize)]
pub struct List {
    pub sessions: Vec<View>,
    pub max_sessions: usize,
    pub max_active: usize,
}

#[derive(Serialize)]
pub struct Closed {
    pub session_id: String,
    pub closed: bool,
    pub profile_removed: bool,
}

#[derive(Default)]
struct Snapshot {
    mappings: Option<Arc<Mappings>>,
    launch: Option<Arc<Launch>>,
    output: String,
    chars: usize,
    finished: Option<Instant>,
    exit_code: Option<i32>,
    reason: Option<&'static str>,
    stop_confirmed: bool,
    stdin_closed: bool,
}

impl Snapshot {
    fn view(
        &self,
        id: Uuid,
        offset: usize,
        length: Option<usize>,
        runtime: Option<&SharedRuntime>,
    ) -> View {
        let decision = runtime.filter(|_| length.is_some()).map(|runtime| {
            let mut runtime = runtime.lock().unwrap_or_else(|error| {
                let mut runtime = error.into_inner();
                runtime.fail();
                runtime
            });
            runtime.session_output(&self.output, self.finished.is_some())
        });
        let filtered = decision
            .as_ref()
            .and_then(|decision| decision.text.as_deref());
        let text = filtered.unwrap_or(&self.output);
        let chars = filtered.map_or(self.chars, |text| text.chars().count());
        let offset = offset.min(chars);
        let next_offset = offset.saturating_add(length.unwrap_or(0)).min(chars);
        let output = length.map(|_| {
            let mut indices = text.char_indices().map(|(index, _)| index);
            let start = indices.nth(offset).unwrap_or(text.len());
            let end = if offset == next_offset {
                start
            } else {
                indices.nth(next_offset - offset - 1).unwrap_or(text.len())
            };
            text[start..end].to_owned()
        });
        View {
            session_id: id.simple().to_string(),
            pid: None,
            completed: self.finished.is_some(),
            exit_code: self.exit_code,
            reason: self.reason,
            output,
            offset,
            next_offset,
            dropped_chars: 0,
            truncated: false,
            sandbox: "windows_sandbox_vm",
            host_files_access: "authorized_roots_and_readonly_toolchains",
            vm_id: self
                .launch
                .as_ref()
                .filter(|_| !self.stop_confirmed)
                .map(|launch| launch.vm_id.clone()),
            vm_stopped: self.stop_confirmed,
            security_output: decision.as_ref().and_then(|decision| decision.status),
            policy_revision: decision.map(|decision| decision.revision),
        }
    }
}

enum Operation {
    Input {
        text: Zeroizing<String>,
        eof: bool,
        reply: oneshot::Sender<Result<usize>>,
    },
    Terminate(oneshot::Sender<Result<()>>),
}

struct Record {
    id: Uuid,
    state: watch::Receiver<Snapshot>,
    operations: mpsc::Sender<Operation>,
    root_id: Option<String>,
    working_directory: Option<String>,
}

impl Record {
    async fn wait(&self, duration: Duration) {
        if !duration.is_zero() {
            let mut state = self.state.clone();
            let _ = timeout(
                duration,
                state.wait_for(|snapshot| snapshot.finished.is_some()),
            )
            .await;
        }
    }

    async fn terminate(&self) -> Result<()> {
        let mut state = self.state.clone();
        if state.borrow().finished.is_none() {
            let (reply, response) = oneshot::channel();
            if self
                .operations
                .send(Operation::Terminate(reply))
                .await
                .is_ok()
                && let Ok(result) = response.await
            {
                result?;
            }
        }
        let state = state
            .wait_for(|snapshot| snapshot.finished.is_some())
            .await
            .context(STOP_UNCONFIRMED)?;
        ensure!(state.stop_confirmed, "{STOP_UNCONFIRMED}");
        Ok(())
    }
}

/// Service-side ownership of a single authenticated connection to the local guard.
pub struct Sessions {
    guard: Arc<tokio::sync::Mutex<GuardClient>>,
    records: Mutex<Vec<Arc<Record>>>,
    cancel: CancellationToken,
    runtime: Option<SharedRuntime>,
}

impl Sessions {
    pub async fn connect(descriptor: &str) -> Result<Self> {
        Ok(Self {
            guard: Arc::new(tokio::sync::Mutex::new(
                GuardClient::connect(descriptor).await?,
            )),
            records: Mutex::new(Vec::new()),
            cancel: CancellationToken::new(),
            runtime: None,
        })
    }

    pub async fn connect_workspace(
        descriptor: &str,
        reader: &ReaderSettings,
        workspace: &WorkspaceSettings,
    ) -> Result<Self> {
        let sessions = Self::connect(descriptor).await?;
        sessions
            .guard
            .lock()
            .await
            .require_scope(reader, workspace)?;
        Ok(sessions)
    }

    pub fn bind(mut self, runtime: SharedRuntime) -> Self {
        self.runtime = Some(runtime);
        self
    }

    pub fn revoke(&self) {
        self.cancel.cancel();
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub async fn start(&self, request: Start, observe: Duration) -> Result<View> {
        validate_command(&request.command)?;
        ensure!(
            (1..=3600).contains(&request.lifetime_seconds),
            "VM_LIFETIME_LIMIT"
        );
        ensure!(observe <= Duration::from_secs(3), "SESSION_WAIT_LIMIT");
        let mut random = [0; 16];
        getrandom::fill(&mut random)?;
        let id = Builder::from_random_bytes(random).into_uuid();
        let (updates, state) = watch::channel(Snapshot::default());
        let (operations, inbox) = mpsc::channel(1);
        let (ready, prepared) = oneshot::channel();
        let record = Arc::new(Record {
            id,
            state,
            operations,
            root_id: request.root_id.clone(),
            working_directory: request.working_directory.clone(),
        });
        {
            let mut records = self.records.lock().unwrap();
            records.retain(|record| {
                let snapshot = record.state.borrow();
                !snapshot.stop_confirmed
                    || snapshot
                        .finished
                        .is_none_or(|finished| finished.elapsed() < SESSION_TTL)
            });
            ensure!(
                records.len() < MAX_SESSIONS,
                "SESSION_LIMIT：工作階段記錄已滿。"
            );
            for record in records.iter() {
                let snapshot = record.state.borrow();
                ensure!(
                    snapshot.finished.is_some(),
                    "SESSION_LIMIT：同時只允許一個 VM 工作階段。"
                );
                ensure!(snapshot.stop_confirmed, "{STOP_UNCONFIRMED}");
            }
            records.push(record.clone());
        }
        tokio::spawn(run(
            self.guard.clone(),
            self.cancel.clone(),
            request,
            updates,
            inbox,
            ready,
        ));
        if let Err(error) = prepared.await.context(STOP_UNCONFIRMED)? {
            if record.state.borrow().stop_confirmed {
                self.records
                    .lock()
                    .unwrap()
                    .retain(|existing| !Arc::ptr_eq(existing, &record));
            }
            return Err(error);
        }
        record.wait(observe).await;
        let view = record
            .state
            .borrow()
            .view(id, 0, Some(24_000), self.runtime.as_ref());
        Ok(view)
    }

    fn lookup(&self, id: &str) -> Result<Arc<Record>> {
        ensure!(id.len() == 32, "{UNKNOWN}");
        let id = Uuid::parse_str(id).context(UNKNOWN)?;
        self.records
            .lock()
            .unwrap()
            .iter()
            .find(|record| record.id == id)
            .cloned()
            .context(UNKNOWN)
    }

    pub fn launch(&self, id: &str) -> Result<Option<Arc<Launch>>> {
        Ok(self.lookup(id)?.state.borrow().launch.clone())
    }

    pub fn mappings(&self, id: &str) -> Result<Arc<Mappings>> {
        self.lookup(id)?
            .state
            .borrow()
            .mappings
            .clone()
            .context(GUEST_PENDING)
    }

    pub fn export_location(&self, id: &str) -> Result<(Option<String>, Option<String>)> {
        let record = self.lookup(id)?;
        let state = record.state.borrow();
        ensure!(
            state.finished.is_some() && state.stop_confirmed && state.launch.is_some(),
            "SESSION_RUNNING：停止並確認 VM 後才能匯出。"
        );
        Ok((record.root_id.clone(), record.working_directory.clone()))
    }

    pub async fn read(
        &self,
        id: &str,
        offset: usize,
        length: usize,
        wait: Duration,
    ) -> Result<View> {
        let record = self.lookup(id)?;
        record.wait(wait).await;
        Ok(record
            .state
            .borrow()
            .view(record.id, offset, Some(length), self.runtime.as_ref()))
    }

    pub fn list(&self) -> List {
        let sessions = self
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.state.borrow().view(record.id, 0, None, None))
            .collect();
        List {
            sessions,
            max_sessions: MAX_SESSIONS,
            max_active: 1,
        }
    }

    pub async fn interact(
        &self,
        id: &str,
        text: Zeroizing<String>,
        eof: bool,
    ) -> Result<InputReceipt> {
        ensure!(
            text.len() <= MAX_STDIN,
            "STDIN_LIMIT：單次輸入最多 4096 bytes。"
        );
        let record = self.lookup(id)?;
        {
            let state = record.state.borrow();
            ensure!(
                state.finished.is_none() && !state.stdin_closed,
                "{CLOSED_INPUT}"
            );
            ensure!(state.launch.is_some(), "{GUEST_PENDING}");
        }
        let (reply, response) = oneshot::channel();
        record
            .operations
            .send(Operation::Input { text, eof, reply })
            .await
            .context(CLOSED_INPUT)?;
        let bytes_sent = response.await.context(CLOSED_INPUT)??;
        Ok(InputReceipt {
            session_id: record.id.simple().to_string(),
            bytes_sent,
            stdin_closed: eof,
        })
    }

    pub async fn terminate(&self, id: &str) -> Result<View> {
        let record = self.lookup(id)?;
        record.terminate().await?;
        Ok(record
            .state
            .borrow()
            .view(record.id, 0, Some(24_000), self.runtime.as_ref()))
    }

    pub async fn close(&self, id: &str) -> Result<Closed> {
        let record = self.lookup(id)?;
        record.terminate().await?;
        let mut records = self.records.lock().unwrap();
        let index = records
            .iter()
            .position(|existing| Arc::ptr_eq(existing, &record))
            .context(UNKNOWN)?;
        records.remove(index);
        Ok(Closed {
            session_id: record.id.simple().to_string(),
            closed: true,
            profile_removed: false,
        })
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Default)]
struct Outcome {
    exit_code: Option<i32>,
    reason: Option<&'static str>,
}

async fn run(
    guard: Arc<tokio::sync::Mutex<GuardClient>>,
    cancel: CancellationToken,
    request: Start,
    updates: watch::Sender<Snapshot>,
    mut inbox: mpsc::Receiver<Operation>,
    ready: oneshot::Sender<Result<()>>,
) {
    let mut decoder = UTF_8.new_decoder_without_bom_handling();
    let mut id = None;
    let mut ready = Some(ready);
    let mut client = guard.lock().await;
    let operation = async {
        let mappings = Arc::new(
            client
                .prepare(
                    &request.command,
                    request.root_id.as_deref(),
                    request.working_directory.as_deref(),
                )
                .await?,
        );
        let lifetime_seconds = request.lifetime_seconds;
        drop(request);
        updates.send_modify(|state| state.mappings = Some(mappings.clone()));
        ready
            .take()
            .unwrap()
            .send(Ok(()))
            .map_err(|_| anyhow::anyhow!("SESSION_START_CANCELLED"))?;
        let vm_id = client.activate(lifetime_seconds).await?;
        id = Some(vm_id);
        let launch = Launch {
            vm_id: vm_id.to_string(),
            mappings,
        };
        updates.send_modify(|state| state.launch = Some(Arc::new(launch)));
        monitor(&mut client, vm_id, &updates, &mut inbox, &mut decoder).await
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => { client.disconnect(); return; },
        result = operation => result,
    };
    let stopped = tokio::select! {
        biased;
        _ = cancel.cancelled() => { client.disconnect(); return; },
        result = async {
            // A rejected poll may have cleaned up inside the guard. Stop(None) is
            // only accepted if the authenticated guard has cleared the session ID.
            client.stop(id).await.is_ok()
                || (id.is_some() && client.stop(None).await.is_ok())
                || client.recover_after_disconnect().await.is_ok()
        } => result,
    };
    drop(client);
    let flushed = append_output(&mut decoder, &updates, &[], true);
    let preparation = ready.map(|ready| {
        let failure = result
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_else(|| "VM_PREPARATION_FAILED".into());
        (ready, failure)
    });
    let outcome = result
        .and_then(|outcome| flushed.map(|()| outcome))
        .unwrap_or(Outcome {
            exit_code: None,
            reason: Some("session_monitor_failed"),
        });
    updates.send_modify(|state| {
        state.stop_confirmed = stopped;
        state.finished = Some(Instant::now());
        state.exit_code = outcome.exit_code;
        state.reason = if stopped {
            outcome.reason
        } else {
            Some("VM_STOP_UNCONFIRMED")
        };
    });
    if let Some((ready, failure)) = preparation {
        let _ = ready.send(Err(anyhow::anyhow!(if stopped {
            failure
        } else {
            STOP_UNCONFIRMED.into()
        })));
    }
}

async fn monitor(
    client: &mut GuardClient,
    id: Uuid,
    updates: &watch::Sender<Snapshot>,
    inbox: &mut mpsc::Receiver<Operation>,
    decoder: &mut encoding_rs::Decoder,
) -> Result<Outcome> {
    let mut offset = 0;
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            operation = inbox.recv() => match operation {
                Some(Operation::Input {text, eof, reply}) => {
                    if reply.is_closed() { continue; }
                    if updates.borrow().stdin_closed {
                        let _ = reply.send(Err(anyhow::anyhow!(CLOSED_INPUT)));
                        continue;
                    }
                    let result = timeout(Duration::from_secs(2), client.input(id, &text, eof)).await;
                    match result {
                        Ok(result) => {
                            if result.is_ok() && eof {
                                updates.send_modify(|state| state.stdin_closed = true);
                            }
                            let _ = reply.send(result);
                            continue;
                        }
                        Err(_) => {
                            let _ = reply.send(Err(anyhow::anyhow!("STDIN_TIMEOUT")));
                            bail!("STDIN_TIMEOUT");
                        }
                    }
                }
                Some(Operation::Terminate(reply)) => {
                    let result = client.terminate(id).await;
                    let accepted = result.is_ok();
                    let _ = reply.send(result);
                    if !accepted { continue; }
                }
                None => bail!("SESSION_CLOSED"),
            },
        }
        loop {
            let page = client.poll(id, offset).await?;
            ensure!(page.offset == offset, "{INVALID_PAGE}");
            let caught_up = page.next_offset == page.total_bytes;
            ensure!(caught_up || page.next_offset > offset, "{INVALID_PAGE}");
            let bytes = page.bytes()?;
            if !bytes.is_empty() {
                append_output(decoder, updates, &bytes, false)?;
            }
            offset = page.next_offset;
            if !caught_up {
                continue;
            }
            if page.terminated {
                return Ok(Outcome {
                    exit_code: None,
                    reason: Some("terminated_by_client"),
                });
            }
            if let Some(result) = page.result {
                let reason = result.error.map(|error| match error {
                    GuestError::GuestIoFailed => "GUEST_IO_FAILED",
                    GuestError::OutputLimit => "GUEST_OUTPUT_LIMIT",
                });
                return Ok(Outcome {
                    exit_code: Some(result.exit_code),
                    reason,
                });
            }
            break;
        }
    }
}

fn append_output(
    decoder: &mut encoding_rs::Decoder,
    updates: &watch::Sender<Snapshot>,
    bytes: &[u8],
    last: bool,
) -> Result<()> {
    let mut text = String::with_capacity(
        decoder
            .max_utf8_buffer_length(bytes.len())
            .context(INVALID_PAGE)?,
    );
    let (status, consumed, _) = decoder.decode_to_string(bytes, &mut text, last);
    ensure!(
        status == CoderResult::InputEmpty && consumed == bytes.len(),
        "{INVALID_PAGE}"
    );
    if !text.is_empty() {
        let chars = text.chars().count();
        updates.send_modify(|state| {
            state.output.push_str(&text);
            state.chars += chars;
        });
    }
    Ok(())
}
