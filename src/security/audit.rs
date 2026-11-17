use std::{path::PathBuf, time::SystemTime};

use anyhow::{Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::{filesystem::PrivateDirectory, listing::hex};

use super::{policy::Risk, storage};

const FILE: &str = "security-audit.jsonl";
const LOCK: &str = "security-audit.lock";
const MAX_BYTES: usize = 4 * 1024 * 1024;
const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const BUSY: &str = "SECURITY_AUDIT_BUSY：其他事件寫入尚未完成。";
const CAPACITY: &str = "SECURITY_AUDIT_CAPACITY：請匯出後清除，禁止遺失事件。";
const INTEGRITY: &str = "SECURITY_AUDIT_INTEGRITY：事件鏈無法驗證，原檔保留。";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    PolicySaved,
    AuthorizationGranted,
    AuthorizationRevoked,
    AuthorizationExpired,
    ChannelFailed,
    ChannelRecovered,
    ResponseMasked,
    ResponseBlocked,
    ScanFailed,
    CommandDenied,
    SessionStopped,
    SessionStopFailed,
    SyntheticTest,
    AuditCleared,
    EventsLost,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub id: String,
    pub time: String,
    pub kind: Kind,
    pub risk: Risk,
    pub count: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    prior_head: Option<String>,
}

impl Event {
    pub fn new(kind: Kind, risk: Risk, count: u32) -> Result<Self> {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce)?;
        Ok(Self {
            id: hex(&nonce),
            time: DateTime::<Utc>::from(SystemTime::now())
                .to_rfc3339_opts(SecondsFormat::Micros, false),
            kind,
            risk,
            count,
            prior_head: None,
        })
    }

    fn valid(&self) -> bool {
        lower_hex(&self.id, 32)
            && self.time.len() <= 40
            && DateTime::parse_from_rfc3339(&self.time).is_ok()
            && (1..=1_000_000).contains(&self.count)
            && self
                .prior_head
                .as_ref()
                .is_none_or(|prior| self.kind == Kind::AuditCleared && lower_hex(prior, 64))
    }
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub event: Event,
    pub previous: String,
    pub hash: String,
}

fn hash(event: &Event, previous: &str) -> Result<String> {
    #[derive(Serialize)]
    struct Content<'a> {
        event: &'a Event,
        previous: &'a str,
    }
    Ok(storage::digest(&storage::encoded(&Content {
        event,
        previous,
    })?))
}

impl Row {
    fn new(event: Event, previous: &str) -> Result<Self> {
        ensure!(event.valid(), "SECURITY_AUDIT_EVENT：事件格式無效。");
        let hash = hash(&event, previous)?;
        Ok(Self {
            event,
            previous: previous.into(),
            hash,
        })
    }
}

#[derive(Clone)]
pub struct Audit(pub PathBuf);

fn revision(rows: &[Row]) -> &str {
    rows.last().map_or(ZERO, |row| &row.hash)
}

impl Audit {
    pub fn load(&self) -> Result<Vec<Row>> {
        match storage::directory(&self.0)? {
            Some(directory) => Self::read(&directory),
            None => Ok(Vec::new()),
        }
    }

    fn read(directory: &PrivateDirectory) -> Result<Vec<Row>> {
        let Some(data) = storage::read(directory, FILE, MAX_BYTES)? else {
            return Ok(Vec::new());
        };
        ensure!(data.len() <= MAX_BYTES, "{CAPACITY}");
        let mut rows = Vec::new();
        let mut input = data.as_slice();
        if let Some(stripped) = input.strip_suffix(b"\n") {
            input = stripped;
        }
        if input.is_empty() && data.is_empty() {
            return Ok(rows);
        }
        for line in input.split(|byte| *byte == b'\n') {
            let row = storage::parse_unique(line)
                .and_then(|value| Ok(serde_json::from_value::<Row>(value)?))
                .map_err(|_| anyhow::anyhow!(INTEGRITY))?;
            ensure!(
                row.event.valid()
                    && row.previous == revision(&rows)
                    && row.hash == hash(&row.event, &row.previous)?,
                "{INTEGRITY}"
            );
            rows.push(row);
        }
        Ok(rows)
    }

    fn replace(directory: &PrivateDirectory, rows: &[Row]) -> Result<()> {
        let mut bytes = Vec::new();
        for row in rows {
            let line = storage::encoded(row)?;
            ensure!(bytes.len() + line.len() < MAX_BYTES, "{CAPACITY}");
            bytes.extend_from_slice(&line);
            bytes.push(b'\n');
        }
        storage::replace(directory, FILE, &bytes)
    }

    pub fn append(&self, event: Event) -> Result<()> {
        self.append_batch(vec![event])
    }

    pub fn append_batch(&self, events: Vec<Event>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let directory = PrivateDirectory::open(&self.0, true)?;
        storage::transaction(&directory, LOCK, BUSY, || {
            let mut rows = Self::read(&directory)?;
            let mut ids: std::collections::HashSet<_> =
                rows.iter().map(|row| row.event.id.clone()).collect();
            let count = rows.len();
            for event in events {
                ensure!(event.valid(), "SECURITY_AUDIT_EVENT：事件格式無效。");
                if ids.insert(event.id.clone()) {
                    rows.push(Row::new(event, revision(&rows))?);
                }
            }
            if rows.len() != count {
                Self::replace(&directory, &rows)?;
            }
            Ok(())
        })
    }

    #[cfg(any(feature = "desktop", test))]
    pub fn export(&self) -> Result<serde_json::Value> {
        let rows = self.load()?;
        Ok(serde_json::json!({
            "version": 1, "contains_sensitive_originals": false, "chain": rows,
            "revision": revision(&rows), "integrity": "sha256_chain_not_account_isolation"
        }))
    }

    #[cfg(any(feature = "desktop", test))]
    pub fn clear(&self, expected: &str, confirmation: &str) -> Result<()> {
        ensure!(
            confirmation == "清除安全事件",
            "SECURITY_AUDIT_CONFIRM：請輸入「清除安全事件」。"
        );
        let directory = PrivateDirectory::open(&self.0, true)?;
        storage::transaction(&directory, LOCK, BUSY, || {
            let rows = Self::read(&directory)?;
            ensure!(
                revision(&rows) == expected,
                "SECURITY_AUDIT_CONFLICT：事件已更新，請重新檢視。"
            );
            let mut event = Event::new(Kind::AuditCleared, Risk::Low, rows.len().max(1) as u32)?;
            event.prior_head = Some(expected.into());
            Self::replace(&directory, &[Row::new(event, ZERO)?])
        })
    }
}
