pub mod audit;
#[cfg(any(feature = "desktop", test))]
pub mod authority;
pub mod channel;
pub mod guard;
pub mod host;
pub mod policy;
mod storage;
#[cfg(test)]
mod tests;
mod transport;

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Result, ensure};
use serde_json::Value;

use self::{
    audit::{Audit, Event, Kind},
    channel::{Client, Control, Counts, EVENTS, Mode, Report, Status},
    guard::{Finding, Guard, Review},
    policy::{Risk, Snapshot, Store},
};

pub use storage::state_directory;
pub use transport::GuardedTransport;

pub type SharedRuntime = Arc<Mutex<Runtime>>;
pub const SESSION_REVISION: &str = "mcp-local/session-policy-revision";

pub struct SessionOutput {
    pub text: Option<String>,
    pub status: Option<&'static str>,
    pub revision: String,
}

pub struct Runtime {
    store: Store,
    audit: Audit,
    snapshot: Snapshot,
    guard: Option<Guard>,
    fault: bool,
    managed: Option<Managed>,
    developer_session: Option<tokio_util::sync::CancellationToken>,
    host_sessions: host::Sessions,
    connection_closed: bool,
}

struct Managed {
    client: Client,
    control: Control,
    pending: VecDeque<Event>,
    lost: u64,
    fault_recovery: Option<u64>,
    counts: Counts,
    masked_latest: bool,
}

pub enum Output {
    Unchanged,
    Payload(Value),
    Block,
}

impl Runtime {
    pub fn open(directory: PathBuf) -> Result<Self> {
        let store = Store(directory.clone());
        let audit = Audit(directory);
        let snapshot = store.load()?;
        let guard = build_guard(&snapshot)?;
        if guard.is_some() {
            audit.load()?;
        }
        Ok(Self {
            store,
            audit,
            snapshot,
            guard,
            fault: false,
            managed: None,
            developer_session: None,
            host_sessions: host::Sessions::default(),
            connection_closed: false,
        })
    }

    pub fn attach(directory: PathBuf, descriptor: &str, mode: Mode) -> Result<Self> {
        if mode == Mode::HostControl {
            crate::windows_token::require_standard_user(
                "HOST_TOKEN_REJECTED：主機模式需要本機未提升權限的一般使用者。",
            )?;
        }
        let (client, control) = Client::attach(descriptor, mode)?;
        let snapshot = Snapshot {
            policy: control.policy.clone(),
            revision: control.revision.clone(),
        };
        let guard = build_guard(&snapshot)?;
        let mut runtime = Self {
            store: Store(directory.clone()),
            audit: Audit(directory),
            snapshot,
            guard,
            fault: false,
            managed: Some(Managed {
                client,
                control,
                pending: VecDeque::new(),
                lost: 0,
                fault_recovery: None,
                counts: Counts::default(),
                masked_latest: false,
            }),
            developer_session: None,
            host_sessions: host::Sessions::default(),
            connection_closed: false,
        };
        runtime.report()?;
        Ok(runtime)
    }

    pub fn managed(&self) -> bool {
        self.managed.is_some()
    }

    pub fn require_mode(&mut self, mode: Mode) -> Result<()> {
        ensure!(
            self.managed
                .as_ref()
                .is_some_and(|managed| managed.control.mode == mode)
                && !self.connection_closed
                && self
                    .developer_session
                    .as_ref()
                    .is_none_or(|cancel| !cancel.is_cancelled())
                && self.refresh_for_output(),
            "SECURITY_CHANNEL_UNCONFIRMED：缺少目前模式的本機授權。"
        );
        Ok(())
    }

    pub fn bind_developer(&mut self, cancel: tokio_util::sync::CancellationToken) -> Result<()> {
        self.require_mode(Mode::DeveloperControl)?;
        ensure!(self.developer_session.is_none(), "SECURITY_CHANNEL_BUSY");
        self.developer_session = Some(cancel);
        Ok(())
    }

