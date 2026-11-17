use std::{
    env,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};

use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};
use zeroize::{Zeroize, Zeroizing};

use crate::{
    filesystem::{PrivateDirectory, missing, replace_atomic},
    listing::hex,
    security::channel::Mode,
    server::Workspace,
    settings::{ReaderSettings, RootSettings, WorkspaceSettings},
};

const VERSION: u8 = 1;
const FILE: &str = "settings.json";
const MAX_BYTES: usize = 512 * 1024;
const INVALID: &str = "設定檔損毀或版本不支援。";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    version: u8,
    pub workspace: WorkspaceSettings,
    pub reader: ReaderSettings,
    pub tunnel: String,
    pub start_hidden: bool,
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default)]
    pub auto_connect: bool,
    pub icon: usize,
    #[serde(default)]
    pub access_mode: Mode,
    key_dpapi: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: VERSION,
            workspace: WorkspaceSettings {
                roots: Vec::new(),
                default_root: String::new(),
            },
            reader: ReaderSettings::default()
                .normalize()
                .expect("built-in reader settings"),
            tunnel: String::new(),
            start_hidden: false,
            auto_start: false,
            auto_connect: false,
            icon: 0,
            access_mode: Mode::ReadOnly,
            key_dpapi: None,
        }
    }
}

