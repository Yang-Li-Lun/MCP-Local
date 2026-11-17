use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use flate2::read::GzDecoder;

use serde_json::Value;
use tempfile::{Builder, TempDir};
use windows_sys::Win32::{System::Threading::CREATE_NO_WINDOW, UI::Shell::FOLDERID_Windows};

use super::*;
use crate::{
    developer_vm::{
        InvocationFailure, MAX_CONFIGURATION, guard::tests::job, tests::BRIDGE_FIXTURE_SCRIPT,
    },
    filesystem::known_folder,
    settings::ReaderSettings,
};

fn fixture() -> (TempDir, PathBuf, Root) {
    let directory = Builder::new()
        .prefix("mcp-guest-launch-")
        .tempdir()
        .unwrap();
    let shared = directory.path().join("shared & project");
    fs::create_dir(&shared).unwrap();
    let root = Root::new(&shared, ReaderSettings::default()).unwrap();
    (directory, shared, root)
}

fn budget() -> Budget {
    Budget::new(CancellationToken::new())
}

#[derive(Default)]
struct Simulator {
    visible: BTreeSet<Uuid>,
    configuration: Option<String>,
    stop_refused: bool,
}

impl Backend for Simulator {
    type Connector = ();

    fn list(&mut self) -> Result<BTreeSet<Uuid>> {
        Ok(self.visible.clone())
    }

    fn start(&mut self, id: Uuid, configuration: &str) -> Result<Uuid, InvocationFailure> {
        self.visible.insert(id);
        self.configuration = Some(configuration.to_owned());
        Ok(id)
    }

    fn connect(&mut self, _: Uuid) -> Result<Self::Connector> {
        Ok(())
    }

    fn stop(&mut self, id: Uuid) -> Result<()> {
        ensure!(!self.stop_refused, "synthetic stop failure");
        self.visible.remove(&id);
        Ok(())
    }

    fn close_connector(&mut self, _: &mut Self::Connector) -> Result<()> {
        Ok(())
    }
}

