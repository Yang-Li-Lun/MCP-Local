use std::fs;

use rmcp::{
    RoleServer,
    service::TxJsonRpcMessage,
    transport::{Transport, async_rw::AsyncRwTransport},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader, duplex, split};

use super::{
    GuardedTransport, Output, Runtime,
    audit::{Audit, Event, Kind},
    authority::{Authority, Effective},
    channel::Mode,
    guard::{BLOCK_TEXT, Guard, Review},
    policy::{Policy, Risk, Rule, RuleKind, Store, Strategy},
    storage,
};

fn enabled() -> Policy {
    let mut policy = Policy::default();
    policy.enabled = true;
    policy
}

#[test]
fn staged_session_output_is_refused_after_the_signed_policy_changes() {
    for mode in [Mode::DeveloperControl, Mode::HostControl] {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().to_owned();
        let mut authority = Authority::new(directory.clone());
        let saved = authority.save(Policy::default(), "").unwrap();
        let descriptor = authority.begin(mode).unwrap();
        let mut runtime = Runtime::attach(directory, &descriptor, mode).unwrap();
        let staged = runtime.session_output("Bearer synthetic_token_123456", false);
        assert!(staged.text.is_none());
        assert!(matches!(
            runtime.protect_response(Some(&staged.revision), || Ok(json!("safe"))),
            Output::Unchanged
        ));
        authority.tick();
        authority.save(enabled(), &saved.revision).unwrap();
        assert!(matches!(
            runtime.protect_response(Some(&staged.revision), || panic!(
                "stale page must not be serialized"
            )),
            Output::Block
        ));
        let current = runtime.session_output("Bearer synthetic_token_123456", false);
        assert_eq!(current.status, Some("withheld_until_complete"));
        assert_eq!(current.text.as_deref(), Some(""));
        authority.close();
    }
}

#[test]
fn managed_policy_confirmations_events_and_local_host_lease_are_bound_to_one_connection() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().to_owned();
    let mut authority = Authority::new(directory.clone());
    let saved = authority.save(enabled(), "").unwrap();
    let descriptor = authority.begin(Mode::HostControl).unwrap();
    assert!(authority.grant_target().is_err());
    let mut runtime = Runtime::attach(directory.clone(), &descriptor, Mode::HostControl).unwrap();
    authority.tick();
    assert_eq!(authority.view().effective, Effective::Confirmed);
    let target = authority.grant_target().unwrap();
    authority.grant(1, target).unwrap();
    runtime.heartbeat();
    authority.tick();
    assert!(authority.view().reports[0].host_authorized);
    assert!((1..=60).contains(&authority.view().lease.remaining()));

    assert!(matches!(
        runtime.protect(|| Ok(json!({"x":"Bearer synthetic_token_secret123"}))),
        Output::Payload(_)
    ));
    authority.tick();
    assert!(
        Audit(directory.clone())
            .load()
            .unwrap()
            .iter()
            .any(|row| row.event.kind == Kind::ResponseMasked)
    );
    runtime.heartbeat();
    assert!(runtime.managed.as_ref().unwrap().pending.is_empty());
    let stale_confirm = authority.grant_target().unwrap();
    let mut next = saved.policy;
    next.strategy = Strategy::Block;
    authority.save(next, &saved.revision).unwrap();
    runtime.heartbeat();
    authority.tick();
    assert_eq!(authority.view().lease.remaining(), 0);
    assert!(!authority.view().reports[0].host_authorized);
    assert!(authority.grant(1, stale_confirm).is_err());
    assert!(matches!(
        runtime.protect(|| Ok(json!({"x":"Bearer synthetic_token_secret123"}))),
        Output::Block
    ));
    authority.close();
    assert!(matches!(
        runtime.protect(|| Ok(json!("ordinary"))),
        Output::Block
    ));
}

