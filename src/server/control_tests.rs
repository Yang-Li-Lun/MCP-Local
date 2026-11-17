use std::{
    cell::{Cell, RefCell},
    fs,
    path::Path,
};

use tempfile::{Builder, TempDir};
use tokio_util::sync::CancellationToken;

use super::*;

fn fixture() -> TempDir {
    Builder::new().prefix("mcp-control-").tempdir().unwrap()
}

fn workspace(path: &Path, mode: Mode, maximum: usize) -> Workspace {
    Workspace::new_for_mode(
        ReaderSettings {
            max_file_bytes: maximum,
            ..ReaderSettings::default()
        },
        WorkspaceSettings::single(path.into()),
        mode,
    )
    .unwrap()
}

fn invoke(workspace: &Workspace, name: &str, args: Value) -> Result<Value> {
    workspace
        .dispatch(
            name,
            args.as_object().unwrap().clone(),
            &mut Budget::new(CancellationToken::new()),
            &|| Ok(()),
        )
        .map(|result| result.structured_content.unwrap())
}

fn refuse(workspace: &Workspace, name: &str, args: Value, message: &str) {
    let error = invoke(workspace, name, args).unwrap_err().to_string();
    assert!(error.contains(message), "{name}: {error}");
}

fn digest(bytes: &[u8]) -> String {
    crate::listing::hex(&Sha256::digest(bytes))
}

#[test]
fn control_manifests_keep_the_original_read_only_tools() {
    let read_only: Value = serde_json::from_str(EXPECTED).unwrap();
    assert_eq!(read_only.as_object().unwrap().len(), 22);
    for (manifest, count) in [(DEVELOPER_CONTRACT, 34), (HOST_CONTRACT, 36)] {
        let expected: Value = serde_json::from_str(manifest).unwrap();
        assert_eq!(expected.as_object().unwrap().len(), count);
        for (name, schema) in read_only.as_object().unwrap() {
            assert_eq!(expected[name], *schema, "{name}");
        }
    }
}

#[test]
fn all_three_modes_register_exactly_their_contract_without_schema_drift() {
    let root = fixture();
    for (mode, count) in [
        (Mode::ReadOnly, 22),
        (Mode::DeveloperControl, 34),
        (Mode::HostControl, 36),
    ] {
        let workspace = workspace(root.path(), mode, 1024);
        assert_eq!(workspace.tools.len(), count);
        let diagnostics = workspace.diagnostics().unwrap();
        assert_eq!(diagnostics["consistent"], true, "{diagnostics}");
        assert_eq!(
            diagnostics["actual_contract_sha256"],
            diagnostics["expected_contract_sha256"]
        );
        if mode == Mode::DeveloperControl {
            assert_eq!(
                diagnostics["mode_ready"],
                diagnostics["developer_backend"]["ready"]
            );
        } else {
            assert_eq!(diagnostics["mode_ready"], true);
        }
    }
}

