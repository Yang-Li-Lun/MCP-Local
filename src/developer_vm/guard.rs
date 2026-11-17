use std::{
    ffi::c_void,
    io::{self, Read, Write},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    process::{Child, Command, Stdio},
    ptr,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_PIPE_NOT_CONNECTED, HANDLE, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_GROUPS, TOKEN_QUERY, TokenLogonSid,
    },
    Storage::FileSystem::SECURITY_IDENTIFICATION,
    System::{
        JobObjects::IsProcessInJob,
        Pipes::GetNamedPipeServerProcessId,
        Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, GetCurrentProcess, OpenProcessToken,
        },
    },
    UI::Shell::{FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_Windows},
};
use zeroize::{Zeroize, Zeroizing};

use crate::{
    budget::Budget,
    filesystem::known_folder,
    listing::hex,
    server::Workspace,
    settings::{ReaderSettings, WorkspaceSettings},
};

pub use super::session::{Launch, Mappings, Progress};
use super::{
    Backend, Lifecycle, STOP_UNCONFIRMED, VM_BUSY, cli,
    guest::Guest,
    session::{INVALID_PAGE, Session},
    spool::{MAX_OUTPUT, MAX_STDIN, validate_command},
};
use wire::{Auth, Direction, FRAME_TIMEOUT, PROTOCOL, Reply, Request};

mod wire;

pub const WORKER: &str = "--developer-vm-guard";
pub const ENV_NAME: &str = "MCP_LOCAL_RUST_DEVELOPER_GUARD";
pub const REQUIRE_DELEGATED: &str = "--require-developer-guard";
pub const DESCRIPTOR_LIMIT: usize = 2048;
const READY: u8 = b'R';
const STOPPED: u8 = b'S';
const BOOT_LIMIT: usize = 128 * 1024;
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(150);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(25);
const UNAVAILABLE: &str = "VM_GUARD_UNAVAILABLE：無法建立獨立 VM 清理程序。";
const LIFETIME_LIMIT: &str = "VM_LIFETIME_LIMIT：工作階段存活期限必須為 1 至 3600 秒。";
const UNKNOWN_SESSION: &str = "VM_SESSION_UNKNOWN：VM 工作階段不符。";
const OPERATION_FAILED: &str = "VM_OPERATION_FAILED：VM 操作尚未確認。";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    version: u8,
    pipe: [u8; 16],
    key: [u8; 32],
    pid: u32,
    scope: [u8; 32],
}

fn scope(reader: &ReaderSettings, workspace: &WorkspaceSettings) -> Result<[u8; 32]> {
    let frame = Zeroizing::new(serde_json::to_vec(&(reader, workspace))?);
    Ok(Sha256::digest(frame.as_slice()).into())
}

