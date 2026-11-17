use std::{
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::AsRawHandle,
    },
    path::{Component, Path, PathBuf, Prefix},
    sync::Arc,
};

use anyhow::{Result, bail, ensure};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_TYPE_DISK, FileBasicInfo, GetDriveTypeW, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFileType, GetFinalPathNameByHandleW,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, VOLUME_NAME_DOS,
    VOLUME_NAME_NT,
};
use windows_sys::{
    Win32::{
        Foundation::RPC_E_CHANGED_MODE,
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
        System::WindowsProgramming::{DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOVABLE},
        UI::Shell::{KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath},
    },
    core::GUID,
};

use crate::{
    budget::Budget,
    settings::{ReaderSettings, fold, syntax_char},
};

const PATH_ERROR: &str = "路徑不存在或無法安全存取。";
const POLICY_ERROR: &str = "不可讀取程式設定、備份及金鑰目錄。";

pub mod control;
#[allow(
    dead_code,
    reason = "Mapping verification helpers are also used by isolated acceptance tests"
)]
pub mod vm_mapping;
#[allow(
    dead_code,
    reason = "Spool verification helpers are also used by isolated acceptance tests"
)]
pub mod vm_spool;

pub fn known_folder(id: &GUID) -> Result<PathBuf> {
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    const ERROR: &str = "無法確認 Windows 已知目錄。";
    let status = unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32) };
    ensure!(status >= 0 || status == RPC_E_CHANGED_MODE, "{ERROR}");
    let _apartment = if status >= 0 { Some(Apartment) } else { None };
    let mut raw = std::ptr::null_mut();
    // Windows can return an allocated string even if the lookup fails.
    let status = unsafe {
        SHGetKnownFolderPath(
            id,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    let value = if status >= 0 && !raw.is_null() {
        (0..32768)
            .find(|&index| unsafe { *raw.add(index) == 0 })
            .map(|len| unsafe { OsString::from_wide(std::slice::from_raw_parts(raw, len)) })
    } else {
        None
    };
    unsafe { CoTaskMemFree(raw.cast()) };
    let path = PathBuf::from(value.ok_or_else(|| anyhow::anyhow!(ERROR))?);
    ensure!(path.is_absolute(), "{ERROR}");
    Ok(path)
}

fn safe_io(error: io::Error) -> anyhow::Error {
    io::Error::new(error.kind(), PATH_ERROR).into()
}

pub fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

pub fn replace_atomic(from: &Path, to: &Path) -> io::Result<()> {
    let from: Vec<_> = from.as_os_str().encode_wide().chain([0]).collect();
    let to: Vec<_> = to.as_os_str().encode_wide().chain([0]).collect();
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub attributes: u32,
    pub volume: u32,
    pub index: u64,
    pub size: u64,
    pub links: u32,
    pub created: i64,
    pub modified: i64,
    pub accessed: i64,
    pub changed: i64,
}

impl Metadata {
    fn read(file: &File) -> Result<Self> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let mut basic = FILE_BASIC_INFO::default();
        let handle = file.as_raw_handle();
        // Both calls write to initialized buffers owned by this stack frame.
        unsafe {
            ensure!(
                GetFileType(handle) == FILE_TYPE_DISK
                    && GetFileInformationByHandle(handle, &mut info) != 0
                    && GetFileInformationByHandleEx(
                        handle,
                        FileBasicInfo,
                        (&mut basic as *mut FILE_BASIC_INFO).cast(),
                        size_of::<FILE_BASIC_INFO>() as u32,
                    ) != 0,
                "{PATH_ERROR}"
            );
        }
        Ok(Self {
            attributes: basic.FileAttributes,
            volume: info.dwVolumeSerialNumber,
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
            links: info.nNumberOfLinks,
            created: basic.CreationTime,
            modified: basic.LastWriteTime,
            accessed: basic.LastAccessTime,
            changed: basic.ChangeTime,
        })
    }

    pub fn directory(self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    fn ensure_safe(self, allow_hidden: bool) -> Result<()> {
        ensure!(
            self.attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
                && (allow_hidden || self.attributes & FILE_ATTRIBUTE_HIDDEN == 0)
                && (self.directory() || self.links == 1),
            "不允許讀取隱藏路徑、連結、重新解析點或多重硬連結檔案。"
        );
        Ok(())
    }

    fn same_identity(self, other: Self) -> bool {
        (self.volume, self.index) == (other.volume, other.index)
    }

    pub fn same_content(self, other: Self) -> bool {
        (
            self.size,
            self.created,
            self.modified,
            self.changed,
            self.links,
            self.attributes,
        ) == (
            other.size,
            other.created,
            other.modified,
            other.changed,
            other.links,
            other.attributes,
        ) && self.same_identity(other)
    }
}

fn final_path(file: &File, flags: u32) -> Result<PathBuf> {
    let mut buffer = vec![0u16; 512];
    loop {
        // Windows returns the required buffer size when the current one is too small.
        let len = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                flags,
            )
        } as usize;
        ensure!((1..=32768).contains(&len), "{PATH_ERROR}");
        if len < buffer.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buffer[..len])));
        }
        buffer.resize(len + 1, 0);
    }
}