#[tokio::test]
async fn host_mcp_wire_registers_36_and_routes_files_sessions_and_denials() {
    use crate::security::{GuardedTransport, Runtime, authority::Authority, policy::Policy};
    use rmcp::transport::async_rw::AsyncRwTransport;
    use std::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let temp = fixture();
    let shared = temp.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::write(shared.join("out.txt"), "isolated fixture").unwrap();
    let state = temp.path().join("state");
    let mut authority = Authority::new(state.clone());
    let mut policy = Policy::default();
    policy.enabled = true;
    authority.save(policy, "").unwrap();
    let descriptor = authority.begin(Mode::HostControl).unwrap();
    let security = Arc::new(Mutex::new(
        Runtime::attach(state, &descriptor, Mode::HostControl).unwrap(),
    ));
    let server = Server::host(
        ReaderSettings::default(),
        WorkspaceSettings::single(shared.clone()),
        security.clone(),
    )
    .unwrap();
    let host = server.host.clone().unwrap();
    let authority = Arc::new(Mutex::new(authority));
    let owner = authority.clone();
    let pulse = CancellationToken::new();
    let stopped = pulse.clone();
    let ticker = tokio::spawn(async move {
        while !stopped.is_cancelled() {
            owner.lock().unwrap().tick();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });
    let (client, service) = tokio::io::duplex(128 * 1024);
    let (rx, tx) = tokio::io::split(service);
    let transport = GuardedTransport::shared(AsyncRwTransport::new(rx, tx), security);
    let running = tokio::spawn(async move {
        server
            .serve(transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let (rx, mut tx) = tokio::io::split(client);
    let mut rx = BufReader::new(rx);
    let mut number = 0;
    macro_rules! request {
        ($method:expr, $params:expr) => {{
            number += 1;
            tx.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":number,"method":$method,"params":$params})).as_bytes()).await.unwrap();
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(10), rx.read_line(&mut line)).await.unwrap().unwrap();
            let response: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["id"], number, "{response}");
            assert!(response["error"].is_null(), "{response}");
            response["result"].clone()
        }};
    }
    macro_rules! call {
        ($name:expr, $args:expr, $success:expr) => {{
            let result = request!("tools/call", json!({"name":$name,"arguments":$args}));
            assert_eq!(result["isError"], !$success, "{}: {result}", $name);
            assert!(result["_meta"].get(security::SESSION_REVISION).is_none());
            let value = result["structuredContent"].clone();
            value
        }};
    }
    let hello = request!(
        "initialize",
        json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"offline fixture","version":"1"}})
    );
    assert_eq!(hello["serverInfo"]["name"], "local-files-host-control");
    tx.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    let listed = request!("tools/list", json!({}));
    let contract: Value = serde_json::from_str(HOST_CONTRACT).unwrap();
    assert_eq!(listed["tools"].as_array().unwrap().len(), 36);
    for tool in listed["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(
            json!({"inputSchema":tool["inputSchema"],"annotations":tool["annotations"]}),
            contract[name]
        );
    }
    let diagnostics = call!("server_diagnostics", json!({}), true);
    assert_eq!(diagnostics["consistent"], true);
    assert_eq!(
        diagnostics["host_backend"]["production_entry_enabled"],
        true
    );
    call!(
        "start_process",
        json!({"command":"Write-Output 'denied'"}),
        false
    );
    call!(
        "write_file",
        json!({"path":"denied.txt","content":"denied"}),
        false
    );
    assert!(!shared.join("denied.txt").exists());
    {
        let mut owner = authority.lock().unwrap();
        owner.tick();
        let target = owner.grant_target().unwrap();
        owner.grant(1, target).unwrap();
    }
    call!(
        "start_process",
        json!({"command":"ignored","timeout_ms":"0"}),
        false
    );
    call!(
        "write_file",
        json!({"path":"created.txt","content":"host fixture"}),
        true
    );
    let read = call!(
        "read_host_file",
        json!({"path":shared.join("created.txt")}),
        true
    );
    assert!(read.to_string().contains("host fixture"));
    call!("list_host_directory", json!({"directory":shared}), true);
    let started = call!(
        "start_process",
        json!({"command":"[Console]::WriteLine([Console]::ReadLine())","timeout_ms":0}),
        true
    );
    let id = started["session_id"].as_str().unwrap();
    call!(
        "export_session_file",
        json!({"session_id":id,"path":"out.txt","destination":"copy.txt"}),
        false
    );
    call!(
        "interact_with_process",
        json!({"session_id":id,"input":"offline\n","close_stdin":true}),
        true
    );
    let output = call!(
        "read_process_output",
        json!({"session_id":id,"wait_ms":3000}),
        true
    );
    assert_eq!(output["job_stopped"], true, "{output}");
    assert!(output["output"].as_str().unwrap().contains("offline"));
    call!("list_sessions", json!({}), true);
    call!(
        "export_session_file",
        json!({"session_id":id,"path":"../out.txt","destination":"copy.txt"}),
        false
    );
    call!(
        "export_session_file",
        json!({"session_id":id,"path":"out.txt","destination":"copy.txt"}),
        true
    );
    assert_eq!(
        fs::read_to_string(shared.join("copy.txt")).unwrap(),
        "isolated fixture"
    );
    call!("force_terminate", json!({"session_id":id}), true);
    call!("close_session", json!({"session_id":id}), true);
    let started = call!(
        "start_process",
        json!({"command":"Start-Sleep -Seconds 30","timeout_ms":0}),
        true
    );
    let id = started["session_id"].as_str().unwrap();
    authority.lock().unwrap().revoke().unwrap();
    call!(
        "interact_with_process",
        json!({"session_id":id,"input":"denied"}),
        false
    );
    call!(
        "write_file",
        json!({"path":"revoked.txt","content":"denied"}),
        false
    );
    assert!(!shared.join("revoked.txt").exists());
    call!("force_terminate", json!({"session_id":id}), true);
    call!("close_session", json!({"session_id":id}), true);
    drop(tx);
    drop(rx);
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
    host.sessions.shutdown().await.unwrap();
    pulse.cancel();
    ticker.await.unwrap();
    authority.lock().unwrap().close();
}

