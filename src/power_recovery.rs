//! Recovery of retired power transactions only. No API creates a transaction.
use crate::filesystem::PrivateDirectory;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::path::Path;
use uuid::Uuid;

const INVALID: &str = "POWER_RUNTIME_STATE_CORRUPT";
const APPLIED: &str = "961cc777-2547-4f9d-8174-7d86181b8a7a";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    owner_pid: u32,
    owner_created: u64,
    original: String,
    owned: Vec<String>,
    mode: String,
    journal_version: Option<u8>,
    user_power_mode_supported: Option<bool>,
    original_ac_power_mode: Option<String>,
    original_dc_power_mode: Option<String>,
    applied_power_mode: Option<String>,
}

struct Recovery {
    pid: u32,
    created: u64,
    original: Uuid,
    owned: Vec<Uuid>,
    user_modes: Option<[Uuid; 2]>,
}

impl Recovery {
    fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= 4096, "{INVALID}");
        let raw: Journal = serde_json::from_slice(bytes).context(INVALID)?;
        let value: serde_json::Value = serde_json::from_slice(bytes).context(INVALID)?;
        let fields = value.as_object().context(INVALID)?.len();
        let version = raw.journal_version.unwrap_or(1);
        ensure!(
            raw.owner_pid > 0 && matches!(raw.mode.as_str(), "off" | "extreme" | "low"),
            "{INVALID}"
        );
        ensure!(
            (version == 1 && fields == 5) || (version == 2 && fields == 10 && raw.mode != "low"),
            "{INVALID}"
        );
        let original = Uuid::parse_str(&raw.original).context(INVALID)?;
        let owned = raw
            .owned
            .iter()
            .map(|id| Uuid::parse_str(id).context(INVALID))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            owned.len() <= 8 && (version != 1 || !owned.is_empty()),
            "{INVALID}"
        );
        ensure!(
            !owned.contains(&original)
                && owned.iter().collect::<std::collections::HashSet<_>>().len() == owned.len(),
            "{INVALID}"
        );
        let user_modes = if version == 2 {
            ensure!(
                raw.applied_power_mode.as_deref() == Some(APPLIED),
                "{INVALID}"
            );
            match raw.user_power_mode_supported {
                Some(true) => Some([
                    Uuid::parse_str(raw.original_ac_power_mode.as_deref().context(INVALID)?)
                        .context(INVALID)?,
                    Uuid::parse_str(raw.original_dc_power_mode.as_deref().context(INVALID)?)
                        .context(INVALID)?,
                ]),
                Some(false)
                    if raw.original_ac_power_mode.is_none()
                        && raw.original_dc_power_mode.is_none() =>
                {
                    None
                }
                _ => anyhow::bail!(INVALID),
            }
        } else {
            None
        };
        Ok(Self {
            pid: raw.owner_pid,
            created: raw.owner_created,
            original,
            owned,
            user_modes,
        })
    }

    fn restore(&self, api: &mut impl Api) -> Result<()> {
        ensure!(
            !api.owner_alive(self.pid, self.created)?,
            "POWER_GUARD_ALREADY_RUNNING"
        );
        let mut first_error = None;
        if let Some(modes) = self.user_modes {
            for (supply, original) in modes.into_iter().enumerate() {
                let restored = (|| {
                    if api.user_mode(supply)? == Uuid::parse_str(APPLIED)? {
                        api.set_user_mode(supply, original)?;
                    }
                    Ok(())
                })();
                if let Err(error) = restored {
                    first_error.get_or_insert(error);
                }
            }
        }
        let restored = (|| {
            if self.owned.contains(&api.active()?) {
                api.set_active(self.original)?;
            }
            Ok(())
        })();
        if let Err(error) = restored {
            first_error.get_or_insert(error);
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        for owned in &self.owned {
            if let Err(error) = api.delete(*owned) {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }
}

trait Api {
    fn owner_alive(&mut self, pid: u32, created: u64) -> Result<bool>;
    fn active(&mut self) -> Result<Uuid>;
    fn set_active(&mut self, id: Uuid) -> Result<()>;
    fn delete(&mut self, id: Uuid) -> Result<()>;
    fn user_mode(&mut self, supply: usize) -> Result<Uuid>;
    fn set_user_mode(&mut self, supply: usize, id: Uuid) -> Result<()>;
}

// A remaining journal blocks migration/start until the user explicitly invokes
// recovery. Inspect only this fixed filename; never read legacy credentials.
pub fn require_clear(directory: &Path) -> Result<()> {
    match std::fs::symlink_metadata(directory.join("power-runtime.json")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => anyhow::bail!(
            "POWER_RECOVERY_REQUIRED：舊版電源回復日誌尚未處理；請明確執行 --recover-legacy-power <日誌路徑>。"
        ),
    }
}

pub fn require_installations_clear() -> Result<()> {
    let local = std::env::var_os("LOCALAPPDATA").context("無法找出本機設定目錄。")?;
    for name in ["MCP-Local", "MCP-Local-Rust"] {
        require_clear(&std::path::PathBuf::from(&local).join(name))?;
    }
    Ok(())
}

pub fn recover(path: &Path) -> Result<()> {
    let absolute = std::path::absolute(path)?;
    ensure!(
        absolute.file_name().and_then(|v| v.to_str()) == Some("power-runtime.json"),
        "{INVALID}"
    );
    let directory = PrivateDirectory::open(absolute.parent().context(INVALID)?, false)?;
    let _lock = directory.exclusive_lock("power-recovery.lock")?;
    directory.consume_regular("power-runtime.json", 4096, |bytes| {
        let recovery = Recovery::parse(bytes).map_err(|_| anyhow::anyhow!(INVALID))?;
        recovery.restore(&mut Native::open()?)
    })
}

struct Native(libloading::Library);
impl Native {
    fn open() -> Result<Self> {
        let windows =
            crate::filesystem::known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Windows)?;
        Ok(Self(unsafe {
            libloading::Library::new(windows.join("System32/powrprof.dll"))
        }?))
    }
    fn user(&self, supply: usize, set: bool, id: &mut windows_sys::core::GUID) -> Result<()> {
        let name: &[u8] = match (supply, set) {
            (0, false) => b"PowerGetUserConfiguredACPowerMode\0",
            (1, false) => b"PowerGetUserConfiguredDCPowerMode\0",
            (0, true) => b"PowerSetUserConfiguredACPowerMode\0",
            (1, true) => b"PowerSetUserConfiguredDCPowerMode\0",
            _ => anyhow::bail!(INVALID),
        };
        let call: libloading::Symbol<
            unsafe extern "system" fn(*mut windows_sys::core::GUID) -> u32,
        > = unsafe { self.0.get(name) }.context("POWER_USER_MODE_UNAVAILABLE")?;
        ensure!(unsafe { call(id) } == 0, "POWER_USER_MODE_RESTORE_FAILED");
        Ok(())
    }
}
fn guid(id: Uuid) -> windows_sys::core::GUID {
    windows_sys::core::GUID::from_u128(id.as_u128())
}
fn uuid(id: windows_sys::core::GUID) -> Uuid {
    Uuid::from_fields(id.data1, id.data2, id.data3, &id.data4)
}
impl Api for Native {
    fn owner_alive(&mut self, pid: u32, created: u64) -> Result<bool> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, FILETIME, GetLastError, WAIT_TIMEOUT},
            System::Threading::{
                GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
                PROCESS_SYNCHRONIZE, WaitForSingleObject,
            },
        };
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if process.is_null() {
            ensure!(
                unsafe { GetLastError() } == 87,
                "POWER_GUARD_ALREADY_RUNNING"
            );
            return Ok(false);
        }
        let mut times = [FILETIME::default(); 4];
        let ok = unsafe {
            GetProcessTimes(
                process,
                &mut times[0],
                &mut times[1],
                &mut times[2],
                &mut times[3],
            )
        } != 0;
        let status = unsafe { WaitForSingleObject(process, 0) };
        unsafe {
            CloseHandle(process);
        }
        ensure!(ok && (status == 0 || status == WAIT_TIMEOUT), "{INVALID}");
        let alive = status == WAIT_TIMEOUT;
        Ok(alive
            && (u64::from(times[0].dwHighDateTime) << 32 | u64::from(times[0].dwLowDateTime))
                == created)
    }
    fn active(&mut self) -> Result<Uuid> {
        type Get = unsafe extern "system" fn(
            *mut std::ffi::c_void,
            *mut *mut windows_sys::core::GUID,
        ) -> u32;
        let call: libloading::Symbol<Get> = unsafe { self.0.get(b"PowerGetActiveScheme\0") }?;
        let mut value = std::ptr::null_mut();
        ensure!(
            unsafe { call(std::ptr::null_mut(), &mut value) } == 0 && !value.is_null(),
            "POWER_SCHEME_READ_FAILED"
        );
        let id = uuid(unsafe { *value });
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(value.cast());
        }
        Ok(id)
    }
    fn set_active(&mut self, id: Uuid) -> Result<()> {
        type Set =
            unsafe extern "system" fn(*mut std::ffi::c_void, *const windows_sys::core::GUID) -> u32;
        let call: libloading::Symbol<Set> = unsafe { self.0.get(b"PowerSetActiveScheme\0") }?;
        ensure!(
            unsafe { call(std::ptr::null_mut(), &guid(id)) } == 0,
            "POWER_SCHEME_RESTORE_FAILED"
        );
        Ok(())
    }
    fn delete(&mut self, id: Uuid) -> Result<()> {
        type Delete =
            unsafe extern "system" fn(*mut std::ffi::c_void, *const windows_sys::core::GUID) -> u32;
        let call: libloading::Symbol<Delete> = unsafe { self.0.get(b"PowerDeleteScheme\0") }?;
        ensure!(
            matches!(unsafe { call(std::ptr::null_mut(), &guid(id)) }, 0 | 2),
            "POWER_SCHEME_DELETE_FAILED"
        );
        Ok(())
    }
    fn user_mode(&mut self, supply: usize) -> Result<Uuid> {
        let mut value = guid(Uuid::nil());
        self.user(supply, false, &mut value)?;
        Ok(uuid(value))
    }
    fn set_user_mode(&mut self, supply: usize, id: Uuid) -> Result<()> {
        self.user(supply, true, &mut guid(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        alive: bool,
        active: Uuid,
        changes: Vec<Uuid>,
        fail: bool,
    }
    impl Api for Fake {
        fn owner_alive(&mut self, _: u32, _: u64) -> Result<bool> {
            Ok(self.alive)
        }
        fn active(&mut self) -> Result<Uuid> {
            Ok(self.active)
        }
        fn set_active(&mut self, id: Uuid) -> Result<()> {
            ensure!(!self.fail, "synthetic");
            self.changes.push(id);
            Ok(())
        }
        fn delete(&mut self, id: Uuid) -> Result<()> {
            self.changes.push(id);
            Ok(())
        }
        fn user_mode(&mut self, _: usize) -> Result<Uuid> {
            Ok(Uuid::nil())
        }
        fn set_user_mode(&mut self, _: usize, _: Uuid) -> Result<()> {
            panic!("user override must be preserved")
        }
    }
    #[test]
    fn recovery_validates_ownership_and_preserves_user_changes_and_failed_journals() {
        let input = br#"{"owner_pid":12,"owner_created":34,"original":"00000000-0000-0000-0000-000000000001","owned":["00000000-0000-0000-0000-000000000002"],"mode":"low"}"#;
        let recovery = Recovery::parse(input).unwrap();
        let mut api = Fake {
            alive: true,
            ..Fake::default()
        };
        assert!(recovery.restore(&mut api).is_err());
        assert!(api.changes.is_empty());
        api.alive = false;
        recovery.restore(&mut api).unwrap();
        assert_eq!(api.changes, recovery.owned);
        api.changes.clear();
        api.active = recovery.owned[0];
        api.fail = true;
        assert!(recovery.restore(&mut api).is_err());
        assert!(api.changes.is_empty());
        api.fail = false;
        recovery.restore(&mut api).unwrap();
        assert_eq!(api.changes, [recovery.original, recovery.owned[0]]);
        for bytes in [
            b"{}".as_slice(),
            b"{\"owner_pid\":1,\"owner_pid\":2}",
            &[b'x'; 4097],
        ] {
            assert!(Recovery::parse(bytes).is_err());
        }
        let temp = tempfile::tempdir().unwrap();
        require_clear(temp.path()).unwrap();
        std::fs::write(temp.path().join("power-runtime.json"), input).unwrap();
        assert!(require_clear(temp.path()).is_err());
        assert_eq!(
            std::fs::read(temp.path().join("power-runtime.json")).unwrap(),
            input
        );
    }

    #[test]
    fn pinned_journal_is_retained_on_failure_and_only_deleted_after_success() {
        let temp = tempfile::tempdir().unwrap();
        let directory = PrivateDirectory::open(temp.path(), false).unwrap();
        let path = temp.path().join("power-runtime.json");
        std::fs::write(&path, b"synthetic").unwrap();
        assert!(
            directory
                .consume_regular("power-runtime.json", 4096, |_| {
                    assert!(std::fs::rename(&path, temp.path().join("replacement.json")).is_err());
                    anyhow::bail!("synthetic failure")
                })
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"synthetic");
        directory
            .consume_regular("power-runtime.json", 4096, |bytes| {
                assert_eq!(bytes, b"synthetic");
                Ok(())
            })
            .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn version_two_restores_only_unchanged_user_power_modes() {
        struct UserPower {
            modes: [Uuid; 2],
            changed: Vec<usize>,
        }
        impl Api for UserPower {
            fn owner_alive(&mut self, _: u32, _: u64) -> Result<bool> {
                Ok(false)
            }
            fn active(&mut self) -> Result<Uuid> {
                Ok(Uuid::nil())
            }
            fn set_active(&mut self, _: Uuid) -> Result<()> {
                panic!("no owned scheme")
            }
            fn delete(&mut self, _: Uuid) -> Result<()> {
                panic!("no owned scheme")
            }
            fn user_mode(&mut self, supply: usize) -> Result<Uuid> {
                Ok(self.modes[supply])
            }
            fn set_user_mode(&mut self, supply: usize, id: Uuid) -> Result<()> {
                self.changed.push(supply);
                self.modes[supply] = id;
                Ok(())
            }
        }
        let original = Uuid::from_u128(1);
        let mut value = serde_json::json!({"owner_pid":1,"owner_created":2,"original":original.to_string(),"owned":[],"mode":"extreme","journal_version":2,"user_power_mode_supported":true,"original_ac_power_mode":original.to_string(),"original_dc_power_mode":original.to_string(),"applied_power_mode":APPLIED});
        let parsed = Recovery::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        let custom = Uuid::from_u128(3);
        let mut api = UserPower {
            modes: [Uuid::parse_str(APPLIED).unwrap(), custom],
            changed: vec![],
        };
        parsed.restore(&mut api).unwrap();
        assert_eq!(api.changed, [0]);
        assert_eq!(api.modes, [original, custom]);
        value["applied_power_mode"] = serde_json::json!(custom.to_string());
        assert!(Recovery::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
}
