mod sandbox;

use std::{
    env,
    io::{self, Read, Write},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::null,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

use crate::budget::Budget;

pub const WORKER: &str = "--format-worker";
pub const MAX_SOURCE: usize = 32 * 1024 * 1024;
const MAX_HEADER: usize = 32 * 1024;
pub(crate) const MAX_MEMORY: usize = 512 * 1024 * 1024;
const MAX_TIME: Duration = Duration::from_secs(10);
const MAX_IMAGE_RESPONSE: usize = 5 * 1024 * 1024;
const MAX_RESPONSE: usize = 256 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "operation", rename_all = "snake_case")]
pub enum Operation {
    Image { format: String },
    Archive { options: crate::archives::Options },
    Document { options: crate::documents::Options },
    Identify { excluded_names: Vec<String> },
    Media,
}

impl Operation {
    fn max_response(&self) -> usize {
        match self {
            Self::Image { .. } => MAX_IMAGE_RESPONSE,
            _ => MAX_RESPONSE,
        }
    }
}

fn worker_job() -> Result<OwnedHandle> {
    // Anonymous, non-inheritable Job. The parent retains the only handle.
    let ptr = unsafe { CreateJobObjectW(null(), null()) };
    ensure!(!ptr.is_null(), "FORMAT_ISOLATION：無法建立解析資源限制。");
    let job = unsafe { OwnedHandle::from_raw_handle(ptr) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_PROCESS_TIME
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.BasicLimitInformation.PerProcessUserTimeLimit = MAX_TIME.as_secs() as i64 * 10_000_000;
    limits.ProcessMemoryLimit = MAX_MEMORY;
    // The information buffer remains valid for this call.
    let configured = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    ensure!(configured != 0, "FORMAT_ISOLATION：無法套用解析資源限制。");
    Ok(job)
}

pub fn parse(operation: Operation, bytes: Vec<u8>, budget: &Budget) -> Result<Value> {
    budget.check()?;
    ensure!(bytes.len() <= MAX_SOURCE, "FORMAT_SOURCE_LIMIT：來源超限。");
    let maximum = operation.max_response();
    let header = serde_json::to_vec(&operation)?;
    ensure!(
        header.len() <= MAX_HEADER,
        "FORMAT_INPUT_LIMIT：解析參數超限。"
    );
    let exe =
        env::current_exe().map_err(|_| anyhow::anyhow!("FORMAT_ISOLATION：無法找到解析程式。"))?;
    let mut profile = sandbox::Profile::create()?;
    let response = parse_isolated(&profile, &exe, header, bytes, maximum, budget);
    profile.close()?;
    response
}

fn parse_isolated(
    profile: &sandbox::Profile,
    exe: &std::path::Path,
    header: Vec<u8>,
    bytes: Vec<u8>,
    maximum: usize,
    budget: &Budget,
) -> Result<Value> {
    let job = worker_job()?;
    budget.check()?;
    let (child, mut input, output) = sandbox::launch(profile, exe, WORKER, &job)?;
    budget.check()?;
    let started = Instant::now();
    let (send, receive) = mpsc::sync_channel(1);
    thread::scope(|scope| {
        let writer = scope.spawn(move || -> io::Result<()> {
            input.write_all(&(header.len() as u32).to_le_bytes())?;
            input.write_all(&header)?;
            input.write_all(&bytes)
        });
        scope.spawn(move || {
            let mut data = Vec::new();
            let read = output.take((maximum + 1) as u64).read_to_end(&mut data);
            let _ = send.send(read.map(|_| data));
        });
        let response = (|| -> Result<Vec<u8>> {
            loop {
                budget.check()?;
                ensure!(
                    started.elapsed() < MAX_TIME,
                    "FORMAT_TIMEOUT：格式解析超過期限。"
                );
                match receive.recv_timeout(Duration::from_millis(50)) {
                    Ok(Ok(data)) => {
                        ensure!(data.len() <= maximum, "FORMAT_OUTPUT_LIMIT：解析輸出超限。");
                        return Ok(data);
                    }
                    Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        bail!("FORMAT_RESOURCE_LIMIT：解析程序失敗或超出資源限制。")
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        })();
        let succeeded = loop {
            match child.poll() {
                Ok(Some(status)) => break status,
                Ok(None)
                    if response.is_ok()
                        && budget.check().is_ok()
                        && started.elapsed() < MAX_TIME =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    drop(job);
                    child.stop()?;
                    break false;
                }
            }
        };
        let response = response?;
        ensure!(
            succeeded && writer.join().is_ok_and(|result| result.is_ok()),
            "FORMAT_RESOURCE_LIMIT：解析程序失敗或超出資源限制。"
        );
        let value: Value = serde_json::from_slice(&response)
            .map_err(|_| anyhow::anyhow!("FORMAT_INVALID：無法解析結果。"))?;
        if let Some(error) = value["error"].as_str() {
            bail!("{error}");
        }
        Ok(value)
    })
}

pub fn worker() -> Result<()> {
    sandbox::verify_worker()?;
    let mut input = io::stdin().lock();
    let mut length = [0; 4];
    input.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(length <= MAX_HEADER, "FORMAT_INPUT_LIMIT：解析參數超限。");
    let mut header = vec![0; length];
    input.read_exact(&mut header)?;
    let operation: Operation = serde_json::from_slice(&header)?;
    let maximum = operation.max_response();
    let mut data = Vec::new();
    input.take((MAX_SOURCE + 1) as u64).read_to_end(&mut data)?;
    ensure!(data.len() <= MAX_SOURCE, "FORMAT_SOURCE_LIMIT：來源超限。");
    let outcome = match operation {
        Operation::Image { format } => crate::images::decode(&data, &format),
        Operation::Archive { options } => crate::archives::inspect(&data, options),
        Operation::Document { options } => crate::documents::read(&data, options),
        Operation::Identify { excluded_names } => crate::documents::identify(&data, excluded_names),
        Operation::Media => crate::media::inspect(&data),
    };
    let response = match outcome {
        Ok(value) => value,
        Err(error) => {
            let message = error.to_string();
            let fixed = message.split_once('：').is_some_and(|(code, _)| {
                code.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
                    && ["FORMAT_", "ARCHIVE_", "DOCUMENT_", "MEDIA_"]
                        .iter()
                        .any(|prefix| code.starts_with(prefix))
            });
            json!({"error": if fixed { &message } else { "FORMAT_INVALID：檔案損毀、格式不符、不支援或超出解析限制。" }})
        }
    };
    let encoded = serde_json::to_vec(&response)?;
    ensure!(
        encoded.len() <= maximum,
        "FORMAT_OUTPUT_LIMIT：解析輸出超限。"
    );
    io::stdout().lock().write_all(&encoded)?;
    Ok(())
}