impl Descriptor {
    fn fresh(pid: u32, scope: [u8; 32]) -> Result<Self> {
        let mut descriptor = Self {
            version: 2,
            pipe: [0; 16],
            key: [0; 32],
            pid,
            scope,
        };
        getrandom::fill(&mut descriptor.pipe)?;
        getrandom::fill(&mut descriptor.key)?;
        Ok(descriptor)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 2 && self.pid != 0 && self.pipe != [0; 16] && self.key != [0; 32],
            "{PROTOCOL}"
        );
        Ok(())
    }

    fn address(&self) -> String {
        format!(r"\\.\pipe\MCP-Local-Rust.Developer.{}", hex(&self.pipe))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Boot {
    descriptor: Descriptor,
    reader: ReaderSettings,
    workspace: WorkspaceSettings,
}

pub struct GuardOwner {
    child: Child,
    descriptor: Descriptor,
    stopped_marker: bool,
}

impl GuardOwner {
    pub fn spawn(reader: ReaderSettings, workspace: WorkspaceSettings) -> Result<Self> {
        let executable = std::env::current_exe()?;
        let windows = known_folder(&FOLDERID_Windows)?;
        let mut command = Command::new(executable);
        command
            .arg(WORKER)
            .current_dir(windows.join("System32"))
            .env_clear()
            .env("SystemRoot", windows)
            .env("LOCALAPPDATA", known_folder(&FOLDERID_LocalAppData)?)
            .env("USERPROFILE", known_folder(&FOLDERID_Profile)?);
        Self::with_command(command, reader, workspace)
    }

    fn with_command(
        mut command: Command,
        reader: ReaderSettings,
        workspace: WorkspaceSettings,
    ) -> Result<Self> {
        let scope = scope(&reader, &workspace)?;
        let parent_job = in_job(unsafe { GetCurrentProcess() })?;
        let flags = CREATE_NO_WINDOW
            | if parent_job {
                CREATE_BREAKAWAY_FROM_JOB
            } else {
                0
            };
        let mut child = command
            .creation_flags(flags)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context(UNAVAILABLE)?;
        let descriptor = Descriptor::fresh(child.id(), scope);
        let result = (|| -> Result<Descriptor> {
            ensure!(!in_job(child.as_raw_handle())?, "{UNAVAILABLE}");
            let boot = Boot {
                descriptor: descriptor?,
                reader,
                workspace,
            };
            let raw = Zeroizing::new(serde_json::to_vec(&boot)?);
            ensure!(!raw.is_empty() && raw.len() <= BOOT_LIMIT, "{PROTOCOL}");
            let input = child.stdin.as_mut().context(UNAVAILABLE)?;
            input
                .write_all(&(raw.len() as u32).to_le_bytes())
                .context(UNAVAILABLE)?;
            input.write_all(&raw).context(UNAVAILABLE)?;
            read_marker(&mut child, READY, Instant::now() + BOOT_TIMEOUT).context(UNAVAILABLE)?;
            ensure!(child.try_wait()?.is_none(), "{UNAVAILABLE}");
            Ok(boot.descriptor)
        })();
        match result {
            Ok(descriptor) => Ok(Self {
                child,
                descriptor,
                stopped_marker: false,
            }),
            Err(error) => {
                // The service never received an endpoint. Closing the owner pipe cancels startup.
                drop(child.stdin.take());
                Err(error)
            }
        }
    }

    pub fn descriptor(&self) -> Result<Zeroizing<String>> {
        ensure!(self.child.stdin.is_some(), "{UNAVAILABLE}");
        Ok(Zeroizing::new(serde_json::to_string(&self.descriptor)?))
    }

    pub fn close(&mut self) -> Result<()> {
        self.close_within(SHUTDOWN_TIMEOUT)
            .context(STOP_UNCONFIRMED)
    }

    fn close_within(&mut self, duration: Duration) -> Result<()> {
        drop(self.child.stdin.take());
        let deadline = Instant::now() + duration;
        if !self.stopped_marker {
            read_marker(&mut self.child, STOPPED, deadline)?;
            self.stopped_marker = true;
        }
        loop {
            if let Some(status) = self.child.try_wait()? {
                ensure!(status.success(), "{STOP_UNCONFIRMED}");
                let output = self.child.stderr.as_ref().context(PROTOCOL)?;
                ensure!(
                    crate::windows_pipe::pending_bytes(output)?.is_none(),
                    "{PROTOCOL}"
                );
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "{STOP_UNCONFIRMED}");
            thread::sleep(POLL);
        }
    }
}

fn in_job(process: HANDLE) -> Result<bool> {
    let mut in_job = 0;
    ensure!(
        unsafe { IsProcessInJob(process, ptr::null_mut(), &mut in_job) } != 0,
        "{UNAVAILABLE}"
    );
    Ok(in_job != 0)
}

fn read_marker(child: &mut Child, expected: u8, deadline: Instant) -> Result<()> {
    loop {
        let output = child.stderr.as_mut().context(PROTOCOL)?;
        match crate::windows_pipe::pending_bytes(output)? {
            Some(1) => {
                let mut marker = [0];
                output.read_exact(&mut marker)?;
                ensure!(marker[0] == expected, "{PROTOCOL}");
                return Ok(());
            }
            Some(0) => {}
            _ => bail!("{PROTOCOL}"),
        }
        ensure!(Instant::now() < deadline, "{PROTOCOL}");
        thread::sleep(POLL);
    }
}

pub struct GuardClient {
    descriptor: Descriptor,
    nonce: [u8; 32],
    sequence: u64,
    pipe: Option<NamedPipeClient>,
}

impl GuardClient {
    pub(super) fn disconnect(&mut self) {
        self.pipe = None;
    }

    pub async fn connect(encoded: &str) -> Result<Self> {
        ensure!(
            !encoded.is_empty() && encoded.len() <= DESCRIPTOR_LIMIT,
            "{PROTOCOL}"
        );
        let descriptor: Descriptor = serde_json::from_str(encoded).context(PROTOCOL)?;
        descriptor.validate()?;
        let mut pipe = ClientOptions::new()
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(descriptor.address())
            .context(UNAVAILABLE)?;
        let mut pid = 0;
        ensure!(
            unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } != 0
                && pid == descriptor.pid,
            "{PROTOCOL}"
        );
        let mut nonce = [0; 32];
        timeout(FRAME_TIMEOUT, pipe.read_exact(&mut nonce))
            .await
            .context(PROTOCOL)??;
        let auth = Auth {
            key: &descriptor.key,
            nonce,
        };
        auth.write(&mut pipe, Direction::Request, 0, &Request::Hello {})
            .await?;
        let reply: Reply = timeout(FRAME_TIMEOUT, auth.read(&mut pipe, Direction::Reply, 0))
            .await
            .context(PROTOCOL)??;
        ensure!(
            matches!(reply, Reply::Ready { scope } if scope == descriptor.scope),
            "{PROTOCOL}"
        );
        Ok(Self {
            descriptor,
            nonce,
            sequence: 1,
            pipe: Some(pipe),
        })
    }

    pub(super) fn require_scope(
        &self,
        reader: &ReaderSettings,
        workspace: &WorkspaceSettings,
    ) -> Result<()> {
        ensure!(
            scope(reader, workspace)? == self.descriptor.scope,
            "VM_GUARD_SCOPE_MISMATCH：開發 VM 的授權資料夾或讀取設定不符。"
        );
        Ok(())
    }

    pub async fn start(
        &mut self,
        command: &str,
        root_id: Option<&str>,
        working_directory: Option<&str>,
        lifetime_seconds: u32,
    ) -> Result<Uuid> {
        let launch = self
            .launch(command, root_id, working_directory, lifetime_seconds)
            .await?;
        Uuid::parse_str(&launch.vm_id).context(PROTOCOL)
    }

    pub async fn launch(
        &mut self,
        command: &str,
        root_id: Option<&str>,
        working_directory: Option<&str>,
        lifetime_seconds: u32,
    ) -> Result<Launch> {
        ensure!((1..=3600).contains(&lifetime_seconds), "{LIFETIME_LIMIT}");
        let mappings = self.prepare(command, root_id, working_directory).await?;
        let id = self.activate(lifetime_seconds).await?;
        Ok(Launch {
            vm_id: id.to_string(),
            mappings: std::sync::Arc::new(mappings),
        })
    }

    pub(super) async fn prepare(
        &mut self,
        command: &str,
        root_id: Option<&str>,
        working_directory: Option<&str>,
    ) -> Result<Mappings> {
        validate_command(command)?;
        let request = Request::Prepare {
            command: command.to_owned(),
            root_id: root_id.map(str::to_owned),
            working_directory: working_directory.map(str::to_owned),
        };
        match self.exchange(request).await? {
            Reply::Prepared { mappings } => Ok(mappings),
            Reply::StopUnconfirmed {} => bail!("{STOP_UNCONFIRMED}"),
            Reply::Rejected {} => bail!("{OPERATION_FAILED}"),
            _ => bail!("{PROTOCOL}"),
        }
    }

    pub(super) async fn activate(&mut self, lifetime_seconds: u32) -> Result<Uuid> {
        ensure!((1..=3600).contains(&lifetime_seconds), "{LIFETIME_LIMIT}");
        match self.exchange(Request::Start { lifetime_seconds }).await? {
            Reply::Started { id } => Uuid::parse_str(&id).context(PROTOCOL),
            Reply::StopUnconfirmed {} => bail!("{STOP_UNCONFIRMED}"),
            Reply::Rejected {} => bail!("{OPERATION_FAILED}"),
            _ => bail!("{PROTOCOL}"),
        }
    }

    pub async fn poll(&mut self, id: Uuid, offset: usize) -> Result<Progress> {
        ensure!(offset <= MAX_OUTPUT, "{INVALID_PAGE}");
        match self
            .exchange(Request::Poll {
                id: id.to_string(),
                offset,
            })
            .await?
        {
            Reply::Progress { page } => {
                ensure!(
                    page.offset == offset.min(page.total_bytes),
                    "{INVALID_PAGE}"
                );
                page.bytes()?;
                Ok(page)
            }
            Reply::StopUnconfirmed {} => bail!("{STOP_UNCONFIRMED}"),
            Reply::Rejected {} => bail!("{OPERATION_FAILED}"),
            _ => bail!("{PROTOCOL}"),
        }
    }

    pub async fn input(&mut self, id: Uuid, text: &str, eof: bool) -> Result<usize> {
        ensure!(
            text.len() <= MAX_STDIN,
            "STDIN_LIMIT：單次輸入最多 4096 bytes。"
        );
        let request = Request::Input {
            id: id.to_string(),
            text: text.to_owned(),
            eof,
        };
        match self.exchange(request).await? {
            Reply::InputAccepted { bytes, eof: closed } if bytes == text.len() && closed == eof => {
                Ok(bytes)
            }
            Reply::Rejected {} => bail!("{OPERATION_FAILED}"),
            _ => bail!("{PROTOCOL}"),
        }
    }

    pub async fn stop(&mut self, id: Option<Uuid>) -> Result<()> {
        match self
            .exchange(Request::Stop {
                id: id.map(|value| value.to_string()),
            })
            .await?
        {
            Reply::Stopped {} => Ok(()),
            Reply::StopUnconfirmed {} => bail!("{STOP_UNCONFIRMED}"),
            Reply::Rejected {} => bail!("{OPERATION_FAILED}"),
            _ => bail!("{PROTOCOL}"),
        }
    }

    pub async fn terminate(&mut self, id: Uuid) -> Result<()> {
        match self
            .exchange(Request::Terminate { id: id.to_string() })
            .await?
        {
            Reply::Stopped {} => Ok(()),
            Reply::StopUnconfirmed {} => bail!("{STOP_UNCONFIRMED}"),
            Reply::Rejected {} => bail!("{OPERATION_FAILED}"),
            _ => bail!("{PROTOCOL}"),
        }
    }

    pub(super) async fn recover_after_disconnect(&mut self) -> Result<()> {
        ensure!(self.pipe.is_none(), "{STOP_UNCONFIRMED}");
        let descriptor = Zeroizing::new(serde_json::to_string(&self.descriptor)?);
        let client = timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(client) = Self::connect(&descriptor).await {
                    return client;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .context(STOP_UNCONFIRMED)?;
        // The same guard authenticates again only after cleaning up the old pipe's VM.
        *self = client;
        Ok(())
    }

    async fn exchange(&mut self, request: Request) -> Result<Reply> {
        // Taking the pipe first makes cancellation of this future disconnect the client.
        let mut pipe = self.pipe.take().context(UNAVAILABLE)?;
        let auth = Auth {
            key: &self.descriptor.key,
            nonce: self.nonce,
        };
        auth.write(&mut pipe, Direction::Request, self.sequence, &request)
            .await?;
        let reply = timeout(
            RESPONSE_TIMEOUT,
            auth.read(&mut pipe, Direction::Reply, self.sequence),
        )
        .await
        .context("VM_GUARD_TIMEOUT")??;
        self.sequence = self.sequence.checked_add(1).context(PROTOCOL)?;
        self.pipe = Some(pipe);
        Ok(reply)
    }
}

trait Engine: Send + 'static {
    fn prepare(
        &mut self,
        command: Zeroizing<String>,
        root_id: Option<&str>,
        working_directory: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Mappings>;
    fn start(&mut self, deadline: Instant, cancel: &CancellationToken) -> Result<Uuid>;
    fn poll(&mut self, offset: usize, cancel: &CancellationToken) -> Result<Progress>;
    fn input(&mut self, text: &[u8], eof: bool, cancel: &CancellationToken) -> Result<usize>;
    fn terminate(&mut self, cancel: &CancellationToken) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn session_id(&self) -> Option<Uuid>;
    fn deadline(&self) -> Option<Instant>;
}

struct Native<B: Backend = cli::Cli> {
    workspace: Workspace,
    lifecycle: Lifecycle<B>,
    session: Option<Session>,
}

impl<B> Native<B>
where
    B: Backend + Send + 'static,
    B::Connector: Send + 'static,
{
    fn require_available(&self) -> Result<()> {
        ensure!(
            self.lifecycle.owned.is_none() && self.session.is_none(),
            "{VM_BUSY}"
        );
        Ok(())
    }

    fn stage(
        &mut self,
        guest: Guest,
        command: Zeroizing<String>,
        budget: &mut Budget,
    ) -> Result<Mappings> {
        let configuration = guest.configuration(budget);
        let mappings = guest.mappings();
        self.session = Some(Session::new(guest, command, Instant::now() + BOOT_TIMEOUT));
        if let Err(error) = configuration {
            self.stop()?;
            return Err(error);
        }
        Ok(mappings)
    }
}

impl<B> Engine for Native<B>
where
    B: Backend + Send + 'static,
    B::Connector: Send + 'static,
{
    fn prepare(
        &mut self,
        command: Zeroizing<String>,
        root_id: Option<&str>,
        working_directory: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Mappings> {
        self.require_available()?;
        let mut budget = Budget::new(cancel.clone());
        let (root_id, root) = self.workspace.developer_root(root_id)?;
        let plan = self.workspace.developer_plan(&mut budget)?;
        let guest = Guest::prepare(root_id, root, working_directory, plan)?;
        self.stage(guest, command, &mut budget)
    }

    fn start(&mut self, deadline: Instant, cancel: &CancellationToken) -> Result<Uuid> {
        let session = self.session.as_mut().context(UNKNOWN_SESSION)?;
        ensure!(
            session.id.is_none() && self.lifecycle.owned.is_none(),
            "{VM_BUSY}"
        );
        let result = session.start(
            &mut self.lifecycle,
            deadline,
            &mut Budget::new(cancel.clone()),
            cancel,
        );
        if result.is_err() && self.lifecycle.owned.is_none() {
            self.stop()?;
        }
        result
    }

    fn poll(&mut self, offset: usize, cancel: &CancellationToken) -> Result<Progress> {
        let session = self.session.as_mut().context(UNKNOWN_SESSION)?;
        let mut budget = Budget::new(cancel.clone());
        let result = session.poll(&mut self.lifecycle, offset, &mut budget);
        if result
            .as_ref()
            .is_err_and(|error| !is_stop_unconfirmed(error))
        {
            self.stop()?;
        }
        result
    }

    fn input(&mut self, text: &[u8], eof: bool, cancel: &CancellationToken) -> Result<usize> {
        self.session.as_mut().context(UNKNOWN_SESSION)?.input(
            text,
            eof,
            &mut Budget::new(cancel.clone()),
        )
    }

    fn terminate(&mut self, cancel: &CancellationToken) -> Result<()> {
        let result = self
            .session
            .as_mut()
            .context(UNKNOWN_SESSION)?
            .terminate(&mut self.lifecycle, &mut Budget::new(cancel.clone()));
        if result
            .as_ref()
            .is_err_and(|error| !is_stop_unconfirmed(error))
        {
            self.stop()?;
        }
        result
    }

    fn stop(&mut self) -> Result<()> {
        match &mut self.session {
            Some(session) => session.close(&mut self.lifecycle)?,
            None => self.lifecycle.stop()?,
        }
        self.session = None;
        Ok(())
    }

    fn session_id(&self) -> Option<Uuid> {
        self.session.as_ref().and_then(|session| session.id)
    }

    fn deadline(&self) -> Option<Instant> {
        self.session.as_ref().and_then(|session| session.deadline)
    }
}

pub fn worker() -> Result<()> {
    let (boot, owner) = watch_owner()?;
    let engine = Native {
        workspace: Workspace::new(boot.reader, boot.workspace)?,
        lifecycle: Lifecycle::open()?,
        session: None,
    };
    run(boot.descriptor, owner, engine, write_marker)
}

fn watch_owner() -> Result<(Boot, CancellationToken)> {
    let owner = CancellationToken::new();
    let monitor = owner.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("vm-guard-owner".into())
        .spawn(move || {
            let mut input = io::stdin().lock();
            let boot = read_boot(&mut input);
            let valid = boot.is_ok();
            if sender.send(boot).is_ok() && valid {
                let _ = input.read(&mut [0; 1]);
            }
            monitor.cancel();
        })?;
    let boot = receiver.recv_timeout(BOOT_TIMEOUT).context(UNAVAILABLE)??;
    Ok((boot, owner))
}

fn read_boot(input: &mut impl Read) -> Result<Boot> {
    let mut size = [0; 4];
    input.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    ensure!((1..=BOOT_LIMIT).contains(&size), "{PROTOCOL}");
    let mut bytes = Zeroizing::new(vec![0; size]);
    input.read_exact(&mut bytes)?;
    let boot: Boot = serde_json::from_slice(&bytes).context(PROTOCOL)?;
    boot.descriptor.validate()?;
    ensure!(
        boot.descriptor.pid == std::process::id()
            && boot.descriptor.scope == scope(&boot.reader, &boot.workspace)?,
        "{PROTOCOL}"
    );
    Ok(boot)
}

fn write_marker(marker: u8) -> Result<()> {
    let mut status = io::stderr().lock();
    status.write_all(&[marker])?;
    status.flush()?;
    Ok(())
}

fn run(
    descriptor: Descriptor,
    owner: CancellationToken,
    engine: impl Engine,
    signal: impl Fn(u8) -> Result<()>,
) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            ensure!(!owner.is_cancelled(), "{UNAVAILABLE}");
            let mut pipe = create_pipe(&descriptor)?;
            signal(READY)?;
            serve(&mut pipe, &descriptor, &owner, engine).await?;
            signal(STOPPED)
        })
}

