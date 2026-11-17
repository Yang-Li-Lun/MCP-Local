#![cfg_attr(
    all(windows, not(debug_assertions), feature = "desktop"),
    windows_subsystem = "windows"
)]

mod archives;
mod budget;
mod build_identity;
#[cfg(feature = "desktop")]
mod connection;
#[cfg(feature = "desktop")]
mod connection_cli;
mod control_files;
#[cfg(feature = "desktop")]
mod desktop;
#[cfg(feature = "desktop")]
mod desktop_store;
#[allow(
    dead_code,
    reason = "The shared VM module also exposes internal verification helpers"
)]
mod developer_vm;
mod documents;
mod filesystem;
mod formats;
mod host_control;
mod images;
mod incremental;
mod listing;
mod local_mapping;
mod media;
#[cfg(feature = "desktop")]
mod monitor;
#[cfg(feature = "desktop")]
mod native_tray;
mod parser;
#[cfg(feature = "desktop")]
mod power_recovery;
mod security;
mod server;
mod settings;
#[cfg(feature = "desktop")]
mod startup;
mod text;
#[cfg(feature = "desktop")]
mod vendor;
mod windows_job;
mod windows_pipe;
mod windows_token;

use std::{env, path::PathBuf};

use anyhow::{Result, bail, ensure};
use security::channel::Mode;
use settings::{ReaderSettings, WorkspaceSettings, decode};
use zeroize::Zeroizing;

