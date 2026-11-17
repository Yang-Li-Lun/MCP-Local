use std::{
    env, fs,
    io::Read,
    mem::{offset_of, size_of},
    ptr,
};

use anyhow::{Result, bail, ensure};
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE},
    Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_READONLY, FILE_DISPOSITION_INFO, FILE_RENAME_INFO,
        FileDispositionInfo, FileRenameInfo, SetFileInformationByHandle,
    },
    System::SystemInformation::GetSystemWindowsDirectoryW,
};

use super::*;

const IO: &str = "CONTROL_IO：無法安全存取檔案、格式不符或路徑被占用。";
pub const ROLLBACK_FAILED: &str = "CONTROL_ROLLBACK_FAILED：無法確認檔案回復，寫入授權已撤銷。";
const PROTECTED: &str = "CONTROL_PLANE_PROTECTED：禁止修改服務、系統執行環境、設定目錄及其祖先。";

pub struct Target {
    parent: Access,
    name: String,
    path: PathBuf,
    relative: String,
}

pub struct LockedFile {
    opened: OpenPath,
}

impl Access {
    pub fn verify_host_directory(&self) -> Result<()> {
        ensure!(self.metadata().directory(), "HOST_CWD");
        for opened in &self.chain {
            let now = Metadata::read(&opened.file)?;
            now.ensure_safe(true)?;
            ensure!(
                opened.metadata.same_identity(now) && opened.metadata.attributes == now.attributes,
                "HOST_CWD_CHANGED"
            );
        }
        check_policy(&self.target().key, &protected_paths()?)
    }
}

fn relative_parts(path: &str) -> Result<Vec<&str>> {
    ensure!(
        !path.is_empty()
            && path.chars().count() <= 4096
            && !path.starts_with(['\\', '/'])
            && !path.contains([':', '\0']),
        "請使用共享根內的相對路徑。"
    );
    let parts: Vec<_> = path
        .split(['\\', '/'])
        .filter(|name| !matches!(*name, "" | "."))
        .collect();
    ensure!(!parts.contains(&".."), "不允許向上跳出共享根。");
    Ok(parts)
}

impl Root {
    pub fn host_working_directory(&self, path: &str) -> Result<(PathBuf, Access)> {
        let absolute = self.host_absolute(path)?;
        let reader = self.host_reader(&absolute)?;
        let mut directory = reader.open(".", false)?;
        ensure!(directory.metadata().directory(), "HOST_CWD：需要既有目錄。");
        directory.chain = pin_directories(&directory.chain)?;
        directory.verify()?;
        Ok((absolute, directory))
    }

    pub fn control_target(&self, path: &str) -> Result<Target> {
        let (anchor, _) = absolute_parts(&self.path)?;
        ensure!(
            matches!(anchor.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_))),
            "CONTROL_PATH：僅允許修改本機磁碟。"
        );
        let parts = relative_parts(path)?;
        let (name, parent) = parts
            .split_last()
            .ok_or_else(|| anyhow::anyhow!("不可操作根目錄。"))?;
        self.name_allowed(name)?;
        let mut parent = self.open(
            &if parent.is_empty() {
                ".".to_owned()
            } else {
                parent.join("/")
            },
            false,
        )?;
        ensure!(parent.metadata().directory(), "父路徑必須為現有目錄。");
        parent.chain = pin_directories(&parent.chain)?;
        let relative = if parent.relative == "." {
            (*name).to_owned()
        } else {
            format!("{}/{name}", parent.relative)
        };
        let target = Target {
            path: parent.target().path.join(name),
            parent,
            name: (*name).into(),
            relative,
        };
        target.verify_location()?;
        Ok(target)
    }

    pub fn host_absolute(&self, path: &str) -> Result<PathBuf> {
        const LOCAL_PATH: &str = "HOST_PATH：僅接受本機磁碟的絕對路徑。";
        ensure!(
            !path.is_empty() && path.chars().count() <= 4096 && !path.contains(|ch| ch < ' '),
            "HOST_PATH：路徑格式無效。"
        );
        let windows = path.replace('/', "\\");
        let value = Path::new(&windows);
        ensure!(
            !windows.split('\\').any(|part| part == ".."),
            "HOST_PATH：不允許向上跳轉。"
        );
        let absolute = if value.is_absolute() {
            ensure!(
                matches!(value.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))),
                "{LOCAL_PATH}"
            );
            value.to_owned()
        } else {
            let parts = relative_parts(&windows)?;
            self.open(".", false)?;
            let mut absolute = self.path.clone();
            for name in parts {
                absolute.push(name);
            }
            absolute
        };
        let (anchor, parts) = absolute_parts(&absolute)?;
        let Some(Component::Prefix(prefix)) = anchor.components().next() else {
            bail!("{LOCAL_PATH}");
        };
        let Prefix::VerbatimDisk(letter) = prefix.kind() else {
            bail!("{LOCAL_PATH}");
        };
        ensure!(local_disk(letter, true), "{LOCAL_PATH}");
        for name in parts {
            self.name_allowed(&name)?;
        }
        Ok(absolute.components().collect())
    }

    pub fn host_reader(&self, path: &Path) -> Result<Root> {
        let reader = Root::new(path, self.settings.clone())?;
        let pinned = reader.open(".", false)?;
        for directory in pinned.chain.iter().skip(1) {
            self.name_allowed(file_name(&directory.path)?)?;
        }
        Ok(reader)
    }
}

