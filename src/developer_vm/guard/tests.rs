use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};

use super::*;

mod exchange;

const FIXTURE_ENGINE: &str = "MCP_LOCAL_RUST_VM_GUARD_ENGINE_FIXTURE";
const FIXTURE_MODE: &str = "MCP_LOCAL_RUST_VM_GUARD_MODE_FIXTURE";
const APP_BINARY: &str = "MCP_LOCAL_RUST_TEST_APP_BINARY";
const FIXTURE_COMMAND: &str = "synthetic fixture command";

struct FixtureEngine {
    journal: PathBuf,
    wait_for_cancel: bool,
    fail_first_stop: bool,
    active: bool,
    deadline: Option<Instant>,
}

impl FixtureEngine {
    fn record(&self, bytes: &[u8]) -> Result<()> {
        let mut journal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal)?;
        journal.write_all(bytes)?;
        Ok(())
    }
}

impl Engine for FixtureEngine {
    fn prepare(
        &mut self,
        _: Zeroizing<String>,
        _: Option<&str>,
        _: Option<&str>,
        _: &CancellationToken,
    ) -> Result<Mappings> {
        ensure!(!self.active, "fixture is already active");
        Ok(Mappings {
            guest_roots: Default::default(),
            guest_toolchains: Vec::new(),
        })
    }

    fn start(&mut self, deadline: Instant, cancel: &CancellationToken) -> Result<Uuid> {
        ensure!(!self.active, "fixture is already active");
        self.active = true;
        self.deadline = Some(deadline);
        self.record(b"start\n")?;
        if self.wait_for_cancel {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !cancel.is_cancelled() {
                ensure!(Instant::now() < deadline, "fixture cancellation deadline");
                thread::sleep(POLL);
            }
            self.record(b"cancel\n")?;
            bail!("fixture start cancelled");
        }
        Ok(Uuid::from_u128(42))
    }

    fn poll(&mut self, _: usize, _: &CancellationToken) -> Result<Progress> {
        bail!("fixture has no output")
    }

    fn input(&mut self, _: &[u8], _: bool, _: &CancellationToken) -> Result<usize> {
        bail!("fixture has no input")
    }

    fn terminate(&mut self, _: &CancellationToken) -> Result<()> {
        self.stop()
    }

    fn stop(&mut self) -> Result<()> {
        if self.active {
            if std::mem::take(&mut self.fail_first_stop) {
                self.record(b"unconfirmed\n")?;
                bail!("fixture stop unconfirmed");
            }
            if self.wait_for_cancel {
                thread::sleep(Duration::from_millis(400));
            }
            self.record(b"stop\n")?;
            self.active = false;
            self.deadline = None;
        }
        Ok(())
    }

    fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn session_id(&self) -> Option<Uuid> {
        self.active.then(|| Uuid::from_u128(42))
    }
}

#[test]
#[ignore = "runs as an isolated synthetic guard child"]
fn guard_fixture_process() {
    let (boot, owner) = watch_owner().unwrap();
    let mode = std::env::var(FIXTURE_MODE).ok();
    let engine = FixtureEngine {
        journal: std::env::var_os(FIXTURE_ENGINE).unwrap().into(),
        wait_for_cancel: mode.as_deref() == Some("wait"),
        fail_first_stop: mode.as_deref() == Some("unconfirmed"),
        active: false,
        deadline: None,
    };
    run(boot.descriptor, owner, engine, write_marker).unwrap();
}

#[test]
#[ignore = "runs as an isolated synthetic service child"]
fn service_fixture_process() {
    let mut encoded = Zeroizing::new(String::new());
    io::stdin().take(2048).read_to_string(&mut encoded).unwrap();
    runtime().block_on(async {
        let mut client = GuardClient::connect(&encoded).await.unwrap();
        let result = client.start(FIXTURE_COMMAND, None, None, 900).await;
        if std::env::var(FIXTURE_MODE).is_ok_and(|mode| mode == "wait") {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), Uuid::from_u128(42));
            // The parent removes this process through its fixture Job.
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    });
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn child_command(test: &str, mode: Option<&str>) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--ignored", "--nocapture"])
        .env_clear()
        .env("SystemRoot", known_folder(&FOLDERID_Windows).unwrap());
    if let Some(mode) = mode {
        command.env(FIXTURE_MODE, mode);
    }
    command
}