#[test]
fn lost_events_require_recovery_and_recovery_rejects_external_policy_downgrade() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().to_owned();
    let mut authority = Authority::new(directory.clone());
    let saved = authority.save(enabled(), "").unwrap();
    let descriptor = authority.begin(Mode::HostControl).unwrap();
    let mut runtime = Runtime::attach(directory.clone(), &descriptor, Mode::HostControl).unwrap();
    authority.tick();
    let target = authority.grant_target().unwrap();
    authority.grant(1, target).unwrap();
    runtime.heartbeat();
    for _ in 0..super::channel::EVENTS {
        runtime.record(Kind::ResponseBlocked, &[]).unwrap();
    }
    assert!(runtime.record(Kind::ResponseBlocked, &[]).is_err());
    runtime.fail();
    runtime.heartbeat();
    authority.tick();
    assert_eq!(authority.view().effective, Effective::Fault);
    assert_eq!(authority.view().lease.remaining(), 0);
    assert!(
        Audit(directory.clone())
            .load()
            .unwrap()
            .iter()
            .any(|row| row.event.kind == Kind::EventsLost)
    );
    assert!(matches!(
        runtime.protect(|| Ok(json!("ordinary"))),
        Output::Block
    ));
    let changed = Store(directory.clone())
        .save(Policy::default(), &saved.revision)
        .unwrap();
    assert!(authority.recover().is_err());
    Store(directory.clone())
        .save(saved.policy, &changed.revision)
        .unwrap();
    authority.recover().unwrap();
    runtime.heartbeat();
    authority.tick();
    let restored = authority.view();
    assert_eq!(restored.effective, Effective::Confirmed);
    assert_eq!(restored.lease.remaining(), 0);
    assert!(!restored.reports[0].host_authorized);
}

#[test]
#[ignore = "managed_stdio_in_separate_process uses this as the Rust SDK peer"]
fn managed_stdio_fixture_peer() {
    let root = std::path::PathBuf::from(std::env::var_os("MCP_TEST_MANAGED_ROOT").unwrap());
    let descriptor = zeroize::Zeroizing::new(std::env::var(super::channel::ENV_NAME).unwrap());
    let state = super::state_directory().unwrap();
    let workspace = crate::server::Workspace::new(
        crate::settings::ReaderSettings::default(),
        crate::settings::WorkspaceSettings::single(root),
    )
    .unwrap();
    let runtime = Runtime::attach(state, &descriptor, Mode::ReadOnly).unwrap();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(crate::server::stdio(
            crate::server::Server::new(workspace),
            runtime,
        ))
        .unwrap();
}

