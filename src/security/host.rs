use std::sync::{Arc, Weak};

use anyhow::{Result, ensure};
use tokio_util::sync::CancellationToken;

use super::{
    Runtime,
    audit::Kind,
    channel::{Control, Identity, Mode},
    policy::SessionExpiry,
};

const REQUIRED: &str = "HOST_AUTH_REQUIRED：請由本機安全中心限時授權。";
const CHANGED: &str = "HOST_AUTH_CHANGED：連線、政策或授權已變更。";

#[derive(PartialEq, Eq)]
struct Stamp {
    identity: Identity,
    revision: String,
    generation: u64,
}

/// Captured before a command is prepared or stdin is queued. Check it again at
/// the point of use. Only the in-process caller holds this lease snapshot.
pub struct Permit(Stamp);

pub struct SessionAuthority {
    grant: Stamp,
    cancel: CancellationToken,
}

impl SessionAuthority {
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancel
    }
}

#[derive(Default)]
pub(super) struct Sessions(Vec<Weak<SessionAuthority>>);

impl Sessions {
    fn track(&mut self, grant: Stamp) -> Arc<SessionAuthority> {
        let session = Arc::new(SessionAuthority {
            grant,
            cancel: CancellationToken::new(),
        });
        self.0.push(Arc::downgrade(&session));
        session
    }

    pub(super) fn enforce(&mut self, control: Option<&Control>) {
        self.0.retain(|entry| {
            let Some(session) = entry.upgrade() else {
                return false;
            };
            let stop = control.is_none_or(|control| {
                control.policy.session_expiry == SessionExpiry::Terminate
                    && control.host_lease_required()
                    && !control.host_authorized()
            });
            if stop {
                session.cancel.cancel();
            }
            true
        });
    }
}

impl Runtime {
    fn host_stamp(&mut self) -> Result<Stamp> {
        self.require_mode(Mode::HostControl)?;
        let managed = self.managed.as_ref().expect("verified managed mode");
        ensure!(managed.control.host_authorized(), "{REQUIRED}");
        Ok(Stamp {
            identity: managed.client.identity,
            revision: managed.control.revision.clone(),
            generation: managed.control.lease.generation,
        })
    }

    fn audit_host_denial<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err()
            && (self.record(Kind::CommandDenied, &[]).is_err() || self.report().is_err())
        {
            self.fail();
        }
        result
    }

    pub fn prepare_host_action(&mut self) -> Result<Permit> {
        let result = self.host_stamp().map(Permit);
        self.audit_host_denial(result)
    }

    fn recheck_host_action(&mut self, permit: Permit) -> Result<Stamp> {
        let result = self.host_stamp().and_then(|current| {
            ensure!(current == permit.0, "{CHANGED}");
            Ok(current)
        });
        self.audit_host_denial(result)
    }

    pub fn check_host_action(&mut self, permit: &Permit) -> Result<()> {
        let result = self.host_stamp().and_then(|current| {
            ensure!(current == permit.0, "{CHANGED}");
            Ok(())
        });
        self.audit_host_denial(result)
    }

    pub fn begin_host_session(&mut self, permit: Permit) -> Result<Arc<SessionAuthority>> {
        let grant = self.recheck_host_action(permit)?;
        Ok(self.host_sessions.track(grant))
    }

    pub fn authorize_host_input(
        &mut self,
        permit: Permit,
        session: &SessionAuthority,
    ) -> Result<()> {
        let grant = self.recheck_host_action(permit)?;
        let result = (|| {
            ensure!(grant.identity == session.grant.identity, "{CHANGED}");
            ensure!(!session.cancel.is_cancelled(), "{CHANGED}");
            Ok(())
        })();
        self.audit_host_denial(result)
    }
}

#[cfg(test)]
mod tests;