impl Config {
    fn general_snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "settings_version": 8,
            "roots": self.workspace.roots,
            "default_root": self.workspace.default_root,
            "tunnel": self.tunnel,
            "reader": self.reader,
            "start_hidden": self.start_hidden,
            "auto_start": self.auto_start,
            "auto_connect": self.auto_connect,
            "icon_style": if self.icon == 1 { "folder_link" } else { "classic" },
            "access_mode": self.access_mode
        })
    }

    pub fn import_snapshot(data: &[u8]) -> Result<Self> {
        fn present<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
            value: D,
        ) -> Result<Option<T>, D::Error> {
            T::deserialize(value).map(Some)
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Legacy {
            #[serde(default)]
            settings_version: u8,
            #[serde(default, deserialize_with = "present")]
            roots: Option<Vec<RootSettings>>,
            #[serde(default, deserialize_with = "present")]
            default_root: Option<String>,
            root: Option<PathBuf>,
            tunnel: String,
            #[serde(default)]
            reader: ReaderSettings,
            #[serde(default)]
            start_hidden: bool,
            #[serde(default)]
            auto_start: bool,
            #[serde(default)]
            auto_connect: bool,
            #[serde(default)]
            recent: Vec<String>,
            icon_style: Option<String>,
            #[serde(default, deserialize_with = "present")]
            access_mode: Option<String>,
            #[serde(rename = "power")]
            _retired_power: Option<serde::de::IgnoredAny>,
            #[serde(rename = "developer_toolchains")]
            _retired_toolchains: Option<serde::de::IgnoredAny>,
        }
        ensure!(data.len() <= MAX_BYTES, "{INVALID}");
        let old: Legacy = serde_json::from_slice(data).map_err(|_| anyhow::anyhow!(INVALID))?;
        ensure!(
            old.settings_version <= 8 && old.recent.iter().all(|item| item.chars().count() <= 4096),
            "{INVALID}"
        );
        let access_mode = match (old.settings_version, old.access_mode.as_deref()) {
            (0..=5, _)
            | (6, Some("developer_control" | "host_control"))
            | (_, None | Some("read_only" | "full_control")) => Mode::ReadOnly,
            (_, Some("developer_control")) => Mode::DeveloperControl,
            (_, Some("host_control")) => Mode::HostControl,
            _ => anyhow::bail!("{INVALID}"),
        };
        let default_root = old
            .default_root
            .or_else(|| (old.settings_version <= 1).then(|| "main".into()))
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        let roots = match old.roots {
            Some(roots) => roots,
            None if old.settings_version <= 1 => {
                WorkspaceSettings::single(old.root.ok_or_else(|| anyhow::anyhow!(INVALID))?).roots
            }
            None => anyhow::bail!("{INVALID}"),
        };
        let icon = match old.icon_style.as_deref() {
            None | Some("classic") => 0,
            Some("folder_link") => 1,
            _ => return Err(anyhow::anyhow!(INVALID)),
        };
        let draft = Self {
            version: VERSION,
            workspace: WorkspaceSettings {
                roots,
                default_root,
            },
            reader: old.reader.normalize()?,
            tunnel: old.tunnel,
            start_hidden: old.start_hidden,
            auto_start: old.auto_start,
            auto_connect: old.auto_connect,
            icon,
            access_mode,
            key_dpapi: None,
        };
        draft.validate_shared()?;
        Ok(draft)
    }

    fn validate_stored(&self) -> Result<ReaderSettings> {
        ensure!(
            self.version == VERSION && self.icon <= 1 && !self.workspace.roots.is_empty(),
            "{INVALID}"
        );
        validate_tunnel(&self.tunnel)?;
        let reader = self.reader.clone().normalize()?;
        ensure!(
            self.key_dpapi
                .as_ref()
                .is_none_or(|value| value.len() <= 87_384),
            "{INVALID}"
        );
        Ok(reader)
    }

    pub fn validate_shared(&self) -> Result<()> {
        let reader = self.validate_stored()?;
        Workspace::new(reader, self.workspace.clone())?;
        Ok(())
    }

    pub fn key_present(&self) -> bool {
        self.key_dpapi.is_some()
    }

    pub fn retain_key_from(&mut self, existing: &Self) {
        self.key_dpapi = existing.key_dpapi.clone();
    }

    pub fn update_key(&mut self, key: &str) -> Result<()> {
        if key.is_empty() {
            return Ok(());
        }
        ensure!(
            key.len() <= 16_384 && !key.contains(['\0', '\r', '\n']),
            "金鑰格式不符合限制。"
        );
        let mut key = Zeroizing::new(key.as_bytes().to_vec());
        self.key_dpapi = Some(STANDARD.encode(protect(&mut key, false)?));
        Ok(())
    }

    pub fn key(&self) -> Result<Zeroizing<String>> {
        let encoded = self
            .key_dpapi
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("請先輸入並儲存 API 金鑰。"))?;
        let mut bytes = STANDARD
            .decode(encoded)
            .map_err(|_| anyhow::anyhow!("已保存的金鑰檔格式錯誤。"))?;
        ensure!(
            (1..=65_536).contains(&bytes.len()),
            "已保存的金鑰檔格式錯誤。"
        );
        let plaintext = protect(&mut bytes, true)?;
        let key = Zeroizing::new(String::from_utf8(plaintext).map_err(|error| {
            error.into_bytes().zeroize();
            anyhow::anyhow!("已保存的金鑰格式錯誤。")
        })?);
        ensure!(
            !key.is_empty() && key.len() <= 16_384 && !key.contains(['\0', '\r', '\n']),
            "已保存的金鑰格式錯誤。"
        );
        Ok(key)
    }

    pub fn set_root(&mut self, selected: Option<usize>, path: PathBuf) -> Result<()> {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .or_else(|| path.to_str())
            .ok_or_else(|| anyhow::anyhow!("資料夾名稱無效。"))?;
        let name: String = name.trim().chars().filter(|c| *c >= ' ').take(80).collect();
        let name = if name.is_empty() {
            "資料夾".into()
        } else {
            name
        };
        let mut candidate = self.workspace.clone();
        if let Some(index) = selected {
            let existing = candidate
                .roots
                .get_mut(index)
                .ok_or_else(|| anyhow::anyhow!("未選取資料夾。"))?;
            existing.path = path;
            existing.name = Some(name);
        } else {
            ensure!(candidate.roots.len() < 8, "最多只能加入 8 個授權資料夾。");
            let mut bytes = [0; 6];
            getrandom::fill(&mut bytes)?;
            candidate.roots.push(RootSettings {
                id: format!("root_{}", hex(&bytes)),
                name: Some(name),
                path,
                excluded_names: vec![],
            });
        }
        candidate.default_root = candidate.roots[0].id.clone();
        Workspace::new(self.reader.clone(), candidate.clone())?;
        self.workspace = candidate;
        Ok(())
    }

    pub fn remove_roots(&mut self, indexes: &[usize]) -> Result<()> {
        if indexes.is_empty() {
            return Ok(());
        }
        let roots = &mut self.workspace.roots;
        ensure!(
            indexes.iter().all(|&index| index < roots.len()),
            "資料夾選取無效。"
        );
        let mut index = 0;
        roots.retain(|_| {
            let keep = !indexes.contains(&index);
            index += 1;
            keep
        });
        self.workspace.default_root = roots
            .first()
            .map(|root| root.id.clone())
            .unwrap_or_default();
        Ok(())
    }
}