#[test]
fn launch_configuration_confines_guest_paths_and_encodes_them_as_data() {
    let (directory, shared, root) = fixture();
    let cwd = "專案 '配置’ & assets";
    fs::create_dir(shared.join(cwd)).unwrap();
    let plan = Plan::fixture("main", &root, directory.path(), &mut budget()).unwrap();
    let mut guest = Guest::prepare("main", &root, Some(cwd), plan).unwrap();
    let spool_name = guest.spool.name().to_owned();
    let mut lifecycle = Lifecycle::new(Simulator::default());
    let id = guest
        .start(&mut lifecycle, &mut budget(), &CancellationToken::new())
        .unwrap();
    assert!(lifecycle.backend.visible.contains(&id));
    let xml = lifecycle.backend.configuration.as_ref().unwrap();
    assert!(xml.encode_utf16().count() <= MAX_CONFIGURATION);
    let xml = roxmltree::Document::parse(xml).unwrap();
    let root = xml.root_element();
    for setting in [
        "Networking",
        "ClipboardRedirection",
        "AudioInput",
        "VideoInput",
        "PrinterRedirection",
        "vGPU",
    ] {
        assert_eq!(
            root.children()
                .find(|node| node.has_tag_name(setting))
                .unwrap()
                .text(),
            Some("Disable")
        );
    }
    let mapping = root
        .descendants()
        .find(|node| node.has_tag_name("HostFolder"))
        .unwrap()
        .text()
        .unwrap();
    assert_eq!(mapping, shared.to_str().unwrap());
    let logon = root
        .children()
        .find(|node| node.has_tag_name("LogonCommand"))
        .unwrap();
    let command = logon
        .children()
        .find(|node| node.has_tag_name("Command"))
        .unwrap()
        .text()
        .unwrap();
    let encoded = command
        .strip_prefix(&format!(
            "{WINDOWS_POWERSHELL} -NoLogo -NoProfile -NonInteractive -EncodedCommand "
        ))
        .unwrap();
    let bytes = STANDARD.decode(encoded).unwrap();
    assert_eq!(bytes.len() % 2, 0);
    let loader = String::from_utf16(
        &bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let (delimiter, _) = loader.split_once("'));").unwrap();
    let blob = delimiter.split_once("FromBase64String('").unwrap().1;
    let compressed = STANDARD.decode(blob).unwrap();
    let mut script = String::new();
    GzDecoder::new(compressed.as_slice())
        .read_to_string(&mut script)
        .unwrap();
    let (line, adapter) = script.split_once('\n').unwrap();
    assert_eq!(adapter, BOOTSTRAP);
    assert!(!line.contains(cwd));
    let data = line
        .split_once("FromBase64String('")
        .unwrap()
        .1
        .split_once("'))")
        .unwrap()
        .0;
    let args: Value = serde_json::from_slice(&STANDARD.decode(data).unwrap()).unwrap();
    assert_eq!(args["cwd"], format!(r"C:\MCP\mapping-0\{cwd}"));
    assert_eq!(args["spool"], format!(r"C:\MCP\mapping-0\{spool_name}"));
    assert_eq!(args["git_roots"][0], "C:/MCP/mapping-0");
    assert_eq!(args["git_roots"][1], format!("C:/MCP/mapping-0/{cwd}"));
    assert!(args.get("command").is_none());
    assert!(
        !args["path"]
            .as_str()
            .unwrap()
            .contains(shared.to_str().unwrap())
    );
    assert!(!shared.join(&spool_name).join("request.json").exists());
    guest.close(&mut lifecycle).unwrap();
}

#[test]
fn teardown_keeps_spool_owned_until_the_vm_is_confirmed_stopped() {
    let (directory, shared, root) = fixture();
    let plan = Plan::fixture("main", &root, directory.path(), &mut budget()).unwrap();
    let mut guest = Guest::prepare("main", &root, None, plan).unwrap();
    let temporary = shared.join(guest.spool.name());
    fs::write(temporary.join("ready.json"), br#"{"ready":true}"#).unwrap();
    let mut lifecycle = Lifecycle::new(Simulator::default());
    let id = guest
        .start(&mut lifecycle, &mut budget(), &CancellationToken::new())
        .unwrap();
    lifecycle.backend.stop_refused = true;
    assert!(
        guest
            .close(&mut lifecycle)
            .unwrap_err()
            .to_string()
            .contains("VM_STOP_UNCONFIRMED")
    );
    assert!(temporary.join("ready.json").exists());
    assert!(lifecycle.backend.visible.contains(&id));
    lifecycle.backend.stop_refused = false;
    guest.close(&mut lifecycle).unwrap();
    assert!(!temporary.exists());
    assert!(lifecycle.owned.is_none());
}

#[test]
fn invalid_working_directories_are_refused_before_staging() {
    let (directory, shared, root) = fixture();
    fs::write(shared.join("regular.txt"), "synthetic").unwrap();
    for path in ["../", "missing", "regular.txt", r"C:\Windows"] {
        let plan = Plan::fixture("main", &root, directory.path(), &mut budget()).unwrap();
        let error = Guest::prepare("main", &root, Some(path), plan)
            .err()
            .unwrap();
        assert!(error.to_string().contains("VM_CWD"), "{path}: {error}");
    }
    assert_eq!(fs::read_dir(shared).unwrap().count(), 1);
}

#[test]
fn windows_powershell_bootstrap_exchanges_utf8_input_output_and_eof() {
    let windows = known_folder(&FOLDERID_Windows).unwrap();
    let executable = windows.join("System32/WindowsPowerShell/v1.0/powershell.exe");
    assert!(Path::new(WINDOWS_POWERSHELL).is_file() && executable.is_file());
    let (directory, shared, root) = fixture();
    let cwd = shared.join("合成 'project' & files");
    fs::create_dir(&cwd).unwrap();
    fs::write(cwd.join("seed.txt"), "合成根目錄").unwrap();
    let plan = Plan::fixture("main", &root, directory.path(), &mut budget()).unwrap();
    let mut guest = Guest::prepare(
        "main",
        &root,
        Some(cwd.file_name().unwrap().to_str().unwrap()),
        plan,
    )
    .unwrap();
    let spool = shared.join(guest.spool.name());
    let system = windows.join("System32");
    let path = std::env::join_paths([&system, &windows]).unwrap();
    let encoded = encode_bootstrap(AdapterArguments {
        spool: spool.to_str().unwrap().to_owned(),
        cwd: cwd.to_str().unwrap().to_owned(),
        path: path.to_str().unwrap().to_owned(),
        git_roots: vec![shared.to_str().unwrap().to_owned()],
    })
    .unwrap();
    let mut child = Command::new(executable)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &encoded,
        ])
        .env_clear()
        .env("SystemRoot", &windows)
        .env("WINDIR", &windows)
        .env("PATH", &path)
        .env("TEMP", directory.path())
        .env("TMP", directory.path())
        .env("LOCALAPPDATA", directory.path())
        .env("USERPROFILE", directory.path())
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .unwrap();
    let job = job(&child);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut budget = budget();
    while !guest.submit(BRIDGE_FIXTURE_SCRIPT, &mut budget).unwrap() {
        let failure = guest.result(&mut budget).unwrap();
        assert!(
            failure.is_none(),
            "bootstrap stopped before readiness: {failure:?}"
        );
        assert!(
            child.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "PowerShell readiness timed out"
        );
        thread::sleep(Duration::from_millis(25));
    }
    let mut stdin_sent = false;
    let result = loop {
        guest.refresh_output(&mut budget).unwrap();
        if let Some(result) = guest.result(&mut budget).unwrap() {
            break result;
        }
        if !stdin_sent
            && String::from_utf8_lossy(guest.output()).contains("sandbox-ready:合成根目錄")
        {
            let text = "測試 🦀\n";
            assert_eq!(
                guest.input(text.as_bytes(), true, &mut budget).unwrap(),
                text.len()
            );
            stdin_sent = true;
        }
        assert!(
            Instant::now() < deadline,
            "PowerShell command timed out, output {:?}",
            String::from_utf8_lossy(guest.output())
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(
        result.exit_code,
        7,
        "{result:?}, output {:?}",
        String::from_utf8_lossy(guest.output())
    );
    assert!(result.error.is_none() && stdin_sent);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "PowerShell adapter did not exit");
        thread::sleep(Duration::from_millis(25));
    }
    drop(job);
    guest.refresh_output(&mut budget).unwrap();
    let output = std::str::from_utf8(guest.output()).unwrap();
    assert!(
        output.contains("sandbox-output:測試 🦀") && output.contains("sandbox-stderr"),
        "{output}"
    );
    assert_eq!(
        fs::read_to_string(cwd.join("proof.txt")).unwrap(),
        "測試 🦀"
    );
    guest
        .close(&mut Lifecycle::new(Simulator::default()))
        .unwrap();
    assert!(!spool.exists());
}
