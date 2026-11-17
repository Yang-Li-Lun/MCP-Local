use super::*;
use crate::{
    filesystem::Root,
    security::{Runtime, authority::Authority, policy::Policy},
    settings::ReaderSettings,
};

struct Fixture {
    sessions: Sessions,
    authority: Arc<Mutex<Authority>>,
    pulse: CancellationToken,
    temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("mcp-host-offline-")
            .tempdir()
            .unwrap();
        let state = temp.path().join("state");
        std::fs::create_dir(temp.path().join("shared")).unwrap();
        let mut authority = Authority::new(state.clone());
        let mut policy = Policy::default();
        policy.enabled = true;
        authority.save(policy, "").unwrap();
        let descriptor = authority.begin(Mode::HostControl).unwrap();
        let security = Arc::new(Mutex::new(
            Runtime::attach(state, &descriptor, Mode::HostControl).unwrap(),
        ));
        authority.tick();
        authority
            .grant(1, authority.grant_target().unwrap())
            .unwrap();
        let authority = Arc::new(Mutex::new(authority));
        let sessions = Sessions::new(security).unwrap();
        let pulse = CancellationToken::new();
        let running = pulse.clone();
        let owner = authority.clone();
        thread::spawn(move || {
            while !running.is_cancelled() {
                owner.lock().unwrap().tick();
                thread::sleep(Duration::from_millis(30));
            }
        });
        Self {
            sessions,
            authority,
            pulse,
            temp,
        }
    }

    fn request(&self, command: &str, lifetime: u64) -> Start {
        let root = Root::new(&self.temp.path().join("shared"), ReaderSettings::default()).unwrap();
        let (cwd, pin) = root.host_working_directory(".").unwrap();
        Start {
            command: Zeroizing::new(command.into()),
            cwd,
            pin,
            root_id: "main".into(),
            lifetime: Duration::from_secs(lifetime),
        }
    }

    async fn start(&self, command: &str, lifetime: u64) -> Output {
        self.sessions
            .start(
                self.request(command, lifetime),
                Duration::ZERO,
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.sessions.cancel.cancel();
        self.pulse.cancel();
        self.authority.lock().unwrap().close();
    }
}