#[test]
fn managed_stdio_in_separate_process_applies_policy_and_returns_authenticated_audit() {
    use std::{
        io::{BufRead, BufReader, Write},
        os::windows::process::CommandExt,
        process::{Child, Command, Stdio},
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    struct Peer(Child);
    impl Drop for Peer {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let temp = tempfile::Builder::new()
        .prefix("mcp-signed-")
        .tempdir()
        .unwrap();
    let shared = temp.path().join("shared");
    let appdata = temp.path().join("appdata");
    let state = appdata.join("MCP-Local-Rust");
    fs::create_dir(&shared).unwrap();
    fs::create_dir(&appdata).unwrap();
    fs::write(
        shared.join("fixture.txt"),
        "Bearer synthetic_cross_process_123456\nordinary\n",
    )
    .unwrap();
    let mut owner = Authority::new(state.clone());
    owner.save(enabled(), "").unwrap();
    let descriptor = owner.begin(Mode::ReadOnly).unwrap();
    #[cfg(feature = "desktop")]
    let monitor = crate::monitor::Bus::create().unwrap();
    let mut command = if let Some(exe) = std::env::var_os("MCP_TEST_MANAGED_EXE") {
        let mut command = Command::new(exe);
        command
            .arg("--require-security-channel")
            .arg("--root")
            .arg(&shared);
        command
    } else {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "security::tests::managed_stdio_fixture_peer",
                "--ignored",
                "--quiet",
            ])
            .env("MCP_TEST_MANAGED_ROOT", &shared);
        command
    };
    #[cfg(feature = "desktop")]
    {
        assert!(!monitor.attached().unwrap());
        command.env(crate::monitor::ENV_NAME, monitor.name());
    }
    let mut peer = Peer(
        command
            .env("LOCALAPPDATA", &appdata)
            .env("USERPROFILE", temp.path())
            .env(super::channel::ENV_NAME, descriptor.as_str())
            .env_remove("CONTROL_PLANE_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap(),
    );
    let mut input = peer.0.stdin.take().unwrap();
    let output = peer.0.stdout.take().unwrap();
    let (send, lines) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else {
                break;
            };
            if let Ok(frame) = serde_json::from_str::<Value>(&line)
                && send.send(frame).is_err()
            {
                break;
            }
        }
    });
    let mut receive = |id: u64| {
        let started = Instant::now();
        loop {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "managed SDK response {id}"
            );
            owner.tick();
            match lines.recv_timeout(Duration::from_millis(40)) {
                Ok(frame) => {
                    assert_eq!(frame["id"], id);
                    return frame;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => panic!("managed peer exited: {error}"),
            }
        }
    };
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"isolated","version":"1"}
    }})).unwrap();
    input.flush().unwrap();
    assert_eq!(
        receive(1)["result"]["serverInfo"]["name"],
        "local-files-readonly"
    );
    #[cfg(feature = "desktop")]
    assert!(monitor.attached().unwrap());
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"read_file","arguments":{"path":"fixture.txt"}
        }})
    )
    .unwrap();
    input.flush().unwrap();
    let frame = receive(2);
    assert_eq!(frame["result"]["isError"], false);
    assert!(frame.to_string().contains("ordinary"));
    assert!(!frame.to_string().contains("synthetic_cross_process"));
    #[cfg(feature = "desktop")]
    {
        use crate::monitor::Status;

        let started = Instant::now();
        let mut cursor = 0;
        let record = loop {
            let (lost, records) = monitor.poll(&mut cursor);
            assert_eq!(lost, 0);
            if let Some((_, _, record)) = records.into_iter().find(|(_, _, record)| {
                record
                    .response
                    .as_ref()
                    .is_some_and(|response| response["id"] == 2)
            }) {
                break record;
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "monitor did not record the managed STDIO response"
            );
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(record.tool, "read_file");
        assert_eq!(record.access_mode, Some(Mode::ReadOnly));
        assert_eq!(record.status, Status::Success);
        assert_eq!(
            record.request.unwrap()["params"]["arguments"]["path"],
            "fixture.txt"
        );
        assert_eq!(record.response, Some(frame));
    }
    assert_eq!(owner.view().effective, Effective::Confirmed);
    owner.tick();
    let audit = Audit(state).load().unwrap();
    assert!(
        audit
            .iter()
            .any(|row| row.event.kind == Kind::ResponseMasked)
    );
    assert!(
        !serde_json::to_string(&audit)
            .unwrap()
            .contains("fixture.txt")
    );
    drop(input);
    let started = Instant::now();
    loop {
        owner.tick();
        if let Some(status) = peer.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "managed peer did not exit"
        );
        thread::sleep(Duration::from_millis(20));
    }
    #[cfg(feature = "desktop")]
    assert!(!monitor.attached().unwrap());
    owner.close();
}

