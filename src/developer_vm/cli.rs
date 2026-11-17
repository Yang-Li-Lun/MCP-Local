use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs::File,
    io::Read,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    ptr, thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS},
    Storage::Packaging::Appx::{
        FindPackagesByPackageFamily, GetPackagePathByFullName, GetStagedPackageOrigin,
        PACKAGE_FILTER_HEAD, PACKAGE_FULL_NAME_MAX_LENGTH, PACKAGE_PROPERTY_DEVELOPMENT_MODE,
        PackageOrigin_Inbox, PackageOrigin_Store, PackageOrigin_Unknown,
    },
    System::Threading::CREATE_NO_WINDOW,
    UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_ProgramFiles, FOLDERID_RoamingAppData,
        FOLDERID_Windows,
    },
};

use crate::{
    filesystem::{known_folder, registered_package_image, valid_name},
    settings::fold,
    windows_pipe::pending_bytes,
    windows_token::require_standard_user,
};

use super::{Backend, InvocationFailure};

const FAMILY: &str = "MicrosoftWindows.WindowsSandbox_cw5n1h2txyewy";
const UNAVAILABLE: &str = "VM_CLI_UNAVAILABLE：找不到可用的 Windows Sandbox 官方 CLI。";
const UNTRUSTED: &str = "VM_CLI_UNTRUSTED_PATH：Windows Sandbox CLI 的套件來源或路徑無法確認。";
const FAILED: &str = "VM_CLI_FAILED：Windows Sandbox CLI 未成功。";
const PROTOCOL: &str = "VM_CLI_PROTOCOL：Windows Sandbox CLI 回傳格式無效。";
const TIMEOUT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(20);
const MAX_OUTPUT: usize = 128 * 1024;
const MAX_COMMAND: usize = 30_000;

pub(super) struct Cli {
    executable: PathBuf,
    _image: File,
    windows: PathBuf,
    program_files: PathBuf,
    local_app_data: PathBuf,
    app_data: PathBuf,
    profile: PathBuf,
}

impl Cli {
    pub(super) fn resolve() -> Result<Self> {
        require_standard_user("VM_TOKEN_REJECTED：開發模式需要本機未提升權限的一般使用者。")?;
        let windows = known_folder(&FOLDERID_Windows)?;
        ensure!(
            windows.join("System32/WindowsSandbox.exe").is_file(),
            "{UNAVAILABLE}"
        );
        let program_files = known_folder(&FOLDERID_ProgramFiles)?;
        let (name, registered) = registered_package()?;
        let package = package_path(&program_files, &name, &registered)?;
        let (image, executable) =
            registered_package_image(&package.join("wsb.exe")).context(UNTRUSTED)?;
        Ok(Self {
            executable,
            _image: image,
            windows,
            program_files,
            local_app_data: known_folder(&FOLDERID_LocalAppData)?,
            app_data: known_folder(&FOLDERID_RoamingAppData)?,
            profile: known_folder(&FOLDERID_Profile)?,
        })
    }

    fn command(&self, arguments: &[&str]) -> Result<Command> {
        let units = std::iter::once(self.executable.as_os_str())
            .chain(arguments.iter().copied().map(OsStr::new))
            .chain([OsStr::new("--raw")])
            .map(|arg| quoted_units(arg) + 1)
            .sum::<usize>();
        ensure!(units <= MAX_COMMAND, "VM_CONFIG_LIMIT");
        let system = self.windows.join("System32");
        let path = std::env::join_paths([&system, &self.windows]).context(UNAVAILABLE)?;
        let temp = self.local_app_data.join("Temp");
        let mut command = Command::new(&self.executable);
        command
            .args(arguments)
            .arg("--raw")
            .creation_flags(CREATE_NO_WINDOW)
            .current_dir(&system)
            .env_clear()
            .env("SystemRoot", &self.windows)
            .env("WINDIR", &self.windows)
            .env("ProgramFiles", &self.program_files)
            .env("LOCALAPPDATA", &self.local_app_data)
            .env("APPDATA", &self.app_data)
            .env("USERPROFILE", &self.profile)
            .env("TEMP", &temp)
            .env("TMP", &temp)
            .env("PATH", path)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        Ok(command)
    }

