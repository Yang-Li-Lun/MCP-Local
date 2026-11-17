use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::{
    budget::Budget,
    filesystem::{Root, vm_mapping::Plan, vm_spool::Directory},
};

pub(super) const MAX_OUTPUT: usize = 1024 * 1024;
pub(super) const MAX_STDIN: usize = 4096;
const MAX_COMMAND: usize = 8192;
const PROTOCOL: &str = "VM_GUEST_PROTOCOL：VM 回傳封包格式或順序無效。";
const STDIN_CLOSED: &str = "VM_STDIN_CLOSED：VM 已無法接收輸入。";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GuestResult {
    pub exit_code: i32,
    pub error: Option<GuestError>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum GuestError {
    #[serde(rename = "GUEST_IO_FAILED")]
    GuestIoFailed,
    #[serde(rename = "GUEST_OUTPUT_LIMIT")]
    OutputLimit,
}

#[derive(Clone, Copy)]
enum Status {
    Booting,
    Running,
    Finished(GuestResult),
    Closing,
    Closed,
}

pub struct Spool {
    directory: Directory,
    status: Status,
    input_sequence: u64,
    stdin_closed: bool,
    output: Vec<u8>,
}

pub(super) fn validate_command(command: &str) -> Result<()> {
    ensure!(
        !command.trim().is_empty()
            && !command.contains('\0')
            && command.chars().count() <= MAX_COMMAND,
        "COMMAND_LIMIT：命令必須為 1 至 8192 字元。"
    );
    Ok(())
}

impl Spool {
    pub fn create(root: &Root) -> Result<Self> {
        Ok(Self {
            directory: Directory::create(root)?,
            status: Status::Booting,
            input_sequence: 0,
            stdin_closed: false,
            output: Vec::new(),
        })
    }

    pub fn name(&self) -> &str {
        self.directory.name()
    }

    pub fn guest_path(&self, root_id: &str, plan: &Plan) -> Result<String> {
        ensure!(matches!(self.status, Status::Booting), "{PROTOCOL}");
        plan.guest_spool_path(root_id, &self.directory)
    }

    pub fn request_if_ready(
        &mut self,
        command: &str,
        root_id: &str,
        plan: &Plan,
        budget: &mut Budget,
    ) -> Result<bool> {
        validate_command(command)?;
        ensure!(matches!(self.status, Status::Booting), "{PROTOCOL}");
        let Some(ready) = self.directory.snapshot("ready.json", MAX_OUTPUT, budget)? else {
            return Ok(false);
        };
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Ready {
            ready: bool,
        }
        let ready: Ready = serde_json::from_slice(&ready).context(PROTOCOL)?;
        ensure!(ready.ready, "{PROTOCOL}");
        plan.verify_spool(root_id, &self.directory, budget)?;
        #[derive(Serialize)]
        struct Request<'a> {
            command: &'a str,
        }
        self.publish("request.json", &Request { command }, budget)?;
        self.status = Status::Running;
        Ok(true)
    }

    pub fn write_stdin(&mut self, bytes: &[u8], eof: bool, budget: &mut Budget) -> Result<usize> {
        ensure!(
            bytes.len() <= MAX_STDIN,
            "STDIN_LIMIT：單次輸入最多 4096 bytes。"
        );
        self.require_stdin()?;
        #[derive(Serialize)]
        struct Input {
            data: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            eof: Option<bool>,
        }
        let next = self.input_sequence.checked_add(1).context(PROTOCOL)?;
        let file = format!("stdin-{:06}.json", self.input_sequence);
        self.publish(
            &file,
            &Input {
                data: STANDARD.encode(bytes),
                eof: eof.then_some(true),
            },
            budget,
        )?;
        self.input_sequence = next;
        self.stdin_closed = eof;
        Ok(bytes.len())
    }

    pub fn refresh_output(&mut self, budget: &mut Budget) -> Result<()> {
        self.require_readable()?;
        let Some(output) = self.directory.snapshot("output.txt", MAX_OUTPUT, budget)? else {
            return Ok(());
        };
        ensure!(
            output.starts_with(&self.output),
            "VM_OUTPUT_CHANGED：VM 已回傳的輸出被改寫或截斷。"
        );
        self.output = output;
        Ok(())
    }

    pub fn output(&self) -> &[u8] {
        &self.output
    }

    pub fn result(&mut self, budget: &mut Budget) -> Result<Option<GuestResult>> {
        self.require_readable()?;
        if let Status::Finished(result) = self.status {
            return Ok(Some(result));
        }
        let Some(result) = self.directory.snapshot("result.json", MAX_OUTPUT, budget)? else {
            return Ok(None);
        };
        let result: GuestResult = serde_json::from_slice(&result).context(PROTOCOL)?;
        ensure!(
            matches!(self.status, Status::Running)
                || result.error.is_some() && result.exit_code != 0,
            "{PROTOCOL}"
        );
        self.status = Status::Finished(result);
        self.stdin_closed = true;
        Ok(Some(result))
    }

    pub fn close_after_stop(
        &mut self,
        budget: &mut Budget,
        confirm_stopped: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        if matches!(self.status, Status::Closed) {
            return Ok(());
        }
        self.status = Status::Closing;
        confirm_stopped()?;
        self.directory.remove(budget)?;
        self.status = Status::Closed;
        Ok(())
    }

    fn publish(&self, filename: &str, payload: &impl Serialize, budget: &mut Budget) -> Result<()> {
        self.directory
            .publish(filename, &serde_json::to_vec(payload)?, budget)
    }

    fn require_stdin(&self) -> Result<()> {
        ensure!(
            !self.stdin_closed && matches!(self.status, Status::Booting | Status::Running),
            "{STDIN_CLOSED}"
        );
        Ok(())
    }

    fn require_readable(&self) -> Result<()> {
        ensure!(
            !matches!(self.status, Status::Closing | Status::Closed),
            "{PROTOCOL}"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
