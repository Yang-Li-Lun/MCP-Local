use serde::{Deserialize, Serialize};
use std::{
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub source_sha256: String,
    pub target: String,
    pub compiler: String,
    pub process_id: u32,
    pub instance_id: Option<String>,
    pub started_unix_ms: u64,
}

pub fn current() -> &'static Identity {
    static IDENTITY: OnceLock<Identity> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        let mut random = [0; 16];
        Identity {
            source_sha256: env!("MCP_SOURCE_SHA256").into(),
            target: env!("MCP_BUILD_TARGET").into(),
            compiler: env!("MCP_BUILD_COMPILER").into(),
            process_id: std::process::id(),
            instance_id: getrandom::fill(&mut random)
                .ok()
                .map(|()| crate::listing::hex(&random)),
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        }
    })
}
