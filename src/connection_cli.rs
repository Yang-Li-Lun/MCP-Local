use std::{
    ffi::OsString,
    io::{self, BufRead, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use crate::{
    connection::{Connection, Event},
    desktop_store::Store,
    security::{authority::Authority, channel::Mode},
    startup::DesktopInstance,
};
use anyhow::{Context, Result, bail, ensure};

static STOP: AtomicBool = AtomicBool::new(false);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AttachConsole(pid: u32) -> i32;
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
    fn GetStdHandle(kind: u32) -> *mut std::ffi::c_void;
    fn GetConsoleMode(handle: *mut std::ffi::c_void, mode: *mut u32) -> i32;
    fn SetConsoleMode(handle: *mut std::ffi::c_void, mode: u32) -> i32;
    fn ReadConsoleW(
        handle: *mut std::ffi::c_void,
        buffer: *mut u16,
        length: u32,
        read: *mut u32,
        control: *mut std::ffi::c_void,
    ) -> i32;
}

unsafe extern "system" fn interrupt(event: u32) -> i32 {
    if event <= 1 {
        STOP.store(true, Ordering::Release);
        1
    } else {
        0
    }
}

struct Console(bool);
impl Console {
    fn open() -> Result<Self> {
        unsafe {
            AttachConsole(u32::MAX);
        }
        let registered = unsafe { SetConsoleCtrlHandler(Some(interrupt), 1) } != 0;
        STOP.store(false, Ordering::Release);
        Ok(Self(registered))
    }
}
impl Drop for Console {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                SetConsoleCtrlHandler(Some(interrupt), 0);
            }
        }
    }
}

fn local_console() -> bool {
    let mut mode = 0;
    unsafe { GetConsoleMode(GetStdHandle(-10i32 as u32), &mut mode) != 0 }
}

fn read_key() -> Result<zeroize::Zeroizing<String>> {
    let handle = unsafe { GetStdHandle(-10i32 as u32) };
    let mut original = 0;
    ensure!(
        unsafe { GetConsoleMode(handle, &mut original) } != 0,
        "請先在 Rust 桌面儲存金鑰，或從本機主控台輸入。"
    );
    struct Restore(*mut std::ffi::c_void, u32);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                SetConsoleMode(self.0, self.1);
            }
        }
    }
    ensure!(
        unsafe { SetConsoleMode(handle, original & !4) } != 0,
        "CLI_CONSOLE_UNAVAILABLE"
    );
    let _restore = Restore(handle, original);
    print!("通道 API 金鑰（隱藏輸入，僅用於此次連線）：");
    io::stdout().flush()?;
    let mut buffer = zeroize::Zeroizing::new(vec![0u16; 16385]);
    let mut read = 0;
    ensure!(
        unsafe {
            ReadConsoleW(
                handle,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        } != 0,
        "CLI_INPUT_FAILED"
    );
    println!();
    ensure!(read < buffer.len() as u32, "金鑰格式不符合限制。");
    let text = zeroize::Zeroizing::new(
        String::from_utf16(&buffer[..read as usize])
            .map_err(|_| anyhow::anyhow!("金鑰格式不符合限制。"))?,
    );
    Ok(zeroize::Zeroizing::new(text.trim().to_owned()))
}

fn mode(value: &std::ffi::OsStr) -> Result<Mode> {
    match value.to_str() {
        Some("read_only") => Ok(Mode::ReadOnly),
        Some("developer_control") => Ok(Mode::DeveloperControl),
        Some("host_control") => Ok(Mode::HostControl),
        _ => bail!("權限模式無效。"),
    }
}

#[derive(Debug, PartialEq)]
enum Request {
    Connect(Option<Mode>),
    Save(Mode),
}

fn parse(args: &[OsString]) -> Result<Request> {
    match args {
        [command] if command == "--connect" => Ok(Request::Connect(None)),
        [command, flag, value] if command == "--connect" && flag == "--access-mode" => {
            Ok(Request::Connect(Some(mode(value)?)))
        }
        [command, value] if command == "--set-access-mode" => Ok(Request::Save(mode(value)?)),
        _ => bail!(
            "用法：--connect [--access-mode read_only|developer_control|host_control] 或 --set-access-mode <模式>"
        ),
    }
}