async fn serve<E: Engine>(
    pipe: &mut NamedPipeServer,
    descriptor: &Descriptor,
    owner: &CancellationToken,
    mut engine: E,
) -> Result<()> {
    loop {
        tokio::select! {
            biased;
            _ = owner.cancelled() => break,
            result = pipe.connect() => result.context(UNAVAILABLE)?,
        }
        let auth = tokio::select! {
            biased;
            _ = owner.cancelled() => break,
            result = timeout(FRAME_TIMEOUT, authenticate(pipe, descriptor)) => result,
        };
        if let Ok(Ok(auth)) = auth {
            engine = connection(pipe, &auth, owner, engine).await?;
            // Finish teardown before accepting another service pipe or acknowledging owner EOF.
            let (returned, stopped) = stop_engine(engine).await?;
            engine = returned;
            if stopped.is_err() {
                // Keep ownership and refuse reconnection until the owner closes.
                // Retry then, even if the first stop failed after the owner died.
                owner.cancelled().await;
                stop_engine(engine).await?.1?;
                break;
            }
        }
        match pipe.disconnect() {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_NOT_CONNECTED as i32) => {}
            Err(error) => return Err(error).context(UNAVAILABLE),
        }
    }
    Ok(())
}

async fn authenticate<'a>(
    pipe: &mut NamedPipeServer,
    descriptor: &'a Descriptor,
) -> Result<Auth<'a>> {
    let mut nonce = [0; 32];
    getrandom::fill(&mut nonce)?;
    pipe.write_all(&nonce).await?;
    let auth = Auth {
        key: &descriptor.key,
        nonce,
    };
    let request: Request = auth.read(pipe, Direction::Request, 0).await?;
    ensure!(matches!(request, Request::Hello {}), "{PROTOCOL}");
    auth.write(
        pipe,
        Direction::Reply,
        0,
        &Reply::Ready {
            scope: descriptor.scope,
        },
    )
    .await?;
    Ok(auth)
}