#[test]
fn directory_cursor_can_resume_a_long_unicode_path() {
    let fixture = fixture();
    let relative = vec!["界".repeat(203); 20].join("\\");
    let directory = fixture.path().join(&relative);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("a.txt"), b"first").unwrap();
    fs::write(directory.join("b.txt"), b"second").unwrap();

    for mode in [Mode::ReadOnly, Mode::DeveloperControl] {
        let workspace = workspace(fixture.path(), mode, 1024);
        let first = invoke(
            &workspace,
            "list_directory",
            json!({"directory":relative,"limit":1}),
        )
        .unwrap();
        let cursor = first["next_cursor"].as_str().unwrap();
        let second = invoke(
            &workspace,
            "list_directory",
            json!({"directory":relative,"limit":1,"cursor":cursor}),
        )
        .unwrap_or_else(|error| panic!("{mode:?}, cursor length {}: {error}", cursor.len()));
        assert_eq!(first["entries"][0]["name"], "a.txt");
        assert_eq!(second["entries"][0]["name"], "b.txt");
        assert!(second["next_cursor"].is_null());
    }
}

#[test]
fn developer_file_lifecycle_preserves_hash_and_encoding() {
    let sandbox = fixture();
    let root = sandbox.path();
    let workspace = workspace(root, Mode::DeveloperControl, 1024);
    let run = |name, args| invoke(&workspace, name, args).unwrap();

    let original = "\u{feff}甲\r\n乙\r\n";
    let created = run("write_file", json!({"path":"note.txt","content":original}));
    assert_eq!(
        created,
        json!({
            "path":"note.txt", "bytes":original.len(), "sha256":digest(original.as_bytes())
        })
    );
    refuse(
        &workspace,
        "write_file",
        json!({"path":"note.txt","content":"other"}),
        "FILE_EXISTS",
    );
    refuse(
        &workspace,
        "write_file",
        json!({
            "path":"note.txt","content":"other","mode":"rewrite","expected_sha256":"0".repeat(64)
        }),
        "CONTENT_CONFLICT",
    );
    assert_eq!(
        fs::read(root.join("note.txt")).unwrap(),
        original.as_bytes()
    );
    let edited = run(
        "edit_block",
        json!({
            "path":"note.txt","old_string":"甲\n乙","new_string":"丙\n丁","expected_sha256":created["sha256"]
        }),
    );
    assert_eq!(edited["replacements"], 1);
    assert_eq!(
        fs::read(root.join("note.txt")).unwrap(),
        "\u{feff}丙\r\n丁\r\n".as_bytes()
    );
    let appended = run(
        "write_file",
        json!({
            "path":"note.txt","content":"末","mode":"append","expected_sha256":edited["sha256"]
        }),
    );
    assert_eq!(
        appended["sha256"],
        digest("\u{feff}丙\r\n丁\r\n末".as_bytes())
    );
    let rewritten = run(
        "write_file",
        json!({
            "path":"note.txt","content":"aaa","mode":"rewrite","expected_sha256":appended["sha256"]
        }),
    );
    refuse(
        &workspace,
        "edit_block",
        json!({
            "path":"note.txt","old_string":"aa","new_string":"x","expected_sha256":rewritten["sha256"]
        }),
        "EDIT_MATCH_COUNT",
    );
    assert_eq!(fs::read(root.join("note.txt")).unwrap(), b"aaa");

    assert_eq!(
        run("create_directory", json!({"path":"folder"})),
        json!({"path":"folder","created":true})
    );
    fs::write(root.join("folder/taken.txt"), b"taken").unwrap();
    refuse(
        &workspace,
        "move_file",
        json!({
            "source":"note.txt","destination":"folder/taken.txt","expected_sha256":rewritten["sha256"]
        }),
        "FILE_EXISTS",
    );
    let moved = run(
        "move_file",
        json!({
            "source":"note.txt","destination":"folder/moved.txt","expected_sha256":rewritten["sha256"]
        }),
    );
    assert_eq!(
        moved,
        json!({
            "source":"note.txt","destination":"folder/moved.txt","sha256":rewritten["sha256"]
        })
    );
    assert!(!root.join("note.txt").exists());
    assert_eq!(fs::read(root.join("folder/moved.txt")).unwrap(), b"aaa");
    assert_eq!(fs::read(root.join("folder/taken.txt")).unwrap(), b"taken");
    let deleted = run(
        "delete_file",
        json!({
            "path":"folder/moved.txt","expected_sha256":moved["sha256"]
        }),
    );
    assert_eq!(
        deleted,
        json!({
            "path":"folder/moved.txt","deleted":true,"sha256":rewritten["sha256"]
        })
    );
    assert!(!root.join("folder/moved.txt").exists());
}

