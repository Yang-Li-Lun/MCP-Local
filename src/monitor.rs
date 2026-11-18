use std::{
    env, io,
    os::windows::io::{FromRawHandle, OwnedHandle},
    ptr::null,
    time::Instant,
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, SYSTEMTIME},
    System::{
        SystemInformation::{GetLocalTime, GetSystemTime},
        Threading::{CreateEventW, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE},
    },
};

use crate::{
    listing::hex,
    local_mapping::{Locked, Mapping},
    security::channel::Mode,
};

pub mod presentation;
mod transport;
pub use transport::ObservedTransport;

pub const ENV_NAME: &str = "MCP_LOCAL_RUST_MONITOR";
const PREFIX: &str = r"Local\MCP-Local-Rust.Monitor.";
const MAGIC: u64 = 0x4d43505255535433;
const SLOTS: usize = 16;
const SLOT: usize = 2 * 1024 * 1024 + 16 * 1024;
const HEADER: usize = 4096;
const SIZE: usize = HEADER + SLOTS * SLOT;
const CAPTURE: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Success,
    Error,
    Cancelled,
    Paused,
    Disconnected,
    Ambiguous,
    Incomplete,
    TransportError,
}

impl Status {
    pub const ALL: [Self; 9] = [
        Self::Running,
        Self::Success,
        Self::Error,
        Self::Cancelled,
        Self::Paused,
        Self::Disconnected,
        Self::Ambiguous,
        Self::Incomplete,
        Self::TransportError,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "等待中",
            Self::Success => "已回應",
            Self::Error => "請求錯誤",
            Self::Cancelled => "已取消",
            Self::Paused => "已暫停",
            Self::Disconnected => "已斷線",
            Self::Ambiguous => "無法對應",
            Self::Incomplete => "紀錄不完整",
            Self::TransportError => "傳輸錯誤",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub id: String,
    pub time: String,
    #[serde(default)]
    pub local_time: String,
    pub clock: String,
    pub tool: String,
    #[serde(default)]
    pub access_mode: Option<Mode>,
    pub status: Status,
    pub elapsed_ms: Option<f64>,
    pub request_notes: Vec<String>,
    pub response_notes: Vec<String>,
    pub request: Option<Value>,
    pub response: Option<Value>,
}

pub struct Bus {
    name: String,
    memory: Mapping,
}

fn valid(name: &str) -> bool {
    name.strip_prefix(PREFIX).is_some_and(|id| {
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

impl Bus {
    pub fn create() -> Result<Self> {
        let mut id = [0; 16];
        getrandom::fill(&mut id)?;
        let name = format!("{PREFIX}{}", hex(&id));
        let bus = Self::connect(name, true)?;
        let guard = bus
            .lock()
            .ok_or_else(|| anyhow::anyhow!("無法初始化請求監控。"))?;
        guard.write(0, &MAGIC.to_le_bytes());
        guard.write(8, &0u64.to_le_bytes());
        guard.write(16, &1u64.to_le_bytes());
        guard.write(24, &1u64.to_le_bytes());
        drop(guard);
        Ok(bus)
    }

    pub fn from_environment() -> Option<Self> {
        let name = env::var(ENV_NAME).ok()?;
        if !valid(&name) {
            return None;
        }
        let bus = Self::connect(name, false).ok()?;
        if bus.lock()?.number(0) != MAGIC {
            return None;
        }
        Some(bus)
    }

    fn connect(name: String, create: bool) -> Result<Self> {
        let memory = Mapping::connect(&name, SIZE, create)?;
        Ok(Self { name, memory })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn attachment_name(&self) -> Vec<u16> {
        self.name
            .encode_utf16()
            .chain(".Attached".encode_utf16())
            .chain([0])
            .collect()
    }

    fn mark_attached(&self) -> Option<OwnedHandle> {
        let bytes = serde_json::to_vec(crate::build_identity::current()).ok()?;
        if bytes.len() > HEADER - 40 {
            return None;
        }
        let guard = self.lock()?;
        guard.write(32, &(bytes.len() as u64).to_le_bytes());
        guard.write(40, &bytes);
        drop(guard);
        // The event exists while any observed STDIO transport owns its handle,
        // including when the transport has not captured a request yet.
        let handle = unsafe { CreateEventW(null(), 1, 0, self.attachment_name().as_ptr()) };
        (!handle.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    pub fn attached(&self) -> io::Result<bool> {
        let name = self.attachment_name();
        let handle = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
        if !handle.is_null() {
            drop(unsafe { OwnedHandle::from_raw_handle(handle) });
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
            Ok(false)
        } else {
            Err(error)
        }
    }

    pub fn runtime_identity(&self) -> Option<crate::build_identity::Identity> {
        if !self.attached().ok()? {
            return None;
        }
        let guard = self.lock()?;
        let length = usize::try_from(guard.number(32)).ok()?;
        if !(1..=HEADER - 40).contains(&length) {
            return None;
        }
        let mut bytes = vec![0; length];
        guard.read(40, &mut bytes);
        serde_json::from_slice(&bytes).ok()
    }

    fn lock(&self) -> Option<Locked<'_>> {
        self.memory.lock(20, true)
    }

    pub fn epoch(&self) -> Option<u64> {
        let guard = self.lock()?;
        (guard.number(0) == MAGIC && guard.number(24) == 1).then(|| guard.number(16))
    }

    pub fn change(&self, enabled: Option<bool>, clear: bool) -> Result<u64> {
        let guard = self
            .lock()
            .ok_or_else(|| anyhow::anyhow!("請求監控目前忙碌。"))?;
        ensure!(guard.number(0) == MAGIC, "請求監控映射無效。");
        let epoch = guard
            .number(16)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("請求監控 epoch 超限。"))?;
        if let Some(enabled) = enabled {
            guard.write(24, &u64::from(enabled).to_le_bytes());
        }
        guard.write(16, &epoch.to_le_bytes());
        if clear {
            // Clear all prior payloads while the writers are excluded.
            guard.zero(HEADER, SLOTS * SLOT);
        }
        Ok(guard.number(8))
    }

    pub fn publish(&self, epoch: u64, record: &Record) {
        let Ok(bytes) = serde_json::to_vec(record) else {
            return;
        };
        if bytes.len() > SLOT - 16 {
            return;
        }
        let Some(guard) = self.lock() else { return };
        if guard.number(0) != MAGIC || guard.number(24) != 1 || guard.number(16) != epoch {
            return;
        }
        let Some(next) = guard.number(8).checked_add(1) else {
            return;
        };
        let offset = slot_offset(next);
        guard.write(offset, &0u64.to_le_bytes());
        guard.write(offset + 8, &(bytes.len() as u64).to_le_bytes());
        guard.write(offset + 16, &bytes);
        guard.write(offset, &next.to_le_bytes());
        guard.write(8, &next.to_le_bytes());
    }

    pub fn poll(&self, cursor: &mut u64) -> (u64, Vec<(u64, usize, Record)>) {
        let Some(guard) = self.lock() else {
            return (0, Vec::new());
        };
        if guard.number(0) != MAGIC {
            return (0, Vec::new());
        }
        let current = guard.number(8);
        if current <= *cursor {
            return (0, Vec::new());
        }
        let start = (*cursor + 1).max(current.saturating_sub(SLOTS as u64 - 1));
        let mut lost = start - (*cursor + 1);
        let mut buffer = Vec::new();
        for sequence in start..=current {
            let offset = slot_offset(sequence);
            let length = guard.number(offset + 8);
            if guard.number(offset) != sequence || !(1..=(SLOT - 16) as u64).contains(&length) {
                lost += 1;
                continue;
            }
            let mut payload = vec![0; length as usize];
            guard.read(offset + 16, &mut payload);
            buffer.push((sequence, payload));
        }
        *cursor = current;
        drop(guard);
        let records = buffer
            .into_iter()
            .filter_map(|(sequence, bytes)| match serde_json::from_slice(&bytes) {
                Ok(value) => Some((sequence, bytes.len(), value)),
                Err(_) => {
                    lost += 1;
                    None
                }
            })
            .collect();
        (lost, records)
    }
}

fn slot_offset(sequence: u64) -> usize {
    HEADER + SLOT * ((sequence - 1) % SLOTS as u64) as usize
}

fn capture(value: Value, notes: &mut Vec<String>) -> Value {
    let data = value.to_string();
    if data.len() <= CAPTURE {
        return value;
    }
    let end = data.floor_char_boundary(CAPTURE / 8);
    let mut note = "內容超過擷取上限，只保留開頭預覽。";
    let mut preview = json!({"preview": &data[..end], "capture": "內容已截斷"});
    if let Some(args) = value
        .pointer("/params/arguments")
        .and_then(Value::as_object)
    {
        let mut targets = serde_json::Map::new();
        for key in [
            "root_id",
            "right_root_id",
            "path",
            "source",
            "destination",
            "left",
            "right",
        ] {
            if let Some(text) = args.get(key).and_then(Value::as_str)
                && text.len() <= 16 * 1024
            {
                targets.insert(key.into(), json!(text));
            }
        }
        if !targets.is_empty() {
            preview["params"] = json!({"arguments": targets});
            note = "內容超過擷取上限，保留開頭預覽與可擷取的目標路徑。";
        }
    }
    notes.push(note.into());
    preview
}

fn clock() -> (String, String, String) {
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        GetSystemTime(&mut utc);
        GetLocalTime(&mut local);
    }
    let time = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        utc.wYear, utc.wMonth, utc.wDay, utc.wHour, utc.wMinute, utc.wSecond, utc.wMilliseconds
    );
    let clock = format!(
        "{:02}:{:02}:{:02}",
        local.wHour, local.wMinute, local.wSecond
    );
    let local_time = format!(
        "{:04}-{:02}-{:02} {clock}",
        local.wYear, local.wMonth, local.wDay
    );
    (time, local_time, clock)
}

impl Record {
    fn begin(request: Value, tool: &str, mode: Mode) -> Option<Self> {
        let mut id = [0; 16];
        getrandom::fill(&mut id).ok()?;
        let (time, local_time, clock) = clock();
        let mut request_notes = Vec::new();
        let request = capture(request, &mut request_notes);
        Some(Self {
            id: hex(&id),
            time,
            local_time,
            clock,
            tool: tool.chars().take(128).collect(),
            access_mode: Some(mode),
            status: Status::Running,
            elapsed_ms: None,
            request_notes,
            response_notes: Vec::new(),
            request: Some(request),
            response: None,
        })
    }

    fn finish(
        &mut self,
        status: Status,
        started: Instant,
        response: Option<Value>,
        notice: Option<&str>,
    ) {
        self.status = status;
        self.elapsed_ms = Some(started.elapsed().as_secs_f64() * 1000.);
        if let Some(notice) = notice {
            self.response_notes.push(notice.into());
        }
        self.response = response.map(|mut response| {
            if let Some(blocks) = response
                .get_mut("result")
                .and_then(|result| result.get_mut("content"))
                .and_then(Value::as_array_mut)
            {
                let mut omitted = false;
                for block in blocks {
                    if matches!(block["type"].as_str(), Some("image" | "audio"))
                        && block.get("data").is_some()
                    {
                        block["data"] = json!("[二進位內容省略]");
                        omitted = true;
                    }
                }
                if omitted {
                    self.response_notes.push("二進位內容已省略。".into());
                }
            }
            capture(response, &mut self.response_notes)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_identity_requires_a_live_attachment_and_bounded_payload() {
        let receiver = Bus::create().unwrap();
        let writer = Bus::connect(receiver.name().into(), false).unwrap();
        assert!(receiver.runtime_identity().is_none());
        let attachment = writer.mark_attached().unwrap();
        let identity = receiver.runtime_identity().unwrap();
        assert_eq!(identity.source_sha256, env!("MCP_SOURCE_SHA256"));
        assert_eq!(identity.source_sha256.len(), 64);
        assert_eq!(identity.process_id, std::process::id());
        assert_eq!(
            identity.instance_id,
            crate::build_identity::current().instance_id
        );
        receiver.change(None, true).unwrap();
        assert!(receiver.runtime_identity().is_some());
        writer.lock().unwrap().write(32, &u64::MAX.to_le_bytes());
        assert!(receiver.runtime_identity().is_none());
        drop(attachment);
        assert!(receiver.runtime_identity().is_none());
    }

    #[test]
    fn monitor_shows_file_targets_and_individual_read_results() {
        let request = json!({"params":{"arguments":{"files":[{"path":"a.txt"},{"path":"b.txt"},{"path":"c.txt"}]}}});
        let mut record = Record::begin(request, "read_files", Mode::ReadOnly).unwrap();
        assert!(presentation::row(&record).0.contains("c.txt"));
        assert!(presentation::detail(&record).contains("等待回應 · b.txt"));
        let response = json!({"result":{"structuredContent":{"files":[
            {"index":0,"returned_lines":0},{"index":1,"skipped_reason":"missing"}
        ]}}});
        record.finish(Status::Success, Instant::now(), Some(response), None);
        let detail = presentation::detail(&record);
        for expected in ["已讀取 · a.txt", "略過 · b.txt", "結果未確認 · c.txt"] {
            assert!(detail.contains(expected));
        }

        let request =
            json!({"params":{"arguments":{"content":"x".repeat(CAPTURE),"path":"large.txt"}}});
        let write = Record::begin(request, "write_file", Mode::HostControl).unwrap();
        assert_eq!(presentation::row(&write).0, "large.txt");
        assert!(!write.request_notes.is_empty());
        assert!(write.request.unwrap().to_string().len() < CAPTURE);
    }

    #[test]
    fn retains_bounded_records_and_omits_image_payloads() {
        let receiver = Bus::create().unwrap();
        let writer = Bus::connect(receiver.name().into(), false).unwrap();
        for index in 0..(SLOTS + 2) {
            let request =
                json!({"id":index,"params":{"name":"read_image","arguments":{"path":"圖片.png"}}});
            let response =
                json!({"id":index,"result":{"content":[{"type":"image","data":"fixture-pixels"}]}});
            let mut record = Record::begin(request, "read_image", Mode::ReadOnly).unwrap();
            record.finish(Status::Success, Instant::now(), Some(response), None);
            writer.publish(writer.epoch().unwrap(), &record);
        }
        let mut cursor = 0;
        let (lost, records) = receiver.poll(&mut cursor);
        assert_eq!(lost, 2);
        assert_eq!(records.len(), SLOTS);
        let last = &records.last().unwrap().2;
        assert_eq!(
            last.request.as_ref().unwrap()["params"]["arguments"]["path"],
            "圖片.png"
        );
        assert_eq!(last.response_notes, ["二進位內容已省略。"]);
        assert_eq!(
            last.response.as_ref().unwrap()["result"]["content"][0]["data"],
            "[二進位內容省略]"
        );
        assert!(receiver.poll(&mut cursor).1.is_empty());
    }
}