fn open_handle(path: &Path, contents: bool, follow: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS
                | if follow {
                    0
                } else {
                    FILE_FLAG_OPEN_REPARSE_POINT
                },
        )
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    if contents {
        options.read(true);
    } else {
        options.access_mode(FILE_READ_ATTRIBUTES);
    }
    options.open(path).map_err(safe_io)
}

fn path_key(path: &Path) -> Result<String> {
    let name = path.to_str().ok_or_else(|| anyhow::anyhow!(PATH_ERROR))?;
    Ok(name.trim_end_matches('\\').to_lowercase())
}

fn within(path: &str, base: &str) -> bool {
    path == base
        || path
            .strip_prefix(base)
            .is_some_and(|suffix| suffix.starts_with('\\'))
}

fn protected_path(path: &Path) -> Result<String> {
    let mut current = path;
    let mut suffix = Vec::new();
    loop {
        match fs::symlink_metadata(current) {
            Ok(_) => {
                let mut final_name =
                    final_path(&open_handle(current, false, true)?, VOLUME_NAME_NT)?;
                for component in suffix.into_iter().rev() {
                    final_name.push(component);
                }
                return path_key(&final_name);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                suffix.push(
                    current
                        .file_name()
                        .ok_or_else(|| anyhow::anyhow!(POLICY_ERROR))?,
                );
                current = current
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!(POLICY_ERROR))?;
            }
            Err(_) => bail!("{POLICY_ERROR}"),
        }
    }
}

fn protected_paths() -> Result<[String; 2]> {
    let local =
        PathBuf::from(env::var_os("LOCALAPPDATA").ok_or_else(|| anyhow::anyhow!(POLICY_ERROR))?);
    Ok([
        protected_path(&local.join("MCP-Local"))?,
        protected_path(&local.join("MCP-Local-Rust"))?,
    ])
}

pub fn valid_name(name: &str) -> Result<()> {
    let device = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let numbered_device = ["COM", "LPT"].iter().any(|prefix| {
        device.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    ensure!(
        !name.is_empty()
            && !name.ends_with([' ', '.'])
            && !name.contains(syntax_char)
            && !matches!(
                device.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            )
            && !numbered_device,
        "不允許特殊裝置名稱或含糊路徑語法。"
    );
    Ok(())
}

fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!(PATH_ERROR))
}

struct OpenPath {
    file: File,
    path: PathBuf,
    key: String,
    metadata: Metadata,
}

impl OpenPath {
    fn open(
        path: &Path,
        parent: Option<&Self>,
        contents: bool,
        allow_hidden: bool,
    ) -> Result<Self> {
        let file = open_handle(path, contents, false)?;
        Self::from_handle(file, parent, allow_hidden)
    }

    fn from_handle(file: File, parent: Option<&Self>, allow_hidden: bool) -> Result<Self> {
        let metadata = Metadata::read(&file)?;
        metadata.ensure_safe(allow_hidden)?;
        Self::from_checked_handle(file, parent, metadata)
    }

    fn from_checked_handle(file: File, parent: Option<&Self>, metadata: Metadata) -> Result<Self> {
        let key = path_key(&final_path(&file, VOLUME_NAME_NT)?)?;
        if let Some(parent) = parent {
            let expected_parent = key.rsplit_once('\\').map(|(path, _)| path);
            ensure!(
                expected_parent == Some(parent.key.as_str()),
                "路徑超出共享資料夾。"
            );
        }
        let path = final_path(&file, VOLUME_NAME_DOS)?;
        Ok(Self {
            file,
            path,
            key,
            metadata,
        })
    }