fn system_path() -> Result<PathBuf> {
    let mut path = vec![0u16; 32768];
    let count =
        unsafe { GetSystemWindowsDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
    ensure!(count > 0 && count < path.len(), "{PROTECTED}");
    Ok(PathBuf::from(OsString::from_wide(&path[..count])))
}

pub(super) fn mutation_policy(key: &str) -> Result<()> {
    let exe = env::current_exe().map_err(|_| anyhow::anyhow!(PROTECTED))?;
    let program = exe.parent().ok_or_else(|| anyhow::anyhow!(PROTECTED))?;
    let state = protected_paths()?;
    let protected = state
        .into_iter()
        .chain([protected_path(program)?, protected_path(&system_path()?)?]);
    ensure!(
        !protected
            .into_iter()
            .any(|path| within(key, &path) || within(&path, key)),
        "{PROTECTED}"
    );
    Ok(())
}

impl Target {
    pub fn relative(&self) -> &str {
        &self.relative
    }

    pub fn verify_location(&self) -> Result<()> {
        self.parent.verify()?;
        let parent = self.parent.target();
        mutation_policy(&format!("{}\\{}", parent.key, self.name.to_lowercase()))
    }

    fn validate_open(&self, file: File) -> Result<LockedFile> {
        let opened = OpenPath::from_handle(file, Some(self.parent.target()), false)?;
        ensure!(
            !opened.metadata.directory()
                && opened.metadata.attributes & FILE_ATTRIBUTE_READONLY == 0
                && fold(file_name(&opened.path)?) == fold(&self.name),
            "拒絕目錄、唯讀屬性或替代名稱。"
        );
        mutation_policy(&opened.key)?;
        Ok(LockedFile { opened })
    }

    pub fn existing(&self, write: bool, delete: bool) -> Result<Option<LockedFile>> {
        self.verify_location()?;
        let access =
            GENERIC_READ | if write { GENERIC_WRITE } else { 0 } | if delete { DELETE } else { 0 };
        let outcome = OpenOptions::new()
            .access_mode(access)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(&self.path);
        match outcome {
            Ok(file) => self.validate_open(file).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(_) => bail!("FILE_BUSY：檔案無法獨占開啟。"),
        }
    }

    pub fn create_file(&self) -> Result<LockedFile> {
        self.verify_location()?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&self.path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    anyhow::anyhow!("FILE_EXISTS：建立模式不可覆寫檔案。")
                } else {
                    anyhow::anyhow!(IO)
                }
            })?;
        // The create-new handle is the only object that can be deleted on a failed commit.
        let checked = file
            .try_clone()
            .map_err(|_| anyhow::anyhow!(IO))
            .and_then(|probe| self.validate_open(probe));
        match checked {
            Ok(checked) => Ok(checked),
            Err(error) => {
                if mark_delete(&file).is_err() {
                    bail!("{ROLLBACK_FAILED}");
                }
                Err(error)
            }
        }
    }

    pub fn create_directory(&self) -> Result<()> {
        self.verify_location()?;
        fs::create_dir(&self.path).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                anyhow::anyhow!("FILE_EXISTS：目錄已存在。")
            } else {
                anyhow::anyhow!(IO)
            }
        })
    }

    pub fn require_absent(&self, conflict: &str) -> Result<()> {
        self.verify_location()?;
        match OpenPath::open(&self.path, Some(self.parent.target()), false, false) {
            Ok(opened) => {
                ensure!(
                    opened.metadata.attributes & FILE_ATTRIBUTE_READONLY == 0,
                    "來源具有唯讀屬性。"
                );
                mutation_policy(&opened.key)?;
                bail!("{conflict}")
            }
            Err(error) if missing(&error) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl LockedFile {
    pub fn read(&mut self, maximum: usize, budget: &mut Budget) -> Result<Vec<u8>> {
        let file = &mut self.opened.file;
        let expected = self.opened.metadata;
        ensure!(
            expected.size <= maximum as u64,
            "WRITE_LIMIT：檔案超過寫入上限。"
        );
        budget.can_read(expected.size as usize)?;
        file.seek(SeekFrom::Start(0))
            .map_err(|_| anyhow::anyhow!(IO))?;
        let mut raw = Vec::with_capacity(expected.size as usize);
        file.take((maximum + 1) as u64)
            .read_to_end(&mut raw)
            .map_err(|_| anyhow::anyhow!(IO))?;
        budget.read(raw.len())?;
        ensure!(
            raw.len() == expected.size as usize,
            "CONTENT_CONFLICT：檔案已變更。"
        );
        self.verify()?;
        Ok(raw)
    }

    fn verify(&self) -> Result<()> {
        let current = Metadata::read(&self.opened.file)?;
        current.ensure_safe(false)?;
        ensure!(
            self.opened.metadata.same_content(current)
                && current.attributes & FILE_ATTRIBUTE_READONLY == 0,
            "CONTENT_CONFLICT：檔案或安全屬性已變更。"
        );
        mutation_policy(&self.opened.key)
    }

    pub fn commit_created(mut self, bytes: &[u8]) -> Result<()> {
        let result = self.verify().and_then(|_| {
            write_content(&mut self.opened.file, bytes, File::sync_all)
                .map_err(|_| anyhow::anyhow!(IO))
        });
        if result.is_err() && mark_delete(&self.opened.file).is_err() {
            bail!("{ROLLBACK_FAILED}");
        }
        result
    }

    pub fn rewrite(mut self, bytes: &[u8], original: &[u8]) -> Result<()> {
        self.verify()?;
        replace_content(&mut self.opened.file, bytes, original, File::sync_all)
    }

    pub fn move_to(self, destination: &Target) -> Result<()> {
        self.verify()?;
        destination.verify_location()?;
        ensure!(
            self.opened.metadata.volume == destination.parent.metadata().volume,
            "HOST_MOVE_VOLUME：移動僅支援同一磁碟。"
        );
        if let Err(error) = rename_open_file(&self.opened.file, &destination.path) {
            if error.kind() == io::ErrorKind::AlreadyExists {
                bail!("FILE_EXISTS：移動不覆寫現有路徑。");
            }
            bail!("{IO}");
        }
        Ok(())
    }

    pub fn delete(self) -> Result<()> {
        self.verify()?;
        mark_delete(&self.opened.file).map_err(|_| anyhow::anyhow!(IO))
    }
}

pub(super) fn rename_open_file(file: &File, destination: &Path) -> io::Result<()> {
    let mut name: Vec<u16> = destination.as_os_str().encode_wide().collect();
    let len = (name.len() * size_of::<u16>()) as u32;
    name.push(0);
    let offset = offset_of!(FILE_RENAME_INFO, FileName);
    let size = (offset + name.len() * size_of::<u16>()).max(size_of::<FILE_RENAME_INFO>());
    let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
    let raw = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // The zeroed buffer leaves ReplaceIfExists false and includes the trailing UTF-16 name.
    let renamed = unsafe {
        (*raw).FileNameLength = len;
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            buffer.as_mut_ptr().cast::<u8>().add(offset).cast(),
            name.len(),
        );
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfo,
            raw.cast(),
            size as u32,
        )
    };
    if renamed == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn mark_delete(file: &File) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn write_content(
    file: &mut File,
    bytes: &[u8],
    flush: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Write;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.set_len(bytes.len() as u64)?;
    flush(file)
}

fn replace_content(
    file: &mut File,
    bytes: &[u8],
    original: &[u8],
    mut flush: impl FnMut(&File) -> io::Result<()>,
) -> Result<()> {
    if write_content(file, bytes, &mut flush).is_err() {
        if write_content(file, original, flush).is_err() {
            bail!("{ROLLBACK_FAILED}");
        }
        bail!("{IO}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn failed_control_commit_restores_every_original_byte() {
        let mut fixture = tempfile::Builder::new()
            .prefix("mcp-commit-")
            .tempfile()
            .unwrap();
        let original = b"\xef\xbb\xbfOriginal\r\n\r\n";
        fixture.write_all(original).unwrap();
        let mut flushes = 0;
        let error = replace_content(fixture.as_file_mut(), b"x", original, |file| {
            flushes += 1;
            if flushes == 1 {
                return Err(io::Error::other("injected flush failure"));
            }
            file.sync_all()
        })
        .unwrap_err();
        assert_eq!(error.to_string(), IO);
        assert_eq!(flushes, 2);
        assert_eq!(fs::read(fixture.path()).unwrap(), original);

        let error = replace_content(fixture.as_file_mut(), b"new", original, |_| {
            Err(io::Error::other("injected commit and rollback failure"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), ROLLBACK_FAILED);
    }
}