    pub fn session_output(&mut self, output: &str, completed: bool) -> SessionOutput {
        use guard::BLOCK_TEXT;
        let (mut text, mut status) = if !self.refresh_for_output() {
            (Some(BLOCK_TEXT.to_owned()), Some("blocked_unconfirmed"))
        } else if self.guard.is_none() {
            (None, None)
        } else if completed {
            let (output, status) = match self.inspect(|| Ok(Value::String(output.into()))) {
                Output::Payload(Value::String(output)) => {
                    let masked = self
                        .managed
                        .as_mut()
                        .is_some_and(|managed| std::mem::take(&mut managed.masked_latest));
                    (output, if masked { "masked" } else { "passed" })
                }
                _ => (BLOCK_TEXT.into(), "blocked"),
            };
            (Some(output), Some(status))
        } else {
            (Some(String::new()), Some("withheld_until_complete"))
        };
        let revision = self.snapshot.revision.clone();
        if self.report().is_err() {
            self.fail();
        }
        if !self.refresh_for_output() || self.snapshot.revision != revision {
            text = Some(BLOCK_TEXT.into());
            status = Some("blocked_unconfirmed");
        }
        SessionOutput {
            text,
            status,
            revision,
        }
    }

    fn refresh(&mut self) -> Result<()> {
        if let Some(managed) = &mut self.managed {
            let frame = managed.client.read()?;
            ensure!(
                frame.recovery >= managed.control.recovery
                    && frame.lease.generation >= managed.control.lease.generation
                    && (frame.lease.generation != managed.control.lease.generation
                        || frame.lease.deadline_ms == managed.control.lease.deadline_ms),
                "SECURITY_CHANNEL_REPLAY"
            );
            for ack in &frame.acks {
                if ack.identity == managed.client.identity {
                    managed
                        .pending
                        .retain(|event| !ack.events.contains(&event.id));
                }
            }
            managed.control = frame;
            ensure!(managed.control.healthy, "SECURITY_AUTHORITY_UNHEALTHY");
            ensure!(
                managed
                    .fault_recovery
                    .is_none_or(|fault| managed.control.recovery > fault),
                "SECURITY_RECOVERY_REQUIRED"
            );
            if managed.control.revision != self.snapshot.revision {
                let snapshot = Snapshot {
                    policy: managed.control.policy.clone(),
                    revision: managed.control.revision.clone(),
                };
                self.guard = build_guard(&snapshot)?;
                self.snapshot = snapshot;
            }
            managed.fault_recovery = None;
            self.fault = false;
            return Ok(());
        }
        let next = self.store.load()?;
        if next.revision == self.snapshot.revision {
            return Ok(());
        }
        ensure!(
            self.snapshot.revision.is_empty() || !next.revision.is_empty(),
            "SECURITY_POLICY_MISSING"
        );
        let guard = build_guard(&next)?;
        if guard.is_some() {
            self.audit.load()?;
        }
        self.snapshot = next;
        self.guard = guard;
        Ok(())
    }

    fn record(&mut self, kind: Kind, findings: &[Finding]) -> Result<()> {
        let risk = findings
            .iter()
            .map(|finding| finding.risk)
            .max()
            .unwrap_or(Risk::High);
        let event = Event::new(kind, risk, 1)?;
        if let Some(managed) = &mut self.managed {
            if managed.pending.len() >= EVENTS {
                managed.lost = managed.lost.saturating_add(1).min(u32::MAX as u64);
                anyhow::bail!("SECURITY_AUDIT_BACKPRESSURE");
            }
            managed.pending.push_back(event);
            Ok(())
        } else {
            self.audit.append(event)
        }
    }

    pub fn protect(&mut self, payload: impl FnOnce() -> serde_json::Result<Value>) -> Output {
        self.protect_response(None, payload)
    }

    fn protect_response(
        &mut self,
        session_revision: Option<&str>,
        payload: impl FnOnce() -> serde_json::Result<Value>,
    ) -> Output {
        let output = if self.response_authorized(session_revision) {
            self.inspect(payload)
        } else {
            Output::Block
        };
        // Resolve the policy again before release. A save or revocation during a
        // long inspection cannot release a response inspected under an old revision.
        let mut output = output;
        if self.managed.is_some() {
            let before = self.snapshot.revision.clone();
            if !self.response_authorized(session_revision) || self.snapshot.revision != before {
                output = Output::Block;
            }
            if let Some(managed) = &mut self.managed {
                let counter = match &output {
                    Output::Unchanged => &mut managed.counts.disabled,
                    Output::Payload(_) if managed.masked_latest => &mut managed.counts.masked,
                    Output::Payload(_) => &mut managed.counts.passed,
                    Output::Block => &mut managed.counts.blocked,
                };
                *counter = counter.saturating_add(1);
                managed.masked_latest = false;
            }
            if self.report().is_err() {
                self.fail();
                output = Output::Block;
            }
        }
        output
    }