    fn pin_directory(&self, parent: Option<&Self>) -> Result<Self> {
        // A metadata-only open is exempt from sharing checks. Request directory
        // read access so the handle holds the no-delete share for its lifetime.
        let pinned = Self::open(&self.path, parent, true, true)?;
        ensure!(
            pinned.metadata.directory()
                && pinned.key == self.key
                && pinned.metadata.same_identity(self.metadata)
                && pinned.metadata.attributes == self.metadata.attributes,
            "路徑安全屬性已變更。"
        );
        Ok(pinned)
    }
}

fn pin_directories(chain: &[Arc<OpenPath>]) -> Result<Vec<Arc<OpenPath>>> {
    let mut pinned = Vec::with_capacity(chain.len());
    for directory in chain {
        pinned.push(Arc::new(
            directory.pin_directory(pinned.last().map(Arc::as_ref))?,
        ));
    }
    Ok(pinned)
}

fn absolute_parts(path: &Path) -> Result<(PathBuf, Vec<String>)> {
    ensure!(path.is_absolute(), "設定中的資料夾必須使用絕對路徑。");
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        bail!("{PATH_ERROR}")
    };
    let anchor = match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            format!(r"\\?\{}:\", char::from(letter))
        }
        Prefix::UNC(host, share) | Prefix::VerbatimUNC(host, share) => {
            let host = host.to_str().ok_or_else(|| anyhow::anyhow!(PATH_ERROR))?;
            let share = share.to_str().ok_or_else(|| anyhow::anyhow!(PATH_ERROR))?;
            valid_name(host)?;
            valid_name(share)?;
            format!(r"\\?\UNC\{host}\{share}\")
        }
        _ => bail!("{PATH_ERROR}"),
    };
    ensure!(
        components.next() == Some(Component::RootDir),
        "{PATH_ERROR}"
    );
    let mut parts = Vec::new();
    for part in components {
        let Component::Normal(name) = part else {
            bail!("{PATH_ERROR}")
        };
        let name = name.to_str().ok_or_else(|| anyhow::anyhow!(PATH_ERROR))?;
        valid_name(name)?;
        parts.push(name.to_owned());
    }
    Ok((anchor.into(), parts))
}

fn local_disk(letter: u8, allow_optical: bool) -> bool {
    let root = [u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0];
    let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
    matches!(kind, DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_RAMDISK)
        || allow_optical && kind == DRIVE_CDROM
}

fn open_root(path: &Path) -> Result<Vec<Arc<OpenPath>>> {
    let (anchor, parts) = absolute_parts(path)?;
    let mut chain = vec![Arc::new(OpenPath::open(&anchor, None, false, true)?)];
    for (index, component) in parts.iter().enumerate() {
        let parent = chain.last().unwrap();
        let last = index + 1 == parts.len();
        let opened = OpenPath::open(&parent.path.join(component), Some(parent), false, !last)?;
        ensure!(opened.metadata.directory(), "指定的共享資料夾不存在。");
        ensure!(
            !last || !file_name(&opened.path)?.starts_with('.'),
            "共享資料夾不可為隱藏路徑。"
        );
        chain.push(Arc::new(opened));
    }
    Ok(chain)
}

pub struct Root {
    path: PathBuf,
    key: String,
    identity: Metadata,
    pub settings: ReaderSettings,
}

pub struct PrivateDirectory {
    chain: Vec<Arc<OpenPath>>,
}

pub fn registered_package_image(path: &Path) -> Result<(File, PathBuf)> {
    let (mut expected, parts) = absolute_parts(path)?;
    ensure!(
        matches!(expected.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::VerbatimDisk(_)))
            && !parts.is_empty(),
        "{PATH_ERROR}"
    );
    expected.extend(parts);
    // A user can traverse WindowsApps without being allowed to open that directory.
    // Verify the canonical name and attributes on the pinned image itself.
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&expected)
        .map_err(safe_io)?;
    let opened = OpenPath::from_handle(file, None, false)?;
    ensure!(
        !opened.metadata.directory() && path_key(&opened.path)? == path_key(&expected)?,
        "{PATH_ERROR}"
    );
    Ok((opened.file, opened.path))
}

