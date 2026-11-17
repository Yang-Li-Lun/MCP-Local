use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use tokio_util::sync::CancellationToken;

pub const TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_BYTES: usize = 1024 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 300_000;
pub const MAX_WORKERS: usize = 4;
pub const MAX_OUTPUT: usize = 2 * 1024 * 1024;

pub struct Budget {
    deadline: Instant,
    cancel: CancellationToken,
    bytes: usize,
    entries: usize,
    byte_limit: usize,
}

impl Budget {
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            deadline: Instant::now() + TIMEOUT,
            cancel,
            bytes: 0,
            entries: 0,
            byte_limit: MAX_BYTES,
        }
    }

    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.cancel.is_cancelled(),
            "OPERATION_CANCELLED：操作已取消。"
        );
        ensure!(
            Instant::now() < self.deadline,
            "OPERATION_TIMEOUT：操作超過期限，請縮小範圍。"
        );
        ensure!(
            self.bytes <= self.byte_limit && self.entries <= MAX_ENTRIES,
            "OPERATION_RESOURCE_LIMIT：操作累計預算已耗盡。"
        );
        Ok(())
    }

    pub fn read(&mut self, bytes: usize) -> Result<()> {
        self.bytes += bytes;
        self.check()
    }

    pub fn restrict_read_bytes(&mut self, bytes: usize) {
        self.byte_limit = self.byte_limit.min(self.bytes.saturating_add(bytes));
    }

    pub fn can_read(&self, bytes: usize) -> Result<()> {
        ensure!(
            bytes <= self.byte_limit.saturating_sub(self.bytes),
            "OPERATION_RESOURCE_LIMIT：操作累計讀取預算已耗盡。"
        );
        self.check()
    }

    pub fn entry(&mut self) -> Result<()> {
        self.entries += 1;
        self.check()
    }
}
