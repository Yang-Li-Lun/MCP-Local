use std::{collections::HashSet, path::PathBuf};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::filesystem::PrivateDirectory;

use super::storage;

const POLICY: &str = "security-policy.json";
const MARKER: &str = "security-policy.present";
const UPDATING: &str = "security-policy.pending";
const PRESENT: &[u8] = b"MCP-Local Rust security policy v1\n";
const MAX_BYTES: usize = 65536;
const BUILTINS: [&str; 5] = ["private_key", "api_key", "bearer", "password", "email"];
const INVALID: &str = "SECURITY_POLICY_INVALID：政策無法驗證，原檔保留。";

#[derive(Debug)]
pub struct PendingSave;

impl std::fmt::Display for PendingSave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SECURITY_POLICY_PENDING：政策與安全事件尚未一起確認。")
    }
}

impl std::error::Error for PendingSave {}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    #[default]
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Mask,
    Block,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionExpiry {
    Terminate,
    Finish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    Builtin,
    Literal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub kind: RuleKind,
    pub pattern: String,
    pub enabled: bool,
    pub risk: Risk,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    version: u8,
    pub enabled: bool,
    pub host_authorization: bool,
    pub detector_enabled: bool,
    pub response_guard: bool,
    pub strategy: Strategy,
    pub session_expiry: SessionExpiry,
    pub max_lease_minutes: u8,
    pub rules: Vec<Rule>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            version: 1,
            enabled: false,
            host_authorization: true,
            detector_enabled: true,
            response_guard: true,
            strategy: Strategy::Mask,
            session_expiry: SessionExpiry::Terminate,
            max_lease_minutes: 15,
            rules: BUILTINS
                .into_iter()
                .map(|name| Rule {
                    id: name.into(),
                    kind: RuleKind::Builtin,
                    pattern: name.into(),
                    enabled: true,
                    risk: if name == "email" {
                        Risk::Medium
                    } else {
                        Risk::High
                    },
                })
                .collect(),
        }
    }
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "SECURITY_POLICY_SCHEMA：政策版本無效。");
        ensure!(
            (1..=60).contains(&self.max_lease_minutes),
            "SECURITY_POLICY_DURATION：授權上限為 1 至 60 分鐘。"
        );
        ensure!(
            !self.enabled || !self.response_guard || self.detector_enabled,
            "SECURITY_POLICY_DEPENDENCY：回傳防護需要啟用偵測器。"
        );
        ensure!(
            (1..=32).contains(&self.rules.len()),
            "SECURITY_RULE_LIMIT：需要 1 至 32 筆規則。"
        );
        let mut seen = HashSet::new();
        for rule in &self.rules {
            ensure!(
                rule.id.len() <= 40
                    && rule
                        .id
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_lowercase)
                    && rule.id.bytes().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_')
                    && seen.insert(rule.id.as_str()),
                "SECURITY_RULE_ID：規則識別碼無效或重複。"
            );
            match rule.kind {
                RuleKind::Builtin => ensure!(
                    BUILTINS.contains(&rule.pattern.as_str()),
                    "SECURITY_RULE_BUILTIN：未知內建規則。"
                ),
                RuleKind::Literal => ensure!(
                    (3..=128).contains(&rule.pattern.chars().count())
                        && !rule.pattern.contains('\0'),
                    "SECURITY_RULE_LITERAL：固定文字長度為 3 至 128 字元。"
                ),
            }
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub policy: Policy,
    pub revision: String,
}

#[derive(Clone)]
pub struct Store(pub PathBuf);

impl Store {
    pub fn load(&self) -> Result<Snapshot> {
        match storage::directory(&self.0)? {
            Some(directory) => Self::read(&directory),
            None => Ok(Snapshot::default()),
        }
    }

    fn read(directory: &PrivateDirectory) -> Result<Snapshot> {
        if storage::read(directory, UPDATING, 1)?.is_some() {
            return Err(PendingSave.into());
        }
        let marker = storage::read(directory, MARKER, PRESENT.len())?;
        let bytes = storage::read(directory, POLICY, MAX_BYTES)?;
        if storage::read(directory, UPDATING, 1)?.is_some() {
            return Err(PendingSave.into());
        }
        ensure!(
            marker.as_deref().is_none_or(|bytes| bytes == PRESENT),
            "{INVALID}"
        );
        let Some(bytes) = bytes else {
            ensure!(
                marker.is_none(),
                "SECURITY_POLICY_MISSING：已建立的政策遺失。"
            );
            return Ok(Snapshot::default());
        };
        ensure!(
            bytes.len() <= MAX_BYTES,
            "SECURITY_POLICY_SIZE：政策超出容量。"
        );
        let invalid = |_| anyhow::anyhow!(INVALID);
        let value = storage::parse_unique(&bytes).map_err(invalid)?;
        let policy: Policy = serde_json::from_value(value).map_err(|_| anyhow::anyhow!(INVALID))?;
        policy.validate().map_err(invalid)?;
        Ok(Snapshot {
            policy,
            revision: storage::digest(&bytes),
        })
    }

    #[cfg(any(feature = "desktop", test))]
    pub fn save(&self, policy: Policy, expected: &str) -> Result<Snapshot> {
        use super::audit::{Audit, Event, Kind};

        policy.validate()?;
        let bytes = storage::encoded(&policy)?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "SECURITY_POLICY_SIZE：政策超出容量。"
        );
        let directory = PrivateDirectory::open(&self.0, true)?;
        storage::transaction(
            &directory,
            "security-policy.lock",
            "SECURITY_POLICY_BUSY：其他儲存作業未完成。",
            || {
                ensure!(
                    Self::read(&directory)?.revision == expected,
                    "SECURITY_POLICY_CONFLICT：政策已變更，請重新載入。"
                );
                let audit = Audit(self.0.clone());
                audit.load()?;
                let event = Event::new(Kind::PolicySaved, Risk::Low, 1)?;
                // The runtime blocks replies for the entire commit, including a
                // partial commit whose audit event could not be persisted.
                storage::replace(&directory, UPDATING, b"v1")?;
                if storage::read(&directory, MARKER, PRESENT.len())?.is_none() {
                    storage::replace(&directory, MARKER, PRESENT)?;
                }
                storage::replace(&directory, POLICY, &bytes)?;
                audit.append(event)?;
                std::fs::remove_file(directory.path().join(UPDATING))?;
                Ok(Snapshot {
                    policy,
                    revision: storage::digest(&bytes),
                })
            },
        )
    }
}