async fn connection<E: Engine>(
    pipe: &mut NamedPipeServer,
    auth: &Auth<'_>,
    owner: &CancellationToken,
    mut engine: E,
) -> Result<E> {
    let (mut input, mut output) = tokio::io::split(pipe);
    for sequence in 1..u64::MAX {
        let current_deadline = engine.deadline();
        let request = tokio::select! {
            biased;
            _ = owner.cancelled() => break,
            _ = expiry(current_deadline) => break,
            result = auth.read(&mut input, Direction::Request, sequence) => result,
        };
        let request = match request {
            Ok(Request::Hello {}) | Err(_) => break,
            Ok(request) => request,
        };
        let operation = CancellationToken::new();
        let token = operation.clone();
        let requested_deadline = match &request {
            Request::Start {
                lifetime_seconds, ..
            } if (1..=3600).contains(lifetime_seconds) => {
                Some(Instant::now() + Duration::from_secs(u64::from(*lifetime_seconds)))
            }
            _ => None,
        };
        let operation_deadline = if engine.session_id().is_none() {
            requested_deadline.or(current_deadline)
        } else {
            current_deadline
        };
        let mut task = tokio::task::spawn_blocking(move || {
            let reply = match dispatch(&mut engine, request, requested_deadline, &token) {
                Ok(reply) => reply,
                Err(error) if is_stop_unconfirmed(&error) => Reply::StopUnconfirmed {},
                Err(_) => Reply::Rejected {},
            };
            (engine, reply)
        });
        let mut interrupted = false;
        let interrupt = async {
            tokio::select! {
                _ = owner.cancelled() => {},
                _ = expiry(operation_deadline) => {},
                // The client must wait for the reply before sending another frame.
                _ = input.read_u8() => {},
            }
        };
        let finished = tokio::select! {
            biased;
            _ = interrupt => {
                interrupted = true;
                operation.cancel();
                (&mut task).await
            }
            finished = &mut task => finished,
        };
        let (returned, reply) = finished.context(STOP_UNCONFIRMED)?;
        engine = returned;
        if interrupted
            || auth
                .write(&mut output, Direction::Reply, sequence, &reply)
                .await
                .is_err()
        {
            break;
        }
    }
    Ok(engine)
}

