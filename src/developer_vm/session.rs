use std::{collections::BTreeMap, time::Instant};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::budget::Budget;

use super::{
    Backend, Lifecycle,
    guest::Guest,
    spool::{GuestResult, MAX_OUTPUT},
};

const PAGE_BYTES: usize = 32 * 1024;
pub(super) const INVALID_PAGE: &str = "VM_OUTPUT_PAGE：VM 輸出分頁無效。";

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Mappings {
    pub guest_roots: BTreeMap<String, String>,
    pub guest_toolchains: Vec<String>,
}

pub struct Launch {
    pub vm_id: String,
    pub mappings: std::sync::Arc<Mappings>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub offset: usize,
    pub next_offset: usize,
    pub total_bytes: usize,
    pub command_sent: bool,
    pub result: Option<GuestResult>,
    pub terminated: bool,
    output_base64: String,
}

impl Progress {
    fn from_output(
        bytes: &[u8],
        requested_offset: usize,
        command_sent: bool,
        result: Option<GuestResult>,
        terminated: bool,
    ) -> Self {
        let offset = requested_offset.min(bytes.len());
        let next_offset = (offset + PAGE_BYTES).min(bytes.len());
        Self {
            offset,
            next_offset,
            total_bytes: bytes.len(),
            command_sent,
            result,
            terminated,
            output_base64: STANDARD.encode(&bytes[offset..next_offset]),
        }
    }

    pub fn bytes(&self) -> Result<Vec<u8>> {
        ensure!(
            self.offset <= self.next_offset
                && self.next_offset <= self.total_bytes
                && self.total_bytes <= MAX_OUTPUT
                && self.next_offset - self.offset <= PAGE_BYTES
                && self.output_base64.len() <= 4 * PAGE_BYTES.div_ceil(3),
            "{INVALID_PAGE}"
        );
        let bytes = STANDARD.decode(&self.output_base64).context(INVALID_PAGE)?;
        ensure!(
            bytes.len() == self.next_offset - self.offset,
            "{INVALID_PAGE}"
        );
        Ok(bytes)
    }
}

pub(super) struct Session {
    guest: Guest,
    command: Option<Zeroizing<String>>,
    command_sent: bool,
    pub(super) id: Option<Uuid>,
    pub(super) deadline: Option<Instant>,
    finished: Option<GuestResult>,
    terminated: bool,
}

impl Session {
    pub(super) fn new(guest: Guest, command: Zeroizing<String>, deadline: Instant) -> Self {
        Self {
            guest,
            command: Some(command),
            command_sent: false,
            id: None,
            deadline: Some(deadline),
            finished: None,
            terminated: false,
        }
    }

    pub(super) fn start<B: Backend>(
        &mut self,
        lifecycle: &mut Lifecycle<B>,
        deadline: Instant,
        budget: &mut Budget,
        cancel: &CancellationToken,
    ) -> Result<Uuid> {
        ensure!(self.id.is_none(), "VM_ALREADY_STARTED");
        self.deadline = Some(deadline);
        let id = self.guest.start(lifecycle, budget, cancel)?;
        self.id = Some(id);
        Ok(id)
    }

    pub(super) fn poll<B: Backend>(
        &mut self,
        lifecycle: &mut Lifecycle<B>,
        offset: usize,
        budget: &mut Budget,
    ) -> Result<Progress> {
        if self.finished.is_none() && !self.terminated {
            // Read a pre-existing terminal packet before attempting to publish a command.
            let mut result = self.guest.result(budget)?;
            if result.is_none()
                && let Some(command) = &self.command
                && self.guest.submit(command, budget)?
            {
                self.command = None;
                self.command_sent = true;
                result = self.guest.result(budget)?;
            }
            self.guest.refresh_output(budget)?;
            if let Some(result) = result {
                lifecycle.stop()?;
                self.guest.refresh_output(budget)?;
                self.command = None;
                self.finished = Some(result);
                self.deadline = None;
            }
        }
        Ok(Progress::from_output(
            self.guest.output(),
            offset,
            self.command_sent,
            self.finished,
            self.terminated,
        ))
    }

    pub(super) fn terminate<B: Backend>(
        &mut self,
        lifecycle: &mut Lifecycle<B>,
        budget: &mut Budget,
    ) -> Result<()> {
        if self.finished.is_none() && !self.terminated {
            self.command = None;
            lifecycle.stop()?;
            self.deadline = None;
            self.terminated = true;
            self.guest.refresh_output(budget)?;
        }
        Ok(())
    }

    pub(super) fn input(&mut self, bytes: &[u8], eof: bool, budget: &mut Budget) -> Result<usize> {
        ensure!(
            self.finished.is_none() && !self.terminated,
            "SESSION_FINISHED：VM 已停止。"
        );
        self.guest.input(bytes, eof, budget)
    }

    pub(super) fn close<B: Backend>(&mut self, lifecycle: &mut Lifecycle<B>) -> Result<()> {
        self.command = None;
        self.guest.close(lifecycle)
    }
}