#[test]
fn policy_conflicts_and_invalid_or_missing_files_fail_closed() {
    let temp = tempfile::Builder::new()
        .prefix("mcp-security-")
        .tempdir()
        .unwrap();
    let directory = temp.path().join("settings");
    let store = Store(directory.clone());
    assert!(!store.load().unwrap().policy.enabled);
    assert!(!directory.exists());

    let saved = store.save(enabled(), "").unwrap();
    assert!(store.save(Policy::default(), "").is_err());
    assert_eq!(store.load().unwrap().revision, saved.revision);
    let invalid = [
        json!({"version":2}),
        json!({"version":1,"enabled":true,"extra":true}),
    ];
    let path = directory.join("security-policy.json");
    for policy in invalid {
        let bytes = serde_json::to_vec(&policy).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(Policy::default(), &saved.revision).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    let mut duplicate = serde_json::to_string(&saved.policy).unwrap();
    duplicate.insert_str(1, "\"enabled\":false,");
    fs::write(&path, duplicate).unwrap();
    assert!(store.load().is_err());
    let encoded = storage::encoded(&saved.policy).unwrap();
    fs::write(&path, encoded).unwrap();
    let mut runtime = Runtime::open(directory.clone()).unwrap();
    fs::remove_file(&path).unwrap();
    assert!(matches!(
        runtime.protect(|| Ok(json!({"text":"safe"}))),
        Output::Block
    ));
    assert!(Runtime::open(directory).is_err());
}

#[test]
fn guard_handles_encoded_json_credentials_binary_limits_and_key_collisions() {
    let policy = enabled();
    let payload = json!({
        "content":[{"type":"text","text":r#"{"password":"unused-synthetic", "list":["Bearer synthetic_token_123456","ghp_synthetic_123456"]}"#}],
        "structuredContent":{"secret":123456789, "Bearer fixture_token_123456":"data"}
    });
    let guard = Guard::new(&policy).unwrap();
    let Review::Masked {
        payload: result,
        findings,
    } = guard.inspect(payload.clone())
    else {
        panic!("expected masking");
    };
    assert!(
        findings
            .iter()
            .any(|row| row.rule == "password" && row.risk == Risk::High)
    );
    let rendered = result.to_string();
    for value in [
        "unused-synthetic",
        "synthetic_token",
        "fixture_token",
        "ghp_synthetic",
        "123456789",
    ] {
        assert!(!rendered.contains(value));
    }
    let embedded: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(embedded["password"], "[已遮罩]");
    assert_eq!(result["structuredContent"]["secret"], "[已遮罩]");

    let mut blocking = policy.clone();
    blocking.strategy = Strategy::Block;
    assert!(matches!(
        Guard::new(&blocking).unwrap().inspect(payload),
        Review::Blocked { checked: true, .. }
    ));
    let mut deep = json!("leaf");
    for _ in 0..33 {
        deep = json!({"key":deep});
    }
    for payload in [
        deep,
        json!({"encoding":"base64","data":"Zml4dHVyZQ=="}),
        json!({"nested":{"blob":"Zml4dHVyZQ=="}}),
        json!({"type":"resource_link","uri":"memory:synthetic"}),
        json!({"text":"z".repeat(4*1024*1024 + 1)}),
        json!({"Bearer synthetic_token_123456":"x","Bearer synthetic_token_654321":"y"}),
    ] {
        assert!(matches!(
            guard.inspect(payload),
            Review::Blocked { checked: false, .. }
        ));
    }
    let private = String::from("-----BEGIN PRIVATE KEY-----") + &"字".repeat(80000);
    let Review::Masked { payload, .. } = guard.inspect(json!(private)) else {
        panic!("expected private block masking");
    };
    assert!(payload.as_str().unwrap().chars().all(|ch| ch == '█'));
    assert_eq!(
        payload.as_str().unwrap().chars().count(),
        private.chars().count()
    );

    let mut custom = policy;
    custom.rules = vec![Rule {
        id: "test_fixture".into(),
        kind: RuleKind::Literal,
        pattern: "a+[b]".into(),
        enabled: true,
        risk: Risk::Low,
    }];
    assert!(matches!(
        Guard::new(&custom).unwrap().inspect(json!("a+[]")),
        Review::Passed(_)
    ));
    assert!(matches!(
        Guard::new(&custom).unwrap().inspect(json!("a+[b]")),
        Review::Masked { .. }
    ));
}

#[test]
fn audit_chain_rejects_tampering_and_clears_only_a_confirmed_revision() {
    let temp = tempfile::tempdir().unwrap();
    let audit = Audit(temp.path().to_owned());
    let event = Event::new(Kind::ResponseBlocked, Risk::High, 1).unwrap();
    audit.append(event.clone()).unwrap();
    audit.append(event).unwrap();
    let exported = audit.export().unwrap();
    assert_eq!(exported["chain"].as_array().unwrap().len(), 1);
    assert_eq!(exported["contains_sensitive_originals"], false);
    let revision = exported["revision"].as_str().unwrap();
    assert!(audit.clear(revision, "yes").is_err());
    assert!(audit.clear("", "清除安全事件").is_err());
    audit.clear(revision, "清除安全事件").unwrap();
    let cleared = audit.export().unwrap();
    assert_eq!(cleared["chain"][0]["event"]["kind"], "audit_cleared");
    assert_eq!(cleared["chain"][0]["event"]["prior_head"], revision);
    let file = temp.path().join("security-audit.jsonl");
    let changed = fs::read_to_string(&file)
        .unwrap()
        .replace("audit_cleared", "synthetic_test");
    fs::write(&file, &changed).unwrap();
    assert!(audit.export().is_err());
    assert!(audit.clear(revision, "清除安全事件").is_err());
    assert_eq!(fs::read_to_string(file).unwrap(), changed);
    assert_eq!(
        storage::encoded(&json!({"z":"測😀\u{7f}","a":1})).unwrap(),
        br#"{"a":1,"z":"\u6e2c\ud83d\ude00\u007f"}"#
    );
}

#[test]
fn pending_policy_commit_never_downgrades_a_running_guard_after_audit_failure() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store(temp.path().to_owned());
    let saved = store.save(enabled(), "").unwrap();
    let mut runtime = Runtime::open(temp.path().to_owned()).unwrap();
    fs::write(temp.path().join("security-audit.lock"), b"synthetic lock").unwrap();
    assert!(store.save(Policy::default(), &saved.revision).is_err());
    assert!(
        store
            .load()
            .err()
            .unwrap()
            .is::<super::policy::PendingSave>()
    );
    assert!(matches!(
        runtime.protect(|| Ok(json!("Bearer synthetic_token_123456"))),
        Output::Block
    ));
    assert!(Runtime::open(temp.path().to_owned()).is_err());
}

#[cfg(feature = "desktop")]
#[tokio::test]
async fn monitor_captures_the_protected_frame_that_reached_stdio() {
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;

    let temp = tempfile::tempdir().unwrap();
    Store(temp.path().to_owned()).save(enabled(), "").unwrap();
    let runtime = Runtime::open(temp.path().to_owned()).unwrap();
    let bus = Arc::new(crate::monitor::Bus::create().unwrap());
    let (client, server) = duplex(64 * 1024);
    let (input, output) = split(server);
    let monitor = crate::monitor::ObservedTransport::new(
        AsyncRwTransport::new(input, output),
        bus.clone(),
        Mode::ReadOnly,
    );
    let mut transport = GuardedTransport::new(monitor, runtime);
    let mut client = BufReader::new(client);
    client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"fixture.txt\"}}}\n").await.unwrap();
    transport.receive().await.unwrap();
    let response = json!({
        "jsonrpc":"2.0","id":1,"result":{
            "content":[{"type":"text","text":"Bearer synthetic_token_123456"}],"isError":false
        }
    });
    transport
        .send(serde_json::from_value(response).unwrap())
        .await
        .unwrap();
    let mut line = String::new();
    client.read_line(&mut line).await.unwrap();
    assert!(!line.contains("synthetic_token_123456"));
    let wire: Value = serde_json::from_str(&line).unwrap();
    let mut cursor = 0;
    let record = bus.poll(&mut cursor).1.pop().unwrap().2;
    assert_eq!(record.response, Some(wire));
    assert_eq!(record.status, crate::monitor::Status::Success);
}

#[tokio::test]
async fn sdk_writer_protects_errors_notifications_and_tool_payloads_without_changing_ids() {
    let temp = tempfile::tempdir().unwrap();
    Store(temp.path().to_owned()).save(enabled(), "").unwrap();
    let runtime = Runtime::open(temp.path().to_owned()).unwrap();
    let (client, server) = duplex(64 * 1024);
    let (input, output) = split(server);
    let mut transport = GuardedTransport::new(AsyncRwTransport::new(input, output), runtime);
    let mut client = BufReader::new(client);
    let message = |value| serde_json::from_value::<TxJsonRpcMessage<RoleServer>>(value).unwrap();
    for value in [
        json!({"jsonrpc":"2.0","id":"fixture","error":{"code":-32000,"message":"Bearer fixture_token_123456","data":{"password":"synthetic-value"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":{"password":"synthetic-value"}}}),
        json!({"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"canary"}],"structuredContent":{"type":"image","data":"synthetic-value"}}}),
    ] {
        transport.send(message(value.clone())).await.unwrap();
        let mut line = String::new();
        client.read_line(&mut line).await.unwrap();
        assert!(!line.contains("fixture_token"));
        assert!(!line.contains("synthetic-value"));
        let actual: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(actual["id"], value["id"]);
        if value.get("error").is_some() {
            assert_eq!(actual["error"]["code"], -32000);
        }
        if value.get("method").is_some() {
            assert_eq!(actual["method"], value["method"]);
        }
        if value.get("result").is_some() {
            assert_eq!(actual["result"]["content"][0]["text"], BLOCK_TEXT);
        }
    }
    let audit = fs::read_to_string(temp.path().join("security-audit.jsonl")).unwrap();
    assert!(!audit.contains("fixture_token"));
    assert!(!audit.contains("synthetic-value"));
    assert_eq!(Audit(temp.path().to_owned()).load().unwrap().len(), 4);
}