pub fn validate_tunnel(tunnel: &str) -> Result<()> {
    ensure!(
        tunnel.strip_prefix("tunnel_").is_some_and(|suffix| {
            (1..=200).contains(&suffix.len())
                && suffix
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        }),
        "通道識別碼必須以 tunnel_ 開頭，且只包含英數字、底線及連字號。"
    );
    Ok(())
}

fn protect(bytes: &mut [u8], decrypt: bool) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // DPAPI writes a LocalFree allocation to output and never prompts.
    let valid = unsafe {
        if decrypt {
            CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptProtectData(
                &input,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    let result = if valid != 0 && !output.pbData.is_null() && output.cbData <= 65_536 {
        Ok(unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec())
    } else {
        Err(anyhow::anyhow!("Windows 無法處理已保存的金鑰。"))
    };
    if !output.pbData.is_null() {
        unsafe {
            std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize).zeroize();
            LocalFree(output.pbData.cast());
        }
    }
    result
}

pub struct Store {
    pub path: PathBuf,
}

pub type Revision = [u8; 32];

pub struct Snapshot {
    pub config: Config,
    pub revision: Revision,
}

#[derive(Debug)]
pub struct Conflict;

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SETTINGS_CONFLICT：磁碟設定已變更，本次儲存已取消。")
    }
}

impl std::error::Error for Conflict {}

impl Store {
    pub fn new() -> Result<Self> {
        let appdata =
            env::var_os("LOCALAPPDATA").ok_or_else(|| anyhow::anyhow!("無法找出本機設定目錄。"))?;
        Ok(Self {
            path: PathBuf::from(appdata).join("MCP-Local-Rust"),
        })
    }

    pub fn import(&self, file: &Path) -> Result<Config> {
        let file = std::path::absolute(file)?;
        let parent = file.parent().ok_or_else(|| anyhow::anyhow!(INVALID))?;
        crate::power_recovery::require_clear(parent)?;
        // The importer accepts an exported copy in a dedicated folder.
        // It must not open the live state of either installation.
        let canonical = fs::canonicalize(parent)?;
        let appdata =
            fs::canonicalize(self.path.parent().ok_or_else(|| anyhow::anyhow!(INVALID))?)?;
        let key = canonical.to_string_lossy().to_lowercase();
        let appdata = appdata.to_string_lossy().to_lowercase();
        for name in ["mcp-local", "mcp-local-rust"] {
            let protected = format!("{}\\{name}", appdata.trim_end_matches('\\'));
            ensure!(
                key != protected && !key.starts_with(&format!("{protected}\\")),
                "請提供隔離的設定快照副本。"
            );
        }
        let directory = PrivateDirectory::open(parent, false)?;
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        let (source, _) = directory.regular(name)?;
        let mut bytes = Vec::new();
        source
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        Config::import_snapshot(&bytes)
    }