    fn invoke(&self, arguments: &[&str]) -> Result<Vec<u8>, InvocationFailure> {
        let mut command = self
            .command(arguments)
            .map_err(InvocationFailure::BeforeSpawn)?;
        capture(&mut command, TIMEOUT, MAX_OUTPUT)
    }
}

impl Backend for Cli {
    type Connector = Child;

    fn list(&mut self) -> Result<BTreeSet<Uuid>> {
        ids(&self.invoke(&["list"]).map_err(InvocationFailure::error)?)
    }

    fn start(&mut self, id: Uuid, configuration: &str) -> Result<Uuid, InvocationFailure> {
        let id = id.hyphenated().to_string();
        let data = self.invoke(&["start", "--id", &id, "--config", configuration])?;
        parse_row(&data).map_err(InvocationFailure::AfterSpawn)
    }

    fn connect(&mut self, id: Uuid) -> Result<Child> {
        let id = id.hyphenated().to_string();
        self.command(&["connect", "--id", &id])?
            .stdout(Stdio::null())
            .spawn()
            .context(FAILED)
    }

    fn stop(&mut self, id: Uuid) -> Result<()> {
        let id = id.hyphenated().to_string();
        let data = self
            .invoke(&["stop", "--id", &id])
            .map_err(InvocationFailure::error)?;
        let data = json_bytes(&data).trim_ascii();
        if !data.is_empty() {
            let _: serde_json::Map<String, serde_json::Value> =
                serde_json::from_slice(data).context(PROTOCOL)?;
        }
        Ok(())
    }

    fn close_connector(&mut self, connector: &mut Child) -> Result<()> {
        if !exited(connector, Duration::from_secs(10))? {
            terminate(connector)?;
        }
        Ok(())
    }
}

