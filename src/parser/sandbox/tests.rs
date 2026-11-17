use std::{
    env,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    time::Duration,
};

use super::*;
use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::{ERROR_CHILD_PROCESS_BLOCKED, ERROR_INVALID_HANDLE},
    Networking::WinSock::{WSACleanup, WSADATA, WSAEACCES, WSASYSCALLFAILURE, WSAStartup},
    System::Threading::{CreateEventW, SetEvent},
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetErrorMode(mode: u32) -> u32;
}

const HANDLE_PROBE: &str = "probe: all isolation checks passed; testing unlisted handle";
const STATUS_INVALID_HANDLE: u32 = 0xC0000008;

#[derive(Serialize, Deserialize)]
struct Fixture {
    private_file: PathBuf,
    loopback: SocketAddr,
    canary_handle: usize,
}

#[test]
#[ignore = "invoked by the parser isolation test inside its worker"]
fn probe_worker() {
    // Suppress a blocking Windows error dialog in this test child only. Strict
    // handle checks remain enabled; their fatal rejection is checked below.
    unsafe { SetErrorMode(0x0001 | 0x0002) };
    println!("probe: reading fixture");
    let mut bytes = String::new();
    io::stdin().read_to_string(&mut bytes).unwrap();
    let fixture: Fixture = serde_json::from_str(&bytes).unwrap();
    println!("probe: checking token and files");
    verify_worker().unwrap();
    assert_eq!(
        File::open(&fixture.private_file).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        OpenOptions::new()
            .write(true)
            .open(&fixture.private_file)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let mut data = WSADATA::default();
    println!("probe: checking loopback");
    match unsafe { WSAStartup(0x0202, &mut data) } {
        0 => {
            assert_eq!(
                TcpStream::connect_timeout(&fixture.loopback, Duration::from_secs(1))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            unsafe { WSACleanup() };
        }
        WSAEACCES | WSASYSCALLFAILURE => {}
        status => panic!("unexpected Winsock startup status {status}"),
    }
    let exe = PathBuf::from(env::var_os("SystemRoot").unwrap())
        .join("System32")
        .join("cmd.exe");
    println!("probe: checking child process restriction");
    let blocked = Command::new(exe)
        .args(["/d", "/c", "exit", "0"])
        .status()
        .unwrap_err();
    assert_eq!(
        blocked.raw_os_error(),
        Some(ERROR_CHILD_PROCESS_BLOCKED as i32)
    );
    println!("{HANDLE_PROBE}");
    io::stdout().flush().unwrap();
    assert_eq!(unsafe { SetEvent(fixture.canary_handle as HANDLE) }, 0);
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(ERROR_INVALID_HANDLE as i32)
    );
    println!("probe: completed");
}

#[test]
fn isolated_worker_denies_host_files_loopback_child_processes_and_unlisted_handles() {
    let dir = tempfile::tempdir().unwrap();
    let private_file = dir.path().join("canary.txt");
    std::fs::write(&private_file, "synthetic marker").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let loopback = listener.local_addr().unwrap();
    let connection = TcpStream::connect(loopback).unwrap();
    listener.accept().unwrap();
    drop(connection);

    let event = unsafe { CreateEventW(null(), 1, 0, null()) };
    assert!(!event.is_null());
    let event = unsafe { OwnedHandle::from_raw_handle(event) };
    let event = inheritable(&event).unwrap();
    let mut profile = Profile::create().unwrap();
    let job = super::super::worker_job().unwrap();
    let (child, mut input, mut output) = launch(
        &profile,
        &env::current_exe().unwrap(),
        "--exact parser::sandbox::tests::probe_worker --ignored --nocapture",
        &job,
    )
    .unwrap();
    serde_json::to_writer(
        &mut input,
        &Fixture {
            private_file,
            loopback,
            canary_handle: event.as_raw_handle() as usize,
        },
    )
    .unwrap();
    drop(input);
    let waited = unsafe { WaitForSingleObject(child.0.as_raw_handle(), 10000) };
    if waited != WAIT_OBJECT_0 {
        let available = crate::windows_pipe::pending_bytes(&output)
            .unwrap()
            .unwrap_or(0);
        let mut progress = String::new();
        Read::by_ref(&mut output)
            .take(available as u64)
            .read_to_string(&mut progress)
            .unwrap();
        panic!("probe wait returned {waited}; progress: {progress}");
    }
    let mut stdout = String::new();
    output.read_to_string(&mut stdout).unwrap();
    let mut code = 0;
    assert_ne!(
        unsafe { GetExitCodeProcess(child.0.as_raw_handle(), &mut code) },
        0
    );
    assert!(stdout.contains(HANDLE_PROBE), "{stdout}");
    if code == STATUS_INVALID_HANDLE {
        // Windows may terminate an invalid-handle caller instead of returning
        // ERROR_INVALID_HANDLE. No other crash or earlier failure is accepted.
        assert!(!stdout.contains("probe: completed"), "{stdout}");
    } else {
        assert_eq!(code, 0, "{stdout}");
        assert!(
            stdout.contains("probe: completed") && stdout.contains("1 passed"),
            "{stdout}"
        );
    }
    assert_eq!(
        unsafe { WaitForSingleObject(event.as_raw_handle(), 0) },
        WAIT_TIMEOUT
    );
    drop(child);
    profile.close().unwrap();
}
