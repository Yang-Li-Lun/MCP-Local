use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rmcp::{ServiceExt, transport::async_rw::AsyncRwTransport};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

use super::*;
use crate::{
    developer_vm::{
        InvocationFailure,
        sessions::{Sessions, Start},
    },
    filesystem::vm_mapping::Plan,
    security::{
        self, GuardedTransport, Runtime,
        authority::Authority,
        channel::Mode,
        policy::{Policy, Store},
    },
    server::Server,
};

struct SandboxFixture {
    root: PathBuf,
    visible: BTreeSet<Uuid>,
}

impl Backend for SandboxFixture {
    type Connector = ();

    fn list(&mut self) -> Result<BTreeSet<Uuid>> {
        Ok(self.visible.clone())
    }

    fn start(&mut self, id: Uuid, _: &str) -> Result<Uuid, InvocationFailure> {
        self.visible.insert(id);
        Ok(id)
    }

    fn connect(&mut self, _: Uuid) -> Result<Self::Connector> {
        Ok(())
    }

    fn stop(&mut self, id: Uuid) -> Result<()> {
        ensure!(
            !self.root.join("hold-stop.txt").exists(),
            "synthetic unconfirmed stop"
        );
        self.visible.remove(&id);
        Ok(())
    }

    fn close_connector(&mut self, _: &mut Self::Connector) -> Result<()> {
        Ok(())
    }
}

struct ExchangeFixture {
    directory: PathBuf,
    native: Native<SandboxFixture>,
}

