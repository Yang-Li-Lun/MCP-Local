use crate::security::{
    authority::Authority,
    channel::{ENV_NAME, Mode},
    policy::Policy,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::windows::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires freshly built native app; signed local STDIO with synthetic roots and policy only"]
fn native_host_stdio_enforces_authorization_environment_and_eof_cleanup() {
    let temp = tempfile::Builder::new()
        .prefix("mcp-host-wire-")
        .tempdir()
        .unwrap();
    let root = temp.path().join("shared");
    let local = temp.path().join("local");
    let tools = temp.path().join("tools");
    for path in [&root, &local, &tools] {
        fs::create_dir(path).unwrap();
    }
    fs::write(tools.join("fixture-tool.cmd"), "@echo native-tool-ok\r\n").unwrap();
    let mut owner = Authority::new(local.join("MCP-Local-Rust"));
    let mut policy = Policy::default();
    policy.enabled = true;
    owner.save(policy, "").unwrap();
    let descriptor = owner.begin(Mode::HostControl).unwrap();
    let monitor = crate::monitor::Bus::create().unwrap();
    let binary = std::env::var_os("MCP_LOCAL_RUST_TEST_APP_BINARY").expect("built app");
    let mut child = Owned(
        Command::new(binary)
            .args([
                "--access-mode",
                "host_control",
                "--require-security-channel",
                "--root",
            ])
            .arg(&root)
            .env("LOCALAPPDATA", &local)
            .env("USERPROFILE", temp.path())
            .env("PATH", &tools)
            .env("PATHEXT", ".COM;.EXE;.BAT;.CMD")
            .env("CONTROL_PLANE_API_KEY", "synthetic-never-forward")
            .env("OPENAI_API_KEY", "synthetic-never-forward")
            .env(ENV_NAME, descriptor.as_str())
            .env(crate::monitor::ENV_NAME, monitor.name())
            .env_remove(crate::developer_vm::guard::ENV_NAME)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .creation_flags(0x08000000)
            .spawn()
            .unwrap(),
    );
    drop(descriptor);
    let mut input = child.0.stdin.take().unwrap();
    let output = child.0.stdout.take().unwrap();
    let (send, receive) = mpsc::sync_channel(8);
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
    let mut sequence = 0;
    let mut request = |owner: &mut Authority, method: &str, params: Value| {
        sequence += 1;
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0", "id":sequence, "method":method, "params":params})
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            owner.tick();
            match receive.recv_timeout(Duration::from_millis(30)) {
                Ok(value) => {
                    assert_eq!(value["id"], sequence);
                    if method == "initialize" {
                        writeln!(
                            input,
                            "{}",
                            json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
                        )
                        .unwrap();
                    }
                    break value["result"].clone();
                }
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                other => panic!("native STDIO response: {other:?}"),
            }
        }
    };
    let hello = request(
        &mut owner,
        "initialize",
        json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}),
    );
    assert_eq!(hello["serverInfo"]["name"], "local-files-host-control");
    let runtime = monitor.runtime_identity().unwrap();
    assert_eq!(runtime.process_id, child.0.id());
    assert_ne!(
        runtime.instance_id,
        crate::build_identity::current().instance_id
    );
    assert_eq!(runtime.source_sha256, env!("MCP_SOURCE_SHA256"));
    let tools = request(&mut owner, "tools/list", json!({}));
    assert_eq!(tools["tools"].as_array().unwrap().len(), 36);
    let denied = request(
        &mut owner,
        "tools/call",
        json!({"name":"start_process","arguments":{"command":"Write-Output denied"}}),
    );
    assert_eq!(denied["isError"], true);
    owner.grant(1, owner.grant_target().unwrap()).unwrap();
    let started = request(
        &mut owner,
        "tools/call",
        json!({"name":"start_process","arguments":{
            "command":"fixture-tool; if ($env:CONTROL_PLANE_API_KEY -or $env:OPENAI_API_KEY -or $env:MCP_LOCAL_RUST_SECURITY_CHANNEL) { throw 'environment leak' }; [Console]::WriteLine('環境正常🙂')", "timeout_ms":3000
        }}),
    );
    assert_eq!(started["isError"], false, "{started}");
    let id = started["structuredContent"]["session_id"].as_str().unwrap();
    let output = request(
        &mut owner,
        "tools/call",
        json!({"name":"read_process_output","arguments":{"session_id":id,"wait_ms":3000}}),
    );
    let view = &output["structuredContent"];
    assert_eq!(view["exit_code"], 0, "{output}");
    assert_eq!(view["sandbox"], "host_process");
    assert!(
        view["output"].as_str().unwrap().contains("native-tool-ok"),
        "{output}"
    );
    assert!(
        view["output"].as_str().unwrap().contains("環境正常🙂"),
        "{output}"
    );
    let started = request(
        &mut owner,
        "tools/call",
        json!({"name":"start_process","arguments":{"command":"Start-Sleep -Seconds 60","timeout_ms":0}}),
    );
    assert_eq!(started["isError"], false, "{started}");
    let pid = started["structuredContent"]["pid"].as_u64().unwrap() as u32;
    drop(request);
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        owner.tick();
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "EOF must stop host sessions");
        thread::sleep(Duration::from_millis(30));
    }
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if !handle.is_null() {
        let stopped = unsafe { WaitForSingleObject(handle, 0) } == 0;
        unsafe {
            CloseHandle(handle);
        }
        assert!(stopped, "owned host process survived STDIO EOF");
    }
    reader.join().unwrap();
    owner.close();
}
