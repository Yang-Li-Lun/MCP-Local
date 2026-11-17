use std::{
    io::{Read, Write},
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use super::*;
use crate::security::{
    audit::Audit,
    authority::Authority,
    channel::{Lease, Owner, now, revision},
    policy::Policy,
};
use windows_sys::Win32::System::{
    JobObjects::{
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
    },
    Threading::CREATE_NO_WINDOW,
};

use crate::windows_job::Job;

#[test]
fn signed_host_actions_recheck_revocations_and_generation_before_dispatch() {
    let temp = tempfile::Builder::new()
        .prefix("mcp-host-auth-")
        .tempdir()
        .unwrap();
    let directory = temp.path().to_owned();
    let mut policy = Policy::default();
    policy.enabled = true;
    let mut authority = Authority::new(directory.clone());
    authority.save(policy, "").unwrap();
    let descriptor = authority.begin(Mode::HostControl).unwrap();
    let mut runtime = Runtime::attach(directory.clone(), &descriptor, Mode::HostControl).unwrap();
    let denied = runtime.prepare_host_action().err().unwrap();
    assert!(denied.to_string().contains("HOST_AUTH_REQUIRED"));
    authority.tick();
    authority
        .grant(1, authority.grant_target().unwrap())
        .unwrap();
    let permit = runtime.prepare_host_action().unwrap();
    let started = runtime.begin_host_session(permit).unwrap();
    let pending = runtime.prepare_host_action().unwrap();
    let input = runtime.prepare_host_action().unwrap();
    let file_action = runtime.prepare_host_action().unwrap();
    runtime.heartbeat();
    authority.tick();
    authority
        .grant(1, authority.grant_target().unwrap())
        .unwrap();
    assert!(
        runtime
            .begin_host_session(pending)
            .err()
            .unwrap()
            .to_string()
            .contains("HOST_AUTH_CHANGED")
    );
    assert!(runtime.authorize_host_input(input, &started).is_err());
    assert!(runtime.check_host_action(&file_action).is_err());
    assert!(!started.cancellation().is_cancelled());
    let input = runtime.prepare_host_action().unwrap();
    runtime.authorize_host_input(input, &started).unwrap();

    let pending = runtime.prepare_host_action().unwrap();
    authority.revoke().unwrap();
    runtime.heartbeat();
    assert!(started.cancellation().is_cancelled());
    assert!(runtime.begin_host_session(pending).is_err());
    authority.tick();
    authority
        .grant(1, authority.grant_target().unwrap())
        .unwrap();
    let input = runtime.prepare_host_action().unwrap();
    assert!(runtime.authorize_host_input(input, &started).is_err());

    authority.tick();
    let denial_count = Audit(directory)
        .load()
        .unwrap()
        .iter()
        .filter(|row| row.event.kind == Kind::CommandDenied)
        .count();
    assert_eq!(denial_count, 6);
    authority.close();
}

#[test]
fn expiry_terminates_or_finishes_existing_sessions_by_policy_and_fault_terminates_both() {
    for (expiry, should_cancel) in [
        (SessionExpiry::Terminate, true),
        (SessionExpiry::Finish, false),
    ] {
        let temp = tempfile::Builder::new()
            .prefix("mcp-host-expiry-")
            .tempdir()
            .unwrap();
        let mut policy = Policy::default();
        policy.enabled = true;
        policy.session_expiry = expiry;
        let mut owner = Owner::create().unwrap();
        let lease = Lease {
            generation: 1,
            deadline_ms: now() + 800,
        };
        let mut frame = Control {
            sequence: 0,
            time_ms: 0,
            revision: revision(&policy).unwrap(),
            policy,
            mode: Mode::HostControl,
            lease,
            recovery: 0,
            healthy: true,
            acks: Vec::new(),
        };
        owner.publish(&frame).unwrap();
        let mut runtime = Runtime::attach(
            temp.path().to_owned(),
            &owner.descriptor().unwrap(),
            Mode::HostControl,
        )
        .unwrap();
        let permit = runtime.prepare_host_action().unwrap();
        let session = runtime.begin_host_session(permit).unwrap();
        let input = runtime.prepare_host_action().unwrap();
        assert!(!session.cancellation().is_cancelled());
        let remaining = lease.deadline_ms.saturating_sub(now());
        thread::sleep(Duration::from_millis(remaining + 25));
        runtime.heartbeat();
        assert_eq!(session.cancellation().is_cancelled(), should_cancel);
        assert!(runtime.authorize_host_input(input, &session).is_err());
        assert!(runtime.prepare_host_action().is_err());
        frame.lease.revoke().unwrap();
        frame.healthy = false;
        owner.publish(&frame).unwrap();
        runtime.heartbeat();
        assert!(session.cancellation().is_cancelled());
    }
}

#[test]
fn disabled_lease_policy_requires_host_mode_and_runtime_drop_signals_shutdown() {
    let temp = tempfile::Builder::new()
        .prefix("mcp-host-owner-")
        .tempdir()
        .unwrap();
    let directory = temp.path().to_owned();
    let mut standalone = Runtime::open(directory.clone()).unwrap();
    assert!(standalone.prepare_host_action().is_err());
    let mut authority = Authority::new(directory.clone());
    let descriptor = authority.begin(Mode::ReadOnly).unwrap();
    let mut read_only = Runtime::attach(directory.clone(), &descriptor, Mode::ReadOnly).unwrap();
    assert!(read_only.prepare_host_action().is_err());
    drop(read_only);
    authority.close();
    let descriptor = authority.begin(Mode::HostControl).unwrap();
    let mut runtime = Runtime::attach(directory, &descriptor, Mode::HostControl).unwrap();
    let permit = runtime.prepare_host_action().unwrap();
    let session = runtime.begin_host_session(permit).unwrap();
    assert!(!session.cancellation().is_cancelled());
    drop(runtime);
    assert!(session.cancellation().is_cancelled());
    authority.close();
}

#[tokio::test]
async fn closing_stdio_revokes_queued_authority_even_while_runtime_is_retained() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use std::sync::Mutex;

    let temp = tempfile::Builder::new()
        .prefix("mcp-host-stdio-close-")
        .tempdir()
        .unwrap();
    let directory = temp.path().to_owned();
    let mut authority = Authority::new(directory.clone());
    let descriptor = authority.begin(Mode::HostControl).unwrap();
    let mut runtime = Runtime::attach(directory, &descriptor, Mode::HostControl).unwrap();
    let started = runtime.prepare_host_action().unwrap();
    let session = runtime.begin_host_session(started).unwrap();
    let queued = runtime.prepare_host_action().unwrap();
    let shared = Arc::new(Mutex::new(runtime));
    let (_peer, stream) = tokio::io::duplex(128);
    let (read, write) = tokio::io::split(stream);
    let inner = AsyncRwTransport::<rmcp::RoleServer, _, _>::new(read, write);
    let transport = crate::security::GuardedTransport::shared(inner, shared.clone());
    drop(transport);
    assert!(session.cancellation().is_cancelled());
    let mut runtime = shared.lock().unwrap();
    assert!(runtime.begin_host_session(queued).is_err());
    assert!(runtime.prepare_host_action().is_err());
}