impl Engine for ExchangeFixture {
    fn prepare(
        &mut self,
        command: Zeroizing<String>,
        root_id: Option<&str>,
        working_directory: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Mappings> {
        self.native.require_available()?;
        let (root_id, root) = self.native.workspace.developer_root(root_id)?;
        let mut budget = Budget::new(cancel.clone());
        let plan = Plan::fixture(root_id, root, &self.directory, &mut budget)?;
        let guest = Guest::prepare(root_id, root, working_directory, plan)?;
        self.native.stage(guest, command, &mut budget)
    }

    fn start(&mut self, deadline: Instant, cancel: &CancellationToken) -> Result<Uuid> {
        if self.directory.join("hold-start.txt").exists() {
            while self.directory.join("hold-start.txt").exists() && !cancel.is_cancelled() {
                thread::sleep(POLL);
            }
        }
        self.native.start(deadline, cancel)
    }

    fn poll(&mut self, offset: usize, cancel: &CancellationToken) -> Result<Progress> {
        self.native.poll(offset, cancel)
    }

    fn input(&mut self, text: &[u8], eof: bool, cancel: &CancellationToken) -> Result<usize> {
        self.native.input(text, eof, cancel)
    }

    fn terminate(&mut self, cancel: &CancellationToken) -> Result<()> {
        self.native.terminate(cancel)
    }

    fn stop(&mut self) -> Result<()> {
        self.native.stop()
    }

    fn session_id(&self) -> Option<Uuid> {
        self.native.session_id()
    }

    fn deadline(&self) -> Option<Instant> {
        self.native.deadline()
    }
}

#[test]
#[ignore = "private named-pipe fixture with a simulated Windows Sandbox"]
fn exchange_process() {
    let (boot, owner) = watch_owner().unwrap();
    let directory = PathBuf::from(std::env::var_os(FIXTURE_ENGINE).unwrap());
    let engine = ExchangeFixture {
        native: Native {
            workspace: Workspace::new(boot.reader, boot.workspace).unwrap(),
            lifecycle: Lifecycle::new(SandboxFixture {
                root: directory.clone(),
                visible: BTreeSet::new(),
            }),
            session: None,
        },
        directory,
    };
    run(boot.descriptor, owner, engine, write_marker).unwrap();
}

fn spool_path(shared: &Path) -> PathBuf {
    let mut entries = fs::read_dir(shared).unwrap().map(Result::unwrap);
    let spool = entries.next().unwrap().path();
    assert!(
        spool.is_dir()
            && spool
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("mcp-vm-session-")
    );
    assert!(entries.next().is_none());
    spool
}

#[test]
fn named_pipe_exchanges_authorized_sessions_and_keeps_terminal_output() {
    let directory = tempfile::Builder::new()
        .prefix("mcp-vm-pipe-io-")
        .tempdir()
        .unwrap();
    let (mut owner, shared) = vm_fixture(directory.path());
    let hold_stop = directory.path().join("hold-stop.txt");
    let encoded = owner.descriptor().unwrap();

    runtime().block_on(async {
        let mut client = GuardClient::connect(&encoded).await.unwrap();
        let reader = ReaderSettings::default();
        let workspace = WorkspaceSettings::single(shared.clone());
        client.require_scope(&reader, &workspace).unwrap();
        assert!(
            client
                .require_scope(&reader, &WorkspaceSettings::single(directory.path().into()))
                .is_err()
        );
        assert!(
            client
                .require_scope(
                    &ReaderSettings {
                        max_file_bytes: 4096,
                        ..reader
                    },
                    &workspace
                )
                .is_err()
        );
        assert!(
            client
                .start(FIXTURE_COMMAND, Some("unknown"), None, 900)
                .await
                .is_err()
        );
        assert!(fs::read_dir(&shared).unwrap().next().is_none());
        let id = client
            .start(FIXTURE_COMMAND, Some("main"), None, 900)
            .await
            .unwrap();
        let wrong_id = Uuid::from_u128(id.as_u128() ^ 1);
        let spool = spool_path(&shared);
        assert!(!client.poll(id, 0).await.unwrap().command_sent);
        assert!(!spool.join("request.json").exists());
        assert!(client.input(wrong_id, "unauthorized", false).await.is_err());
        assert!(client.poll(wrong_id, 0).await.is_err());
        assert!(client.stop(Some(wrong_id)).await.is_err());
        assert!(client.stop(None).await.is_err());
        assert!(client.terminate(wrong_id).await.is_err());
        assert!(!spool.join("stdin-000000.json").exists());

        let text = "固定測試輸入";
        assert_eq!(client.input(id, text, true).await.unwrap(), text.len());
        let input: Value =
            serde_json::from_slice(&fs::read(spool.join("stdin-000000.json")).unwrap()).unwrap();
        assert_eq!(
            input,
            json!({"data":STANDARD.encode(text.as_bytes()),"eof":true})
        );
        assert!(client.input(id, "late", false).await.is_err());
        assert!(!spool.join("stdin-000001.json").exists());

        fs::write(spool.join("ready.json"), br#"{"ready":true}"#).unwrap();
        assert!(client.poll(id, 0).await.unwrap().command_sent);
        let packet: Value =
            serde_json::from_slice(&fs::read(spool.join("request.json")).unwrap()).unwrap();
        assert_eq!(packet, json!({"command":FIXTURE_COMMAND}));

        let bytes: Vec<u8> = (0..70_000).map(|index| (index % 251) as u8).collect();
        let path = spool.join("output.txt");
        fs::write(&path, &bytes).unwrap();
        let first = client.poll(id, 0).await.unwrap();
        assert_eq!(first.total_bytes, bytes.len());
        assert_eq!(first.offset, 0);
        assert_eq!(first.next_offset, 32 * 1024);
        assert_eq!(first.bytes().unwrap(), bytes[..first.next_offset]);
        assert!(first.result.is_none());

        fs::write(spool.join("result.json"), br#"{"exit_code":7}"#).unwrap();
        fs::write(&hold_stop, b"fixture").unwrap();
        let error = client.poll(id, first.next_offset).await.unwrap_err();
        assert!(error.to_string().contains("VM_STOP_UNCONFIRMED"), "{error}");
        assert!(spool.is_dir());
        fs::remove_file(&hold_stop).unwrap();

        let second = client.poll(id, first.next_offset).await.unwrap();
        assert_eq!(second.result.unwrap().exit_code, 7);
        assert_eq!(second.offset, first.next_offset);
        assert_eq!(
            second.bytes().unwrap(),
            bytes[second.offset..second.next_offset]
        );
        assert!(spool.is_dir());
        assert!(client.input(id, "after stop", false).await.is_err());
        fs::write(&path, b"after confirmed stop").unwrap();
        let third = client.poll(id, second.next_offset).await.unwrap();
        assert_eq!(third.next_offset, bytes.len());
        assert_eq!(third.bytes().unwrap(), bytes[third.offset..]);
        assert_eq!(
            client.poll(id, 0).await.unwrap().bytes().unwrap(),
            first.bytes().unwrap()
        );
        assert_eq!(
            client.poll(id, bytes.len() + 4).await.unwrap().offset,
            bytes.len()
        );
        client.stop(Some(id)).await.unwrap();
        assert!(!spool.exists());
        assert!(client.poll(id, 0).await.is_err());

        let id = client
            .start(FIXTURE_COMMAND, None, None, 900)
            .await
            .unwrap();
        let spool = spool_path(&shared);
        fs::write(spool.join("ready.json"), br#"{"ready":true}"#).unwrap();
        fs::write(spool.join("result.json"), br#"{"exit_code":0}"#).unwrap();
        fs::write(&hold_stop, b"fixture").unwrap();
        assert!(
            client
                .poll(id, 0)
                .await
                .unwrap_err()
                .to_string()
                .contains("VM_STOP_UNCONFIRMED")
        );
        assert!(!spool.join("request.json").exists());
        fs::remove_file(&hold_stop).unwrap();
        client.stop(Some(id)).await.unwrap();
        assert!(!spool.exists());
    });
    owner.close_within(Duration::from_secs(5)).unwrap();
}

fn vm_fixture(directory: &Path) -> (GuardOwner, PathBuf) {
    let shared = directory.join("approved");
    let local = directory.join("local-app-data");
    fs::create_dir(&shared).unwrap();
    fs::create_dir(&local).unwrap();
    let mut command = child_command(
        "developer_vm::guard::tests::exchange::exchange_process",
        None,
    );
    command
        .env(FIXTURE_ENGINE, directory)
        .env("LOCALAPPDATA", local)
        .env("USERPROFILE", directory);
    let owner = GuardOwner::with_command(
        command,
        ReaderSettings::default(),
        WorkspaceSettings::single(shared.clone()),
    )
    .unwrap();
    (owner, shared)
}

fn fixture_start() -> Start {
    Start {
        command: Zeroizing::new(FIXTURE_COMMAND.into()),
        root_id: Some("main".into()),
        working_directory: None,
        lifetime_seconds: 60,
    }
}

async fn eventually(mut check: impl FnMut() -> bool) {
    timeout(Duration::from_secs(5), async {
        while !check() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .unwrap();
}

fn packet(spool: &Path, name: &str, bytes: &[u8]) {
    let pending = spool.join("fixture.tmp");
    fs::write(&pending, bytes).unwrap();
    fs::rename(pending, spool.join(name)).unwrap();
}

#[test]
fn background_sessions_dispatch_without_reads_and_keep_unicode_character_offsets() {
    let directory = tempfile::Builder::new()
        .prefix("mcp-vm-sessions-")
        .tempdir()
        .unwrap();
    let (mut owner, shared) = vm_fixture(directory.path());
    let encoded = owner.descriptor().unwrap();

    runtime().block_on(async {
        let service = Sessions::connect(&encoded).await.unwrap();
        let hold_start = directory.path().join("hold-start.txt");
        fs::write(&hold_start, b"fixture").unwrap();
        let initial = timeout(
            Duration::from_secs(5),
            service.start(fixture_start(), Duration::ZERO),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!initial.completed);
        assert!(initial.vm_id.is_none());
        assert_eq!(initial.session_id.len(), 32);
        assert!(
            service
                .start(fixture_start(), Duration::ZERO)
                .await
                .is_err()
        );
        let id = &initial.session_id;
        let mappings = service.mappings(id).unwrap();
        assert_eq!(mappings.guest_roots["main"], r"C:\MCP\mapping-0");
        assert!(mappings.guest_toolchains.is_empty());
        assert!(service.launch(id).unwrap().is_none());
        assert!(!spool_path(&shared).join("request.json").exists());
        fs::remove_file(hold_start).unwrap();
        eventually(|| service.launch(id).unwrap().is_some()).await;
        let launch = service.launch(id).unwrap().unwrap();
        assert_eq!(launch.mappings.guest_roots["main"], r"C:\MCP\mapping-0");
        assert!(launch.mappings.guest_toolchains.is_empty());
        let spool = spool_path(&shared);
        packet(&spool, "ready.json", br#"{"ready":true}"#);
        eventually(|| spool.join("request.json").is_file()).await;
        assert!(
            service
                .interact(id, Zeroizing::new("漢字".into()), true)
                .await
                .unwrap()
                .stdin_closed
        );
        assert!(
            service
                .interact(id, Zeroizing::new("late".into()), false)
                .await
                .is_err()
        );

        let prefix = "a".repeat(32 * 1024 - 1);
        let output = format!("{prefix}🦀中文");
        packet(&spool, "output.txt", output.as_bytes());
        packet(&spool, "result.json", br#"{"exit_code":7}"#);
        let terminal = service
            .read(id, prefix.len() - 1, 3, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(terminal.completed && terminal.vm_stopped);
        assert_eq!(terminal.exit_code, Some(7));
        assert_eq!(terminal.reason, None);
        assert_eq!(terminal.offset, prefix.len() - 1);
        assert_eq!(terminal.next_offset, prefix.len() + 2);
        assert_eq!(terminal.output.as_deref(), Some("a🦀中"));
        assert!(terminal.vm_id.is_none());
        assert!(!spool.exists());
        let list = serde_json::to_value(service.list()).unwrap();
        assert_eq!(list["max_active"], 1);
        assert!(list["sessions"][0].get("output").is_none());

        let next = service
            .start(fixture_start(), Duration::ZERO)
            .await
            .unwrap();
        assert_ne!(next.session_id, *id);
        eventually(|| service.launch(&next.session_id).unwrap().is_some()).await;
        let spool = spool_path(&shared);
        packet(&spool, "ready.json", br#"{"ready":true}"#);
        eventually(|| spool.join("request.json").is_file()).await;
        packet(&spool, "output.txt", "停止前的輸出".as_bytes());
        let hold_stop = directory.path().join("hold-stop.txt");
        fs::write(&hold_stop, b"fixture").unwrap();
        let error = service.terminate(&next.session_id).await.err().unwrap();
        assert!(error.to_string().contains("VM_STOP_UNCONFIRMED"), "{error}");
        assert!(
            !service
                .read(&next.session_id, 0, 12, Duration::ZERO)
                .await
                .unwrap()
                .vm_stopped
        );
        fs::remove_file(hold_stop).unwrap();
        let terminal = service.terminate(&next.session_id).await.unwrap();
        assert!(terminal.completed && terminal.vm_stopped);
        assert_eq!(terminal.reason, Some("terminated_by_client"));
        assert_eq!(terminal.output.as_deref(), Some("停止前的輸出"));
        assert!(!spool.exists());
        assert!(service.close(&next.session_id).await.unwrap().closed);
        assert!(service.close(&next.session_id).await.is_err());
        assert_eq!(
            service
                .read(id, prefix.len(), 3, Duration::ZERO)
                .await
                .unwrap()
                .output
                .as_deref(),
            Some("🦀中文")
        );
        let closed = service.close(id).await.unwrap();
        assert!(closed.closed && !closed.profile_removed);
        assert!(service.list().sessions.is_empty());
        assert!(
            service
                .start(
                    Start {
                        root_id: Some("unknown".into()),
                        ..fixture_start()
                    },
                    Duration::ZERO
                )
                .await
                .is_err()
        );
        assert!(service.list().sessions.is_empty());
        assert!(fs::read_dir(&shared).unwrap().next().is_none());

        let expired = service
            .start(
                Start {
                    lifetime_seconds: 3,
                    ..fixture_start()
                },
                Duration::ZERO,
            )
            .await
            .unwrap();
        eventually(|| service.launch(&expired.session_id).unwrap().is_some()).await;
        let spool = spool_path(&shared);
        packet(&spool, "ready.json", br#"{"ready":true}"#);
        eventually(|| spool.join("request.json").is_file()).await;
        packet(&spool, "output.txt", b"before expiry\xF0");
        let terminal = service
            .read(&expired.session_id, 0, 30, Duration::from_secs(8))
            .await
            .unwrap();
        assert!(terminal.completed && terminal.vm_stopped);
        assert_eq!(terminal.output.as_deref(), Some("before expiry\u{fffd}"));
        assert!(!spool.exists());
        assert!(service.close(&expired.session_id).await.unwrap().closed);
    });
    owner.close_within(Duration::from_secs(5)).unwrap();
}

struct McpPeer {
    input: WriteHalf<DuplexStream>,
    output: BufReader<ReadHalf<DuplexStream>>,
    id: u64,
}

impl McpPeer {
    async fn send(&mut self, message: Value) {
        self.input
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        self.send(json!({"jsonrpc":"2.0", "id":self.id, "method":method, "params":params}))
            .await;
        let mut line = String::new();
        timeout(Duration::from_secs(5), self.output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], self.id, "{reply}");
        assert!(reply["error"].is_null(), "{reply}");
        reply["result"].clone()
    }

    async fn call(&mut self, name: &str, args: Value, success: bool) -> Value {
        let result = self
            .request("tools/call", json!({"name":name, "arguments":args}))
            .await;
        assert_eq!(result["isError"], !success, "{name} {result}");
        assert!(result["_meta"].get(security::SESSION_REVISION).is_none());
        result["structuredContent"].clone()
    }
}

#[test]
fn developer_mcp_registers_original_contract_and_guards_session_output_and_export() {
    let directory = tempfile::Builder::new()
        .prefix("mcp-vm-wire-")
        .tempdir()
        .unwrap();
    let (mut owner, shared) = vm_fixture(directory.path());
    let encoded = owner.descriptor().unwrap();
    let settings = directory.path().join("settings");
    let mut policy = Policy::default();
    policy.enabled = true;
    Store(settings.clone()).save(policy, "").unwrap();
    let mut authority = Authority::new(settings.clone());
    let signed = authority.begin(Mode::DeveloperControl).unwrap();
    assert!(Runtime::attach(settings.clone(), &signed, Mode::HostControl).is_err());
    let security = Arc::new(Mutex::new(
        Runtime::attach(settings, &signed, Mode::DeveloperControl).unwrap(),
    ));
    fs::create_dir(shared.join("project")).unwrap();
    fs::write(shared.join("project").join("out.txt"), "開發 VM 合成產出").unwrap();

    runtime().block_on(async {
        let sessions = Sessions::connect(&encoded).await.unwrap();
        let server = Server::developer(ReaderSettings::default(), WorkspaceSettings::single(shared.clone()), sessions, security.clone()).unwrap();
        let (client, service) = tokio::io::duplex(64 * 1024);
        let (rx, tx) = tokio::io::split(service);
        let transport = GuardedTransport::shared(AsyncRwTransport::new(rx, tx), security);
        let running = tokio::spawn(async move { server.serve(transport).await.unwrap().waiting().await.unwrap() });
        let (rx, tx) = tokio::io::split(client);
        let mut peer = McpPeer { input: tx, output: BufReader::new(rx), id: 0 };
        let initialized = peer.request("initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}})).await;
        assert_eq!(initialized["serverInfo"]["name"], "local-files-developer-control");
        peer.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        let listed = peer.request("tools/list", json!({})).await;
        let contract: Value = serde_json::from_str(include_str!("../../../../contracts/tool-contract-developer-control.json")).unwrap();
        let tools = listed["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 34);
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            assert_eq!(json!({"inputSchema": tool["inputSchema"], "annotations": tool["annotations"]}), contract[name], "{name}");
        }
        let diagnostics = peer.call("server_diagnostics", json!({}), true).await;
        assert_eq!(diagnostics["access_mode"], "developer_control");
        assert_eq!(diagnostics["consistent"], true);
        peer.call("read_host_file", json!({"path":"out.txt"}), false).await;
        peer.call("start_process", json!({"command":FIXTURE_COMMAND,"timeout_ms":"0"}), false).await;
        peer.call("write_file", json!({"path":"from-tool.txt","content":"測試"}), true).await;
        assert_eq!(fs::read_to_string(shared.join("from-tool.txt")).unwrap(), "測試");

        let started = peer.call("start_process", json!({"command":FIXTURE_COMMAND,"timeout_ms":0,"working_directory":"project"}), true).await;
        let id = started["session_id"].as_str().unwrap();
        assert_eq!(started["guest_roots"]["main"], r"C:\MCP\mapping-0");
        assert_eq!(started["security_output"], "withheld_until_complete");
        let spool = spool_path_in_fixture_containing_session(&shared);
        packet(&spool, "ready.json", br#"{"ready":true}"#);
        eventually(|| spool.join("request.json").is_file()).await;
        let before = peer.call("export_session_file", json!({"session_id":id,"path":"out.txt","destination":"copy.txt"}), false).await;
        assert!(before.is_null());
        assert!(!shared.join("copy.txt").exists());
        packet(&spool, "output.txt", b"Bearer synthetic_token_123456\nvisible");
        let active = peer.call("read_process_output", json!({"session_id":id,"wait_ms":200}), true).await;
        assert_eq!(active["security_output"], "withheld_until_complete");
        assert_eq!(active["output"], "");
        packet(&spool, "result.json", br#"{"exit_code":0}"#);
        let result = peer.call("read_process_output", json!({"session_id":id,"offset":8,"length":4,"wait_ms":2000}), true).await;
        assert_eq!(result["vm_stopped"], true);
        assert_eq!(result["security_output"], "masked");
        assert!(!result["output"].as_str().unwrap().contains("ynth"));
        authority.tick();

        peer.call("export_session_file", json!({"session_id":id,"path":"../from-tool.txt","destination":"copy.txt"}), false).await;
        let receipt = peer.call("export_session_file", json!({"session_id":id,"path":"out.txt","destination":"copy.txt"}), true).await;
        assert_eq!(receipt["session_id"], id);
        assert_eq!(fs::read_to_string(shared.join("copy.txt")).unwrap(), "開發 VM 合成產出");
        peer.call("export_session_file", json!({"session_id":id,"path":"out.txt","destination":"copy.txt"}), false).await;
        peer.call("close_session", json!({"session_id":id}), true).await;

        peer.call("start_process", json!({"command":FIXTURE_COMMAND,"timeout_ms":0}), true).await;
        let spool = spool_path_in_fixture_containing_session(&shared);
        packet(&spool, "ready.json", br#"{"ready":true}"#);
        eventually(|| spool.join("request.json").is_file()).await;
        authority.close();
        eventually(|| !spool.exists()).await;
        peer.call("write_file", json!({"path":"revoked.txt","content":"fixture"}), false).await;
        assert!(!shared.join("revoked.txt").exists());
        drop(peer);
        timeout(Duration::from_secs(5), running).await.unwrap().unwrap();
    });
    owner.close_within(Duration::from_secs(5)).unwrap();
}

fn spool_path_in_fixture_containing_session(shared: &Path) -> PathBuf {
    let mut entries = fs::read_dir(shared)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("mcp-vm-session-")
        });
    let path = entries.next().unwrap().path();
    assert!(path.is_dir() && entries.next().is_none());
    path
}

#[test]
#[ignore = "requires the built application and installed official CLI, opens an idle native guard only"]
fn native_developer_stdio_authenticates_and_lists_tools_without_starting_a_sandbox() {
    let directory = tempfile::Builder::new()
        .prefix("mcp-native-developer-wire-")
        .tempdir()
        .unwrap();
    let shared = directory.path().join("approved");
    let local = directory.path().join("local");
    fs::create_dir(&shared).unwrap();
    fs::create_dir(&local).unwrap();
    let mut authority = Authority::new(local.join("MCP-Local-Rust"));
    let signed = authority.begin(Mode::DeveloperControl).unwrap();
    let mut command = native_mcp_command();
    command
        .args([
            "--access-mode",
            "developer_control",
            "--require-security-channel",
            "--root",
        ])
        .arg(&shared)
        .env("LOCALAPPDATA", &local)
        .env("USERPROFILE", directory.path())
        .env(crate::security::channel::ENV_NAME, signed.as_str());
    let (mut child, mut io) = NativeMcp::spawn(command);
    io.send(HELLO);
    assert_eq!(
        io.reply(&mut authority)["result"]["serverInfo"]["name"],
        "local-files-developer-control"
    );
    io.send(INITIALIZED);
    io.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    assert_eq!(
        io.reply(&mut authority)["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        34
    );
    let reader = io.close();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        authority.tick();
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "idle developer service did not shut down"
        );
        thread::sleep(POLL);
    }
    reader.join().unwrap();
    authority.close();
    assert!(fs::read_dir(shared).unwrap().next().is_none());
}

const HELLO: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}"#;
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

struct NativeMcp {
    input: std::process::ChildStdin,
    replies: std::sync::mpsc::Receiver<Value>,
    reader: thread::JoinHandle<()>,
}

fn native_mcp_command() -> Command {
    let exe = PathBuf::from(std::env::var_os(APP_BINARY).expect("built app executable"));
    assert!(exe.is_absolute() && exe.file_name().unwrap() == "mcp-local-rust.exe");
    let mut command = Command::new(exe);
    command
        .env_remove("CONTROL_PLANE_API_KEY")
        .env_remove(ENV_NAME)
        .env_remove(crate::security::channel::ENV_NAME)
        .creation_flags(CREATE_NO_WINDOW);
    command
}

impl NativeMcp {
    fn spawn(mut command: Command) -> (Child, Self) {
        use std::io::{BufRead, BufReader};
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (send, replies) = std::sync::mpsc::sync_channel(4);
        let reader = thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if send
                    .send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                    .is_err()
                {
                    break;
                }
            }
        });
        (
            child,
            Self {
                input,
                replies,
                reader,
            },
        )
    }

    fn send(&mut self, frame: &str) {
        writeln!(self.input, "{frame}").unwrap();
    }

    fn reply(&self, authority: &mut Authority) -> Value {
        self.reply_within(authority, Duration::from_secs(10))
    }

    fn reply_within(&self, authority: &mut Authority, wait: Duration) -> Value {
        let deadline = Instant::now() + wait;
        loop {
            authority.tick();
            match self.replies.recv_timeout(Duration::from_millis(100)) {
                Ok(reply) => return reply,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(error) => panic!("developer stdio: {error}"),
            }
        }
    }

    fn close(self) -> thread::JoinHandle<()> {
        drop(self.input);
        drop(self.replies);
        self.reader
    }
}

#[test]
#[ignore = "requires the freshly built native STDIO app, uses a synthetic VM and local Windows Job"]
fn delegated_desktop_guard_cleans_after_native_service_job_is_terminated() {
    let directory = tempfile::Builder::new()
        .prefix("mcp-delegated-job-")
        .tempdir()
        .unwrap();
    let (mut owner, shared) = vm_fixture(directory.path());
    let local = directory.path().join("security-local");
    fs::create_dir(&local).unwrap();
    let mut authority = Authority::new(local.join("MCP-Local-Rust"));
    let signed = authority.begin(Mode::DeveloperControl).unwrap();
    let encoded = owner.descriptor().unwrap();
    let mut command = native_mcp_command();
    command
        .args([
            "--access-mode",
            "developer_control",
            "--require-security-channel",
            REQUIRE_DELEGATED,
            "--root",
        ])
        .arg(&shared)
        .env("LOCALAPPDATA", &local)
        .env("USERPROFILE", directory.path())
        .env(crate::security::channel::ENV_NAME, signed.as_str())
        .env(ENV_NAME, encoded.as_str());
    let (mut child, mut io) = NativeMcp::spawn(command);
    let job = crate::windows_job::Job::connection().unwrap();
    job.assign(&child).unwrap();
    assert!(in_job(child.as_raw_handle()).unwrap());
    assert!(!in_job(owner.child.as_raw_handle()).unwrap());
    io.send(HELLO);
    assert_eq!(
        io.reply(&mut authority)["result"]["serverInfo"]["name"],
        "local-files-developer-control"
    );
    io.send(INITIALIZED);
    io.send(&json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"start_process", "arguments":{"command":FIXTURE_COMMAND,"timeout_ms":0}}}).to_string());
    let reply = io.reply(&mut authority);
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let spool = spool_path(&shared);
    packet(&spool, "ready.json", br#"{"ready":true}"#);
    runtime().block_on(eventually(|| spool.join("request.json").is_file()));
    job.terminate().unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(owner.child.try_wait().unwrap().is_none());
    owner.close_within(Duration::from_secs(5)).unwrap();
    assert!(!spool.exists());
    io.close().join().unwrap();
    authority.close();
}

#[test]
#[ignore = "starts one dedicated offline Windows Sandbox with an isolated synthetic mapped root"]
fn native_offline_sandbox_runs_mcp_stdin_output_and_export() {
    assert_eq!(
        std::env::var("MCP_LOCAL_RUST_VM_ACCEPTANCE").as_deref(),
        Ok("1"),
        "offline Windows Sandbox acceptance requires explicit opt-in"
    );
    let mut cli = cli::Cli::resolve().unwrap();
    assert!(
        cli.list().unwrap().is_empty(),
        "Windows Sandbox is already in use"
    );
    let directory = tempfile::Builder::new()
        .prefix("mcp-native-sandbox-")
        .tempdir()
        .unwrap();
    let shared = directory.path().join("approved");
    let project = shared.join("project");
    let local = directory.path().join("local");
    fs::create_dir(&shared).unwrap();
    fs::create_dir(&project).unwrap();
    fs::create_dir(&local).unwrap();
    fs::write(project.join("seed.txt"), "合成根目錄").unwrap();
    let mut authority = Authority::new(local.join("MCP-Local-Rust"));
    let signed = authority.begin(Mode::DeveloperControl).unwrap();
    let mut command = native_mcp_command();
    command
        .args([
            "--access-mode",
            "developer_control",
            "--require-security-channel",
            "--root",
        ])
        .arg(&shared)
        .env("LOCALAPPDATA", &local)
        .env("USERPROFILE", directory.path())
        .env(crate::security::channel::ENV_NAME, signed.as_str());
    let (mut child, mut io) = NativeMcp::spawn(command);
    let acceptance = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        io.send(HELLO);
        assert_eq!(
            io.reply(&mut authority)["result"]["serverInfo"]["name"],
            "local-files-developer-control"
        );
        io.send(INITIALIZED);
        let script = crate::developer_vm::tests::BRIDGE_FIXTURE_SCRIPT;
        let began = Instant::now();
        let deadline = began + Duration::from_secs(175);
        let mut request_id = 1;
        let mut call = |name: &str, arguments: Value| {
            request_id += 1;
            io.send(&json!({"jsonrpc":"2.0", "id":request_id, "method":"tools/call", "params":{"name":name, "arguments":arguments}}).to_string());
            let response = io.reply_within(&mut authority, Duration::from_secs(35));
            assert_eq!(response["id"], request_id);
            let result = &response["result"];
            assert_eq!(result["isError"], false, "{name}: {result}");
            result["structuredContent"].clone()
        };
        let started = call(
            "start_process",
            json!({"command":script,"working_directory":"project","lifetime_seconds":180,"timeout_ms":0}),
        );
        assert_eq!(started["sandbox"], "windows_sandbox_vm");
        assert_eq!(started["guest_roots"], json!({"main":r"C:\MCP\mapping-0"}));
        let id = started["session_id"].as_str().unwrap();
        loop {
            let progress = call(
                "read_process_output",
                json!({"session_id":id,"wait_ms":500}),
            );
            if progress["output"]
                .as_str()
                .is_some_and(|output| output.contains("sandbox-ready:合成根目錄"))
            {
                assert!(progress["vm_id"].is_string(), "{progress}");
                break;
            }
            assert_eq!(
                progress["completed"], false,
                "VM completed before command readiness: {progress}"
            );
            assert!(
                Instant::now() < deadline,
                "isolated Sandbox startup did not complete"
            );
        }
        println!(
            "isolated Sandbox boot to guest output: {:.2} s",
            began.elapsed().as_secs_f64()
        );
        let input = "測試 🦀\n";
        let receipt = call(
            "interact_with_process",
            json!({"session_id":id,"input":input,"close_stdin":true}),
        );
        assert_eq!(receipt["bytes_sent"], input.len());
        assert_eq!(receipt["stdin_closed"], true);
        let terminal = loop {
            let progress = call(
                "read_process_output",
                json!({"session_id":id,"wait_ms":2000}),
            );
            if progress["completed"] == true {
                break progress;
            }
            assert!(
                Instant::now() < deadline,
                "isolated Sandbox command did not finish"
            );
        };
        assert_eq!(terminal["vm_stopped"], true, "{terminal}");
        assert_eq!(terminal["exit_code"], 7, "{terminal}");
        let output = terminal["output"].as_str().unwrap();
        assert!(
            output.contains("sandbox-output:測試 🦀") && output.contains("sandbox-stderr"),
            "{terminal}"
        );
        let receipt = call(
            "export_session_file",
            json!({"session_id":id,"path":"proof.txt","destination":"export.txt"}),
        );
        assert_eq!(receipt["session_id"], id);
        assert_eq!(
            fs::read_to_string(shared.join("export.txt")).unwrap(),
            "測試 🦀"
        );
        assert_eq!(
            call("close_session", json!({"session_id":id}))["closed"],
            true
        );
        assert_eq!(call("list_sessions", json!({}))["sessions"], json!([]));
        println!(
            "isolated Sandbox MCP lifecycle: {:.2} s",
            began.elapsed().as_secs_f64()
        );
    }));
    let reader = io.close();
    let shutdown_result =
        wait_for_native_stdio(&mut child, &mut authority, Duration::from_secs(200));
    authority.close();
    let inventory = cli.list();
    if !shutdown_result || !inventory.as_ref().is_ok_and(BTreeSet::is_empty) {
        let retained = directory.keep();
        panic!(
            "Sandbox shutdown could not be established; isolated fixtures retained at {retained:?}"
        );
    }
    reader.join().unwrap();
    if let Err(error) = acceptance {
        std::panic::resume_unwind(error);
    }
    assert!(fs::read_dir(shared).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("mcp-vm-session-")
    }));
}

fn wait_for_native_stdio(child: &mut Child, authority: &mut Authority, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        authority.tick();
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            _ => return false,
        }
    }
}