fn is_stop_unconfirmed(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string() == STOP_UNCONFIRMED)
}

fn dispatch(
    engine: &mut impl Engine,
    request: Request,
    requested_deadline: Option<Instant>,
    cancel: &CancellationToken,
) -> Result<Reply> {
    match request {
        Request::Prepare {
            command,
            root_id,
            working_directory,
        } => {
            let command = Zeroizing::new(command);
            validate_command(&command)?;
            let mappings = engine.prepare(
                command,
                root_id.as_deref(),
                working_directory.as_deref(),
                cancel,
            )?;
            Ok(Reply::Prepared { mappings })
        }
        Request::Start { .. } => {
            let id = engine.start(requested_deadline.context(LIFETIME_LIMIT)?, cancel)?;
            Ok(Reply::Started { id: id.to_string() })
        }
        Request::Poll { id, offset } => {
            ensure!(
                engine.session_id() == Some(Uuid::parse_str(&id).context(UNKNOWN_SESSION)?),
                "{UNKNOWN_SESSION}"
            );
            ensure!(offset <= MAX_OUTPUT, "{INVALID_PAGE}");
            Ok(Reply::Progress {
                page: engine.poll(offset, cancel)?,
            })
        }
        Request::Input { id, text, eof } => {
            let text = Zeroizing::new(text);
            ensure!(
                engine.session_id() == Some(Uuid::parse_str(&id).context(UNKNOWN_SESSION)?),
                "{UNKNOWN_SESSION}"
            );
            ensure!(
                text.len() <= MAX_STDIN,
                "STDIN_LIMIT：單次輸入最多 4096 bytes。"
            );
            Ok(Reply::InputAccepted {
                bytes: engine.input(text.as_bytes(), eof, cancel)?,
                eof,
            })
        }
        Request::Terminate { id } => {
            ensure!(
                engine.session_id() == Some(Uuid::parse_str(&id).context(UNKNOWN_SESSION)?),
                "{UNKNOWN_SESSION}"
            );
            engine.terminate(cancel)?;
            Ok(Reply::Stopped {})
        }
        Request::Stop { id } => {
            let id = id
                .as_deref()
                .map(Uuid::parse_str)
                .transpose()
                .context(UNKNOWN_SESSION)?;
            ensure!(engine.session_id() == id, "{UNKNOWN_SESSION}");
            engine.stop().context(STOP_UNCONFIRMED)?;
            Ok(Reply::Stopped {})
        }
        Request::Hello {} => bail!("{PROTOCOL}"),
    }
}