fn registered_package() -> Result<(String, PathBuf)> {
    let family: Vec<u16> = FAMILY.encode_utf16().chain([0]).collect();
    let mut count = 0;
    let mut units = 0;
    let queried = unsafe {
        FindPackagesByPackageFamily(
            family.as_ptr(),
            PACKAGE_FILTER_HEAD,
            &mut count,
            ptr::null_mut(),
            &mut units,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    ensure!(
        queried == ERROR_INSUFFICIENT_BUFFER
            && count == 1
            && (2..=PACKAGE_FULL_NAME_MAX_LENGTH + 1).contains(&units),
        "{UNAVAILABLE}"
    );
    let mut names = [ptr::null_mut()];
    let mut name_buffer = vec![0u16; units as usize];
    let mut properties = 0;
    let fetched = unsafe {
        FindPackagesByPackageFamily(
            family.as_ptr(),
            PACKAGE_FILTER_HEAD,
            &mut count,
            names.as_mut_ptr(),
            &mut units,
            name_buffer.as_mut_ptr(),
            &mut properties,
        )
    };
    ensure!(
        fetched == ERROR_SUCCESS && count == 1 && names[0] == name_buffer.as_mut_ptr(),
        "{UNAVAILABLE}"
    );
    ensure!(
        properties & PACKAGE_PROPERTY_DEVELOPMENT_MODE == 0,
        "{UNTRUSTED}"
    );
    let name = String::from_utf16(nul_terminated(&name_buffer)?).context(UNAVAILABLE)?;
    valid_name(&name).context(UNTRUSTED)?;
    let mut origin = PackageOrigin_Unknown;
    let status = unsafe { GetStagedPackageOrigin(names[0], &mut origin) };
    ensure!(
        status == ERROR_SUCCESS && (origin == PackageOrigin_Inbox || origin == PackageOrigin_Store),
        "{UNTRUSTED}"
    );

    let mut units = 0;
    let sizing = unsafe { GetPackagePathByFullName(names[0], &mut units, ptr::null_mut()) };
    ensure!(
        sizing == ERROR_INSUFFICIENT_BUFFER && (2..=32768).contains(&units),
        "{UNAVAILABLE}"
    );
    let mut path = vec![0u16; units as usize];
    let fetched = unsafe { GetPackagePathByFullName(names[0], &mut units, path.as_mut_ptr()) };
    ensure!(fetched == ERROR_SUCCESS, "{UNAVAILABLE}");
    Ok((
        name,
        PathBuf::from(OsString::from_wide(nul_terminated(&path)?)),
    ))
}

fn nul_terminated(value: &[u16]) -> Result<&[u16]> {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .filter(|end| *end > 0)
        .ok_or_else(|| anyhow::anyhow!(UNAVAILABLE))?;
    Ok(&value[..end])
}

fn package_path(program_files: &Path, name: &str, registered: &Path) -> Result<PathBuf> {
    valid_name(name).context(UNTRUSTED)?;
    let expected = program_files.join("WindowsApps").join(name);
    let windows_key = |path: &Path| path.to_str().map(|path| fold(path.trim_end_matches('\\')));
    ensure!(
        registered.is_absolute()
            && windows_key(registered).is_some_and(|key| Some(key) == windows_key(&expected)),
        "{UNTRUSTED}"
    );
    Ok(expected)
}

fn quoted_units(argument: &OsStr) -> usize {
    let mut length = 2;
    let mut slashes = 0;
    for unit in argument.encode_wide() {
        length += 1;
        if unit == b'\\' as u16 {
            slashes += 1;
        } else {
            if unit == b'"' as u16 {
                length += slashes + 1;
            }
            slashes = 0;
        }
    }
    length + slashes
}

fn capture(
    command: &mut Command,
    timeout: Duration,
    maximum: usize,
) -> Result<Vec<u8>, InvocationFailure> {
    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| FAILED)
        .map_err(InvocationFailure::BeforeSpawn)?;
    match collect(&mut child, timeout, maximum) {
        Ok(bytes) => Ok(bytes),
        Err(error) => {
            terminate(&mut child).map_err(InvocationFailure::AfterSpawn)?;
            Err(InvocationFailure::AfterSpawn(error))
        }
    }
}

fn collect(child: &mut Child, timeout: Duration, maximum: usize) -> Result<Vec<u8>> {
    let mut stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!(FAILED))?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let status = child.try_wait().context(FAILED)?;
        ensure!(status.is_none_or(|status| status.success()), "{FAILED}");
        match pending_bytes(&stdout).context(FAILED)? {
            None if status.is_some() => {
                return Ok(bytes);
            }
            Some(pending) if pending != 0 => {
                ensure!(
                    pending <= maximum.saturating_sub(bytes.len()),
                    "VM_CLI_OUTPUT_LIMIT"
                );
                let available = pending.min(chunk.len());
                let read = stdout.read(&mut chunk[..available]).context(FAILED)?;
                ensure!(read > 0, "{FAILED}");
                bytes.extend_from_slice(&chunk[..read]);
                ensure!(Instant::now() < deadline, "VM_CLI_TIMEOUT");
                continue;
            }
            _ => {}
        }
        ensure!(Instant::now() < deadline, "VM_CLI_TIMEOUT");
        thread::sleep(POLL);
    }
}

fn exited(child: &mut Child, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().context(FAILED)?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(POLL);
    }
}

fn terminate(child: &mut Child) -> Result<()> {
    if child.try_wait().context(FAILED)?.is_none() {
        // The process can exit between the check and kill, so verify afterwards.
        let _ = child.kill();
        ensure!(
            exited(child, Duration::from_secs(5))?,
            "VM_CLI_PROCESS_UNCONFIRMED"
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct Row {
    #[serde(rename = "Id")]
    id: String,
}

fn json_bytes(input: &[u8]) -> &[u8] {
    input.strip_prefix(b"\xef\xbb\xbf").unwrap_or(input)
}

fn ids(input: &[u8]) -> Result<BTreeSet<Uuid>> {
    #[derive(Deserialize)]
    struct List {
        #[serde(rename = "WindowsSandboxEnvironments")]
        environments: Vec<Row>,
    }
    let list: List = serde_json::from_slice(json_bytes(input)).context(PROTOCOL)?;
    let mut result = BTreeSet::new();
    for row in list.environments {
        ensure!(
            result.insert(Uuid::parse_str(&row.id).context(PROTOCOL)?),
            "{PROTOCOL}"
        );
    }
    Ok(result)
}

fn parse_row(input: &[u8]) -> Result<Uuid> {
    let row: Row = serde_json::from_slice(json_bytes(input)).context(PROTOCOL)?;
    Uuid::parse_str(&row.id).context(PROTOCOL)
}

#[cfg(test)]
mod tests;