#[test]
#[ignore = "subprocess fixture for the bounded Windows Job, waits on test stdin only"]
fn job_fixture_waits_for_test_owner() {
    let mut marker = [0; 1];
    let mut stdin = std::io::stdin().lock();
    stdin.read_exact(&mut marker).unwrap();
    assert_eq!(marker, [b'R']);
    assert_eq!(stdin.read(&mut marker).unwrap(), 0);
}

#[test]
fn revoked_grant_can_stop_only_its_owned_bounded_windows_job() {
    let temp = tempfile::Builder::new()
        .prefix("mcp-host-job-")
        .tempdir()
        .unwrap();
    let directory = temp.path().to_owned();
    let mut policy = Policy::default();
    policy.enabled = true;
    let mut authority = Authority::new(directory.clone());
    authority.save(policy, "").unwrap();
    let descriptor = authority.begin(Mode::HostControl).unwrap();
    let mut runtime = Runtime::attach(directory.clone(), &descriptor, Mode::HostControl).unwrap();
    authority.tick();
    authority
        .grant(1, authority.grant_target().unwrap())
        .unwrap();
    let permit = runtime.prepare_host_action().unwrap();
    let job = Job::host_session().unwrap();
    let limits = job.limits().unwrap();
    let flags = limits.BasicLimitInformation.LimitFlags;
    assert_eq!(
        flags,
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_JOB_MEMORY
            | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    );
    assert_eq!(
        flags & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK),
        0
    );
    assert_eq!(limits.BasicLimitInformation.ActiveProcessLimit, 32);
    assert_eq!(limits.JobMemoryLimit, 4 * 1024 * 1024 * 1024);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "security::host::tests::job_fixture_waits_for_test_owner",
        ])
        .env_clear()
        .env("SystemRoot", std::env::var_os("SystemRoot").unwrap())
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .unwrap();
    job.assign(&child).unwrap();
    let session = runtime.begin_host_session(permit).unwrap();
    child.stdin.as_mut().unwrap().write_all(b"R").unwrap();
    assert!(
        (1..=limits.BasicLimitInformation.ActiveProcessLimit)
            .contains(&job.active_processes().unwrap())
    );
    assert!(child.try_wait().unwrap().is_none());
    authority.revoke().unwrap();
    runtime.heartbeat();
    assert!(session.cancellation().is_cancelled());
    job.terminate().unwrap();
    assert_eq!(job.active_processes().unwrap(), 0);
    assert_eq!(child.wait().unwrap().code(), Some(1));
}