#[test]
fn create_checks_for_existing_file_before_hash_or_capacity() {
    let sandbox = fixture();
    let root = sandbox.path();
    let write = workspace(root, Mode::DeveloperControl, 8);
    let name = "present.txt";
    fs::write(root.join(name), b"before").unwrap();

    for expected in [Value::Null, json!(digest(b"before"))] {
        refuse(
            &write,
            "write_file",
            json!({"path":name,"content":"over-limit","expected_sha256":expected}),
            "FILE_EXISTS：建立模式不可覆寫檔案。",
        );
    }
    refuse(
        &write,
        "write_file",
        json!({
            "path":"missing.txt","content":"over-limit","expected_sha256":digest(b"before")
        }),
        "FILE_MISSING",
    );
    assert!(!root.join("missing.txt").exists());
    assert_eq!(fs::read(root.join(name)).unwrap(), b"before");

    let invalid = b"\xff";
    fs::write(root.join("invalid.txt"), invalid).unwrap();
    refuse(
        &write,
        "write_file",
        json!({
            "path":"invalid.txt","mode":"append","content":"over-limit",
            "expected_sha256":digest(invalid)
        }),
        "來源不是有效的 UTF-8",
    );
    assert_eq!(fs::read(root.join("invalid.txt")).unwrap(), invalid);
}

#[test]
fn managed_file_mutations_recheck_the_connection_before_committing() {
    let sandbox = fixture();
    let root = sandbox.path();
    let write = workspace(root, Mode::DeveloperControl, 32);
    fs::write(root.join("sample.txt"), b"before").unwrap();
    let state = fixture();
    let mut authority = crate::security::authority::Authority::new(state.path().to_owned());
    authority
        .save(crate::security::policy::Policy::default(), "")
        .unwrap();
    let descriptor = authority.begin(Mode::DeveloperControl).unwrap();
    let runtime = RefCell::new(
        crate::security::Runtime::attach(
            state.path().to_owned(),
            &descriptor,
            Mode::DeveloperControl,
        )
        .unwrap(),
    );
    let checks = Cell::new(0);
    let authorize = || {
        let mut runtime = runtime.borrow_mut();
        let prior = checks.replace(checks.get() + 1);
        if prior == 1 {
            runtime.close_controls();
        }
        runtime.require_mode(Mode::DeveloperControl)
    };

    let replace = json!({
        "path":"sample.txt","content":"after","mode":"rewrite","expected_sha256":digest(b"before")
    });
    let error = write
        .dispatch(
            "write_file",
            replace.as_object().unwrap().clone(),
            &mut Budget::new(CancellationToken::new()),
            &authorize,
        )
        .unwrap_err();
    assert!(error.to_string().contains("SECURITY_CHANNEL_UNCONFIRMED"));
    assert_eq!(checks.get(), 2);
    assert_eq!(fs::read(root.join("sample.txt")).unwrap(), b"before");

    let mkdir = json!({"path":"queued"});
    let error = write
        .dispatch(
            "create_directory",
            mkdir.as_object().unwrap().clone(),
            &mut Budget::new(CancellationToken::new()),
            &authorize,
        )
        .unwrap_err();
    assert!(error.to_string().contains("SECURITY_CHANNEL_UNCONFIRMED"));
    assert_eq!(checks.get(), 3);
    assert!(!root.join("queued").exists());
}