fn main() {
    let _ = build_identity::current();
    if let Err(error) = run() {
        eprintln!("啟動失敗：{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut arguments = env::args_os().skip(1).peekable();
    #[cfg(feature = "desktop")]
    if arguments.len() == 2
        && arguments
            .peek()
            .is_some_and(|arg| arg == "--recover-legacy-power")
    {
        return power_recovery::recover(&PathBuf::from(arguments.nth(1).unwrap()));
    }
    if arguments.len() == 1 && arguments.peek().is_some_and(|arg| arg == parser::WORKER) {
        return parser::worker();
    }
    if arguments.len() == 1
        && arguments
            .peek()
            .is_some_and(|arg| arg == developer_vm::guard::WORKER)
    {
        return developer_vm::guard::worker();
    }
    #[cfg(feature = "desktop")]
    {
        if arguments
            .peek()
            .is_some_and(|arg| arg == "--connect" || arg == "--set-access-mode")
        {
            return connection_cli::run(arguments.collect());
        }
        if arguments.len() == 0 {
            return desktop::run(None, false);
        }
        if arguments.len() == 1
            && arguments
                .peek()
                .is_some_and(|arg| arg == connection::WORKER)
        {
            let status = connection::worker()?;
            std::process::exit(status);
        }
        if arguments.len() == 1 && arguments.peek().is_some_and(|arg| arg == startup::ARGUMENT) {
            return desktop::run(None, true);
        }
        if arguments.len() == 1 && arguments.peek().is_some_and(|arg| arg == "--verify-vendor") {
            vendor::Vendor::open(&env::current_exe()?)?;
            return Ok(());
        }
        if arguments.len() == 2
            && arguments
                .peek()
                .is_some_and(|arg| arg == "--import-settings")
        {
            let path = PathBuf::from(arguments.nth(1).unwrap());
            return desktop::run(Some(&path), false);
        }
    }
    let mut root = None;
    let mut workspace = None;
    let mut reader = None;
    let mut mode = None;
    let mut require_security_channel = false;
    let mut require_developer_guard = false;
    while let Some(argument) = arguments.next() {
        let argument = argument
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("啟動參數無效。"))?;
        if argument == "--help" || argument == "-h" {
            println!(
                "MCP-Local Rust --root <資料夾> [--reader-settings <Base64>] [--workspace-settings <Base64>] [--access-mode read_only|developer_control|host_control]\n控制模式需要本機簽署的安全連線；主機命令另依安全政策核對本機授權。\n--connect [--access-mode <模式>]：以前景 CLI 啟動已儲存的 Rust 通道\n--set-access-mode <模式>：保存本機模式\n--import-settings <快照副本>：載入一般設定草稿\n--recover-legacy-power <power-runtime.json>：明確回復舊電源交易（會修改系統電源）"
            );
            return Ok(());
        }
        if argument == "--require-security-channel" {
            ensure!(!require_security_channel, "啟動參數重複。");
            require_security_channel = true;
            continue;
        }
        if argument == developer_vm::guard::REQUIRE_DELEGATED {
            ensure!(!require_developer_guard, "啟動參數重複。");
            require_developer_guard = true;
            continue;
        }
        let target = match argument {
            "--root" => &mut root,
            "--workspace-settings" => &mut workspace,
            "--reader-settings" => &mut reader,
            "--access-mode" => &mut mode,
            _ => bail!("不支援的啟動參數。"),
        };
        ensure!(target.is_none(), "啟動參數重複。");
        *target = Some(
            arguments
                .next()
                .ok_or_else(|| anyhow::anyhow!("啟動參數缺少值。"))?,
        );
    }
    let mode = match mode.as_deref() {
        None => Mode::ReadOnly,
        Some(value) if value == "read_only" => Mode::ReadOnly,
        Some(value) if value == "developer_control" => Mode::DeveloperControl,
        Some(value) if value == "host_control" => Mode::HostControl,
        _ => bail!("不支援的權限模式。"),
    };
    let descriptor = match env::var(security::channel::ENV_NAME) {
        Ok(value) => Some(Zeroizing::new(value)),
        Err(env::VarError::NotPresent) => None,
        Err(_) => bail!("SECURITY_CHANNEL_DESCRIPTOR"),
    };
    let delegated_guard = if require_developer_guard {
        ensure!(mode == Mode::DeveloperControl, "VM_GUARD_MODE_MISMATCH");
        let value = env::var(developer_vm::guard::ENV_NAME)
            .map_err(|_| anyhow::anyhow!("VM_GUARD_UNAVAILABLE：缺少桌面授權的清理程序。"))?;
        ensure!(
            (1..=developer_vm::guard::DESCRIPTOR_LIMIT).contains(&value.len()),
            "VM_GUARD_PROTOCOL"
        );
        Some(Zeroizing::new(value))
    } else {
        None
    };
    ensure!(
        (!require_security_channel && mode == Mode::ReadOnly) || descriptor.is_some(),
        "SECURITY_CHANNEL_UNCONFIRMED：缺少本機安全連線。"
    );
    // This runs before constructing the Tokio runtime. Windows serializes environment changes.
    unsafe {
        env::remove_var("CONTROL_PLANE_API_KEY");
        env::remove_var(security::channel::ENV_NAME);
        env::remove_var(developer_vm::guard::ENV_NAME);
    }
    let settings = match reader {
        Some(value) => decode::<ReaderSettings>(
            value
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("設定快照無效。"))?,
            16000,
        )?,
        None => ReaderSettings::default(),
    };
    let workspace = match workspace {
        Some(value) => decode::<WorkspaceSettings>(
            value
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("設定快照無效。"))?,
            65536,
        )?,
        None => {
            let mut path = match root {
                Some(path) => PathBuf::from(path),
                None => env::current_exe()?.with_file_name("shared"),
            };
            if !path.is_absolute() {
                path = std::path::absolute(path)?;
            }
            WorkspaceSettings::single(path)
        }
    };
    let directory = security::state_directory()?;
    let mut security = match descriptor {
        Some(descriptor) => security::Runtime::attach(directory, &descriptor, mode)?,
        None => security::Runtime::open(directory)?,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    if mode == Mode::ReadOnly {
        let service = server::Server::new(server::Workspace::new(settings, workspace)?);
        return runtime.block_on(server::stdio(service, security));
    }
    if mode == Mode::HostControl {
        let security = std::sync::Arc::new(std::sync::Mutex::new(security));
        let service = server::Server::host(settings, workspace, security.clone())?;
        return runtime.block_on(server::shared_stdio(service, security));
    }
    security.require_mode(Mode::DeveloperControl)?;
    let mut guard = None;
    let result = runtime.block_on(async {
        let descriptor = match delegated_guard {
            Some(descriptor) => descriptor,
            None => guard
                .insert(developer_vm::guard::GuardOwner::spawn(
                    settings.clone(),
                    workspace.clone(),
                )?)
                .descriptor()?,
        };
        let sessions =
            developer_vm::sessions::Sessions::connect_workspace(&descriptor, &settings, &workspace)
                .await?;
        drop(descriptor);
        let security = std::sync::Arc::new(std::sync::Mutex::new(security));
        let service = server::Server::developer(settings, workspace, sessions, security.clone())?;
        server::shared_stdio(service, security).await
    });
    if let Some(guard) = &mut guard {
        guard.close()?;
    }
    result
}