fn owner(directory: &Path, mode: Option<&str>) -> GuardOwner {
    let mut command = child_command("developer_vm::guard::tests::guard_fixture_process", mode);
    command.env(FIXTURE_ENGINE, directory.join("events.txt"));
    GuardOwner::with_command(
        command,
        ReaderSettings::default(),
        WorkspaceSettings::single(directory.to_owned()),
    )
    .unwrap()
}

fn spawn_service(delayed: bool) -> Child {
    child_command(
        "developer_vm::guard::tests::service_fixture_process",
        delayed.then_some("wait"),
    )
    .creation_flags(CREATE_NO_WINDOW)
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap()
}

fn boot_service(service: &mut Child, guard: &GuardOwner) {
    service
        .stdin
        .take()
        .unwrap()
        .write_all(guard.descriptor().unwrap().as_bytes())
        .unwrap();
}

fn wait_for_event(journal: &Path, expected: &str, service: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if fs::read_to_string(journal).is_ok_and(|events| events.contains(expected)) {
            return;
        }
        assert!(
            service.try_wait().unwrap().is_none(),
            "service exited before {expected}"
        );
        assert!(Instant::now() < deadline, "missing {expected}");
        thread::sleep(POLL);
    }
}

pub(in crate::developer_vm) fn job(child: &Child) -> OwnedHandle {
    let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    assert!(!raw.is_null());
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut config = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    config.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let set = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&config as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    assert_ne!(set, 0);
    assert_ne!(
        unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) },
        0
    );
    job
}

#[test]
fn service_job_can_die_before_owner_without_killing_its_independent_guard() {
    let directory = tempfile::Builder::new()
        .prefix("mcp-vm-guard-job-")
        .tempdir()
        .unwrap();
    let journal = directory.path().join("events.txt");
    let mut owner = owner(directory.path(), None);
    assert!(!in_job(owner.child.as_raw_handle()).unwrap());
    let mut service = spawn_service(false);
    let service_job = job(&service);
    boot_service(&mut service, &owner);
    wait_for_event(&journal, "start\n", &mut service);
    assert_ne!(
        unsafe { TerminateJobObject(service_job.as_raw_handle(), 12) },
        0
    );
    assert_eq!(service.wait().unwrap().code(), Some(12));
    let deadline = Instant::now() + Duration::from_secs(8);
    while fs::read_to_string(&journal).unwrap() != "start\nstop\n" {
        assert!(
            owner.child.try_wait().unwrap().is_none(),
            "guard died before cleaning up"
        );
        assert!(
            Instant::now() < deadline,
            "guard did not stop after service EOF"
        );
        thread::sleep(POLL);
    }
    assert!(
        owner.child.try_wait().unwrap().is_none(),
        "guard must still be watching its owner"
    );
    owner.close_within(Duration::from_secs(5)).unwrap();
}

#[test]
fn owner_eof_cancels_start_and_timeout_does_not_kill_guard() {
    let directory = tempfile::Builder::new()
        .prefix("mcp-vm-guard-eof-")
        .tempdir()
        .unwrap();
    let journal = directory.path().join("events.txt");
    let mut owner = owner(directory.path(), Some("wait"));
    let pid = owner.child.id();
    let mut service = spawn_service(true);
    let _job = job(&service);
    boot_service(&mut service, &owner);
    wait_for_event(&journal, "start\n", &mut service);
    assert!(owner.close_within(Duration::from_millis(50)).is_err());
    assert_eq!(owner.child.id(), pid);
    assert!(owner.child.try_wait().unwrap().is_none());
    owner.close_within(Duration::from_secs(5)).unwrap();
    assert!(service.wait().unwrap().success());
    assert_eq!(
        fs::read_to_string(&journal).unwrap(),
        "start\ncancel\nstop\n"
    );
}