pub fn run(args: Vec<OsString>) -> Result<()> {
    let request = parse(&args)?;
    crate::power_recovery::require_installations_clear()?;
    let _console = Console::open()?;
    let store = Store::new()?;
    let instance = DesktopInstance::open(&store.path, false)?;
    if instance.is_none() {
        if let Request::Save(mode) = request {
            if store.load()?.is_some_and(|s| s.config.access_mode == mode) {
                return Ok(());
            }
            DesktopInstance::request_mode(&store.path, mode)?;
            println!("已請求執行中的 Rust 桌面切換模式；若出現權限確認，請在本機視窗完成。");
            let deadline = std::time::Instant::now() + Duration::from_secs(120);
            while std::time::Instant::now() < deadline && !STOP.load(Ordering::Acquire) {
                if store.load()?.is_some_and(|s| s.config.access_mode == mode) {
                    println!("已儲存本機模式；連線清理與重連狀態請查看 Rust 桌面。");
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(200));
            }
            bail!("MODE_SWITCH_UNCONFIRMED：未確認模式已保存，請查看 Rust 桌面。");
        }
        bail!("已有 Rust 桌面或 CLI，請在該視窗操作連線。");
    }
    let _instance = instance;
    let snapshot = store
        .load()?
        .context("請先在 Rust 桌面設定分享範圍、通道及金鑰。")?;
    let mut config = snapshot.config;
    match request {
        Request::Save(mode) => {
            config.access_mode = mode;
            store.commit(config, Some(snapshot.revision), false)?;
            println!("已儲存本機權限模式。新的連線會採用此設定。");
            return Ok(());
        }
        Request::Connect(Some(mode)) => config.access_mode = mode,
        Request::Connect(None) => {}
    }
    config.validate_shared()?;
    if !config.key_present() {
        config.update_key(&read_key()?)?;
    }
    let authority = Arc::new(Mutex::new(Authority::new(store.path)));
    let monitor = crate::monitor::Bus::create()?;
    let connection = Connection::start(config, monitor.name().into(), authority.clone())?;
    println!("輸入 stop 停止；本機主機授權使用 authorize <分鐘>，revoke 撤銷。Ctrl+C 會清理連線。");
    let (send, input) = mpsc::sync_channel(8);
    thread::spawn(move || {
        // Bound each input before decoding; never retain arbitrarily large stdin.
        let mut stdin = io::stdin().lock();
        loop {
            let mut bytes = Vec::new();
            let mut oversized = false;
            loop {
                let Ok(buffer) = stdin.fill_buf() else { return };
                if buffer.is_empty() {
                    let _ = send.send(String::from("stop"));
                    return;
                }
                let end = buffer.iter().position(|b| *b == b'\n').map(|n| n + 1);
                let count = end.unwrap_or(buffer.len());
                if bytes.len() + count <= 128 {
                    bytes.extend_from_slice(&buffer[..count]);
                } else {
                    oversized = true;
                }
                stdin.consume(count);
                if end.is_some() {
                    break;
                }
            }
            let line = if oversized {
                String::new()
            } else {
                String::from_utf8(bytes).unwrap_or_default()
            };
            if send.send(line).is_err() {
                return;
            }
        }
    });
    loop {
        if STOP.load(Ordering::Acquire) {
            connection.stop();
        }
        while let Ok(line) = input.try_recv() {
            let line = line.trim();
            if line == "stop" {
                connection.stop();
            } else if line == "revoke" {
                authority
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SECURITY_UNCONFIRMED"))?
                    .revoke()?;
                println!("已撤銷主機授權。");
            } else if let Some(minutes) = line.strip_prefix("authorize ") {
                let result = (|| -> Result<()> {
                    ensure!(
                        local_console(),
                        "HOST_AUTH_REQUIRED：必須從本機互動式主控台確認。"
                    );
                    let minutes: u8 = minutes.parse().context("請輸入有效的授權分鐘數。")?;
                    let mut owner = authority
                        .lock()
                        .map_err(|_| anyhow::anyhow!("SECURITY_UNCONFIRMED"))?;
                    let target = owner.grant_target()?;
                    owner.grant(minutes, target)
                })();
                match result {
                    Ok(()) => println!("已授予限時主機權限。"),
                    Err(error) => eprintln!("{error}"),
                }
            }
        }
        match connection.events.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::State(state)) => {
                println!("{state}");
                io::stdout().flush()?;
            }
            Ok(Event::Finished {
                error,
                cleanup_failed,
                ..
            }) => {
                drop(connection);
                ensure!(!cleanup_failed, "RESOURCE_CLEANUP_FAILED");
                if let Some(error) = error {
                    bail!("{error}");
                }
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("CLI_CONNECTION_UNCONFIRMED"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_modes_are_explicit_and_do_not_accept_retired_or_ambiguous_options() {
        let args = |items: &[&str]| items.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            parse(&args(&["--connect"])).unwrap(),
            Request::Connect(None)
        );
        for name in ["read_only", "developer_control", "host_control"] {
            assert!(parse(&args(&["--connect", "--access-mode", name])).is_ok());
            assert!(parse(&args(&["--set-access-mode", name])).is_ok());
        }
        for items in [
            vec!["--set-access-mode", "full_control"],
            vec!["--connect", "--access-mode"],
            vec!["--connect", "--api-key", "synthetic"],
            vec!["--set-access-mode", "read_only", "extra"],
        ] {
            assert!(parse(&args(&items)).is_err());
        }
    }
}