async fn expiry(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

async fn stop_engine<E: Engine>(mut engine: E) -> Result<(E, Result<()>)> {
    tokio::task::spawn_blocking(move || {
        let result = engine.stop().context(STOP_UNCONFIRMED);
        (engine, result)
    })
    .await
    .context(STOP_UNCONFIRMED)
}

struct LocalMemory(*mut c_void);

impl Drop for LocalMemory {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

fn create_pipe(descriptor: &Descriptor) -> Result<NamedPipeServer> {
    let sid = logon_sid()?;
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})")
        .encode_utf16()
        .chain([0])
        .collect();
    let mut raw = ptr::null_mut();
    ensure!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut raw,
                ptr::null_mut(),
            )
        } != 0
            && !raw.is_null(),
        "{UNAVAILABLE}"
    );
    let security = LocalMemory(raw);
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0,
        bInheritHandle: 0,
    };
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .max_instances(1)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                descriptor.address(),
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
    .context(UNAVAILABLE)
}

fn logon_sid() -> Result<String> {
    let mut raw = ptr::null_mut();
    ensure!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } != 0,
        "{UNAVAILABLE}"
    );
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut size = 0;
    let sizing = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenLogonSid,
            ptr::null_mut(),
            0,
            &mut size,
        )
    };
    ensure!(
        sizing == 0
            && io::Error::last_os_error().raw_os_error() == Some(ERROR_INSUFFICIENT_BUFFER as i32)
            && (size_of::<TOKEN_GROUPS>() as u32..=16384).contains(&size),
        "{UNAVAILABLE}"
    );
    let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    let read = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenLogonSid,
            data.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    };
    ensure!(
        read != 0 && size as usize >= size_of::<TOKEN_GROUPS>(),
        "{UNAVAILABLE}"
    );
    // TokenLogonSid returns a single TOKEN_GROUPS entry and its SID in this buffer.
    let groups = unsafe { &*data.as_ptr().cast::<TOKEN_GROUPS>() };
    ensure!(
        groups.GroupCount == 1 && !groups.Groups[0].Sid.is_null(),
        "{UNAVAILABLE}"
    );
    let mut string = ptr::null_mut();
    let converted = unsafe { ConvertSidToStringSidW(groups.Groups[0].Sid, &mut string) };
    ensure!(converted != 0 && !string.is_null(), "{UNAVAILABLE}");
    let memory = LocalMemory(string.cast());
    let length = (0..192)
        .find(|&index| unsafe { *memory.0.cast::<u16>().add(index) } == 0)
        .context(UNAVAILABLE)?;
    String::from_utf16(unsafe { std::slice::from_raw_parts(memory.0.cast(), length) })
        .context(UNAVAILABLE)
}

#[cfg(test)]
pub(super) mod tests;