impl PrivateDirectory {
    pub fn open(path: &Path, create: bool) -> Result<Self> {
        let parent = path.parent().ok_or_else(|| anyhow::anyhow!(PATH_ERROR))?;
        let ancestors = open_root(parent)?;
        if create {
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(safe_io(error)),
            }
        }
        let name = file_name(path)?;
        let last = ancestors.last().unwrap();
        let opened = OpenPath::open(&last.path.join(name), Some(last), false, false)?;
        ensure!(opened.metadata.directory(), "{PATH_ERROR}");
        let mut chain = ancestors;
        chain.push(Arc::new(opened));
        Ok(Self { chain })
    }

    #[cfg(feature = "desktop")]
    pub fn pin(mut self) -> Result<Self> {
        self.chain = pin_directories(&self.chain)?;
        Ok(self)
    }

    pub fn path(&self) -> &Path {
        &self.chain.last().unwrap().path
    }

    pub fn regular(&self, name: &str) -> Result<(File, PathBuf)> {
        self.open_regular(name, false)
    }

    #[cfg(feature = "desktop")]
    pub fn consume_regular(
        &self,
        name: &str,
        maximum: usize,
        action: impl FnOnce(&[u8]) -> Result<()>,
    ) -> Result<()> {
        use windows_sys::Win32::{Foundation::GENERIC_READ, Storage::FileSystem::DELETE};
        valid_name(name)?;
        let parent = self.chain.last().unwrap();
        let file = OpenOptions::new()
            .access_mode(GENERIC_READ | DELETE)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(parent.path.join(name))
            .map_err(safe_io)?;
        let opened = OpenPath::from_handle(file, Some(parent), false)?;
        ensure!(
            !opened.metadata.directory() && file_name(&opened.path)?.eq_ignore_ascii_case(name),
            "{PATH_ERROR}"
        );
        let mut bytes = Vec::new();
        (&opened.file)
            .take((maximum + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= maximum, "私有檔案超出容量限制。");
        action(&bytes)?;
        // Delete the exclusively pinned object, never a pathname replacement.
        control::mark_delete(&opened.file)?;
        Ok(())
    }

    #[cfg(feature = "desktop")]
    pub fn exclusive_lock(&self, name: &str) -> Result<File> {
        self.open_regular(name, true).map(|(file, _)| file)
    }

    fn open_regular(&self, name: &str, exclusive: bool) -> Result<(File, PathBuf)> {
        valid_name(name)?;
        let parent = self.chain.last().unwrap();
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(if exclusive { 0 } else { FILE_SHARE_READ });
        if exclusive {
            options.write(true).create(true).truncate(false);
        }
        let file = options.open(parent.path.join(name)).map_err(safe_io)?;
        let opened = OpenPath::from_handle(file, Some(parent), false)?;
        ensure!(
            !opened.metadata.directory() && file_name(&opened.path)?.eq_ignore_ascii_case(name),
            "{PATH_ERROR}"
        );
        Ok((opened.file, opened.path))
    }
}

pub struct Access {
    chain: Vec<Arc<OpenPath>>,
    protected: [String; 2],
    pub relative: String,
}

impl Access {
    fn target(&self) -> &OpenPath {
        self.chain.last().unwrap()
    }

    pub fn metadata(&self) -> Metadata {
        self.target().metadata
    }

    pub fn read(&self, offset: u64, length: usize, budget: &mut Budget) -> Result<Vec<u8>> {
        let target = self.target();
        ensure!(!target.metadata.directory(), "目標必須是一般檔案。");
        let mut file = &target.file;
        let length = length.min(target.metadata.size.saturating_sub(offset) as usize);
        budget.can_read(length)?;
        let mut data = Vec::with_capacity(length);
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| anyhow::anyhow!(PATH_ERROR))?;
        let mut buffer = [0u8; 65536];
        while data.len() < length {
            budget.check()?;
            let remaining = (length - data.len()).min(buffer.len());
            let read = file
                .read(&mut buffer[..remaining])
                .map_err(|_| anyhow::anyhow!(PATH_ERROR))?;
            ensure!(read != 0, "讀取期間檔案已變更，請重試。");
            budget.read(read)?;
            data.extend_from_slice(&buffer[..read]);
        }
        self.verify()?;
        Ok(data)
    }

    pub fn verify(&self) -> Result<()> {
        for opened in &self.chain {
            let now = Metadata::read(&opened.file)?;
            now.ensure_safe(true)?;
            ensure!(
                opened.metadata.same_identity(now)
                    && (!opened.metadata.directory()
                        || opened.metadata.attributes == now.attributes),
                "路徑安全屬性已變更。"
            );
        }
        let target = self.target();
        ensure!(
            target.metadata.same_content(Metadata::read(&target.file)?),
            "讀取期間檔案已變更，請重試。"
        );
        check_policy(&target.key, &protected_paths()?)?;
        Ok(())
    }
}

fn check_policy(key: &str, protected: &[String]) -> Result<()> {
    ensure!(
        !protected.iter().any(|path| within(key, path)),
        "{POLICY_ERROR}"
    );
    Ok(())
}

