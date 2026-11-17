use std::io::{self, Write};

use super::*;

#[test]
fn missing_invalid_or_duplicate_identifiers_are_inconclusive() {
    let a = Uuid::from_u128(42);
    let b = Uuid::from_u128(43);
    let list = format!(
        r#"{{"WindowsSandboxEnvironments":[{{"Id":"{a}","Status":"Running"}},{{"Id":"{b}"}}]}}"#
    );
    let mut bom = b"\xef\xbb\xbf".to_vec();
    bom.extend_from_slice(list.as_bytes());
    assert_eq!(ids(&bom).unwrap(), BTreeSet::from([a, b]));
    assert_eq!(
        parse_row(format!(r#"{{"Id":"{a}"}}"#).as_bytes()).unwrap(),
        a
    );
    assert!(
        ids(br#"{"WindowsSandboxEnvironments":[]}"#)
            .unwrap()
            .is_empty()
    );
    for invalid in [
        &b""[..],
        br#"{}"#,
        br#"{"WindowsSandboxEnvironments":[{"Id":"invalid"}]}"#,
        br#"{"WindowsSandboxEnvironments":[{}]}"#,
        br#"{"WindowsSandboxEnvironments":null}"#,
    ] {
        assert!(ids(invalid).is_err(), "{invalid:?}");
    }
    let duplicate = format!(r#"{{"WindowsSandboxEnvironments":[{{"Id":"{a}"}},{{"Id":"{a}"}}]}}"#);
    assert!(ids(duplicate.as_bytes()).is_err());
    assert!(parse_row(br#"{"Id":"wrong"}"#).is_err());
}

#[test]
fn cli_path_must_match_the_registered_microsoft_package_location() {
    let program_files = Path::new(r"C:\Program Files");
    let name = "MicrosoftWindows.WindowsSandbox_1.0.0.0_x64__cw5n1h2txyewy";
    let correct = program_files.join("WindowsApps").join(name);
    assert_eq!(
        package_path(program_files, name, &correct).unwrap(),
        correct
    );
    for outside in [
        Path::new(r"C:\Users\public").join(name),
        program_files.join("WindowsApps-other").join(name),
        program_files
            .join("WindowsApps")
            .join("unregistered")
            .join(name),
        PathBuf::from(name),
    ] {
        assert!(
            package_path(program_files, name, &outside).is_err(),
            "{outside:?}"
        );
    }
}

#[test]
fn verified_package_image_pins_the_exact_file_and_refuses_aliases() {
    let fixture = tempfile::Builder::new()
        .prefix("mcp-cli-image-")
        .tempdir()
        .unwrap();
    let executable = fixture.path().join("wsb.exe");
    std::fs::write(&executable, b"synthetic; never executed").unwrap();
    let (image, path) = registered_package_image(&executable).unwrap();
    assert_eq!(path.file_name().unwrap(), "wsb.exe");
    let renamed = fixture.path().join("renamed.exe");
    assert!(std::fs::rename(&executable, &renamed).is_err());
    drop(image);
    std::fs::hard_link(&executable, &renamed).unwrap();
    assert!(registered_package_image(&executable).is_err());
}

#[test]
#[ignore = "started as a synthetic child by bounded_windows_cli_output_and_deadline"]
fn pipe_fixture() {
    let case = std::env::var("MCP_LOCAL_RUST_VM_TEST_PIPE").unwrap();
    let mut stdout = io::stdout().lock();
    match case.as_str() {
        "output" => stdout.write_all(b"SYNTHETIC_PIPE_MARKER").unwrap(),
        "overflow" => stdout.write_all(&[b'x'; 8192]).unwrap(),
        "timeout" => thread::sleep(Duration::from_secs(10)),
        _ => panic!("unknown fixture"),
    }
    stdout.flush().unwrap();
}

#[test]
fn bounded_windows_cli_output_and_deadline() {
    let executable = std::env::current_exe().unwrap();
    let make = |name| {
        let mut command = Command::new(&executable);
        command
            .args([
                "--exact",
                "developer_vm::cli::tests::pipe_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("SystemRoot", known_folder(&FOLDERID_Windows).unwrap())
            .env("MCP_LOCAL_RUST_VM_TEST_PIPE", name)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        command
    };
    let output = capture(&mut make("output"), Duration::from_secs(5), 4096).unwrap();
    assert!(
        output
            .windows(21)
            .any(|fragment| fragment == b"SYNTHETIC_PIPE_MARKER")
    );
    let error = capture(&mut make("overflow"), Duration::from_secs(5), 4096).unwrap_err();
    assert!(
        matches!(error, InvocationFailure::AfterSpawn(error) if error.to_string() == "VM_CLI_OUTPUT_LIMIT")
    );
    let start = Instant::now();
    let error = capture(&mut make("timeout"), Duration::from_millis(250), 4096).unwrap_err();
    assert!(
        matches!(error, InvocationFailure::AfterSpawn(error) if error.to_string() == "VM_CLI_TIMEOUT")
    );
    assert!(start.elapsed() < Duration::from_secs(7));
}

#[test]
#[ignore = "requires an installed production Windows Sandbox package; resolves its executable only"]
fn resolves_installed_microsoft_package_without_executing_it() {
    let cli = Cli::resolve().unwrap();
    assert!(
        cli.executable
            .file_name()
            .unwrap()
            .eq_ignore_ascii_case("wsb.exe")
    );
    let command = cli.command(&["list"]).unwrap();
    assert_eq!(command.get_program(), cli.executable.as_os_str());
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["list", "--raw"]);
    assert!(command.get_envs().all(|(name, value)| value.is_some()
        && !name.to_string_lossy().contains("MCP")
        && !name.to_string_lossy().contains("KEY")));
}

#[test]
#[ignore = "reads the official CLI inventory on the current Windows host without starting or stopping a Sandbox"]
fn official_cli_inventory_parses_without_mutating_sandbox_state() {
    let mut cli = Cli::resolve().unwrap();
    let current = cli.list().unwrap();
    println!("Windows Sandbox inventory count: {}", current.len());
}
