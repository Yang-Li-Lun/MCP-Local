use std::{
    env,
    io::{self, Read, Write},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    ptr::null,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::{CREATE_NO_WINDOW, CreateMutexW},
    },
};
use zeroize::{Zeroize, Zeroizing};

use crate::{
    desktop_store::{Config, Store, validate_tunnel},
    developer_vm::guard::{self, GuardOwner},
    filesystem::{PrivateDirectory, missing},
    monitor,
    security::{
        authority::{Effective, Shared},
        channel::{self, Mode},
    },
    settings::{ReaderSettings, WorkspaceSettings, decode},
    vendor::{CLIENT, Vendor},
    windows_job::Job,
    windows_pipe::pending_bytes,
};

pub const WORKER: &str = "--tunnel-stage";
pub fn validate_vendor_tunnel(value: &str) -> Result<()> {
    ensure!(
        value
            .strip_prefix("tunnel_")
            .is_some_and(|id| id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())),
        "TUNNEL_ID_INVALID：通道識別碼須為 tunnel_ 加上 32 個小寫英文字母或數字。"
    );
    Ok(())
}
const PROFILE: &str = "local-files-gui";
const PROFILE_FILE: &str = "local-files-gui.yaml";
const CLEANUP_FAILED: &str = "RESOURCE_CLEANUP_FAILED：無法確認通道程序已停止。";
const SECURITY_UNCONFIRMED: &str = "SECURITY_UNCONFIRMED：無法確認本機安全連線已關閉。";
const MAX_INPUT: usize = 128 * 1024;
const WORKER_VENDOR_START_FAILED: i32 = 70;
const WORKER_VENDOR_FAILED: i32 = 71;
const WORKER_READY: u8 = 1;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Init,
    Doctor,
    Run,
}

impl Stage {
    fn label(self) -> &'static str {
        match self {
            Self::Init => "建立設定",
            Self::Doctor => "檢查連線",
            Self::Run => "通道程序執行中",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    stage: Stage,
    mode: Mode,
    tunnel: String,
    workspace: String,
    reader: String,
    key: String,
    monitor: String,
    security: String,
    developer_guard: Option<String>,
}

impl Drop for Request {
    fn drop(&mut self) {
        self.key.zeroize();
        self.security.zeroize();
        self.developer_guard.zeroize();
    }
}

pub enum Event {
    State(&'static str),
    Finished {
        error: Option<String>,
        cleanup_failed: bool,
        retry: Option<RetryableFailure>,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct RetryableFailure {
    pub stable_run: bool,
}

#[derive(Debug)]
struct VendorFailure {
    stage: Stage,
    message: &'static str,
    stable_run: bool,
}

impl std::fmt::Display for VendorFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.stage.label(), self.message)
    }
}

impl std::error::Error for VendorFailure {}

pub struct Connection {
    pub events: Receiver<Event>,
    pub mode: Mode,
    cancel: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl Connection {
    pub fn start(config: Config, monitor: String, security: Shared) -> Result<Self> {
        crate::power_recovery::require_installations_clear()?;
        ensure!(
            security
                .lock()
                .is_ok_and(|authority| authority.view().effective != Effective::Fault),
            "SECURITY_UNCONFIRMED：請先在安全中心復原本機安全連線。"
        );
        validate_vendor_tunnel(&config.tunnel)?;
        let mode = config.access_mode;
        let (send, events) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let join = thread::Builder::new()
            .name("connection".into())
            .spawn(move || {
                let (result, cleanup_failed) =
                    run(config, &monitor, &security, &worker_cancel, &send);
                let retry = result.as_ref().err().and_then(|error| {
                    let failure = error.downcast_ref::<VendorFailure>()?;
                    (!cleanup_failed && !worker_cancel.load(Ordering::Acquire)).then_some(
                        RetryableFailure {
                            stable_run: failure.stable_run,
                        },
                    )
                });
                let _ = send.send(Event::Finished {
                    error: result.err().map(|err| err.to_string()),
                    cleanup_failed,
                    retry,
                });
            })?;
        Ok(Self {
            events,
            mode,
            cancel,
            join: Some(join),
        })
    }

    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn connection_mutex() -> Result<OwnedHandle> {
    let name: Vec<u16> = r"Local\MCP_Local_Connection"
        .encode_utf16()
        .chain([0])
        .collect();
    let raw = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    ensure!(!raw.is_null(), "無法建立連線鎖定。");
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    ensure!(!exists, "已有連線流程，請先停止原連線。");
    Ok(handle)
}

fn tunnel_running() -> Result<bool> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    ensure!(snapshot != INVALID_HANDLE_VALUE, "無法查詢既有通道程序。");
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut process = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut found = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut process) };
    while found != 0 {
        let end = process
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(process.szExeFile.len());
        if String::from_utf16_lossy(&process.szExeFile[..end]).eq_ignore_ascii_case(CLIENT) {
            return Ok(true);
        }
        found = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut process) };
    }
    ensure!(
        unsafe { GetLastError() } == ERROR_NO_MORE_FILES,
        "無法確認現行通道程序。"
    );
    Ok(false)
}