impl Root {
    pub fn new(path: &Path, settings: ReaderSettings) -> Result<Self> {
        let chain = open_root(path)?;
        let root = chain.last().unwrap();
        for protected in protected_paths()? {
            ensure!(
                !within(&root.key, &protected)
                    && (chain.len() == 1 || !within(&protected, &root.key)),
                "共享範圍不可與程式設定、備份及金鑰目錄重疊。"
            );
        }
        if let Some(home) = env::var_os("USERPROFILE") {
            ensure!(
                protected_path(Path::new(&home))? != root.key,
                "不可共享整個使用者家目錄。"
            );
        }
        Ok(Self {
            path: path.to_owned(),
            key: root.key.clone(),
            identity: root.metadata,
            settings,
        })
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        within(&self.key, &other.key) || within(&other.key, &self.key)
    }

    fn name_allowed(&self, name: &str) -> Result<()> {
        valid_name(name)?;
        ensure!(
            !name.starts_with('.')
                && self
                    .settings
                    .excluded_names
                    .binary_search(&fold(name))
                    .is_err(),
            "此路徑已被安全或產物排除規則封鎖。"
        );
        Ok(())
    }

    pub fn open(&self, relative: &str, contents: bool) -> Result<Access> {
        ensure!(
            !relative.is_empty()
                && relative.chars().count() <= 4096
                && !relative.starts_with(['\\', '/'])
                && !relative.contains([':', '\0']),
            "請提供共享資料夾內的相對路徑。"
        );
        let parts: Vec<_> = relative
            .split(['\\', '/'])
            .filter(|part| !matches!(*part, "." | ""))
            .collect();
        for part in &parts {
            self.name_allowed(part)?;
        }
        let protected = protected_paths()?;
        let mut chain = open_root(&self.path)?;
        let root = chain.last().unwrap();
        ensure!(
            root.key == self.key && root.metadata.same_identity(self.identity),
            "共享資料夾身分已變更，請重新啟動服務。"
        );
        check_policy(&root.key, &protected)?;
        let mut names = Vec::with_capacity(parts.len());
        for (index, part) in parts.iter().enumerate() {
            let parent = chain.last().unwrap();
            ensure!(parent.metadata.directory(), "{PATH_ERROR}");
            let candidate = format!("{}\\{}", parent.key, part.to_lowercase());
            check_policy(&candidate, &protected)?;
            let handle = OpenPath::open(
                &parent.path.join(part),
                Some(parent),
                contents && index + 1 == parts.len(),
                false,
            )?;
            check_policy(&handle.key, &protected)?;
            let actual = file_name(&handle.path)?;
            self.name_allowed(actual)?;
            names.push(actual.to_owned());
            chain.push(Arc::new(handle));
        }
        let relative = if names.is_empty() {
            ".".into()
        } else {
            names.join("/")
        };
        Ok(Access {
            chain,
            protected,
            relative,
        })
    }