#[tokio::test]
async fn native_host_output_input_export_location_and_unicode_offsets() {
    let f = Fixture::new();
    let started = f.start("[Console]::WriteLine('界🙂'); $line = [Console]::ReadLine(); [Console]::WriteLine($line); [IO.File]::WriteAllText((Join-Path (Get-Location) 'out.txt'), 'fixture')", 10).await;
    let id = started.value["session_id"].as_str().unwrap();
    assert_eq!(started.value["sandbox"], "host_process");
    assert_eq!(
        started.value["host_files_access"],
        "current_user_trusted_commands"
    );
    assert_eq!(started.value["security_output"], "withheld_until_complete");
    assert!(f.sessions.export_location(id).is_err());
    let receipt = f
        .sessions
        .interact(
            id,
            Zeroizing::new("offline\n".into()),
            true,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(receipt["bytes_sent"], 8);
    let result = f
        .sessions
        .read(id, 0, 32000, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(result.value["completed"], true, "{}", result.value);
    assert_eq!(result.value["job_stopped"], true);
    assert_eq!(result.value["exit_code"], 0);
    assert!(
        result.value["output"]
            .as_str()
            .unwrap()
            .contains("界🙂\r\noffline"),
        "{}",
        result.value
    );
    assert_eq!(
        f.sessions
            .read(id, 1, 1, Duration::ZERO)
            .await
            .unwrap()
            .value["output"],
        "🙂"
    );
    assert!(f.sessions.export_location(id).is_ok());
    assert!(
        f.sessions
            .interact(
                id,
                Zeroizing::new("x".into()),
                false,
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(f.sessions.close(id).await.unwrap()["closed"], true);
    assert!(f.sessions.read(id, 0, 1, Duration::ZERO).await.is_err());
    f.sessions.shutdown().await.unwrap();
}

#[tokio::test]
async fn powershell_errors_return_text_and_a_failure_exit_code() {
    let f = Fixture::new();
    let started = f.start("Write-Error 'synthetic offline failure'", 5).await;
    let id = started.value["session_id"].as_str().unwrap();
    let output = f
        .sessions
        .read(id, 0, 32000, Duration::from_secs(3))
        .await
        .unwrap()
        .value;
    assert_eq!(output["exit_code"], 1, "{output}");
    assert_eq!(output["job_stopped"], true);
    assert!(
        output["output"]
            .as_str()
            .unwrap()
            .contains("synthetic offline failure")
    );
    assert!(!output["output"].as_str().unwrap().contains("CLIXML"));
    f.sessions.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_host_revocation_timeout_cancellation_and_output_limit_stop_jobs() {
    let f = Fixture::new();
    let started = f.start("Start-Sleep -Seconds 30", 1).await;
    let id = started.value["session_id"].as_str().unwrap();
    let result = f
        .sessions
        .read(id, 0, 100, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(result.value["reason"], "lifetime_limit");
    assert_eq!(result.value["job_stopped"], true);
    assert_eq!(
        f.sessions.terminate(id).await.unwrap().value["reason"],
        "terminated_by_client"
    );
    f.sessions.close(id).await.unwrap();

    let started = f.start("Start-Sleep -Seconds 30", 30).await;
    let id = started.value["session_id"].as_str().unwrap();
    let stopped = f.sessions.terminate(id).await.unwrap().value;
    assert_eq!(stopped["reason"], "terminated_by_client");
    assert_eq!(stopped["job_stopped"], true);
    f.sessions.close(id).await.unwrap();

    let started = f.start("Start-Sleep -Seconds 30", 30).await;
    let id = started.value["session_id"].as_str().unwrap();
    assert!(
        f.sessions
            .start(
                f.request("Write-Output 'never'", 5),
                Duration::ZERO,
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    f.authority.lock().unwrap().revoke().unwrap();
    let result = f
        .sessions
        .read(id, 0, 100, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(result.value["reason"], "HOST_CANCELLED");
    assert_eq!(result.value["job_stopped"], true);
    assert!(
        f.sessions
            .start(
                f.request("Write-Output 'never'", 5),
                Duration::ZERO,
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(
        f.sessions
            .interact(
                id,
                Zeroizing::new("denied".into()),
                false,
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    f.sessions.close(id).await.unwrap();
    {
        let mut owner = f.authority.lock().unwrap();
        owner.tick();
        let target = owner.grant_target().unwrap();
        owner.grant(1, target).unwrap();
    }
    let token = CancellationToken::new();
    token.cancel();
    assert!(
        f.sessions
            .start(
                f.request("Start-Sleep -Seconds 30", 30),
                Duration::from_secs(3),
                token
            )
            .await
            .is_err()
    );
    f.sessions.shutdown().await.unwrap();

    let f = Fixture::new();
    let started = f.start("[Console]::Write(('x' * 1100000))", 10).await;
    let id = started.value["session_id"].as_str().unwrap();
    let result = f
        .sessions
        .read(id, 0, 100, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(
        result.value["reason"], "HOST_OUTPUT_LIMIT",
        "{}",
        result.value
    );
    assert_eq!(result.value["job_stopped"], true);
    assert_eq!(result.value["security_output"], "blocked_output_limit");
    assert!(!result.value["output"].as_str().unwrap().contains("xxxx"));
    f.sessions.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_host_masks_complete_output_before_paging_and_channel_loss_kills() {
    let f = Fixture::new();
    let started = f
        .start(
            "[Console]::Write('Bearer synthetic_fixture_token_123456 visible')",
            10,
        )
        .await;
    let id = started.value["session_id"].as_str().unwrap();
    let result = f
        .sessions
        .read(id, 8, 4, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(
        result.value["security_output"], "masked",
        "{}",
        result.value
    );
    assert!(!result.value.to_string().contains("ynth"));
    f.sessions.close(id).await.unwrap();
    let started = f.start("Start-Sleep -Seconds 30", 30).await;
    let id = started.value["session_id"].as_str().unwrap();
    f.pulse.cancel();
    f.authority.lock().unwrap().close();
    wait(&f.sessions.lookup(id).unwrap(), Duration::from_secs(3)).await;
    assert!(f.sessions.lookup(id).unwrap().state.lock().unwrap().stopped);
    f.sessions.shutdown().await.unwrap();
    assert!(f.sessions.lookup(id).unwrap().state.lock().unwrap().stopped);
    assert!(f.sessions.authorize().is_err());
}

#[tokio::test]
async fn suspended_host_cannot_execute_before_final_grant_and_children_share_the_job() {
    let f = Fixture::new();
    let request = f.request(
        "[IO.File]::WriteAllText((Join-Path (Get-Location) 'must-not-run.txt'), 'unexpected')",
        5,
    );
    let permit = f
        .sessions
        .security
        .lock()
        .unwrap()
        .prepare_host_action()
        .unwrap();
    let process = native::Process::prepare(&request.command, &request.cwd).unwrap();
    assert_eq!(process.job.active_processes().unwrap(), 1);
    assert!(process.poll().unwrap().is_none());
    assert!(!request.cwd.join("must-not-run.txt").exists());
    f.authority.lock().unwrap().revoke().unwrap();
    assert!(
        f.sessions
            .security
            .lock()
            .unwrap()
            .begin_host_session(permit)
            .is_err()
    );
    process.job.terminate().unwrap();
    assert_eq!(process.job.active_processes().unwrap(), 0);
    assert!(!request.cwd.join("must-not-run.txt").exists());
    drop(process);
    drop(request);
    f.sessions.shutdown().await.unwrap();

    let f = Fixture::new();
    let result = f.start("$s = [Diagnostics.ProcessStartInfo]::new(); $s.FileName = Join-Path $PSHOME 'powershell.exe'; $s.Arguments = '-NoLogo -NoProfile -NonInteractive -Command \"Start-Sleep -Seconds 30\"'; $s.UseShellExecute = $false; $s.CreateNoWindow = $true; $p = [Diagnostics.Process]::Start($s); [Console]::WriteLine($p.Id)", 10).await;
    let id = result.value["session_id"].as_str().unwrap();
    let result = f
        .sessions
        .read(id, 0, 100, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(result.value["exit_code"], 0, "{}", result.value);
    assert_eq!(result.value["job_stopped"], true);
    assert!(
        result.value["output"]
            .as_str()
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap_or_else(|error| panic!("child pid: {error}: {}", result.value))
            > 0
    );
    f.sessions.shutdown().await.unwrap();
}

#[tokio::test]
async fn blocked_stdin_is_bounded_and_cancels_the_owned_job() {
    let f = Fixture::new();
    let result = f.start("Start-Sleep -Seconds 30", 10).await;
    let id = result.value["session_id"].as_str().unwrap();
    let mut refused = false;
    for _ in 0..16 {
        if f.sessions
            .interact(
                id,
                Zeroizing::new("x".repeat(4096)),
                false,
                CancellationToken::new(),
            )
            .await
            .is_err()
        {
            refused = true;
            break;
        }
    }
    assert!(
        refused,
        "bounded stdin must stop accepting input when the child does not read"
    );
    wait(&f.sessions.lookup(id).unwrap(), Duration::from_secs(3)).await;
    assert!(f.sessions.lookup(id).unwrap().state.lock().unwrap().stopped);
    f.sessions.shutdown().await.unwrap();
}