    pub fn load(&self) -> Result<Option<Snapshot>> {
        let directory = match PrivateDirectory::open(&self.path, false) {
            Ok(directory) => directory,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        Self::read(&directory)
    }

    fn read(directory: &PrivateDirectory) -> Result<Option<Snapshot>> {
        let mut bytes = Vec::new();
        match directory.regular(FILE) {
            Ok((file, _)) => file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        ensure!(bytes.len() <= MAX_BYTES, "{INVALID}");
        let mut config: Config =
            serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!(INVALID))?;
        config.reader = config.validate_stored()?;
        Ok(Some(Snapshot {
            config,
            revision: Sha256::digest(&bytes).into(),
        }))
    }

    pub fn commit(
        &self,
        config: Config,
        expected: Option<Revision>,
        owns_background: bool,
    ) -> Result<Snapshot> {
        config.validate_shared()?;
        let directory = PrivateDirectory::open(&self.path, true)?;
        let _lock = directory
            .exclusive_lock("settings.lock")
            .map_err(|_| anyhow::anyhow!("SETTINGS_LOCK：無法取得設定儲存鎖。請稍後重試。"))?;
        let previous = Self::read(&directory)?;
        if previous.as_ref().map(|snapshot| snapshot.revision) != expected {
            return Err(Conflict.into());
        }
        let save = || Self::write(&directory, &config);
        let revision =
            if config.auto_start || previous.is_some_and(|snapshot| snapshot.config.auto_start) {
                let task = crate::startup::Task::for_store(&self.path)?;
                if config.auto_start {
                    config
                        .key()
                        .map_err(|_| anyhow::anyhow!("登入自動啟動需要可用的 DPAPI 金鑰。"))?;
                }
                task.transaction(config.auto_start, owns_background, save)?
            } else {
                save()?
            };
        Ok(Snapshot { config, revision })
    }

    fn write(directory: &PrivateDirectory, config: &Config) -> Result<Revision> {
        let data = serde_json::to_vec_pretty(config)?;
        ensure!(data.len() <= MAX_BYTES, "設定檔容量超限。");
        if let Some(previous) = Self::read(directory)? {
            if previous.config == *config {
                return Ok(previous.revision);
            }
            Self::backup_general(directory, &previous)?;
        }
        let mut suffix = [0; 16];
        getrandom::fill(&mut suffix)?;
        let staged = directory
            .path()
            .join(format!("settings-{}.tmp", hex(&suffix)));
        let destination = directory.path().join(FILE);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        let written = file.write_all(&data).and_then(|_| file.sync_all());
        drop(file);
        let result = written.map_err(Into::into).and_then(|_| {
            replace_atomic(&staged, &destination).map_err(|_| anyhow::anyhow!("設定無法原子儲存。"))
        });
        if result.is_err() {
            let _ = fs::remove_file(&staged);
        }
        result?;
        Ok(Sha256::digest(&data).into())
    }

    fn backup_general(directory: &PrivateDirectory, previous: &Snapshot) -> Result<()> {
        let backups = PrivateDirectory::open(&directory.path().join("settings-backups"), true)?;
        let data = serde_json::to_vec_pretty(&previous.config.general_snapshot())?;
        let name = format!("settings-{}.json", hex(&Sha256::digest(&data)));
        let path = backups.path().join(&name);
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(&data).and_then(|_| file.sync_all()) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(error.into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let (file, _) = backups.regular(&name)?;
                let mut existing = Vec::new();
                file.take((MAX_BYTES + 1) as u64)
                    .read_to_end(&mut existing)?;
                ensure!(
                    existing == data,
                    "SETTINGS_BACKUP_CONFLICT：已保留同名備份。"
                );
            }
            Err(error) => return Err(error.into()),
        }
        let mut owned = Vec::new();
        for entry in fs::read_dir(backups.path())? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(hash) = name
                .strip_prefix("settings-")
                .and_then(|s| s.strip_suffix(".json"))
            else {
                continue;
            };
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let (file, _) = backups.regular(name)?;
            let mut bytes = Vec::new();
            let modified = file.metadata()?.modified()?;
            file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
            // Preserve unknown or hand-edited files rather than treating the
            // filename alone as permission to delete them.
            if bytes.len() > MAX_BYTES || hex(&Sha256::digest(&bytes)) != hash {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if value.get("settings_version") != Some(&serde_json::json!(8))
                || value.get("key_dpapi").is_some()
                || value.as_object().is_none_or(|v| v.len() != 10)
            {
                continue;
            }
            owned.push((modified, name.to_owned()));
        }
        owned.sort();
        let remove = owned.len().saturating_sub(5);
        for (_, name) in owned
            .into_iter()
            .filter(|(_, name)| name != path.file_name().unwrap().to_string_lossy().as_ref())
            .take(remove)
        {
            let expected = name
                .strip_prefix("settings-")
                .unwrap()
                .strip_suffix(".json")
                .unwrap();
            backups.consume_regular(&name, MAX_BYTES, |bytes| {
                ensure!(
                    hex(&Sha256::digest(bytes)) == expected,
                    "SETTINGS_BACKUP_CONFLICT"
                );
                Ok(())
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn general_backups_are_bounded_importable_and_never_contain_credentials() {
        let temp = tempfile::Builder::new()
            .prefix("mcp-settings-backup-")
            .tempdir()
            .unwrap();
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let store = Store {
            path: temp.path().join("state"),
        };
        let mut config = Config {
            workspace: WorkspaceSettings::single(shared),
            tunnel: "tunnel_fixture".into(),
            ..Config::default()
        };
        config.update_key("synthetic-secret-only").unwrap();
        let mut revision = None;
        for index in 0..8 {
            config.tunnel = format!("tunnel_fixture_{index}");
            revision = Some(
                store
                    .commit(config.clone(), revision, false)
                    .unwrap()
                    .revision,
            );
        }
        let files = fs::read_dir(store.path.join("settings-backups"))
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 5);
        for file in files {
            let bytes = fs::read(file.path()).unwrap();
            let text = String::from_utf8(bytes.clone()).unwrap();
            assert!(!text.contains("key_dpapi") && !text.contains("synthetic-secret-only"));
            let restored = Config::import_snapshot(&bytes).unwrap();
            assert!(!restored.key_present());
        }
        assert_eq!(
            store
                .load()
                .unwrap()
                .unwrap()
                .config
                .key()
                .unwrap()
                .as_str(),
            "synthetic-secret-only"
        );
    }

    #[test]
    fn invalid_host_settings_do_not_touch_state_or_scheduler() {
        let fixture = tempfile::tempdir().unwrap();
        let store = Store {
            path: fixture.path().join("uncreated-state"),
        };
        let config = Config {
            workspace: WorkspaceSettings::single(fixture.path().join("nonexistent-root")),
            access_mode: Mode::HostControl,
            auto_start: true,
            key_dpapi: Some("invalid-fixture".into()),
            ..Config::default()
        };
        let error = store.commit(config, None, false).err().unwrap();
        assert!(!error.to_string().is_empty());
        assert!(!store.path.exists());
    }

    #[test]
    fn commit_requires_current_revision_and_an_exclusive_writer() {
        let fixture = tempfile::Builder::new()
            .prefix("mcp-settings-revision-")
            .tempdir()
            .unwrap();
        let shared = fixture.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let store = Store {
            path: fixture.path().join("state"),
        };
        let config = Config {
            workspace: WorkspaceSettings::single(shared),
            tunnel: "tunnel_settings_fixture".into(),
            ..Config::default()
        };
        assert!(store.load().unwrap().is_none());
        let first = store.commit(config.clone(), None, false).unwrap();
        assert!(
            store
                .commit(config.clone(), None, false)
                .err()
                .unwrap()
                .is::<Conflict>()
        );

        let directory = PrivateDirectory::open(&store.path, false).unwrap();
        let lock = directory.exclusive_lock("settings.lock").unwrap();
        let other = Store {
            path: store.path.clone(),
        };
        let candidate = config.clone();
        let blocked =
            std::thread::spawn(move || other.commit(candidate, Some(first.revision), false))
                .join()
                .unwrap()
                .err()
                .unwrap();
        assert!(blocked.to_string().starts_with("SETTINGS_LOCK"));
        drop(lock);

        let mut new = config.clone();
        new.start_hidden = true;
        let second = store.commit(new, Some(first.revision), false).unwrap();
        let saved = fs::read(store.path.join(FILE)).unwrap();
        let rejected = store
            .commit(config, Some(first.revision), false)
            .err()
            .unwrap();
        assert!(rejected.is::<Conflict>());
        assert_eq!(fs::read(store.path.join(FILE)).unwrap(), saved);
        let current = store.load().unwrap().unwrap();
        assert_ne!(first.revision, second.revision);
        assert_eq!(current.revision, second.revision);
        assert!(current.config.start_hidden);
    }

    #[test]
    fn imports_isolated_v8_snapshot_without_dropping_root_exclusions() {
        let fixture = tempfile::Builder::new()
            .prefix("mcp-desktop-settings-")
            .tempdir()
            .unwrap();
        let first = fixture.path().join("first");
        let second = fixture.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let mut legacy = serde_json::json!({
            "settings_version": 8,
            "roots": [{"id":"a","name":"第一個","path":first,"excluded_names":["keep-private"]},
                {"id":"b","name":"第二個","path":second}],
            "default_root":"a","tunnel":"tunnel_example","reader":{},"access_mode":"read_only"
        });
        let draft = Config::import_snapshot(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(draft.workspace.roots[0].excluded_names, ["keep-private"]);
        assert_eq!(draft.workspace.roots[1].name.as_deref(), Some("第二個"));
        assert!(!draft.key_present());
        assert_eq!(draft.access_mode, Mode::ReadOnly);
        legacy["access_mode"] = serde_json::json!("developer_control");
        let draft = Config::import_snapshot(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        let store = Store {
            path: fixture.path().join("settings"),
        };
        store.commit(draft.clone(), None, false).unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().config.access_mode,
            Mode::DeveloperControl
        );
        let mut stored = serde_json::to_value(draft).unwrap();
        stored.as_object_mut().unwrap().remove("access_mode");
        stored["reader"] = serde_json::json!({});
        fs::write(store.path.join(FILE), serde_json::to_vec(&stored).unwrap()).unwrap();
        let loaded = store.load().unwrap().unwrap().config;
        assert_eq!(loaded.access_mode, Mode::ReadOnly);
        assert!(loaded.reader == ReaderSettings::default().normalize().unwrap());
        assert!(!loaded.auto_start && !loaded.auto_connect);
        legacy["auto_start"] = serde_json::json!(true);
        legacy["auto_connect"] = serde_json::json!(true);
        let migrated = Config::import_snapshot(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(migrated.auto_start && migrated.auto_connect && !migrated.key_present());
        let snapshot = store.load().unwrap().unwrap();
        let saved_bytes = fs::read(store.path.join(FILE)).unwrap();
        let error = store
            .commit(migrated, Some(snapshot.revision), false)
            .err()
            .unwrap();
        assert!(error.to_string().contains("LOGIN_STARTUP_UNAVAILABLE"));
        assert_eq!(fs::read(store.path.join(FILE)).unwrap(), saved_bytes);
        legacy["access_mode"] = serde_json::json!("host_control");
        assert_eq!(
            Config::import_snapshot(&serde_json::to_vec(&legacy).unwrap())
                .unwrap()
                .access_mode,
            Mode::HostControl
        );
    }

    #[test]
    fn legacy_import_keeps_retired_permissions_and_ambiguous_roots_out_of_the_draft() {
        let fixture = tempfile::Builder::new()
            .prefix("mcp-legacy-migration-")
            .tempdir()
            .unwrap();
        let shared = fixture.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let mut input = serde_json::json!({
            "settings_version": 1, "root": shared, "tunnel":"tunnel_fixture"
        });
        let import = |snapshot: &serde_json::Value| {
            Config::import_snapshot(&serde_json::to_vec(snapshot).unwrap())
        };
        let single = import(&input).unwrap();
        assert_eq!(single.workspace.default_root, "main");
        assert_eq!(single.workspace.roots[0].path, shared);
        for roots in [serde_json::json!([]), serde_json::Value::Null] {
            input["roots"] = roots;
            assert!(import(&input).is_err());
        }
        input["roots"] = serde_json::json!([{"id":"main","path":shared}]);
        assert_eq!(import(&input).unwrap().workspace.default_root, "main");
        input["default_root"] = serde_json::Value::Null;
        assert!(import(&input).is_err());
        input["default_root"] = serde_json::json!("main");
        for (version, mode) in [
            (0, "host_control"),
            (5, "developer_control"),
            (6, "developer_control"),
            (6, "host_control"),
            (8, "full_control"),
        ] {
            input["settings_version"] = serde_json::json!(version);
            input["access_mode"] = serde_json::json!(mode);
            input["power"] = serde_json::json!({"mode":"extreme"});
            let draft = import(&input).unwrap();
            assert_eq!(draft.access_mode, Mode::ReadOnly);
            assert!(serde_json::to_value(&draft).unwrap().get("power").is_none());
        }
        input["access_mode"] = serde_json::json!("developer_control");
        input["developer_toolchains"] = serde_json::json!(["C:/obsolete-toolchain"]);
        assert_eq!(import(&input).unwrap().access_mode, Mode::DeveloperControl);
        input["key_dpapi"] = serde_json::json!("synthetic marker");
        assert!(import(&input).is_err());
    }
}
