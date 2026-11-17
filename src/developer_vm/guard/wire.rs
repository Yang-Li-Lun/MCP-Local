use std::time::Duration;

use anyhow::{Context, Result, ensure};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Sha256;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::timeout,
};
use zeroize::Zeroizing;

use crate::developer_vm::session::{Mappings, Progress};

pub const FRAME_TIMEOUT: Duration = Duration::from_secs(3);
const FRAME_MAX: usize = 128 * 1024;
pub const PROTOCOL: &str = "VM_GUARD_PROTOCOL：無法驗證 VM 清理通訊。";

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Hello {},
    Prepare {
        command: String,
        root_id: Option<String>,
        working_directory: Option<String>,
    },
    Start {
        lifetime_seconds: u32,
    },
    Poll {
        id: String,
        offset: usize,
    },
    Input {
        id: String,
        text: String,
        eof: bool,
    },
    Terminate {
        id: String,
    },
    Stop {
        id: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Ready { scope: [u8; 32] },
    Prepared { mappings: Mappings },
    Started { id: String },
    Progress { page: Progress },
    InputAccepted { bytes: usize, eof: bool },
    Stopped {},
    Rejected {},
    StopUnconfirmed {},
}

pub struct Auth<'a> {
    pub key: &'a [u8; 32],
    pub nonce: [u8; 32],
}

#[derive(Clone, Copy)]
pub enum Direction {
    Request,
    Reply,
}

impl Auth<'_> {
    fn mac(&self, direction: Direction, sequence: u64, body: &[u8]) -> Hmac<Sha256> {
        let mut mac = Hmac::new_from_slice(self.key).expect("fixed HMAC key");
        mac.update(b"MCP-Local-Rust/VM-Guard/1");
        mac.update(&self.nonce);
        mac.update(match direction {
            Direction::Request => b"Q",
            Direction::Reply => b"R",
        });
        mac.update(&sequence.to_le_bytes());
        mac.update(body);
        mac
    }

    pub async fn read<T: DeserializeOwned>(
        &self,
        input: &mut (impl AsyncRead + Unpin),
        direction: Direction,
        sequence: u64,
    ) -> Result<T> {
        let mut size = [0; 4];
        // An idle connection can own a VM. Only a partially received frame times out.
        size[0] = input.read_u8().await.context(PROTOCOL)?;
        timeout(FRAME_TIMEOUT, async {
            input.read_exact(&mut size[1..]).await.context(PROTOCOL)?;
            let size = u32::from_le_bytes(size) as usize;
            ensure!((33..=FRAME_MAX).contains(&size), "{PROTOCOL}");
            let mut bytes = Zeroizing::new(vec![0; size]);
            input.read_exact(&mut bytes).await.context(PROTOCOL)?;
            let (signature, body) = bytes.split_at(32);
            self.mac(direction, sequence, body)
                .verify_slice(signature)
                .context(PROTOCOL)?;
            serde_json::from_slice(body).context(PROTOCOL)
        })
        .await
        .context(PROTOCOL)?
    }

    pub async fn write<T: Serialize>(
        &self,
        output: &mut (impl AsyncWrite + Unpin),
        direction: Direction,
        sequence: u64,
        message: &T,
    ) -> Result<()> {
        let body = Zeroizing::new(serde_json::to_vec(message)?);
        let size = body.len() + 32;
        ensure!(size <= FRAME_MAX, "{PROTOCOL}");
        let signature = self.mac(direction, sequence, &body).finalize().into_bytes();
        timeout(FRAME_TIMEOUT, async {
            output.write_all(&(size as u32).to_le_bytes()).await?;
            output.write_all(&signature).await?;
            output.write_all(&body).await
        })
        .await
        .context(PROTOCOL)?
        .context(PROTOCOL)
    }
}
