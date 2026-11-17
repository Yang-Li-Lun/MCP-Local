use std::collections::BTreeSet;

use anyhow::{Context, Error, Result, ensure};
use tokio_util::sync::CancellationToken;
use uuid::{Builder, Uuid};

mod cli;
pub mod guard;
mod guest;
mod session;
pub mod sessions;
mod spool;

const MAX_CONFIGURATION: usize = 28_000;
const START_CANCELLED: &str = "VM_START_CANCELLED：已取消 Windows Sandbox 啟動。";
const STOP_UNCONFIRMED: &str = "VM_STOP_UNCONFIRMED：未確認專屬 VM 停止，禁止重連。";
const VM_BUSY: &str = "VM_BUSY：已有 Windows Sandbox，禁止接管其他工作階段。";

pub fn availability() -> (bool, &'static str) {
    match cli::Cli::resolve() {
        Ok(_) => (true, "READY_TO_START"),
        Err(_) => (false, "VM_BACKEND_UNAVAILABLE"),
    }
}

trait Backend {
    type Connector;

    fn list(&mut self) -> Result<BTreeSet<Uuid>>;
    fn start(&mut self, id: Uuid, configuration: &str) -> Result<Uuid, InvocationFailure>;
    fn connect(&mut self, id: Uuid) -> Result<Self::Connector>;
    fn stop(&mut self, id: Uuid) -> Result<()>;
    fn close_connector(&mut self, connector: &mut Self::Connector) -> Result<()>;
}

#[derive(Debug)]
enum InvocationFailure {
    BeforeSpawn(Error),
    AfterSpawn(Error),
}

impl InvocationFailure {
    fn error(self) -> Error {
        match self {
            Self::BeforeSpawn(error) | Self::AfterSpawn(error) => error,
        }
    }
}

struct OwnedVm<C> {
    id: Uuid,
    observed: bool,
    connector: Option<C>,
}

struct Lifecycle<B: Backend> {
    backend: B,
    owned: Option<OwnedVm<B::Connector>>,
}

impl Lifecycle<cli::Cli> {
    fn open() -> Result<Self> {
        Ok(Self::new(cli::Cli::resolve()?))
    }
}

impl<B: Backend> Lifecycle<B> {
    fn new(backend: B) -> Self {
        Self {
            backend,
            owned: None,
        }
    }

    // The guard passes only its validated configuration from the approved mapping plan.
    fn start(&mut self, configuration: &str, cancel: &CancellationToken) -> Result<Uuid> {
        ensure!(self.owned.is_none(), "{VM_BUSY}");
        ensure!(
            configuration.encode_utf16().count() <= MAX_CONFIGURATION,
            "VM_CONFIG_LIMIT"
        );
        ensure!(!cancel.is_cancelled(), "{START_CANCELLED}");
        ensure!(self.backend.list()?.is_empty(), "{VM_BUSY}");
        ensure!(!cancel.is_cancelled(), "{START_CANCELLED}");
        let mut random = [0; 16];
        getrandom::fill(&mut random)?;
        let id = Builder::from_random_bytes(random).into_uuid();
        // Record ownership before the CLI can send the start request.
        self.owned = Some(OwnedVm {
            id,
            observed: false,
            connector: None,
        });
        let result = match self.backend.start(id, configuration) {
            Ok(returned) if returned == id => {
                self.owned.as_mut().unwrap().observed = true;
                self.connect(id, cancel)
            }
            Ok(_) => Err(anyhow::anyhow!("VM_CLI_PROTOCOL：啟動回報的 VM ID 不符。")),
            Err(InvocationFailure::BeforeSpawn(error)) => {
                self.owned = None;
                return Err(error);
            }
            Err(InvocationFailure::AfterSpawn(error)) => Err(error),
        };
        if result.is_err() {
            self.stop()?;
        }
        result.map(|()| id)
    }

    fn connect(&mut self, id: Uuid, cancel: &CancellationToken) -> Result<()> {
        ensure!(!cancel.is_cancelled(), "{START_CANCELLED}");
        let connector = self.backend.connect(id)?;
        self.owned.as_mut().unwrap().connector = Some(connector);
        ensure!(!cancel.is_cancelled(), "{START_CANCELLED}");
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        let Some(vm) = &mut self.owned else {
            return Ok(());
        };
        (|| -> Result<()> {
            let listed = self.backend.list()?;
            if listed.contains(&vm.id) {
                vm.observed = true;
                self.backend.stop(vm.id)?;
            }
            // Absence on one list is inconclusive if a start request timed out
            // or returned an invalid acknowledgement and has never been observed.
            ensure!(vm.observed, "未取得啟動回報或觀察到請求的 VM。");
            ensure!(
                !self.backend.list()?.contains(&vm.id),
                "VM 仍在 Sandbox 清單中。"
            );
            if let Some(connector) = &mut vm.connector {
                self.backend.close_connector(connector)?;
            }
            Ok(())
        })()
        .context(STOP_UNCONFIRMED)?;
        self.owned = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