    fn response_authorized(&mut self, session_revision: Option<&str>) -> bool {
        match session_revision {
            Some(revision) => {
                let mode = self.managed.as_ref().map(|managed| managed.control.mode);
                matches!(mode, Some(Mode::DeveloperControl | Mode::HostControl))
                    && self.require_mode(mode.unwrap()).is_ok()
                    && self.snapshot.revision == revision
            }
            None => self.refresh_for_output(),
        }
    }

    fn refresh_for_output(&mut self) -> bool {
        let confirmed = if self.fault && self.managed.is_none() {
            false
        } else {
            match self.refresh() {
                Ok(()) => !self.fault,
                Err(error) => {
                    if !error.is::<policy::PendingSave>() {
                        self.fail();
                    }
                    false
                }
            }
        };
        let control = self.managed.as_ref().map(|managed| &managed.control);
        self.host_sessions.enforce(control.filter(|_| confirmed));
        confirmed
    }

    fn inspect(&mut self, payload: impl FnOnce() -> serde_json::Result<Value>) -> Output {
        let Some(guard) = &self.guard else {
            return Output::Unchanged;
        };
        let review = match payload() {
            Ok(payload) => guard.inspect(payload),
            Err(_) => Review::Blocked {
                findings: Vec::new(),
                checked: false,
            },
        };
        match review {
            Review::Passed(payload) => Output::Payload(payload),
            Review::Masked { payload, findings } => {
                if self.record(Kind::ResponseMasked, &findings).is_ok() {
                    if let Some(managed) = &mut self.managed {
                        managed.masked_latest = true;
                    }
                    Output::Payload(payload)
                } else {
                    self.fail();
                    Output::Block
                }
            }
            Review::Blocked { findings, checked } => {
                let findings = if checked { findings.as_slice() } else { &[] };
                if self.record(Kind::ResponseBlocked, findings).is_err() {
                    self.fail();
                }
                Output::Block
            }
        }
    }

    pub(crate) fn fail(&mut self) {
        let was_fault = self.fault;
        self.fault = true;
        self.cancel_control_sessions();
        if let Some(managed) = &mut self.managed {
            let recovery = managed.control.recovery;
            managed.fault_recovery = Some(
                managed
                    .fault_recovery
                    .map_or(recovery, |fault| fault.max(recovery)),
            );
        }
        if !was_fault {
            let _ = self.record(Kind::ChannelFailed, &[]);
        }
    }

    pub(super) fn cancel_control_sessions(&mut self) {
        if let Some(cancel) = &self.developer_session {
            cancel.cancel();
        }
        self.host_sessions.enforce(None);
    }

    pub(super) fn close_controls(&mut self) {
        self.connection_closed = true;
        self.cancel_control_sessions();
    }

    fn report(&mut self) -> Result<()> {
        if let Some(managed) = &mut self.managed {
            managed.publish(
                if self.fault {
                    Status::Fault
                } else {
                    Status::Ready
                },
                &self.snapshot.revision,
            )?;
        }
        Ok(())
    }

    pub fn heartbeat(&mut self) {
        self.refresh_for_output();
        if self.report().is_err() {
            self.fail();
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.close_controls();
        if let Some(managed) = &mut self.managed {
            let _ = managed.publish(Status::Closed, &self.snapshot.revision);
        }
    }
}

impl Managed {
    fn publish(&mut self, status: Status, revision: &str) -> Result<()> {
        let host_authorized = status == Status::Ready && self.control.host_authorized();
        self.client.report(&mut Report {
            identity: self.client.identity,
            sequence: 0,
            time_ms: 0,
            mode: self.control.mode,
            status,
            revision: revision.into(),
            generation: self.control.lease.generation,
            recovery: self.control.recovery,
            counters: self.counts,
            host_authorized,
            events: self.pending.iter().cloned().collect(),
            lost_events: self.lost,
        })
    }
}

fn build_guard(snapshot: &Snapshot) -> Result<Option<Guard>> {
    (snapshot.policy.enabled && snapshot.policy.response_guard)
        .then(|| Guard::new(&snapshot.policy))
        .transpose()
}