fn current_exe() -> Result<PathBuf> {
    env::current_exe().map_err(|_| anyhow::anyhow!("找不到原生服務程式。"))
}

fn run(
    config: Config,
    monitor: &str,
    security: &Shared,
    cancel: &AtomicBool,
    send: &Sender<Event>,
) -> (Result<()>, bool) {
    let mut cleanup_failed = false;
    let result = (|| {
        config.validate_shared()?;
        let store = Store::new()?;
        let exe = current_exe()?;
        let _vendor = Vendor::open(&exe)?;
        let mutex = connection_mutex()?;
        ensure!(!tunnel_running()?, "已有 tunnel-client 程序執行中。");
        let job = Job::connection()?;
        let mut developer_guard = if config.access_mode == Mode::DeveloperControl {
            Some(GuardOwner::spawn(
                config.reader.clone(),
                config.workspace.clone(),
            )?)
        } else {
            None
        };
        let mut stage_input = None;
        let result = (|| {
            let guard_descriptor = developer_guard
                .as_ref()
                .map(GuardOwner::descriptor)
                .transpose()?;
            let descriptor = security
                .lock()
                .map_err(|_| anyhow::anyhow!("SECURITY_UNCONFIRMED"))?
                .begin(config.access_mode)?;
            let key = config.key()?;
            let profile = PrivateDirectory::open(&store.path.join("profiles"), true)?;
            let initialize = match profile.regular(PROFILE_FILE) {
                Ok(_) => false,
                Err(error) if missing(&error) => true,
                Err(_) => bail!("通道設定必須為一般檔案。"),
            };
            let mut request = Request {
                stage: Stage::Init,
                mode: config.access_mode,
                tunnel: config.tunnel.clone(),
                workspace: STANDARD.encode(serde_json::to_vec(&config.workspace)?),
                reader: STANDARD.encode(serde_json::to_vec(&config.reader)?),
                key: key.to_string(),
                monitor: monitor.to_owned(),
                security: descriptor.to_string(),
                developer_guard: guard_descriptor
                    .as_ref()
                    .map(|value| value.as_str().to_owned()),
            };
            drop(descriptor);
            drop(guard_descriptor);
            let mut last_security = Instant::now();
            for stage in [Stage::Init, Stage::Doctor, Stage::Run] {
                if cancel.load(Ordering::Acquire) {
                    return Ok(());
                }
                if matches!(stage, Stage::Init) && !initialize {
                    continue;
                }
                request.stage = stage;
                let _ = send.send(Event::State(if matches!(stage, Stage::Run) {
                    "啟動通道中"
                } else {
                    stage.label()
                }));
                let deadline = Instant::now() + Duration::from_secs(60);
                let (mut child, input) = spawn_stage(&exe, &job, &request)?;
                stage_input = Some(input);
                let mut output = child.stdout.take().context("無法取得通道啟動回報。")?;
                let mut started = None;
                loop {
                    if last_security.elapsed() >= Duration::from_millis(250) {
                        security
                            .lock()
                            .map_err(|_| anyhow::anyhow!("SECURITY_UNCONFIRMED"))?
                            .tick();
                        last_security = Instant::now();
                    }
                    if cancel.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if let Some(input) = &stage_input {
                        match input.try_recv() {
                            Ok(result) => {
                                result.context("無法傳送通道啟動設定。")?;
                                stage_input = None;
                            }
                            Err(mpsc::TryRecvError::Empty) => {}
                            Err(mpsc::TryRecvError::Disconnected) => {
                                bail!("通道啟動設定傳輸已中斷。");
                            }
                        }
                    }
                    let status = child.try_wait()?;
                    if stage_input.is_none() && started.is_none() && worker_ready(&mut output)? {
                        started = Some(Instant::now());
                        if matches!(stage, Stage::Run) {
                            let _ = send.send(Event::State(stage.label()));
                        }
                    }
                    if let Some(status) = status
                        && stage_input.is_none()
                    {
                        finish_stage(stage, status, started.map(|time| time.elapsed()))?;
                        break;
                    }
                    if Instant::now() >= deadline
                        && (!matches!(stage, Stage::Run) || started.is_none())
                    {
                        if started.is_none() {
                            bail!("{}逾時，未確認通道已啟動。", stage.label());
                        }
                        return Err(VendorFailure {
                            stage,
                            message: "逾時。",
                            stable_run: false,
                        }
                        .into());
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
            Ok(())
        })();
        let revoked = security
            .lock()
            .map_err(|_| anyhow::anyhow!(SECURITY_UNCONFIRMED))
            .and_then(|mut security| security.revoke());
        let job_result = job.terminate().context(CLEANUP_FAILED);
        let input_result = (|| {
            if job_result.is_ok()
                && let Some(input) = stage_input
            {
                // Stopping the worker closes its end of the pipe and releases a stalled write.
                ensure!(
                    input.recv_timeout(Duration::from_secs(1)).err()
                        != Some(mpsc::RecvTimeoutError::Timeout),
                    "{CLEANUP_FAILED}"
                );
            }
            Ok(())
        })();
        let guard_result = developer_guard.as_mut().map(GuardOwner::close).transpose();
        if let Err(error) = job_result.and(input_result).and(guard_result) {
            cleanup_failed = true;
            return Err(error);
        }
        drop(mutex);
        revoked?;
        result
    })();
    (close_security(security, result), cleanup_failed)
}

fn close_security(security: &Shared, result: Result<()>) -> Result<()> {
    let confirmed = if let Ok(mut security) = security.lock() {
        security.close();
        security.view().effective != Effective::Fault
    } else {
        false
    };
    if !confirmed
        && result
            .as_ref()
            .err()
            .is_none_or(|error| error.is::<VendorFailure>())
    {
        bail!(SECURITY_UNCONFIRMED);
    }
    result
}

fn worker_ready(output: &mut (impl Read + AsRawHandle)) -> Result<bool> {
    match pending_bytes(output)? {
        None | Some(0) => Ok(false),
        Some(1) => {
            let mut marker = [0];
            output.read_exact(&mut marker)?;
            ensure!(marker == [WORKER_READY], "通道啟動回報無效。");
            Ok(true)
        }
        _ => bail!("通道啟動回報無效。"),
    }
}

fn finish_stage(stage: Stage, status: ExitStatus, started_for: Option<Duration>) -> Result<()> {
    let message = match (status.code(), started_for) {
        (Some(0), Some(_)) if !matches!(stage, Stage::Run) => return Ok(()),
        (Some(WORKER_VENDOR_START_FAILED), None) => "無法啟動通道程式。",
        (Some(WORKER_VENDOR_FAILED), Some(_)) => "失敗。請檢查網路、通道設定及金鑰。",
        (Some(0), Some(_)) => "已結束。",
        _ => bail!(
            "{}未完成本機驗證或執行異常（結束碼 {}）。",
            stage.label(),
            status.code().unwrap_or(-1)
        ),
    };
    Err(VendorFailure {
        stage,
        message,
        stable_run: matches!(stage, Stage::Run)
            && started_for.is_some_and(|time| time >= Duration::from_secs(60)),
    }
    .into())
}

fn spawn_stage(
    exe: &Path,
    job: &Job,
    request: &Request,
) -> Result<(Child, Receiver<io::Result<()>>)> {
    let input = Zeroizing::new(serde_json::to_vec(request)?);
    ensure!(input.len() <= MAX_INPUT, "通道啟動設定超出容量。");
    let mut child = Command::new(exe)
        .arg(WORKER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove("CONTROL_PLANE_API_KEY")
        .env_remove(monitor::ENV_NAME)
        .env_remove(channel::ENV_NAME)
        .env_remove(guard::ENV_NAME)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    if let Err(error) = job.assign(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).context("通道程序未進入 Job，已拒絕啟動。");
    }
    let stdin = child.stdin.take().context("無法取得通道啟動輸入。")?;
    let input = send_stage_input(stdin, input)?;
    Ok((child, input))
}

fn send_stage_input(
    mut stdin: impl Write + Send + 'static,
    input: Zeroizing<Vec<u8>>,
) -> io::Result<Receiver<io::Result<()>>> {
    let (send, receive) = mpsc::channel();
    thread::Builder::new()
        .name("connection-input".into())
        .spawn(move || {
            let result = stdin.write_all(&input);
            drop(stdin);
            drop(input);
            let _ = send.send(result);
        })?;
    Ok(receive)
}

fn mcp_command(exe: &Path, request: &Request) -> Result<String> {
    let path = exe
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("原生服務路徑格式無效。"))?;
    let path = path
        .strip_prefix(r"\\?\")
        .unwrap_or(path)
        .replace('\\', "/");
    ensure!(!path.contains(['"', '\r', '\n']), "原生服務路徑格式無效。");
    let mode = match (request.mode, request.developer_guard.as_deref()) {
        (Mode::ReadOnly, None) => "read_only".to_owned(),
        (Mode::HostControl, None) => "host_control".to_owned(),
        (Mode::DeveloperControl, Some(descriptor))
            if (1..=guard::DESCRIPTOR_LIMIT).contains(&descriptor.len()) =>
        {
            format!("developer_control {}", guard::REQUIRE_DELEGATED)
        }
        _ => bail!("VM_GUARD_MODE_MISMATCH：通道模式與本機安全連線不符。"),
    };
    let command = format!(
        "\"{path}\" --access-mode {mode} --require-security-channel --workspace-settings {} --reader-settings {}",
        request.workspace, request.reader
    );
    ensure!(
        command.encode_utf16().count() <= 26_000,
        "連線命令超過安全長度上限。"
    );
    Ok(command)
}

pub fn worker() -> Result<i32> {
    let mut bytes = Zeroizing::new(Vec::new());
    io::stdin()
        .take((MAX_INPUT + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_INPUT, "通道啟動設定超限。");
    let request: Request =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("通道啟動設定無效。"))?;
    bytes.zeroize();
    validate_tunnel(&request.tunnel)?;
    validate_vendor_tunnel(&request.tunnel)?;
    ensure!(
        !request.key.is_empty()
            && request.key.len() <= 16_384
            && !request.key.contains(['\0', '\r', '\n']),
        "通道啟動設定無效。"
    );
    ensure!(
        (1..=1024).contains(&request.security.len())
            && request
                .security
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "SECURITY_CHANNEL_DESCRIPTOR"
    );
    let workspace = decode::<WorkspaceSettings>(&request.workspace, 65_536)?;
    let reader = decode::<ReaderSettings>(&request.reader, 16_000)?;
    crate::server::Workspace::new(reader, workspace)?;
    let exe = current_exe()?;
    let vendor = Vendor::open(&exe)?;
    let store = Store::new()?;
    crate::security::policy::Store(store.path.clone()).load()?;
    crate::security::audit::Audit(store.path.clone()).load()?;
    let profiles = PrivateDirectory::open(&store.path.join("profiles"), false)?;
    match (request.stage, profiles.regular(PROFILE_FILE)) {
        (Stage::Init, Err(error)) if missing(&error) => {}
        (Stage::Doctor | Stage::Run, Ok(_)) => {}
        _ => bail!("通道設定已變更或格式不安全。"),
    };
    let mcp = mcp_command(&exe, &request)?;
    let mut process = Command::new(&vendor.client);
    process
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    process
        .env_clear()
        .env("CONTROL_PLANE_API_KEY", request.key.as_str())
        .env(monitor::ENV_NAME, &request.monitor)
        .env(channel::ENV_NAME, &request.security);
    if let Some(descriptor) = &request.developer_guard {
        process.env(guard::ENV_NAME, descriptor);
    }
    if !matches!(request.stage, Stage::Init) {
        process.env("CLOUDFLARED_PATH", &vendor.companion);
    }
    for value in crate::host_control::environment::NAMES {
        if let Some(value_os) = env::var_os(value) {
            process.env(value, value_os);
        }
    }
    process.arg(match request.stage {
        Stage::Init => "init",
        Stage::Doctor => "doctor",
        Stage::Run => "run",
    });
    process
        .args(["--profile", PROFILE, "--profile-dir"])
        .arg(profiles.path());
    if matches!(request.stage, Stage::Init) {
        process.args([
            "--sample",
            "sample_mcp_stdio_local",
            "--health-listen-addr",
            "127.0.0.1:0",
            "--tunnel-id",
            &request.tunnel,
            "--mcp-command",
            &mcp,
        ]);
    } else {
        process.args([
            "--health.listen-addr",
            "127.0.0.1:0",
            "--control-plane.tunnel-id",
            &request.tunnel,
            "--mcp.command",
            &mcp,
        ]);
        if matches!(request.stage, Stage::Doctor) {
            process.arg("--explain");
        }
    }
    let Ok(mut child) = process.spawn() else {
        return Ok(WORKER_VENDOR_START_FAILED);
    };
    let ready = {
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(&[WORKER_READY])
            .and_then(|()| stdout.flush())
    };
    if let Err(error) = ready {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).context("無法回報通道啟動狀態。");
    }
    Ok(if child.wait()?.success() {
        0
    } else {
        WORKER_VENDOR_FAILED
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::ExitStatusExt;

    #[test]
    fn vendor_tunnel_ids_match_the_bundled_client_requirement() {
        assert!(validate_vendor_tunnel("tunnel_0123456789abcdefghijklmnopqrstuv").is_ok());
        for bad in [
            "tunnel_example",
            "tunnel_0123456789abcdefghijklmnopqrstuvw",
            "tunnel_0123456789Abcdefghijklmnopqrstuv",
            "tunnel_0123456789abcdefghijklmnopqrstu_",
        ] {
            assert!(validate_vendor_tunnel(bad).is_err());
        }
    }

    #[test]
    fn unconfirmed_security_shutdown_reports_failure_and_prevents_reconnect() {
        use std::sync::Mutex;

        use crate::security::{Runtime, authority::Authority};

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().to_owned();
        let authority = Arc::new(Mutex::new(Authority::new(directory.clone())));
        assert!(close_security(&authority, Ok(())).is_ok());

        let mut active = authority.lock().unwrap();
        let descriptor = active.begin(Mode::ReadOnly).unwrap();
        let mut runtime = Runtime::attach(directory, &descriptor, Mode::ReadOnly).unwrap();
        active.tick();
        assert_eq!(active.view().effective, Effective::Confirmed);
        runtime.fail();
        runtime.heartbeat();
        active.tick();
        assert_eq!(active.view().effective, Effective::Fault);
        drop(runtime);
        drop(active);

        let error = close_security(&authority, Ok(())).unwrap_err();
        assert_eq!(error.to_string(), SECURITY_UNCONFIRMED);
        let vendor_error = finish_stage(
            Stage::Run,
            ExitStatus::from_raw(WORKER_VENDOR_FAILED as u32),
            Some(Duration::from_secs(1)),
        );
        let error = close_security(&authority, vendor_error).unwrap_err();
        assert_eq!(error.to_string(), SECURITY_UNCONFIRMED);
        assert!(!error.is::<VendorFailure>());

        let Err(error) = Connection::start(Config::default(), String::new(), authority) else {
            panic!("a connection started with unconfirmed security");
        };
        assert!(error.to_string().contains("SECURITY_UNCONFIRMED"));
    }

    #[test]
    fn only_the_validated_vendor_outcomes_are_retryable() {
        let exit = |code: i32| ExitStatus::from_raw(code as u32);
        let short = Some(Duration::from_secs(59));
        let stable = Some(Duration::from_secs(60));
        assert!(finish_stage(Stage::Doctor, exit(0), short).is_ok());
        for (code, running) in [(1, None), (1, stable), (0, None), (70, short), (71, None)] {
            assert!(
                !finish_stage(Stage::Doctor, exit(code), running)
                    .unwrap_err()
                    .is::<VendorFailure>()
            );
        }
        for (stage, code, running, stable_run) in [
            (Stage::Init, WORKER_VENDOR_START_FAILED, None, false),
            (Stage::Doctor, WORKER_VENDOR_FAILED, short, false),
            (Stage::Run, WORKER_VENDOR_FAILED, short, false),
            (Stage::Run, 0, stable, true),
        ] {
            let error = finish_stage(stage, exit(code), running).unwrap_err();
            assert_eq!(
                error.downcast_ref::<VendorFailure>().unwrap().stable_run,
                stable_run
            );
        }
    }

    #[test]
    fn readiness_requires_the_single_private_worker_marker() {
        let (mut reader, mut writer) = io::pipe().unwrap();
        assert!(!worker_ready(&mut reader).unwrap());
        writer.write_all(&[WORKER_READY]).unwrap();
        drop(writer);
        assert!(worker_ready(&mut reader).unwrap());
        assert!(!worker_ready(&mut reader).unwrap());

        for malformed in [vec![0], vec![WORKER_READY, 0]] {
            let (mut reader, mut writer) = io::pipe().unwrap();
            writer.write_all(&malformed).unwrap();
            assert!(worker_ready(&mut reader).is_err());
        }
    }

    #[test]
    fn startup_write_delivers_the_full_payload_and_eof() {
        let (mut reader, writer) = io::pipe().unwrap();
        let expected = vec![b'x'; MAX_INPUT];
        let transfer = send_stage_input(writer, Zeroizing::new(expected.clone())).unwrap();
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert_eq!(received, expected);
        transfer
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
    }

    #[test]
    #[ignore = "subprocess fixture for an owned stage that does not read stdin"]
    fn stage_fixture_does_not_read_stdin() {
        thread::sleep(Duration::from_secs(10));
    }

    #[test]
    fn stopping_the_stage_job_releases_a_blocked_startup_write() {
        let job = Job::connection().unwrap();
        let mut child = Command::new(env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "connection::tests::stage_fixture_does_not_read_stdin",
            ])
            .env_clear()
            .env("SystemRoot", env::var_os("SystemRoot").unwrap())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        if let Err(error) = job.assign(&child) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{error}");
        }
        let sender = child.stdin.take().unwrap();
        let transfer = send_stage_input(sender, Zeroizing::new(vec![b'x'; MAX_INPUT])).unwrap();
        assert_eq!(
            transfer
                .recv_timeout(Duration::from_millis(100))
                .unwrap_err(),
            mpsc::RecvTimeoutError::Timeout
        );
        job.terminate().unwrap();
        assert_eq!(job.active_processes().unwrap(), 0);
        assert_eq!(child.wait().unwrap().code(), Some(1));
        assert!(
            transfer
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn selects_only_the_scoped_mode_and_excludes_capabilities_from_tunnel_arguments() {
        let mut request = Request {
            stage: Stage::Run,
            mode: Mode::ReadOnly,
            tunnel: "tunnel_fixture".into(),
            workspace: "d29ya3NwYWNl".into(),
            reader: "cmVhZGVy".into(),
            key: "synthetic-stage-key".into(),
            monitor: "monitor-fixture".into(),
            security: "security-fixture".into(),
            developer_guard: None,
        };
        let exe = Path::new(r"C:\test path\mcp-local-rust.exe");
        let read_only = mcp_command(exe, &request).unwrap();
        assert!(
            read_only.starts_with(r#""C:/test path/mcp-local-rust.exe" --access-mode read_only "#)
        );
        assert!(!read_only.contains(guard::REQUIRE_DELEGATED));
        request.mode = Mode::DeveloperControl;
        assert!(mcp_command(exe, &request).is_err());
        request.developer_guard = Some("private-guard-fixture".into());
        let developer = mcp_command(exe, &request).unwrap();
        assert!(developer.contains(
            "--access-mode developer_control --require-developer-guard --require-security-channel"
        ));
        for private in [
            &request.key,
            &request.security,
            request.developer_guard.as_ref().unwrap(),
        ] {
            assert!(!developer.contains(private));
        }
        request.mode = Mode::ReadOnly;
        assert!(mcp_command(exe, &request).is_err());
        request.mode = Mode::HostControl;
        assert!(mcp_command(exe, &request).is_err());
        request.developer_guard = None;
        let host = mcp_command(exe, &request).unwrap();
        assert!(host.contains("--access-mode host_control --require-security-channel"));
        assert!(!host.contains(&request.key));
        assert!(!host.contains(&request.security));
    }
}