#[test]
fn control_refusals_leave_the_files_intact() {
    let sandbox = fixture();
    let root = sandbox.path();
    let write = workspace(root, Mode::DeveloperControl, 8);
    let readonly = workspace(root, Mode::ReadOnly, 8);
    let small = root.join("sample.txt");
    fs::write(&small, b"before").unwrap();
    fs::hard_link(&small, root.join("linked.txt")).unwrap();
    refuse(
        &write,
        "write_file",
        json!({
            "path":"linked.txt","content":"after","mode":"rewrite","expected_sha256":digest(b"before")
        }),
        "多重硬連結",
    );
    fs::remove_file(root.join("linked.txt")).unwrap();

    refuse(
        &readonly,
        "write_file",
        json!({"path":"extra.txt","content":"after"}),
        "契約",
    );
    for path in [
        "../escape.txt",
        ".hidden.txt",
        "secrets.json",
        "sample.txt:stream",
        "\\sample.txt",
        "C:sample.txt",
        "folder/missing.txt",
        "NUL.txt",
        "a.exe",
    ] {
        assert!(
            invoke(&write, "write_file", json!({"path":path,"content":"x"})).is_err(),
            "{path}"
        );
    }
    refuse(
        &write,
        "write_file",
        json!({"path":"huge.txt","content":"甲乙丙"}),
        "WRITE_LIMIT",
    );
    refuse(
        &write,
        "write_file",
        json!({"path":"zero-char.txt","content":"a\u{0}b"}),
        "WRITE_INVALID",
    );
    refuse(
        &write,
        "create_directory",
        json!({"path":"missing/child"}),
        "路徑",
    );
    let existing = fs::File::open(&small).unwrap();
    let delete = json!({"path":"sample.txt","expected_sha256":digest(b"before")});
    refuse(&write, "delete_file", delete.clone(), "FILE_BUSY");
    drop(existing);
    let mut attributes = fs::metadata(&small).unwrap().permissions();
    attributes.set_readonly(true);
    fs::set_permissions(&small, attributes.clone()).unwrap();
    let refused = invoke(&write, "delete_file", delete)
        .unwrap_err()
        .to_string();
    attributes.set_readonly(false);
    fs::set_permissions(&small, attributes).unwrap();
    assert!(refused.contains("唯讀屬性"), "{refused}");
    refuse(
        &write,
        "write_file",
        json!({
            "path":"sample.txt","content":"abc","mode":"append","expected_sha256":digest(b"before")
        }),
        "WRITE_LIMIT",
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mkdir = json!({"path":"revoked"});
    let err = write
        .dispatch(
            "create_directory",
            mkdir.as_object().unwrap().clone(),
            &mut Budget::new(cancel),
            &|| Ok(()),
        )
        .unwrap_err();
    assert!(err.to_string().contains("OPERATION_CANCELLED"));
    write.files.close();
    refuse(&write, "create_directory", mkdir, "CONTROL_CLOSED");
    assert_eq!(fs::read(&small).unwrap(), b"before");
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
}

#[test]
fn host_files_use_absolute_receipts_and_scoped_pagination() {
    let sandbox = fixture();
    let root = sandbox.path().join("shared");
    let outside = sandbox.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    let developer = workspace(&root, Mode::DeveloperControl, 1024);
    let workspace = workspace(&root, Mode::HostControl, 1024);
    let run = |name, args| invoke(&workspace, name, args).unwrap();
    let source = outside.join("outside.txt");
    let destination = root.join("inside.txt");
    fs::write(&source, b"outside").unwrap();
    fs::write(outside.join("extra.txt"), b"other").unwrap();
    fs::create_dir(outside.join("nested")).unwrap();
    fs::write(outside.join("nested/invisible.txt"), b"nested").unwrap();
    refuse(
        &developer,
        "write_file",
        json!({"path":source,"content":"after"}),
        "相對",
    );
    refuse(&developer, "read_host_file", json!({"path":source}), "契約");
    for path in [
        r"C:sample.txt",
        r"\sample.txt",
        r"\\server\share\file",
        r"C:\a\..\b",
        r"C:\a\secret.txt:stream",
        r"C:\.hidden\sample.txt",
    ] {
        assert!(
            invoke(&workspace, "read_host_file", json!({"path":path})).is_err(),
            "{path}"
        );
    }
    let read = run("read_host_file", json!({"path":source}));
    assert_eq!(
        read,
        json!({
            "path": source, "content": "1: outside", "start_line": 1,
            "total_lines": 1, "next_start_line": null,
        })
    );
    let first = run(
        "list_host_directory",
        json!({"directory":outside,"limit":1}),
    );
    assert_eq!(first["directory"], json!(outside));
    assert!(first["next_cursor"].is_string());
    assert_eq!(first["sorted_entries"], 3);
    refuse(
        &workspace,
        "list_host_directory",
        json!({
            "directory":root,"limit":1,"cursor":first["next_cursor"]
        }),
        "cursor",
    );
    let alternate = format!(r"{}\\.\\", outside.display());
    let second = run(
        "list_host_directory",
        json!({
            "directory":alternate,"limit":1,"cursor":first["next_cursor"]
        }),
    );
    assert_eq!(second["directory"], json!(outside));
    assert_ne!(first["entries"], second["entries"]);
    let moved = run(
        "move_file",
        json!({
            "source":source, "destination":"inside.txt","expected_sha256":digest(b"outside")
        }),
    );
    assert_eq!(moved["source"], json!(source));
    assert_eq!(moved["destination"], json!(destination));
    assert!(!source.exists());
    assert_eq!(fs::read(destination).unwrap(), b"outside");
}