#[test]
fn unconfirmed_cleanup_keeps_ownership_until_the_owner_retries_shutdown() {
    let directory = tempfile::Builder::new()
        .prefix("mcp-vm-guard-retry-")
        .tempdir()
        .unwrap();
    let journal = directory.path().join("events.txt");
    let mut owner = owner(directory.path(), Some("unconfirmed"));
    let encoded = owner.descriptor().unwrap();
    runtime().block_on(async {
        let mut client = GuardClient::connect(&encoded).await.unwrap();
        assert_eq!(
            client
                .start(FIXTURE_COMMAND, None, None, 900)
                .await
                .unwrap(),
            Uuid::from_u128(42)
        );
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    while fs::read_to_string(&journal).unwrap() != "start\nunconfirmed\n" {
        assert!(owner.child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "missing unconfirmed stop");
        thread::sleep(POLL);
    }
    assert!(runtime().block_on(GuardClient::connect(&encoded)).is_err());
    assert!(owner.child.try_wait().unwrap().is_none());
    owner.close_within(Duration::from_secs(5)).unwrap();
    assert_eq!(
        fs::read_to_string(&journal).unwrap(),
        "start\nunconfirmed\nstop\n"
    );
}

#[test]
fn deadline_tears_down_the_guest_while_its_service_connection_is_idle() {
    let directory = tempfile::Builder::new()
        .prefix("mcp-vm-guard-deadline-")
        .tempdir()
        .unwrap();
    let journal = directory.path().join("events.txt");
    let mut owner = owner(directory.path(), None);
    let encoded = owner.descriptor().unwrap();
    runtime().block_on(async {
        let mut client = GuardClient::connect(&encoded).await.unwrap();
        assert!(client.start(FIXTURE_COMMAND, None, None, 0).await.is_err());
        assert!(!journal.exists());
        assert_eq!(
            client.start(FIXTURE_COMMAND, None, None, 1).await.unwrap(),
            Uuid::from_u128(42)
        );
        timeout(Duration::from_secs(5), async {
            loop {
                if fs::read_to_string(&journal).is_ok_and(|events| events == "start\nstop\n") {
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .unwrap();
        assert!(client.stop(Some(Uuid::from_u128(42))).await.is_err());
    });
    assert!(owner.child.try_wait().unwrap().is_none());
    owner.close_within(Duration::from_secs(5)).unwrap();
}

#[test]
fn framed_authentication_rejects_wrong_keys_replays_reflection_and_extra_fields() {
    runtime().block_on(async {
        let auth = Auth {
            key: &[7; 32],
            nonce: [8; 32],
        };
        let other_key = Auth {
            key: &[6; 32],
            nonce: [8; 32],
        };
        let other_nonce = Auth {
            key: &[7; 32],
            nonce: [9; 32],
        };
        for (reader, direction, sequence) in [
            (&other_key, Direction::Request, 7),
            (&other_nonce, Direction::Request, 7),
            (&auth, Direction::Request, 8),
            (&auth, Direction::Reply, 7),
        ] {
            let (mut sender, mut receiver) = tokio::io::duplex(256);
            let request = Request::Prepare {
                command: FIXTURE_COMMAND.into(),
                root_id: None,
                working_directory: None,
            };
            auth.write(&mut sender, Direction::Request, 7, &request)
                .await
                .unwrap();
            assert!(
                reader
                    .read::<Request>(&mut receiver, direction, sequence)
                    .await
                    .is_err()
            );
        }
        let (mut sender, mut receiver) = tokio::io::duplex(256);
        auth.write(
            &mut sender,
            Direction::Request,
            7,
            &serde_json::json!({"op":"start","command":FIXTURE_COMMAND,"lifetime_seconds":900,"configuration":"invalid"}),
        )
        .await
        .unwrap();
        assert!(
            auth.read::<Request>(&mut receiver, Direction::Request, 7)
                .await
                .is_err()
        );
    });
}

#[test]
#[ignore = "requires the built app executable and installed official CLI; opens an idle guard only"]
fn native_worker_authenticates_and_exits_without_starting_a_sandbox() {
    let directory = tempfile::Builder::new()
        .prefix("mcp-idle-vm-guard-")
        .tempdir()
        .unwrap();
    let executable = PathBuf::from(
        std::env::var_os(APP_BINARY).expect("provide the freshly built app executable"),
    );
    assert!(executable.is_absolute() && executable.file_name().unwrap() == "mcp-local-rust.exe");
    let windows = known_folder(&FOLDERID_Windows).unwrap();
    let mut command = Command::new(executable);
    command
        .arg(WORKER)
        .env_clear()
        .env("SystemRoot", windows)
        .env(
            "LOCALAPPDATA",
            known_folder(&FOLDERID_LocalAppData).unwrap(),
        )
        .env("USERPROFILE", known_folder(&FOLDERID_Profile).unwrap());
    let mut owner = GuardOwner::with_command(
        command,
        ReaderSettings::default(),
        WorkspaceSettings::single(directory.path().into()),
    )
    .unwrap();
    let encoded = owner.descriptor().unwrap();
    runtime().block_on(async {
        let mut client = GuardClient::connect(&encoded).await.unwrap();
        // No owned ID exists, so the native lifecycle has no CLI call to make.
        client.stop(None).await.unwrap();
    });
    owner.close_within(Duration::from_secs(5)).unwrap();
}
