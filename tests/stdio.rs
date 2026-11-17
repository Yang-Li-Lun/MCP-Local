use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::windows::{ffi::OsStrExt, fs::FileTimesExt, process::CommandExt},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tempfile::TempDir;
use windows_sys::Win32::{
    Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW},
    System::Threading::CREATE_NO_WINDOW,
};

// Allows the same regression executable to validate an extracted deployment in
// a clean Windows Sandbox, without a compiler or the host build directory.
fn executable() -> std::path::PathBuf {
    std::env::var_os("MCP_LOCAL_RUST_TEST_APP_BINARY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_mcp-local-rust").into())
}
const HELLO: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}"#;

struct Peer {
    child: Child,
    input: ChildStdin,
    response: Receiver<Value>,
    id: u64,
}

impl Peer {
    fn new(fixture: &TempDir) -> Self {
        Self::with_args(fixture, &[])
    }

    fn with_args(fixture: &TempDir, args: &[&str]) -> Self {
        let mut child = Command::new(executable())
            .args(["--root", fixture.path().join("shared").to_str().unwrap()])
            .args(args)
            .env_remove("CONTROL_PLANE_API_KEY")
            .env("LOCALAPPDATA", fixture.path().join("appdata"))
            .env("USERPROFILE", fixture.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (send, response) = mpsc::sync_channel(4);
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let value = serde_json::from_str(&line.unwrap()).unwrap();
                if send.send(value).is_err() {
                    break;
                }
            }
        });
        let mut peer = Self {
            child,
            input,
            response,
            id: 1,
        };
        peer.send(HELLO);
        let initialized = peer.recv();
        assert_eq!(
            initialized["result"]["serverInfo"]["name"],
            "local-files-readonly"
        );
        peer.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        peer
    }

    fn send(&mut self, message: &str) {
        writeln!(self.input, "{message}").unwrap();
        self.input.flush().unwrap();
    }

    fn recv(&self) -> Value {
        let response = self
            .response
            .recv_timeout(Duration::from_secs(35))
            .expect("MCP response");
        assert_eq!(response["id"], self.id);
        response
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        self.send(
            &json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
                .to_string(),
        );
        self.recv()
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))["result"].clone()
    }

    fn success(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.call(name, arguments);
        assert_eq!(result["isError"], false, "{name} {result}");
        result["structuredContent"].clone()
    }

    fn refused(&mut self, name: &str, arguments: Value) {
        let result = self.call(name, arguments);
        assert_eq!(result["isError"], true, "{name} {result}");
        assert!(result["structuredContent"].is_null());
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture() -> TempDir {
    let fixture = tempfile::Builder::new()
        .prefix("mcp-local-rust-test-")
        .tempdir()
        .unwrap();
    fs::create_dir(fixture.path().join("appdata")).unwrap();
    fs::create_dir(fixture.path().join("shared")).unwrap();
    fixture
}

#[test]
fn cli_persists_each_mode_without_credentials_or_remote_connection() {
    let temp = fixture();
    let state = temp.path().join("appdata/MCP-Local-Rust");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("settings.json"), json!({
        "version":1,
        "workspace":{"roots":[{"id":"main","name":"fixture","path":temp.path().join("shared"),"excluded_names":[]}],"default_root":"main"},
        "reader":{},"tunnel":"tunnel_fixture","start_hidden":false,"auto_start":false,"auto_connect":false,
        "icon":0,"access_mode":"read_only","key_dpapi":null
    }).to_string()).unwrap();
    for mode in ["host_control", "developer_control", "read_only"] {
        let result = Command::new(executable())
            .args(["--set-access-mode", mode])
            .env_clear()
            .env("SystemRoot", std::env::var_os("SystemRoot").unwrap())
            .env("LOCALAPPDATA", temp.path().join("appdata"))
            .env("USERPROFILE", temp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let stored: Value =
            serde_json::from_slice(&fs::read(state.join("settings.json")).unwrap()).unwrap();
        assert_eq!(stored["access_mode"], mode);
        assert_eq!(stored["key_dpapi"], Value::Null);
        assert_eq!(stored["auto_start"], false);
        assert!(!state.join("profiles").exists());
    }
}

#[test]
fn managed_start_never_falls_back_to_an_unsigned_policy() {
    let temp = fixture();
    for descriptor in [None, Some("invalid-descriptor")] {
        let mut command = Command::new(executable());
        command
            .arg("--require-security-channel")
            .arg("--root")
            .arg(temp.path().join("shared"))
            .env_remove("CONTROL_PLANE_API_KEY")
            .env_remove("MCP_LOCAL_RUST_SECURITY_CHANNEL")
            .env("LOCALAPPDATA", temp.path().join("appdata"))
            .env("USERPROFILE", temp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        if let Some(descriptor) = descriptor {
            command.env("MCP_LOCAL_RUST_SECURITY_CHANNEL", descriptor);
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
    }
}

#[test]
fn host_cli_requires_a_signed_local_channel_before_state_access() {
    let temp = fixture();
    let state = temp.path().join("untouched-state");
    let output = Command::new(executable())
        .args([
            "--access-mode",
            "host_control",
            "--require-security-channel",
            "--workspace-settings",
            "invalid-fixture",
        ])
        .env_clear()
        .env("SystemRoot", std::env::var_os("SystemRoot").unwrap())
        .env("LOCALAPPDATA", &state)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("SECURITY_CHANNEL_UNCONFIRMED"));
    assert!(!state.exists());
}

#[test]
fn standalone_policy_guards_real_stdio_and_latches_policy_loss() {
    let fixture = fixture();
    let root = fixture.path().join("shared");
    fs::write(
        root.join("credential.txt"),
        b"Bearer synthetic_fixture_token_123456\nordinary line\n",
    )
    .unwrap();
    fs::write(root.join("bytes.dat"), b"synthetic fixture").unwrap();
    let state = fixture.path().join("appdata").join("MCP-Local-Rust");
    fs::create_dir(&state).unwrap();
    let policy = state.join("security-policy.json");
    let enabled = json!({
        "version":1, "enabled":true, "host_authorization":true, "detector_enabled":true, "response_guard":true,
        "strategy":"mask", "session_expiry":"terminate", "max_lease_minutes":15,
        "rules":[{"id":"bearer","kind":"builtin","pattern":"bearer","enabled":true,"risk":"high"}],
    });
    fs::write(&policy, serde_json::to_vec(&enabled).unwrap()).unwrap();
    let mut peer = Peer::new(&fixture);
    let response = peer.call("read_file", json!({"path":"credential.txt"}));
    assert_eq!(response["isError"], false, "{response}");
    assert!(response.to_string().contains("ordinary line"));
    assert!(!response.to_string().contains("synthetic_fixture_token"));
    let binary = peer.call("read_binary", json!({"path":"bytes.dat"}));
    assert_eq!(binary["isError"], true);
    assert!(binary.to_string().contains("SECURITY_RESPONSE_BLOCKED"));
    fs::remove_file(&policy).unwrap();
    let blocked = peer.call("workspace_info", json!({}));
    assert_eq!(blocked["isError"], true);
    assert!(blocked.to_string().contains("SECURITY_RESPONSE_BLOCKED"));
    fs::write(&policy, serde_json::to_vec(&enabled).unwrap()).unwrap();
    assert_eq!(peer.call("workspace_info", json!({}))["isError"], true);
    let audit = fs::read_to_string(state.join("security-audit.jsonl")).unwrap();
    for kind in ["response_masked", "response_blocked", "channel_failed"] {
        assert!(audit.contains(kind));
    }
    assert!(!audit.contains("credential.txt"));
    assert!(!audit.contains("synthetic_fixture_token"));
}

#[test]
fn windows_hidden_files_and_unavailable_modes_fail_closed() {
    let fixture = fixture();
    let path = fixture.path().join("shared").join("private.txt");
    fs::write(&path, "hidden fixture").unwrap();
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    assert_ne!(
        unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) },
        0
    );
    let mut peer = Peer::new(&fixture);
    peer.refused("read_binary", json!({"path":"private.txt"}));
    assert_eq!(
        peer.success("list_directory", json!({}))["returned_count"],
        0
    );
    for mode in ["developer_control", "host_control"] {
        let output = Command::new(executable())
            .args([
                "--access-mode",
                mode,
                "--root",
                fixture.path().join("shared").to_str().unwrap(),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{mode}");
        assert!(output.stdout.is_empty(), "{mode}");
    }
}

#[test]
fn read_only_wire_contract_and_refusals() {
    let fixture = fixture();
    let shared = fixture.path().join("shared");
    fs::create_dir(shared.join("專案")).unwrap();
    fs::create_dir(shared.join("target")).unwrap();
    fs::write(
        shared.join("專案").join("README.md"),
        "\u{feff}第一行\r\nStraße\u{2028}末行",
    )
    .unwrap();
    fs::write(
        shared.join("target").join("blocked.txt"),
        "outside permitted paths",
    )
    .unwrap();
    fs::write(
        shared.join("invalid.txt"),
        b"valid first line\nbad later \xff",
    )
    .unwrap();
    fs::write(shared.join(".hidden.txt"), "not visible").unwrap();
    fs::write(shared.join("sample.json"), r#"{"z":[1,2],"a":"好"}"#).unwrap();
    fs::write(shared.join("duplicate.json"), r#"{"a":1,"a":2}"#).unwrap();
    fs::write(shared.join("image.ppm"), b"P6\n1 1\n255\n\xff\x00\x00").unwrap();
    fs::write(
        shared.join("test.tar"),
        make_tar("inside.txt", b"tar payload"),
    )
    .unwrap();
    let mut peer = Peer::new(&fixture);

    let catalog = peer.request("tools/list", json!({}));
    let actual = catalog["result"]["tools"].as_array().unwrap();
    let contract: Value =
        serde_json::from_str(include_str!("../contracts/tool-contract.json")).unwrap();
    assert_eq!(actual.len(), 22);
    for tool in actual {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(tool["inputSchema"], contract[name]["inputSchema"], "{name}");
        assert_eq!(tool["annotations"], contract[name]["annotations"], "{name}");
    }
    let diagnostics = peer.success("server_diagnostics", json!({}));
    assert_eq!(diagnostics["consistent"], true);
    assert_eq!(diagnostics["mode_ready"], true);
    for capability in [
        "formats",
        "metadata_identity",
        "snapshots",
        "client_verification",
    ] {
        assert!(
            diagnostics["capabilities"][capability].is_string(),
            "{capability}"
        );
    }
    assert_eq!(
        diagnostics["mcp_version"],
        concat!("rmcp ", env!("MCP_SDK_VERSION"))
    );
    assert_eq!(
        diagnostics["parser_versions"],
        json!({
            "lopdf": env!("MCP_PDF_VERSION"), "lofty": env!("MCP_AUDIO_VERSION"),
            "image": env!("MCP_IMAGE_VERSION"), "yaml-rust2": env!("MCP_YAML_VERSION")
        })
    );
    let workspace = peer.success("workspace_info", json!({}));
    assert_eq!(workspace["default_root"], "main");
    assert_eq!(workspace["roots"][0]["name"], "main");
    assert_eq!(
        workspace["format_limits"]["media_formats"],
        json!([
            "MP3",
            "FLAC",
            "OGG",
            "WAV",
            "MP4",
            "AVI",
            "AIFF",
            "AU",
            "MATROSKA",
            "WEBM",
            "static image metadata (see image_limits)"
        ])
    );
    let list = peer.success("list_directory", json!({"limit":1}));
    assert_eq!(list["has_more"], true);
    assert!(list["next_cursor"].is_string());
    peer.refused("list_files", json!({"cursor": list["next_cursor"]}));
    let listed = peer.success("list_files", json!({}));
    assert!(!listed["files"].to_string().contains("blocked.txt"));
    assert!(!listed["files"].to_string().contains(".hidden.txt"));
    let query = peer.success(
        "query_files",
        json!({"extensions":["txt"],"sort_by":"size","include_sha256":true}),
    );
    assert_eq!(query["files"][0]["path"], "invalid.txt");
    assert_eq!(query["files"][0]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(
        peer.success("find_files", json!({"queries":["README"]}))["results"][0]["matches"][0]["path"],
        "專案/README.md"
    );

    assert_eq!(
        peer.success(
            "read_file",
            json!({"path":"專案\\README.md","start_line":2,"line_count":1})
        )["text"],
        "2: Straße"
    );
    let ranges = peer.success("read_file_ranges", json!({"path":"專案/README.md","ranges":[{"start_line":3,"line_count":1},{"start_line":1,"line_count":1}]}));
    assert_eq!(ranges["ranges"][0]["content"], "3: 末行");
    assert_eq!(ranges["ranges"][1]["content"], "1: 第一行");
    let batch = peer.success(
        "read_files",
        json!({"files":[{"path":"專案/README.md"},{"path":"invalid.txt"}]}),
    );
    assert_eq!(batch["files"][0]["returned_lines"], 3);
    assert!(batch["files"][1]["skipped_reason"].is_string());
    assert_eq!(peer.success("search_text", json!({"query":"STRASSE","context_lines":1}))["matches"][0]["context"].as_array().unwrap().len(), 3);
    let groups = peer.success("search_texts", json!({"queries":["末行","straße"]}));
    assert_eq!(groups["results"][0]["matches"][0]["line"], 3);
    assert_eq!(groups["results"][1]["matches"][0]["line"], 2);
    assert_eq!(
        peer.success("list_projects", json!({}))["projects"][0]["path"],
        "專案"
    );
    assert_eq!(
        peer.success("project_context", json!({"directory":"專案"}))["files"][0]["has_more"],
        false
    );
    let hash = peer.success("hash_files", json!({"paths":["專案/README.md"]}));
    assert_eq!(hash["files"][0]["sha256"].as_str().unwrap().len(), 64);
    let comparison = peer.success(
        "compare_paths",
        json!({"left":"專案/README.md","right":"sample.json"}),
    );
    assert_eq!(comparison["counts"]["modified"], 1);
    let baseline = peer.success("project_status", json!({"directory":"專案"}));
    fs::write(shared.join("專案").join("new.txt"), "new").unwrap();
    let diff = peer.success(
        "project_status",
        json!({"directory":"專案","baseline_id":baseline["baseline_id"]}),
    );
    assert_eq!(diff["counts"]["added"], 1);

    let document = peer.success("read_document", json!({"path":"sample.json","limit":1}));
    assert_eq!(document["units"][0]["location"]["pointer"], "/z/0");
    assert_eq!(document["next_start"], 1);
    assert_eq!(
        peer.success("file_info", json!({"path":"sample.json"}))["format"],
        "JSON"
    );
    assert_eq!(
        peer.success("read_binary", json!({"path":"sample.json","length":2}))["data"],
        "eyI="
    );
    let image = peer.call("read_image", json!({"path":"image.ppm"}));
    assert_eq!(image["isError"], false);
    assert_eq!(image["content"][1]["mimeType"], "image/png");
    assert!(image["structuredContent"].is_null());
    let media = peer.success("inspect_media", json!({"path":"image.ppm"}));
    assert_eq!(media["source_width"], 1);
    let archive = peer.success(
        "inspect_archive",
        json!({"path":"test.tar","member_path":"inside.txt","offset":4,"length":2}),
    );
    assert_eq!(archive["data"], "cGE=");

    peer.refused(
        "read_document",
        json!({"path":"duplicate.json", "format_hint":"JSON"}),
    );
    peer.refused(
        "read_document",
        json!({"path":"sample.json", "expected_sha256":"0".repeat(64)}),
    );
    peer.refused("read_file", json!({"path":"invalid.txt","line_count":1}));
    peer.refused("read_file", json!({"path":"sample.json","line_count":"1"}));
    peer.refused("read_file", json!({"path":"sample.json","line_count":1.0}));
    peer.refused("list_directory", json!({"unexpected":true}));
    peer.refused("read_binary", json!({"path":"target/blocked.txt"}));
    peer.refused("read_binary", json!({"path":"../shared/sample.json"}));
    peer.refused("read_binary", json!({"path":"sample.json:unknown"}));
    peer.refused(
        "read_binary",
        json!({"path":"sample.json","root_id":"unknown"}),
    );
    peer.refused(
        "write_file",
        json!({"path":"sample.json","content":"changed"}),
    );
    assert_eq!(
        fs::read_to_string(shared.join("sample.json")).unwrap(),
        r#"{"z":[1,2],"a":"好"}"#
    );
}

#[test]
fn project_listing_reports_the_scan_after_the_first_hundred_projects() {
    let fixture = fixture();
    let shared = fixture.path().join("shared");
    for index in 0..105 {
        fs::create_dir(shared.join(format!("project-{index:03}"))).unwrap();
    }
    fs::create_dir(shared.join("project-000/nested")).unwrap();
    fs::create_dir(shared.join("target")).unwrap();
    fs::write(shared.join("zz-after.txt"), "after the directories").unwrap();

    let mut peer = Peer::new(&fixture);
    let result = peer.success("list_projects", json!({}));
    let projects = result["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 100);
    assert_eq!(projects[0]["path"], "project-000");
    assert_eq!(projects[99]["path"], "project-099");
    assert_eq!(result["scanned_entries"], 107);
    assert_eq!(result["skipped_entries"], 1);
    assert_eq!(result["truncated"], true);
}

#[test]
fn query_keeps_windows_metadata_and_binds_cursor_to_exact_name() {
    let fixture = fixture();
    let shared = fixture.path().join("shared");
    let files = [
        ("Fixture.BAT", 0o100777),
        ("Fixture.cmd", 0o100777),
        ("Fixture.com", 0o100777),
        ("Fixture.Exe", 0o100777),
        ("Fixture.txt", 0o100666),
    ];
    let nanos = 1_794_854_752_834_272_000_i64;
    let seconds = 1_794_854_752.834272;
    let timestamp = UNIX_EPOCH + Duration::from_nanos(nanos as u64);
    for (name, _) in files {
        let mut file = fs::File::create(shared.join(name)).unwrap();
        file.write_all(b"file metadata only").unwrap();
        file.set_times(
            fs::FileTimes::new()
                .set_created(timestamp)
                .set_modified(timestamp),
        )
        .unwrap();
    }
    let mut peer = Peer::new(&fixture);
    let first = peer.success("query_files", json!({"name":"fixture","limit":1}));
    assert_eq!(first["matched_count"], files.len());
    assert_eq!(first["returned_count"], 1);
    let cursor = first["next_cursor"].as_str().unwrap();
    peer.refused("query_files", json!({"name":"Fixture","cursor":cursor}));
    let second = peer.success(
        "query_files",
        json!({"name":"fixture","limit":5,"cursor":cursor}),
    );
    assert_eq!(second["has_more"], false);
    assert_eq!(second["returned_count"], files.len() - 1);
    let exact = peer.success(
        "query_files",
        json!({
            "created_after":seconds,"created_before":seconds,
            "modified_after":seconds,"modified_before":seconds
        }),
    );
    assert_eq!(exact["returned_count"], files.len());
    let rows = first["files"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["files"].as_array().unwrap());
    for (row, (name, mode)) in rows.zip(files) {
        assert_eq!(row["path"], name);
        assert_eq!(row["attributes"]["mode"], mode, "{name}");
        assert_eq!(row["created_time"], seconds, "{name}");
        assert_eq!(row["created_time_ns"], nanos, "{name}");
        assert_eq!(row["modified_time"], seconds, "{name}");
        let info = peer.success("file_info", json!({"path":name}));
        assert_eq!(info["attributes"]["mode"], 0o100666, "{name}");
        assert_eq!(info["created_time"], seconds, "{name}");
    }
}

#[test]
fn search_context_preserves_file_chunk_boundaries_and_per_query_snippets() {
    let fixture = fixture();
    let shared = fixture.path().join("shared");
    fs::write(
        shared.join("ascii.txt"),
        format!("{}abcdef{}", "x".repeat(65_534), "y".repeat(800)),
    )
    .unwrap();
    fs::create_dir(shared.join("unicode")).unwrap();
    fs::write(
        shared.join("unicode").join("a.txt"),
        format!(
            "\u{feff}前\r\n{}界ße{}\u{2028}末行",
            "x".repeat(65_527),
            "Y".repeat(620),
        ),
    )
    .unwrap();
    let mut peer = Peer::new(&fixture);
    let ascii = peer.success(
        "search_texts",
        json!({"queries":["abcdef","ab","z".repeat(200)],"limit_per_query":3}),
    );
    let a = &ascii["results"][0]["matches"][0];
    assert_eq!(a["match_snippet"], format!("xxxabcdef{}", "y".repeat(591)));
    assert_eq!(
        ascii["results"][1]["matches"][0]["match_snippet"],
        format!("{}ab", "x".repeat(120))
    );
    assert!(
        ascii["results"][2]["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let unicode = peer.success(
        "search_texts",
        json!({"directory":"unicode","queries":["界SSE","界","末行"],"context_lines":1}),
    );
    let line = &unicode["results"][0]["matches"][0];
    assert_eq!(line["line"], 2);
    assert_eq!(
        line["match_snippet"],
        format!("xxx界sse{}", "y".repeat(593))
    );
    assert_eq!(line["context"][0]["text"], "前");
    assert_eq!(line["context"][2]["text"], "末行");
    assert_eq!(
        unicode["results"][1]["matches"][0]["match_snippet"],
        format!("x界sse{}", "y".repeat(595))
    );
}

#[test]
fn search_output_budgets_include_context_and_continue_matching_groups_in_file() {
    let fixture = fixture();
    let shared = fixture.path().join("shared");
    fs::create_dir(shared.join("contexts")).unwrap();
    fs::write(
        shared.join("contexts").join("a.txt"),
        format!("needle{}\n", "x".repeat(353)).repeat(100),
    )
    .unwrap();
    fs::create_dir(shared.join("multilingual")).unwrap();
    fs::write(
        shared.join("multilingual").join("a.txt"),
        format!(
            "{}tiny\n",
            format!("alpha{}\n", "界".repeat(595)).repeat(95)
        ),
    )
    .unwrap();
    let mut peer = Peer::new(&fixture);
    let single = peer.success(
        "search_text",
        json!({"query":"needle","directory":"contexts","limit":100,"context_lines":3}),
    );
    assert_eq!(single["matches"].as_array().unwrap().len(), 30);
    assert_eq!(single["truncated"], true);
    let context = peer.success(
        "search_texts",
        json!({"queries":["needle"],"directory":"contexts","limit_per_query":100,"context_lines":3}),
    );
    assert_eq!(context["results"][0]["matches"], single["matches"]);
    let groups = peer.success(
        "search_texts",
        json!({"queries":["alpha","tiny"],"directory":"multilingual","limit_per_query":100}),
    );
    assert_eq!(
        groups["results"][0]["matches"].as_array().unwrap().len(),
        53
    );
    assert_eq!(groups["results"][1]["matches"][0]["line"], 96);
    assert_eq!(groups["results"][1]["truncated"], true);
    assert_eq!(groups["truncated"], true);
}

#[test]
fn search_single_stops_when_a_valid_file_fills_the_scan_budget() {
    let fixture = fixture();
    let shared = fixture.path().join("shared");
    fs::write(shared.join("a.txt"), format!("needle\n{}", "x".repeat(121))).unwrap();
    fs::write(shared.join("b.txt"), "needle\n").unwrap();
    let settings = STANDARD.encode(r#"{"max_scan_bytes":128}"#);
    let mut peer = Peer::with_args(&fixture, &["--reader-settings", &settings]);
    let single = peer.success("search_text", json!({"query":"needle"}));
    let multi = peer.success("search_texts", json!({"queries":["needle"]}));
    assert_eq!(single["scanned_bytes"], 128);
    assert_eq!(single["scanned_entries"], 1);
    assert_eq!(single["truncated"], true);
    assert_eq!(multi["scanned_bytes"], 128);
    assert_eq!(multi["scanned_entries"], 2);
    assert_eq!(multi["results"][0]["matches"], single["matches"]);
    peer.refused("search_text", json!({"query":"\u{1f}"}));
}

#[test]
fn tar_extensions_resolve_names_and_sizes_before_member_checks() {
    use flate2::{Compression, write::GzEncoder};

    let fixture = fixture();
    let file = fixture.path().join("shared/fixture.tar");
    let path = json!({"path":"fixture.tar"});
    let mut peer = Peer::new(&fixture);
    let name = format!("folder/{}/note.txt", "中文".repeat(27));
    let bytes = b"archive fixture";

    let (global, path_record, size_record) = (
        pax_record("mtime", "12.5"),
        pax_record("path", &name),
        pax_record("size", &bytes.len().to_string()),
    );
    let local_records = [path_record, size_record].concat();
    let mut pax = Vec::new();
    append_tar_entry(&mut pax, "PaxHeaders/global", b'g', global.len(), &global);
    append_tar_entry(
        &mut pax,
        "PaxHeaders/local",
        b'x',
        local_records.len(),
        &local_records,
    );
    append_tar_entry(&mut pax, "fallback.txt", b'0', 0, bytes);
    append_tar_entry(&mut pax, "next.txt", b'0', 4, b"next");
    pax.extend_from_slice(&[0; 1024]);

    let mut gnu = Vec::new();
    let long_name = format!("{name}\0");
    append_tar_entry(
        &mut gnu,
        "././@LongLink",
        b'L',
        long_name.len(),
        long_name.as_bytes(),
    );
    append_tar_entry(&mut gnu, "fallback.txt", b'0', bytes.len(), bytes);
    append_tar_entry(&mut gnu, "next.txt", b'0', 4, b"next");
    gnu.extend_from_slice(&[0; 1024]);

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&pax).unwrap();
    let gzip = encoder.finish().unwrap();
    for (archive, format, compression) in [
        (pax, "TAR", "stored"),
        (gnu, "TAR", "stored"),
        (gzip, "TAR.GZ", "gzip"),
    ] {
        fs::write(&file, archive).unwrap();
        let first = peer.success("inspect_archive", json!({"path":"fixture.tar","limit":1}));
        assert_eq!(first["format"], format);
        assert_eq!(first["total_entries"], 2);
        assert_eq!(first["next_start"], 1);
        assert_eq!(first["entries"][0]["name"], name);
        assert_eq!(first["entries"][0]["compression"], compression);
        assert_eq!(first["entries"][0]["size"], bytes.len());
        let next = peer.success("inspect_archive", json!({"path":"fixture.tar","start":1}));
        assert_eq!(next["entries"][0]["name"], "next.txt");
        let member = peer.success(
            "inspect_archive",
            json!({"path":"fixture.tar","member_path":name,"offset":1,"length":4}),
        );
        assert_eq!(member["data"], "cmNoaQ==");
        assert_eq!(member["next_offset"], 5);
        assert_eq!(member["member_contents_validated"], true);
    }

    let hidden = pax_record("path", ".private/note.txt");
    let mut archive = Vec::new();
    append_tar_entry(
        &mut archive,
        "PaxHeaders/local",
        b'x',
        hidden.len(),
        &hidden,
    );
    append_tar_entry(&mut archive, "visible.txt", b'0', bytes.len(), bytes);
    archive.extend_from_slice(&[0; 1024]);
    fs::write(&file, archive).unwrap();
    let listing = peer.success("inspect_archive", path.clone());
    assert_eq!(listing["total_entries"], 0);
    assert_eq!(listing["skipped_entries"], 1);
    peer.refused(
        "inspect_archive",
        json!({"path":"fixture.tar","member_path":".private/note.txt"}),
    );
    peer.refused(
        "inspect_archive",
        json!({"path":"fixture.tar","member_path":"visible.txt"}),
    );

    for (metadata, kind) in [
        (pax_record("path", "../outside.txt"), b'0'),
        (pax_record("GNU.sparse.map", "0,1"), b'0'),
        (pax_record("size", "8388609"), b'0'),
        (b"999 path=note.txt\n".to_vec(), b'0'),
        (pax_record("path", "safe.txt"), b'2'),
    ] {
        let mut archive = Vec::new();
        append_tar_entry(
            &mut archive,
            "PaxHeaders/local",
            b'x',
            metadata.len(),
            &metadata,
        );
        append_tar_entry(&mut archive, "fallback.txt", kind, 0, b"");
        archive.extend_from_slice(&[0; 1024]);
        fs::write(&file, archive).unwrap();
        peer.refused("inspect_archive", path.clone());
    }
}

#[test]
fn office_relationships_and_spreadsheet_pagination_match_contract() {
    let fixture = fixture();
    let file = fixture.path().join("shared/office.bin");
    let path = json!({"path":"office.bin"});
    let mut peer = Peer::new(&fixture);
    let long = "文".repeat(2000);
    let worksheet = format!(
        "<worksheet><sheetData><row r=\"2\"><c r=\"A2\" t=\"s\"><v> +0 </v></c>\
         <c r=\"B2\"><f>1+2</f><v>3</v></c></row>\
         <row r=\"3\"><c r=\"A3\" t=\"inlineStr\"><is><t>{long}</t></is></c></row>\
         </sheetData></worksheet>"
    );
    let workbook = |relationships: &[u8]| {
        stored_zip(&[
            (
                "[Content_Types].xml",
                br#"<Types><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<workbook xmlns:r="rel"><sheet name="資料" id="main"/><sheet r:id="hidden"/><sheet r:id="external"/></workbook>"#.as_bytes(),
            ),
            ("xl/_rels/workbook.xml.rels", relationships),
            (
                "xl/sharedStrings.xml",
                "<sst><si><t>共用字串</t></si></sst>".as_bytes(),
            ),
            ("xl/worksheets/main.xml", worksheet.as_bytes()),
            ("xl/worksheets/.private/sheet.xml", b"excluded content"),
        ])
    };
    fs::write(
        &file,
        workbook(
            br#"<Relationships>
                <Relationship Id="main" Target="/xl/worksheets/main.xml"/>
                <Relationship Id="hidden" Target="worksheets/.private/sheet.xml"/>
                <Relationship Id="external" TargetMode="External" Target="file:///isolated-fixture.xml"/>
            </Relationships>"#,
        ),
    )
    .unwrap();
    let first = peer.success("read_document", json!({"path":"office.bin","limit":1}));
    assert_eq!(first["format"], "XLSX");
    assert_eq!(first["next_start"], 1);
    assert_eq!(
        first["units"][0]["location"],
        json!({"sheet":"資料","row":"2"})
    );
    assert_eq!(
        first["units"][0]["text"],
        r#"[{"cell": "A2", "value": "共用字串", "formula": null}, {"cell": "B2", "value": "3", "formula": "1+2"}]"#
    );
    let rest = peer.success("read_document", json!({"path":"office.bin","start":1}));
    assert_eq!(rest["total_units"], 3);
    assert_eq!(rest["units"][0]["char_offset"], 0);
    assert_eq!(rest["units"][1]["char_offset"], 2000);
    let text = [
        "[{\"cell\": \"A3\", \"value\": \"",
        &long,
        "\", \"formula\": null}]",
    ]
    .concat();
    assert_eq!(
        rest["units"][0]["text"],
        text.chars().take(2000).collect::<String>()
    );
    assert_eq!(
        rest["units"][1]["text"],
        text.chars().skip(2000).collect::<String>()
    );
    fs::write(
        &file,
        workbook(
            br#"<Relationships><Relationship Id="main" Target="../outside.xml"/></Relationships>"#,
        ),
    )
    .unwrap();
    peer.refused("read_document", path.clone());

    fs::write(
        &file,
        stored_zip(&[
            (
                "[Content_Types].xml",
                br#"<Types><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#,
            ),
            (
                "ppt/presentation.xml",
                br#"<presentation xmlns:r="rel"><sldId r:id="two"/><sldId r:id="hidden"/><sldId r:id="one"/></presentation>"#,
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                br#"<Relationships><Relationship Id="one" Target="slides/one.xml"/>
                  <Relationship Id="two" Target="slides/two.xml"/>
                  <Relationship Id="hidden" Target="slides/.private/hidden.xml"/></Relationships>"#,
            ),
            ("ppt/slides/one.xml", b"<sld><p><t>second</t></p></sld>"),
            ("ppt/slides/two.xml", b"<sld><p><t>first</t></p></sld>"),
            ("ppt/slides/.private/hidden.xml", b"excluded content"),
        ]),
    )
    .unwrap();
    let slides = peer.success("read_document", path);
    assert_eq!(slides["format"], "PPTX");
    assert_eq!(slides["total_units"], 2);
    assert_eq!(slides["units"][0]["text"], "first");
    assert_eq!(slides["units"][1]["text"], "second");
    assert_eq!(slides["units"][1]["location"], json!({"slide":2}));
}

#[test]
fn delimited_and_xml_documents_preserve_serialized_positions() {
    let fixture = fixture();
    let file = fixture.path().join("shared/structured.txt");
    let path = "structured.txt";
    let mut peer = Peer::new(&fixture);

    let long = "文".repeat(1990);
    fs::write(
        &file,
        format!("title,value\r\n\"first\r\nline\",3\r\n\"{long}，\"\"尾\",4\r\n"),
    )
    .unwrap();
    let first = peer.success(
        "read_document",
        json!({"path":path,"format_hint":"CSV","limit":1}),
    );
    assert_eq!(
        first["units"][0]["location"],
        json!({"row":1,"line_start":2,"line_end":3})
    );
    assert_eq!(first["units"][0]["text"], r#"["first\r\nline", "3"]"#);
    let tail = peer.success(
        "read_document",
        json!({"path":path,"format_hint":"CSV","start":2}),
    );
    assert_eq!(tail["units"][0]["char_offset"], 2000);
    assert_eq!(tail["units"][0]["text"], r#"4"]"#);

    fs::write(&file, "欄一\t欄二\n\"甲\t乙\"\t丙\n").unwrap();
    let tsv = peer.success("read_document", json!({"path":path,"format_hint":"TSV"}));
    assert_eq!(tsv["units"][0]["text"], r#"["甲\t乙", "丙"]"#);

    fs::write(
        &file,
        "<root xmlns:n=\"urn:fixture\" n:k=\"值\">前<!--comment-->中<?fixture ignored?>後\
         <n:item>內</n:item>尾<!--comment-->末<last/></root>",
    )
    .unwrap();
    let xml = peer.success("read_document", json!({"path":path,"format_hint":"XML"}));
    assert_eq!(xml["total_units"], 3);
    assert_eq!(
        xml["units"][0]["text"],
        r#"{"tag": "root", "attributes": {"{urn:fixture}k": "值"}, "text": "前中後", "tail": null}"#
    );
    assert_eq!(
        xml["units"][1]["location"],
        json!({"element":"/root[1]/{urn:fixture}item[1]"})
    );
    assert_eq!(
        xml["units"][1]["text"],
        r#"{"tag": "{urn:fixture}item", "attributes": {}, "text": "內", "tail": "尾末"}"#
    );
    assert_eq!(
        xml["units"][2]["text"],
        r#"{"tag": "last", "attributes": {}, "text": null, "tail": null}"#
    );
}

#[test]
fn extended_document_contract_through_worker() {
    let fixture = fixture();
    let root = fixture.path().join("shared");
    let mut peer = Peer::new(&fixture);
    let path = "document.data";
    let file = root.join(path);
    fs::write(
        &file,
        "---\r\ntitle: 測試\r\nitems: [1, true, null]\r\ndate: 2026-11-16\r\n",
    )
    .unwrap();
    let first = peer.success("read_document", json!({"path":path,"limit":2}));
    assert_eq!(first["format"], "YAML");
    assert_eq!(first["units"][0]["location"]["pointer"], "/title");
    let more = peer.success(
        "read_document",
        json!({
            "path":path, "start":first["next_start"], "expected_sha256":first["source_sha256"]
        }),
    );
    assert_eq!(more["units"][2]["location"]["value_type"], "datetime");
    for bad in [
        "a: 1\na: 2",
        "x: &a [1]\ny: *a",
        "x: !!str a",
        "---\na: 1\n---\nb: 2",
        "x: .nan",
    ] {
        fs::write(&file, bad).unwrap();
        peer.refused(
            "read_document",
            json!({"path":path,"format_hint":"YAML","limit":1}),
        );
    }
    for (format, mime, content, marker) in [
        (
            "ODT",
            "text",
            "<text><p>文件 marker</p></text>",
            "文件 marker",
        ),
        (
            "ODS",
            "spreadsheet",
            "<table name=\"Sheet\"><table-row number-rows-repeated=\"2\"><table-cell formula=\"of:=1+2\"><p>3</p></table-cell></table-row></table>",
            "of:=1+2",
        ),
        (
            "ODP",
            "presentation",
            "<page><p>slide marker</p></page>",
            "slide marker",
        ),
    ] {
        fs::write(
            &file,
            stored_zip(&[
                (
                    "mimetype",
                    format!("application/vnd.oasis.opendocument.{mime}").as_bytes(),
                ),
                (
                    "content.xml",
                    format!("<document-content>{content}</document-content>").as_bytes(),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(
            peer.success("file_info", json!({"path":path}))["format"],
            format
        );
        let result = peer.success("read_document", json!({"path":path}));
        assert_eq!(result["format"], format);
        assert!(
            result["units"][0]["text"]
                .as_str()
                .unwrap()
                .contains(marker)
        );
        if format == "ODS" {
            assert_eq!(result["units"][1]["location"]["row"], 2);
            let row = r#"[{"value_type": "", "value": "", "date_value": "", "boolean_value": "", "formula": "of:=1+2", "text": "3"}]"#;
            assert_eq!(result["units"][0]["text"], row);
            assert_eq!(result["units"][1]["text"], row);
        }
    }
    fs::write(
        &file,
        "title = \"demo\"\nwhen = 2026-11-16T01:23:45.012345678Z\n",
    )
    .unwrap();
    let result = peer.success("read_document", json!({"path":path,"format_hint":"TOML"}));
    assert_eq!(result["units"][1]["location"]["value_type"], "datetime");
    assert_eq!(
        result["units"][1]["text"],
        "\"2026-11-16 01:23:45.012345+00:00\""
    );
    fs::write(&file, "nonfinite = inf").unwrap();
    peer.refused("read_document", json!({"path":path,"format_hint":"TOML"}));
    let epub = |href: &str| {
        stored_zip(&[
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", b"<container><rootfile full-path=\"OPS/package.opf\"/></container>"),
        ("OPS/package.opf", format!("<package><manifest><item id=\"chapter\" media-type=\"application/xhtml+xml\" href=\"{href}\"/></manifest><spine><itemref idref=\"chapter\"/></spine></package>").as_bytes()),
        ("OPS/chapter name.xhtml", b"<html><body><p>chapter marker</p><script>hidden marker</script></body></html>"),
    ])
    };
    fs::write(&file, epub("chapter%20name.xhtml")).unwrap();
    let result = peer.success("read_document", json!({"path":path}));
    assert_eq!(result["format"], "EPUB");
    assert_eq!(result["units"][0]["text"], "chapter marker");
    for href in [
        "../chapter.xhtml",
        "%2e%2e/chapter.xhtml",
        "https://example.invalid/chapter.xhtml",
    ] {
        fs::write(&file, epub(href)).unwrap();
        peer.refused("read_document", json!({"path":path}));
    }

    let raw = concat!(
        "From: fixture@example.invalid\r\nSubject: =?UTF-8?B?6YO15Lu2?=\r\n",
        "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=sample\r\n\r\n",
        "--sample\r\nContent-Type: text/html; charset=utf-8\r\n\r\n",
        "<body><p>html &amp; marker</p><script>hidden</script><iframe>hidden</iframe><object>hidden</object></body>\r\n",
        "--sample\r\nContent-Type: application/octet-stream\r\n",
        "Content-Disposition: attachment; filename=opaque.bin\r\nContent-Transfer-Encoding: base64\r\n\r\n",
        "YWJj\r\n--sample--\r\n",
    );
    fs::write(&file, raw).unwrap();
    let result = peer.success("read_document", json!({"path":path}));
    assert_eq!(result["format"], "EML");
    assert_eq!(
        result["units"][0]["text"],
        r#"{"From": "fixture@example.invalid", "To": "", "Cc": "", "Subject": "郵件", "Date": "", "Message-ID": ""}"#
    );
    assert_eq!(result["units"][1]["text"], "\nhtml & marker");
    assert_eq!(
        result["units"][2]["text"],
        r#"{"attachment": true, "filename": "opaque.bin", "size": 3, "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}"#
    );
    fs::write(
        &file,
        format!("From fixture Sat Nov 14 00:00:00 2026\n{raw}").repeat(2),
    )
    .unwrap();
    let result = peer.success("read_document", json!({"path":path}));
    assert_eq!(result["messages"], 2);
    assert_eq!(result["units"][3]["location"]["message"], 2);
    for bad in [
        "From: a@b\nContent-Transfer-Encoding: base64\n\n!!!!",
        "From: a@b\nContent-Type: text/plain; charset=unknown-codec\n\nbody",
        "From: a@b\nContent-Type: multipart/mixed; boundary=sample\n\nno delimiter",
        "From: a@b\nContent-Length: 4\n\nbody",
    ] {
        fs::write(&file, bad).unwrap();
        peer.refused("read_document", json!({"path":path,"format_hint":"EML"}));
    }
    fs::write(&file, simple_pdf()).unwrap();
    let result = peer.success("read_document", json!({"path":path}));
    assert_eq!(result["pages"], 1);
    assert_eq!(result["units"][0]["text"], "PDF page\nPDF form");
    fs::write(&file, b"%PDF-1.7\n").unwrap();
    peer.refused("read_document", json!({"path":path}));
    peer.refused("read_document", json!({"path":path,"format_hint":"CSV"}));
}

#[test]
fn sqlite_snapshot_catalog_rows_and_refusals() {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::ptr::{null, null_mut};

    let fixture = fixture();
    let root = fixture.path().join("shared");
    let file = root.join("sample.sqlite");
    let mut peer = Peer::new(&fixture);
    let limits = peer.success("workspace_info", json!({}));
    if limits["format_limits"]["sqlite_available"] == false {
        return;
    }
    let fts5 = unsafe {
        use libloading::os::windows::{LOAD_LIBRARY_SEARCH_SYSTEM32, Library};
        type Open =
            unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int, *const c_char) -> c_int;
        type Exec = unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            *const c_void,
            *mut c_void,
            *mut *mut c_char,
        ) -> c_int;
        type Close = unsafe extern "C" fn(*mut c_void) -> c_int;
        let library =
            Library::load_with_flags("winsqlite3.dll", LOAD_LIBRARY_SEARCH_SYSTEM32).unwrap();
        let open = library.get::<Open>(b"sqlite3_open_v2\0").unwrap();
        let exec = library.get::<Exec>(b"sqlite3_exec\0").unwrap();
        let close = library.get::<Close>(b"sqlite3_close\0").unwrap();
        let compile_option = library
            .get::<unsafe extern "C" fn(*const c_char) -> c_int>(b"sqlite3_compileoption_used\0")
            .unwrap();
        let name = CString::new(file.to_str().unwrap()).unwrap();
        let mut db = null_mut();
        assert_eq!(open(name.as_ptr(), &mut db, 6, null()), 0);
        let setup = c"CREATE TABLE sample(id INTEGER PRIMARY KEY, title TEXT, payload BLOB);
            INSERT INTO sample VALUES(1, 'first', x'0001'), (2, 'second', NULL);
            CREATE TABLE generated(a INTEGER, b INTEGER GENERATED ALWAYS AS (a+1));
            CREATE VIEW sample_view AS SELECT title FROM sample;";
        let code = exec(db, setup.as_ptr(), null(), null_mut(), null_mut());
        assert_eq!(code, 0);
        let fts5 = compile_option(c"ENABLE_FTS5".as_ptr()) != 0;
        if fts5 {
            let seed = c"CREATE VIRTUAL TABLE [搜尋 USING] USING fts5(body);
                INSERT INTO [搜尋 USING] VALUES ('internal fixture cell');
                CREATE TABLE [搜尋 USING_notes](text);
                INSERT INTO [搜尋 USING_notes] VALUES ('ordinary fixture cell');";
            assert_eq!(exec(db, seed.as_ptr(), null(), null_mut(), null_mut()), 0);
        }
        if compile_option(c"ENABLE_FTS4".as_ptr()) != 0 {
            let seed = c"CREATE VIRTUAL TABLE [older_search] USING fts4(body);
                INSERT INTO [older_search] VALUES ('internal fixture cell');";
            assert_eq!(exec(db, seed.as_ptr(), null(), null_mut(), null_mut()), 0);
        }
        if compile_option(c"ENABLE_RTREE".as_ptr()) != 0 {
            let seed = c"CREATE VIRTUAL TABLE [map] USING rtree(id, lo, hi);
                INSERT INTO [map] VALUES(1, 0, 1);";
            assert_eq!(exec(db, seed.as_ptr(), null(), null_mut(), null_mut()), 0);
        }
        assert_eq!(close(db), 0);
        fts5
    };
    let data = fs::read(&file).unwrap();
    let path = "sample.sqlite";
    let catalog = peer.success("read_document", json!({"path":path}));
    assert_eq!(catalog["mode"], "table schema");
    assert_eq!(catalog["units"][0]["location"]["table"], "sample");
    assert_eq!(catalog["total_units"], if fts5 { 2 } else { 1 });
    if fts5 {
        assert_eq!(catalog["units"][1]["location"]["table"], "搜尋 USING_notes");
        let neighbor = peer.success(
            "read_document",
            json!({"path":path,"table":"搜尋 USING_notes"}),
        );
        assert_eq!(
            serde_json::from_str::<Value>(neighbor["units"][0]["text"].as_str().unwrap()).unwrap(),
            json!(["ordinary fixture cell"])
        );
    }
    let first = peer.success(
        "read_document",
        json!({"path":path,"table":"sample","limit":1}),
    );
    assert_eq!(first["columns"][0]["declared_type"], "INTEGER");
    assert_eq!(
        first["units"][0]["text"],
        r#"[1, "first", {"type": "blob", "encoding": "base64", "data": "AAE="}]"#
    );
    let second = peer.success("read_document", json!({
        "path":path, "table":"sample","start":first["next_start"],"expected_sha256":first["source_sha256"]
    }));
    assert_eq!(second["units"][0]["location"]["row"], 1);
    for table in [
        "sample_view",
        "generated",
        "missing",
        "搜尋 USING",
        "搜尋 USING_content",
        "搜尋 USING_data",
        "older_search",
        "older_search_content",
        "map",
        "map_node",
    ] {
        peer.refused("read_document", json!({"path":path,"table":table}));
    }
    assert_eq!(fs::read(&file).unwrap(), data);
    assert_eq!(root.read_dir().unwrap().count(), 1);
    if fts5 {
        let at = data
            .windows(b"USING fts5".len())
            .position(|bytes| bytes == b"USING fts5")
            .unwrap();
        let mut unknown = data.clone();
        unknown[at + 6..at + 10].copy_from_slice(b"mock");
        fs::write(&file, unknown).unwrap();
        let result = peer.call("read_document", json!({"path":path,"table":"sample"}));
        assert_eq!(result["isError"], true, "{result}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("無法安全辨識此虛擬資料表")
        );
    }
    let mut wal = data.clone();
    wal[18..20].copy_from_slice(&[2, 2]);
    fs::write(&file, wal).unwrap();
    peer.refused("read_document", json!({"path":path}));
    fs::write(&file, &data[..200]).unwrap();
    peer.refused("read_document", json!({"path":path}));
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut file = Vec::new();
    let mut directory = Vec::new();
    for (name, data) in entries {
        let offset = (file.len() as u32).to_le_bytes();
        let size = (data.len() as u32).to_le_bytes();
        let crc = crc32fast::hash(data).to_le_bytes();
        let name_len = (name.len() as u16).to_le_bytes();
        let mut details = vec![20, 0, 0, 8, 0, 0, 0, 0, 0, 0];
        details.extend_from_slice(&crc);
        details.extend_from_slice(&size);
        details.extend_from_slice(&size);
        details.extend_from_slice(&name_len);
        details.extend_from_slice(&[0, 0]);
        file.extend_from_slice(b"PK\x03\x04");
        file.extend_from_slice(&details);
        file.extend_from_slice(name.as_bytes());
        file.extend_from_slice(data);
        directory.extend_from_slice(b"PK\x01\x02\x14\x00");
        directory.extend_from_slice(&details);
        directory.extend_from_slice(&[0; 10]);
        directory.extend_from_slice(&offset);
        directory.extend_from_slice(name.as_bytes());
    }
    let offset = (file.len() as u32).to_le_bytes();
    let size = (directory.len() as u32).to_le_bytes();
    let count = (entries.len() as u16).to_le_bytes();
    file.extend_from_slice(&directory);
    file.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00");
    file.extend_from_slice(&count);
    file.extend_from_slice(&count);
    file.extend_from_slice(&size);
    file.extend_from_slice(&offset);
    file.extend_from_slice(&[0, 0]);
    file
}

fn simple_pdf() -> Vec<u8> {
    use lopdf::{Document, Stream, dictionary};

    let mut doc = Document::with_version("1.7");
    let pages = doc.new_object_id();
    let font =
        doc.add_object(dictionary! {"Type"=>"Font", "Subtype"=>"Type1", "BaseFont"=>"Helvetica"});
    let form = doc.add_object(Stream::new(dictionary! {
        "Type"=>"XObject", "Subtype"=>"Form", "BBox"=>vec![0.into(),0.into(),200.into(),200.into()]
    }, b"BT /F1 12 Tf (PDF form) Tj ET".to_vec()));
    let resources = doc.add_object(
        dictionary! {"Font"=>dictionary! {"F1"=>font}, "XObject"=>dictionary! {"Fm"=>form}},
    );
    let content = doc.add_object(Stream::new(
        dictionary! {},
        b"BT /F1 12 Tf (PDF page) Tj ET /Fm Do".to_vec(),
    ));
    let page = doc.add_object(dictionary! {"Type"=>"Page", "Parent"=>pages, "Contents"=>content});
    doc.objects.insert(
        pages,
        dictionary! {
            "Type"=>"Pages", "Kids"=>vec![page.into()], "Count"=>1, "Resources"=>resources,
            "MediaBox"=>vec![0.into(),0.into(),300.into(),300.into()]
        }
        .into(),
    );
    let catalog = doc.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn pdf_cmap_and_content_streams_through_worker() {
    let fixture = fixture();
    let file = fixture.path().join("shared/cmap.pdf");
    let mut peer = Peer::new(&fixture);
    for (composite, expected) in [(false, "中𠮷C"), (true, "文𠮷")] {
        fs::write(&file, pdf_with_cmap(composite, false)).unwrap();
        let first = peer.success("read_document", json!({"path":"cmap.pdf","limit":1}));
        assert_eq!(first["pages"], 2);
        assert_eq!(first["next_start"], 1);
        assert_eq!(first["units"][0]["location"], json!({"page":1}));
        assert_eq!(first["units"][0]["text"], expected);
        let second = peer.success("read_document", json!({"path":"cmap.pdf","start":1}));
        assert_eq!(second["total_units"], 2);
        assert_eq!(second["units"][0]["location"], json!({"page":2}));
        assert_eq!(second["units"][0]["text"], "");
    }
    fs::write(&file, pdf_with_cmap(false, true)).unwrap();
    peer.refused("read_document", json!({"path":"cmap.pdf"}));
}

fn pdf_with_cmap(composite: bool, invalid_filter: bool) -> Vec<u8> {
    use flate2::{Compression, write::ZlibEncoder};
    use lopdf::{Document, Object, Stream, dictionary};

    let mut doc = Document::with_version("1.7");
    let pages = doc.new_object_id();
    let (subtype, encoding, codes, map, glyphs) = if composite {
        (
            "Type0",
            "UniCNS-UTF16-H",
            "<0000> <FFFF>",
            "<1234> <6587>\n<5678> <D842DFB7>",
            "<12345678>",
        )
    } else {
        (
            "Type1",
            "WinAnsiEncoding",
            "<00> <FF>",
            "<41> <4E2D>\n<42> <D842DFB7>",
            "<414243>",
        )
    };
    let mapping = format!(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n{codes}\nendcodespacerange\n2 beginbfchar\n{map}\nendbfchar\n\
         endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n"
    );
    let cmap = doc.add_object(Stream::new(dictionary! {}, mapping.into_bytes()));
    let mut font = dictionary! {
        "Type" => "Font", "Subtype" => subtype, "BaseFont" => "Helvetica",
        "Encoding" => encoding, "ToUnicode" => cmap,
    };
    if composite {
        let descendant = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "CIDFontType2", "BaseFont" => "Helvetica", "DW" => 1000,
        });
        font.set("DescendantFonts", vec![Object::Reference(descendant)]);
    }
    let font = doc.add_object(font);
    let raw = format!("Tf 10 10 Td {glyphs} Tj ET").into_bytes();
    let next = if invalid_filter {
        Stream::new(dictionary! {"Filter"=>"InvalidFixtureFilter"}, raw)
    } else {
        let filter = doc.add_object(vec![Object::Name(b"FlateDecode".to_vec())]);
        let mut zlib = ZlibEncoder::new(Vec::new(), Compression::default());
        zlib.write_all(&raw).unwrap();
        Stream::new(dictionary! {"Filter"=>filter}, zlib.finish().unwrap())
    };
    let begin = doc.add_object(Stream::new(dictionary! {}, b"BT /F1 12".to_vec()));
    let next = doc.add_object(next);
    let streams = doc.add_object(vec![begin.into(), Object::Null, next.into()]);
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages, "Contents" => streams,
        "Resources" => dictionary! {"Font"=>dictionary! {"F1"=>font}},
    });
    let blank = doc.add_object(dictionary! {"Type"=>"Page","Parent"=>pages});
    doc.objects.insert(
        pages,
        dictionary! {
            "Type" => "Pages", "Kids" => vec![page.into(), blank.into()], "Count" => 2,
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
        }
        .into(),
    );
    let catalog = doc.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages});
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn extensible_wave_and_id3_text_through_worker() {
    let fixture = fixture();
    let file = fixture.path().join("shared/fixture.wav");
    let mut peer = Peer::new(&fixture);
    let path = json!({"path":"fixture.wav"});

    let riff_chunk = |name: &[u8; 4], body: &[u8]| {
        let mut data = name.to_vec();
        data.extend_from_slice(&(body.len() as u32).to_le_bytes());
        data.extend_from_slice(body);
        if body.len() % 2 != 0 {
            data.push(0);
        }
        data
    };
    let wave = |format: &[u8], id3: Option<&[u8]>| {
        let alignment = u16::from_le_bytes(format[12..14].try_into().unwrap());
        let mut data = b"WAVE".to_vec();
        data.extend(riff_chunk(b"fmt ", format));
        data.extend(riff_chunk(b"data", &vec![0; usize::from(alignment) * 48]));
        if let Some(id3) = id3 {
            data.extend(riff_chunk(b"ID3 ", id3));
        }
        riff_chunk(b"RIFF", &data)
    };
    let format = |encoding: u16, bits: u16, valid_bits: Option<u16>| {
        let align = 2 * bits / 8;
        let format_tag = if valid_bits.is_some() {
            0xfffe
        } else {
            encoding
        };
        let mut bytes = format_tag.to_le_bytes().to_vec();
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&(48_000 * u32::from(align)).to_le_bytes());
        bytes.extend_from_slice(&align.to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
        if let Some(valid_bits) = valid_bits {
            bytes.extend_from_slice(&22_u16.to_le_bytes());
            bytes.extend_from_slice(&valid_bits.to_le_bytes());
            bytes.extend_from_slice(&3_u32.to_le_bytes());
            bytes.extend_from_slice(&u32::from(encoding).to_le_bytes());
            bytes.extend_from_slice(b"\0\0\x10\0\x80\0\0\xaa\0\x38\x9b\x71");
        }
        bytes
    };

    for (encoding, bits, valid_bits) in [(1, 24, 20), (3, 32, 32)] {
        fs::write(&file, wave(&format(encoding, bits, Some(valid_bits)), None)).unwrap();
        let info = peer.success("inspect_media", path.clone());
        assert_eq!(info["format"], "WAV");
        assert_eq!(info["tags"], json!({}));
        assert_eq!(
            info["tracks"],
            json!([{
                "type":"audio", "codec":"WAVE", "sample_rate":48000, "channels":2,
                "duration":0.001, "bitrate":48000 * 2 * u32::from(bits), "bits_per_sample":bits
            }])
        );
    }

    let synchsafe = |size: usize| [21, 14, 7, 0].map(|shift| ((size >> shift) & 127) as u8);
    let frame = |name: &[u8; 4], body: &[u8]| {
        let mut data = name.to_vec();
        data.extend_from_slice(&synchsafe(body.len()));
        data.extend_from_slice(b"\0\0");
        data.extend_from_slice(body);
        data
    };
    let title = format!("\u{3}{}", "長".repeat(1025));
    let mut frames = frame(b"TIT2", title.as_bytes());
    frames.extend(frame(b"TXXX", "\u{3}合成\0甲\0乙\0丙\0丁\0戊".as_bytes()));
    frames.extend(frame(b"APIC", b"\0image/png\0\x03\0fixture artwork"));
    let mut id3 = b"ID3\x04\0\0".to_vec();
    id3.extend_from_slice(&synchsafe(frames.len()));
    id3.extend(frames);
    let pcm = format(1, 16, None);
    fs::write(&file, wave(&pcm, Some(&id3))).unwrap();
    assert_eq!(
        peer.success("inspect_media", path.clone())["tags"],
        json!({"TIT2":["長".repeat(1024)], "TXXX:合成":["甲","乙","丙","丁"]})
    );

    let mut bad_extension = format(1, 24, Some(20));
    bad_extension[16] = 21;
    let mut bad_guid = format(1, 24, Some(20));
    bad_guid[39] ^= 1;
    for corrupted in [
        wave(&bad_extension, None),
        wave(&bad_guid, None),
        wave(&format(1, 24, Some(25)), None),
        wave(&pcm, Some(&id3[..15])),
    ] {
        fs::write(&file, corrupted).unwrap();
        peer.refused("inspect_media", path.clone());
    }
}

#[test]
fn audio_and_video_metadata_through_worker() {
    let fixture = fixture();
    let file = fixture.path().join("shared/media.data");
    let mut peer = Peer::new(&fixture);
    let path = json!({"path":"media.data"});

    let aiff = {
        let mut data = b"FORM\0\0\0\x3eAIFFCOMM\0\0\0\x12".to_vec();
        data.extend_from_slice(&[
            0, 1, 0, 0, 0, 8, 0, 16, 0x40, 0x0b, 0xfa, 0, 0, 0, 0, 0, 0, 0,
        ]);
        data.extend_from_slice(b"SSND\0\0\0\x18");
        data.extend_from_slice(&[0; 24]);
        data
    };
    fs::write(&file, &aiff).unwrap();
    let info = peer.success("inspect_media", path.clone());
    assert_eq!(info["format"], "AIFF");
    assert_eq!(info["tracks"][0]["sample_rate"], 8000);
    assert_eq!(info["tracks"][0]["bits_per_sample"], 16);

    // Four MPEG-1 Layer III frames, 128 kbps, 44.1 kHz.
    let mut frame = vec![0; 417];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
    fs::write(&file, frame.repeat(4)).unwrap();
    let info = peer.success("inspect_media", path.clone());
    assert_eq!(info["format"], "MPEG_AUDIO");
    assert_eq!(info["tracks"][0]["sample_rate"], 44100);
    assert_eq!(info["tracks"][0]["bitrate"], 128000);
    fs::write(&file, &frame[..4]).unwrap();
    peer.refused("inspect_media", path.clone());

    let mut opus_head = b"OpusHead\x01\x01".to_vec();
    opus_head.extend_from_slice(&312_u16.to_le_bytes());
    opus_head.extend_from_slice(&48_000_u32.to_le_bytes());
    opus_head.extend_from_slice(&[0; 3]);
    let mut ogg = ogg_page(2, 0, 0, &opus_head);
    ogg.extend(ogg_page(0, 0, 1, b"OpusTags\x01\0\0\0x\0\0\0\0"));
    ogg.extend(ogg_page(4, 48_312, 2, b"\xf8\xff\xfe"));
    fs::write(&file, &ogg).unwrap();
    let info = peer.success("inspect_media", path.clone());
    assert_eq!(info["tracks"][0]["codec"], "Opus");
    assert_eq!(info["tracks"][0]["sample_rate"], 48000);

    let mut avih = [0_u8; 56];
    avih[..4].copy_from_slice(&40000_u32.to_le_bytes());
    avih[16..20].copy_from_slice(&50_u32.to_le_bytes());
    avih[32..36].copy_from_slice(&640_u32.to_le_bytes());
    avih[36..40].copy_from_slice(&360_u32.to_le_bytes());
    let mut strh = [0_u8; 56];
    strh[..8].copy_from_slice(b"vidsMJPG");
    strh[20..24].copy_from_slice(&1_u32.to_le_bytes());
    strh[24..28].copy_from_slice(&25_u32.to_le_bytes());
    strh[32..36].copy_from_slice(&50_u32.to_le_bytes());
    let mut avi = b"RIFF\0\0\0\0AVI avih\x38\0\0\0".to_vec();
    avi.extend(avih);
    avi.extend(b"strh\x38\0\0\0");
    avi.extend(strh);
    let size = (avi.len() - 8) as u32;
    avi[4..8].copy_from_slice(&size.to_le_bytes());
    fs::write(&file, &avi).unwrap();
    let info = peer.success("inspect_media", path.clone());
    assert_eq!(info["format"], "AVI");
    assert_eq!(info["tracks"][0]["codec"], "MJPG");
    assert_eq!(info["fps"].as_f64(), Some(25.));
    fs::write(&file, &avi[..avi.len() - 1]).unwrap();
    peer.refused("inspect_media", path.clone());

    let mut mdhd = [0_u8; 24];
    mdhd[12..16].copy_from_slice(&1000_u32.to_be_bytes());
    mdhd[16..20].copy_from_slice(&2000_u32.to_be_bytes());
    let mut sample = [0_u8; 28];
    sample[24..26].copy_from_slice(&640_u16.to_be_bytes());
    sample[26..28].copy_from_slice(&360_u16.to_be_bytes());
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsd.extend(mp4_box(b"avc1", &sample));
    let mut stbl = mp4_box(b"stsd", &stsd);
    stbl.extend(mp4_box(
        b"stts",
        &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 50, 0, 0, 0, 40],
    ));
    let mut mdia = mp4_box(b"mdhd", &mdhd);
    mdia.extend(mp4_box(b"hdlr", b"\0\0\0\0\0\0\0\0vide"));
    mdia.extend(mp4_box(b"minf", &mp4_box(b"stbl", &stbl)));
    let mut mp4 = mp4_box(b"ftyp", b"isom\0\0\0\0");
    mp4.extend(mp4_box(
        b"moov",
        &mp4_box(b"trak", &mp4_box(b"mdia", &mdia)),
    ));
    fs::write(&file, &mp4).unwrap();
    let info = peer.success("inspect_media", path.clone());
    assert_eq!(info["format"], "MP4");
    assert_eq!(info["tracks"][0]["codecs"], json!(["avc1"]));
    assert_eq!(info["tracks"][0]["width"], 640);
    assert_eq!(info["tracks"][0]["fps"].as_f64(), Some(25.));
    fs::write(&file, &mp4[..mp4.len() - 1]).unwrap();
    peer.refused("inspect_media", path.clone());

    let mut video = ebml(b"\xb0", b"\x02\x80");
    video.extend(ebml(b"\xba", b"\x01\x68"));
    let mut track = ebml(b"\xd7", &[1]);
    track.extend(ebml(b"\x83", &[1]));
    track.extend(ebml(b"\x86", b"V_VP9"));
    track.extend(ebml(b"\xe0", &video));
    let info = ebml(
        b"\x15\x49\xa9\x66",
        &ebml(b"\x44\x89", &2000_f64.to_be_bytes()),
    );
    for (doctype, format) in [("matroska", "MATROSKA"), ("webm", "WEBM")] {
        let header = ebml(b"\x1a\x45\xdf\xa3", &ebml(b"\x42\x82", doctype.as_bytes()));
        let mut data = header.clone();
        let mut segment = info.clone();
        segment.extend(ebml(b"\x16\x54\xae\x6b", &ebml(b"\xae", &track)));
        data.extend(ebml(b"\x18\x53\x80\x67", &segment));
        fs::write(&file, &data).unwrap();
        let result = peer.success("inspect_media", path.clone());
        assert_eq!(result["format"], format);
        assert_eq!(result["tracks"][0]["width"], 640);
        assert_eq!(result["duration"].as_f64(), Some(2.));
        fs::write(&file, &data[..data.len() - 1]).unwrap();
        peer.refused("inspect_media", path.clone());
        let mut data = header;
        let mut segment = info.clone();
        segment.extend(ebml(b"\x16\x54\xae\x6b", &ebml(b"\xae", &track).repeat(33)));
        data.extend(ebml(b"\x18\x53\x80\x67", &segment));
        fs::write(&file, data).unwrap();
        peer.refused("inspect_media", path.clone());
    }
}

#[test]
fn static_avif_to_png_through_worker() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    // A synthetic 32 × 24 solid-color image encoded by the reference Pillow build.
    let avif = STANDARD.decode("AAAAIGZ0eXBhdmlmAAAAAGF2aWZtaWYxbWlhZk1BMUIAAADrbWV0YQAAAAAAAAAhaGRscgAAAAAAAAAAcGljdAAAAAAAAAAAAAAAAAAAAAAOcGl0bQAAAAAAAQAAAB5pbG9jAAAAAEQAAAEAAQAAAAEAAAETAAAAKQAAAChpaW5mAAAAAAABAAAAGmluZmUCAAAAAAEAAGF2MDFDb2xvcgAAAABqaXBycAAAAEtpcGNvAAAAFGlzcGUAAAAAAAAAIAAAABgAAAAQcGl4aQAAAAADCAgIAAAADGF2MUOBAAwAAAAAE2NvbHJuY2x4AAEADQAGgAAAABdpcG1hAAAAAAAAAAEAAQQBAoMEAAAAMW1kYXQSAAoJGBE/dogIaDQgMhoUx4eGZQIIIJ5QAAAARdWbC68rTWa8fdcF1w==").unwrap();
    let fixture = fixture();
    let file = fixture.path().join("shared/static.avif");
    fs::write(&file, &avif).unwrap();
    let mut peer = Peer::new(&fixture);
    let path = json!({"path":"static.avif"});
    let info = peer.success("file_info", path.clone());
    assert_eq!(info["format"], "AVIF");
    let media = peer.success("inspect_media", path.clone());
    assert_eq!(media["source_width"], 32);
    assert_eq!(media["source_height"], 24);
    let image = peer.call("read_image", path.clone());
    assert_eq!(image["isError"], false, "{image}");
    assert_eq!(image["content"][1]["mimeType"], "image/png");
    let png = STANDARD
        .decode(image["content"][1]["data"].as_str().unwrap())
        .unwrap();
    let image = image::load_from_memory(&png).unwrap().into_rgb8();
    assert_eq!((image.width(), image.height()), (32, 24));
    for (actual, expected) in image.get_pixel(0, 0).0.into_iter().zip([30_u8, 60, 90]) {
        assert!(actual.abs_diff(expected) < 5, "{actual} versus {expected}");
    }
    for bad in [&avif[..36], b"fake avif".as_slice()] {
        fs::write(&file, bad).unwrap();
        peer.refused("read_image", path.clone());
    }
}

#[test]
fn monochrome_avif_preserves_alpha_through_worker() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    // Synthetic 24 × 16 stills encoded by Pillow with 4:0:0 chroma.
    let straight = concat!(
        "AAAAHGZ0eXBhdmlmAAAAAGF2aWZtaWYxbWlhZgAAAWptZXRhAAAAAAAAACFoZGxyAAAAAAAAAABwaWN0",
        "AAAAAAAAAAAAAAAAAAAAAA5waXRtAAAAAAABAAAALGlsb2MAAAAARAAAAgABAAAAAQAAAaEAAAAWAAIA",
        "AAABAAABjgAAABMAAABCaWluZgAAAAAAAgAAABppbmZlAgAAAAABAABhdjAxQ29sb3IAAAAAGmluZmUC",
        "AAAAAAIAAGF2MDFBbHBoYQAAAAAaaXJlZgAAAAAAAAAOYXV4bAACAAEAAQAAAKdpcHJwAAAAgWlwY28A",
        "AAAUaXNwZQAAAAAAAAAYAAAAEAAAAA5waXhpAAAAAAEIAAAADGF2MUOBABwAAAAAE2NvbHJuY2x4AAEA",
        "DQAGgAAAADhhdXhDAAAAAHVybjptcGVnOm1wZWdCOmNpY3A6c3lzdGVtczphdXhpbGlhcnk6YWxwaGEA",
        "AAAAHmlwbWEAAAAAAAAAAgABBAECgwQAAgQBAoMFAAAAMW1kYXQSAAoFGBDv7CoyCBAAAC17BJYkEgAK",
        "CBgQ7+wwENBqMggQAAAtev/prA==",
    );
    let premultiplied = concat!(
        "AAAAHGZ0eXBhdmlmAAAAAGF2aWZtaWYxbWlhZgAAAXhtZXRhAAAAAAAAACFoZGxyAAAAAAAAAABwaW",
        "N0AAAAAAAAAAAAAAAAAAAAAA5waXRtAAAAAAABAAAALGlsb2MAAAAARAAAAgABAAAAAQAAAa8AAAAW",
        "AAIAAAABAAABnAAAABMAAABCaWluZgAAAAAAAgAAABppbmZlAgAAAAABAABhdjAxQ29sb3IAAAAAGm",
        "luZmUCAAAAAAIAAGF2MDFBbHBoYQAAAAAoaXJlZgAAAAAAAAAOcHJlbQABAAEAAgAAAA5hdXhsAAIA",
        "AQABAAAAp2lwcnAAAACBaXBjbwAAABRpc3BlAAAAAAAAABgAAAAQAAAADnBpeGkAAAAAAQgAAAAMYX",
        "YxQ4EAHAAAAAATY29scm5jbHgAAQANAAaAAAAAOGF1eEMAAAAAdXJuOm1wZWc6bXBlZ0I6Y2ljcDpz",
        "eXN0ZW1zOmF1eGlsaWFyeTphbHBoYQAAAAAeaXBtYQAAAAAAAAACAAEEAQKDBAACBAECgwUAAAAxbW",
        "RhdBIACgUYEO/sKjIIEAAALXsI85ASAAoIGBDv7DAQ0GoyCBAAAC16/oht",
    );
    let fixture = fixture();
    let mut peer = Peer::new(&fixture);
    for (name, encoded, expected) in [
        ("straight.avif", straight, [90, 90, 90, 61]),
        ("premultiplied.avif", premultiplied, [50, 50, 50, 102]),
    ] {
        fs::write(
            fixture.path().join("shared").join(name),
            STANDARD.decode(encoded).unwrap(),
        )
        .unwrap();
        let response = peer.call("read_image", json!({"path": name}));
        assert_eq!(response["isError"], false, "{response}");
        assert_eq!(response["content"][1]["mimeType"], "image/png");
        let png = STANDARD
            .decode(response["content"][1]["data"].as_str().unwrap())
            .unwrap();
        let image = image::load_from_memory(&png).unwrap().into_rgba8();
        assert_eq!(image.dimensions(), (24, 16));
        assert_eq!(image.get_pixel(0, 0).0, expected, "{name}");
    }
    let file = fixture.path().join("shared/premultiplied.avif");
    let mut invalid_reference = fs::read(&file).unwrap();
    let reference = invalid_reference
        .windows(10)
        .position(|bytes| bytes == b"prem\0\x01\0\x01\0\x02")
        .unwrap();
    invalid_reference[reference + 9] = 1;
    fs::write(file, invalid_reference).unwrap();
    peer.refused("read_image", json!({"path": "premultiplied.avif"}));
}

#[test]
fn grid_avif_preserves_tile_order_crop_and_alpha_through_worker() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use gamut_isobmff::{ImageGrid, IsoBmffImage, Item, ItemReference, Property, PropertyKind};

    let tile = |encoded: &str, id| {
        let mut tile = gamut_isobmff::read(&STANDARD.decode(encoded).unwrap())
            .unwrap()
            .items
            .remove(0);
        tile.id = id;
        tile.hidden = true;
        tile
    };
    // Synthetic 64 × 64 stills with 4:4:4 color or 4:0:0 alpha.
    let color = concat!(
        "AAAAIGZ0eXBhdmlmAAAAAGF2aWZtaWYxbWlhZk1BMUEAAADrbWV0YQAAAAAAAAAhaGRscgAAAAAAAAAA",
        "cGljdAAAAAAAAAAAAAAAAAAAAAAOcGl0bQAAAAAAAQAAAB5pbG9jAAAAAEQAAAEAAQAAAAEAAAETAAAA",
        "JgAAAChpaW5mAAAAAAABAAAAGmluZmUCAAAAAAEAAGF2MDFDb2xvcgAAAABqaXBycAAAAEtpcGNvAAAA",
        "FGlzcGUAAAAAAAAAQAAAAEAAAAAQcGl4aQAAAAADCAgIAAAADGF2MUOBIAAAAAAAE2NvbHJuY2x4AAEA",
        "DQAGgAAAABdpcG1hAAAAAAAAAAEAAQQBAoMEAAAALm1kYXQSAAoJOBV//YQENBpAMhcQAAC0UbT",
    );
    let first = tile(&format!("{color}wTszBGPyV0YX2D4+8pzAX6A=="), 2);
    let second = tile(&format!("{color}+Q8EZAesrGX0TQeTcrOBlQA=="), 3);
    let alpha = tile(
        concat!(
            "AAAAHGZ0eXBhdmlmAAAAAGF2aWZtaWYxbWlhZgAAAOltZXRhAAAAAAAAACFoZGxyAAAAAAAAAABwaWN0",
            "AAAAAAAAAAAAAAAAAAAAAA5waXRtAAAAAAABAAAAHmlsb2MAAAAARAAAAQABAAAAAQAAAQ0AAAAZAAAA",
            "KGlpbmYAAAAAAAEAAAAaaW5mZQIAAAAAAQAAYXYwMUNvbG9yAAAAAGhpcHJwAAAASWlwY28AAAAUaXNw",
            "ZQAAAAAAAABAAAAAQAAAAA5waXhpAAAAAAEIAAAADGF2MUOBABwAAAAAE2NvbHJuY2x4AAEADQAGgAAA",
            "ABdpcG1hAAAAAAAAAAEAAQQBAoMEAAAAIW1kYXQSAAoJGBV//YYCGg1AMgoQALRUBLQVV9lA",
        ),
        5,
    );
    let grid = |id, ids, hidden| Item {
        id,
        item_type: *b"grid",
        name: String::new(),
        content_type: None,
        content_encoding: None,
        hidden,
        references: vec![ItemReference {
            reference_type: *b"dimg",
            to_item_ids: ids,
        }],
        properties: vec![
            Property {
                essential: false,
                kind: PropertyKind::ImageSpatialExtents {
                    width: 117,
                    height: 56,
                },
            },
            Property {
                essential: false,
                kind: PropertyKind::PixelInformation {
                    bits_per_channel: if hidden { vec![8] } else { vec![8, 8, 8] },
                },
            },
        ],
        payload: ImageGrid {
            rows: 1,
            columns: 2,
            output_width: 117,
            output_height: 56,
        }
        .to_bytes()
        .unwrap(),
    };
    let mut color_grid = grid(1, vec![2, 3], false);
    color_grid.properties.extend([
        Property {
            essential: true,
            kind: PropertyKind::CleanAperture {
                width_n: 115,
                width_d: 1,
                height_n: 54,
                height_d: 1,
                horiz_off_n: 0,
                horiz_off_d: 1,
                vert_off_n: 0,
                vert_off_d: 1,
            },
        },
        Property {
            essential: true,
            kind: PropertyKind::Rotation(1),
        },
    ]);
    let mut alpha_grid = grid(4, vec![5, 6], true);
    alpha_grid.properties.extend([
        Property {
            essential: true,
            kind: PropertyKind::AuxiliaryType {
                aux_type: "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha".into(),
                aux_subtype: Vec::new(),
            },
        },
        Property {
            essential: true,
            kind: PropertyKind::Rotation(1),
        },
    ]);
    alpha_grid.references.push(ItemReference {
        reference_type: *b"auxl",
        to_item_ids: vec![1],
    });
    let mut second_alpha = alpha.clone();
    second_alpha.id = 6;
    let mut image = IsoBmffImage {
        major_brand: *b"avif",
        minor_version: 0,
        compatible_brands: vec![*b"avif", *b"mif1", *b"miaf"],
        primary_item_id: 1,
        items: vec![color_grid, first, second, alpha_grid, alpha, second_alpha],
        groups: Vec::new(),
    };
    let fixture = fixture();
    let file = fixture.path().join("shared/grid.avif");
    fs::write(&file, gamut_isobmff::write(&image).unwrap()).unwrap();
    let mut peer = Peer::new(&fixture);
    let path = json!({"path": "grid.avif"});
    let media = peer.success("inspect_media", path.clone());
    assert_eq!(media["source_width"], 117);
    assert_eq!(media["source_height"], 56);
    let response = peer.call("read_image", path.clone());
    assert_eq!(response["isError"], false, "{response}");
    assert_eq!(response["content"][1]["mimeType"], "image/png");
    let data = STANDARD
        .decode(response["content"][1]["data"].as_str().unwrap())
        .unwrap();
    let output = image::load_from_memory(&data).unwrap().into_rgba8();
    assert_eq!(output.dimensions(), (56, 117));
    for (y, expected) in [(116, [30_u8, 60, 90, 113]), (0, [190, 100, 20, 113])] {
        for (actual, expected) in output.get_pixel(0, y).0.into_iter().zip(expected) {
            assert!(
                actual.abs_diff(expected) <= 3,
                "{actual} versus {expected} at y={y}"
            );
        }
    }
    image.items[0].references[0].to_item_ids.pop();
    fs::write(&file, gamut_isobmff::write(&image).unwrap()).unwrap();
    peer.refused("read_image", path.clone());
    image.items[0].references[0].to_item_ids.push(3);
    image.items[3].references[0].to_item_ids[1] = 5;
    fs::write(&file, gamut_isobmff::write(&image).unwrap()).unwrap();
    peer.refused("read_image", path.clone());
    image.items[3].references[0].to_item_ids[1] = 6;
    let mut sequence = gamut_isobmff::write(&image).unwrap();
    sequence.extend_from_slice(b"\0\0\0\x08moov");
    fs::write(&file, sequence).unwrap();
    peer.refused("read_image", path.clone());

    image.items.remove(3);
    for (alpha_tile, color_id) in image.items[3..].iter_mut().zip([2, 3]) {
        alpha_tile.properties.push(Property {
            essential: true,
            kind: PropertyKind::AuxiliaryType {
                aux_type: "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha".into(),
                aux_subtype: Vec::new(),
            },
        });
        alpha_tile.references.push(ItemReference {
            reference_type: *b"auxl",
            to_item_ids: vec![color_id],
        });
    }
    fs::write(file, gamut_isobmff::write(&image).unwrap()).unwrap();
    let per_tile_alpha = peer.call("read_image", path);
    assert_eq!(per_tile_alpha["isError"], false, "{per_tile_alpha}");
    assert_eq!(per_tile_alpha["content"][1], response["content"][1]);
}

fn ogg_page(flags: u8, granule: u64, index: u32, packet: &[u8]) -> Vec<u8> {
    let mut page = b"OggS\0".to_vec();
    page.push(flags);
    page.extend(granule.to_le_bytes());
    page.extend(1_u32.to_le_bytes());
    page.extend(index.to_le_bytes());
    page.extend([0; 4]);
    page.extend([1, u8::try_from(packet.len()).unwrap()]);
    page.extend(packet);
    let mut crc = 0_u32;
    for &byte in &page {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            let polynomial = if crc & 0x80000000 == 0 { 0 } else { 0x04c11db7 };
            crc = (crc << 1) ^ polynomial;
        }
    }
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut data = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    data.extend(kind);
    data.extend(payload);
    data
}

fn ebml(tag: &[u8], content: &[u8]) -> Vec<u8> {
    let mut data = tag.to_vec();
    if content.len() < 127 {
        data.push(0x80 | content.len() as u8);
    } else {
        assert!(content.len() < 16383);
        data.extend((0x4000 | content.len() as u16).to_be_bytes());
    }
    data.extend(content);
    data
}

fn make_tar(name: &str, contents: &[u8]) -> Vec<u8> {
    let mut tar = Vec::new();
    append_tar_entry(&mut tar, name, b'0', contents.len(), contents);
    tar.extend_from_slice(&[0; 1024]);
    tar
}

fn append_tar_entry(out: &mut Vec<u8>, name: &str, kind: u8, size: usize, contents: &[u8]) {
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..108].copy_from_slice(b"0000644\0");
    header[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    header[148..156].fill(b' ');
    header[156] = kind;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u32 = header.iter().copied().map(u32::from).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(contents);
    out.resize(out.len().next_multiple_of(512), 0);
}

fn pax_record(key: &str, value: &str) -> Vec<u8> {
    let body = format!(" {key}={value}\n");
    let mut length = body.len() + 1;
    loop {
        let digit_count = length.to_string().len();
        let actual = body.len() + digit_count;
        if length == actual {
            return format!("{length}{body}").into_bytes();
        }
        length = actual;
    }
}