    pub fn is_text(&self, path: &str) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        let suffix = Path::new(name).extension().and_then(|ext| ext.to_str());
        self.settings.text_names.binary_search(&fold(name)).is_ok()
            || suffix.is_some_and(|ext| {
                self.settings
                    .extensions
                    .binary_search(&format!(".{}", fold(ext)))
                    .is_ok()
            })
    }

    pub fn open_entry(&self, folder: &Access, entry: &Entry) -> Result<Access> {
        let parent = folder.target();
        let handle = OpenPath::open(&parent.path.join(&entry.name), Some(parent), true, false)?;
        let protected = protected_paths()?;
        check_policy(&handle.key, &protected)?;
        self.name_allowed(file_name(&handle.path)?)?;
        ensure!(
            entry.metadata.same_content(handle.metadata),
            "檔案在掃描期間變更，請重試。"
        );
        let mut chain = folder.chain.clone();
        chain.push(Arc::new(handle));
        Ok(Access {
            chain,
            protected,
            relative: entry.path.clone(),
        })
    }

    pub fn visit(
        &self,
        directory: &str,
        shallow: bool,
        strict: bool,
        budget: &mut Budget,
        mut visit: impl FnMut(&Access, Entry, &mut Budget, &mut Scan) -> Result<bool>,
    ) -> Result<Scan> {
        let start = self.open(directory, false)?;
        ensure!(start.metadata().directory(), "列舉起點必須是資料夾。");
        let mut scan = Scan::default();
        let mut pending_bytes = path_cost(&start.relative);
        let mut pending = vec![start.relative.clone()];
        let mut file_count = 0;
        while let Some(directory) = pending.pop() {
            budget.check()?;
            pending_bytes -= path_cost(&directory);
            let result = self.open(&directory, false).and_then(|folder| {
                let entries =
                    fs::read_dir(&folder.target().path).map_err(|_| anyhow::anyhow!(PATH_ERROR))?;
                Ok((folder, entries))
            });
            let (folder, entries) = match result {
                Ok(result) => result,
                Err(error) if strict => return Err(error),
                Err(_) => {
                    scan.skipped_entries += 1;
                    continue;
                }
            };
            let mut names = Vec::new();
            let mut batch_bytes = 0;
            for entry in entries.take(self.settings.max_scan_entries - scan.scanned_entries + 1) {
                budget.entry()?;
                match entry {
                    Ok(entry) => {
                        let name = entry.file_name();
                        let characters = name.len();
                        batch_bytes +=
                            256 + 4 * (2 * characters + folder.target().path.as_os_str().len());
                        if batch_bytes > 8 * 1024 * 1024 {
                            scan.truncate("DIRECTORY_BYTES");
                            break;
                        }
                        let name = name.into_string().ok();
                        names.push((name.as_deref().map(fold), name));
                    }
                    Err(_) if strict => bail!("{PATH_ERROR}"),
                    Err(_) => names.push((None, None)),
                }
            }
            names.sort_unstable();
            for (_, name) in names {
                budget.check()?;
                if scan.scanned_entries == self.settings.max_scan_entries {
                    scan.truncated = true;
                    return Ok(scan);
                }
                scan.scanned_entries += 1;
                let entry = match name.as_deref().map(|name| self.child_entry(&folder, name)) {
                    Some(Ok(entry)) => entry,
                    Some(Err(error)) if strict && error.downcast_ref::<io::Error>().is_some() => {
                        return Err(error);
                    }
                    _ => {
                        scan.skipped_entries += 1;
                        continue;
                    }
                };
                if entry.metadata.directory() && !shallow {
                    pending_bytes += path_cost(&entry.path);
                    if pending_bytes > 16 * 1024 * 1024 {
                        scan.truncate("PENDING_DIRECTORY_BYTES");
                        return Ok(scan);
                    }
                    pending.push(entry.path);
                    continue;
                }
                if !entry.metadata.directory() {
                    file_count += 1;
                    if file_count > self.settings.max_scan_files {
                        scan.truncated = true;
                        return Ok(scan);
                    }
                }
                if !visit(&folder, entry, budget, &mut scan)? {
                    return Ok(scan);
                }
            }
            if scan.truncated {
                break;
            }
        }
        Ok(scan)
    }

    fn child_entry(&self, folder: &Access, name: &str) -> Result<Entry> {
        self.name_allowed(name)?;
        let parent = folder.target();
        check_policy(
            &format!("{}\\{}", parent.key, name.to_lowercase()),
            &folder.protected,
        )?;
        let child = OpenPath::open(&parent.path.join(name), Some(parent), false, false)?;
        check_policy(&child.key, &folder.protected)?;
        let name = file_name(&child.path)?;
        self.name_allowed(name)?;
        let path = if folder.relative == "." {
            name.to_owned()
        } else {
            format!("{}/{name}", folder.relative)
        };
        ensure!(path.chars().count() <= 4096, "相對路徑過長。");
        Ok(Entry {
            path,
            name: name.to_owned(),
            metadata: child.metadata,
        })
    }
}

fn path_cost(path: &str) -> usize {
    512 + 4 * path.len()
}

pub struct Entry {
    pub path: String,
    pub name: String,
    pub metadata: Metadata,
}

#[derive(Clone, Default)]
pub struct Scan {
    pub scanned_entries: usize,
    pub skipped_entries: usize,
    pub truncated: bool,
    pub reason: Option<&'static str>,
}

impl Scan {
    pub fn truncate(&mut self, reason: &'static str) {
        self.truncated = true;
        self.reason = Some(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::Metadata;

    #[test]
    fn metadata_reuse_tracks_creation_but_not_access_time() {
        let before = Metadata {
            attributes: 0,
            volume: 1,
            index: 2,
            size: 3,
            links: 1,
            created: 4,
            modified: 5,
            accessed: 6,
            changed: 7,
        };
        assert!(!before.same_content(Metadata {
            created: 8,
            ..before
        }));
        assert!(before.same_content(Metadata {
            accessed: 8,
            ..before
        }));
    }
}
