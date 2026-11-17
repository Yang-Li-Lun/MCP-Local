use windows_sys::Win32::{
    Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE},
    Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_READONLY, FILE_DISPOSITION_FLAG_DELETE,
        FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_INFO_EX,
        FILE_WRITE_ATTRIBUTES, FileDispositionInfoEx, SetFileInformationByHandle,
    },
};

use super::{
    control::{mark_delete, mutation_policy, rename_open_file, write_content},
    *,
};

const IO: &str = "VM_SPOOL_IO：無法安全讀寫 VM 暫存檔。";
const CHANGED: &str = "VM_SPOOL_CHANGED：VM 暫存目錄或檔案身分已變更。";
const CLEANUP: &str = "VM_SPOOL_CLEANUP_FAILED：無法安全清理 VM 暫存目錄。";

pub struct Directory {
    ancestors: Vec<Arc<OpenPath>>,
    folder: Option<Arc<OpenPath>>,
    name: String,
    identity: Metadata,
    removed: bool,
}

impl Directory {
    pub fn create(root: &Root) -> Result<Self> {
        let name = random_name("mcp-vm-session-", "")?;
        let destination = root.control_target(&name)?;
        destination.create_directory()?;
        let mut chain = pin_directories(&root.open(&name, false)?.chain)?;
        let folder = chain.pop().unwrap();
        mutation_policy(&folder.key)?;
        ensure!(fold(file_name(&folder.path)?) == fold(&name), "{CHANGED}");
        Ok(Self {
            identity: folder.metadata,
            ancestors: chain,
            folder: Some(folder),
            name,
            removed: false,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn under(&self, root: &OpenPath) -> bool {
        let parent = self.parent();
        parent.key == root.key && parent.metadata.same_identity(root.metadata)
    }

    fn parent(&self) -> &OpenPath {
        self.ancestors.last().expect("shared root is pinned")
    }

    fn folder(&self) -> Result<&OpenPath> {
        for ancestor in &self.ancestors {
            verify_directory(ancestor)?;
        }
        let folder = self
            .folder
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!(CHANGED))?;
        verify_directory(folder)?;
        mutation_policy(&folder.key)?;
        Ok(folder)
    }

    pub fn publish(&self, name: &str, bytes: &[u8], budget: &mut Budget) -> Result<()> {
        valid_name(name)?;
        let folder = self.folder()?;
        budget.check()?;
        let pending = random_name("pending-", ".json")?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(folder.path.join(&pending))
            .map_err(safe_io)?;
        let result = (|| {
            let mut temp =
                OpenPath::from_handle(file.try_clone().map_err(safe_io)?, Some(folder), false)?;
            ensure!(
                !temp.metadata.directory() && file_name(&temp.path)? == pending,
                "{CHANGED}"
            );
            write_content(&mut temp.file, bytes, File::sync_all).map_err(safe_io)?;
            verify_directory(folder)?;
            budget.check()?;
            rename_open_file(&temp.file, &folder.path.join(name)).map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    anyhow::anyhow!("VM_SPOOL_EXISTS：暫存封包已存在。")
                } else {
                    safe_io(error)
                }
            })
        })();
        if result.is_err() && mark_delete(&file).is_err() {
            bail!("{CLEANUP}");
        }
        result
    }

    pub fn snapshot(
        &self,
        name: &str,
        maximum: usize,
        budget: &mut Budget,
    ) -> Result<Option<Vec<u8>>> {
        valid_name(name)?;
        let folder = self.folder()?;
        budget.check()?;
        let file = match OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(folder.path.join(name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound || transient_io(&error) => {
                return Ok(None);
            }
            Err(error) => return Err(safe_io(error)),
        };
        let entry = entry_at(file, folder, name)?;
        let previous = entry.metadata;
        previous.ensure_safe(false)?;
        ensure!(!previous.directory(), "{IO}");
        ensure!(
            previous.size <= maximum as u64,
            "VM_OUTPUT_LIMIT：VM 回傳檔案超出大小上限。"
        );
        budget.can_read(previous.size as usize)?;
        let mut bytes = Vec::with_capacity(previous.size as usize);
        match (&entry.file).take(previous.size).read_to_end(&mut bytes) {
            Ok(_) => {}
            Err(error) if transient_io(&error) => return Ok(None),
            Err(error) => return Err(safe_io(error)),
        }
        budget.read(bytes.len())?;
        let current = Metadata::read(&entry.file)?;
        current.ensure_safe(false)?;
        verify_directory(folder)?;
        if !previous.same_content(current) || bytes.len() != previous.size as usize {
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    pub fn remove(&mut self, budget: &mut Budget) -> Result<()> {
        if self.removed {
            return Ok(());
        }
        if self.folder.is_none() {
            let parent = self.parent();
            let probe = OpenPath::open(&parent.path.join(&self.name), Some(parent), false, false)?;
            ensure!(probe.metadata.same_identity(self.identity), "{CHANGED}");
            self.folder = Some(Arc::new(probe.pin_directory(Some(parent))?));
        }
        let folder = self.folder()?;
        remove_contents(folder, budget)?;
        budget.check()?;
        // Release the read lock only after removing the contents. Re-open with
        // DELETE and check the identity again before touching the directory.
        self.folder.take();
        unlink(self.parent(), &self.name, self.identity)?;
        self.removed = true;
        Ok(())
    }
}

fn random_name(prefix: &str, suffix: &str) -> Result<String> {
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce)?;
    Ok(format!("{prefix}{}{suffix}", crate::listing::hex(&nonce)))
}

fn transient_io(error: &io::Error) -> bool {
    error
        .raw_os_error()
        .is_some_and(|code| matches!(code as u32, ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION))
}

fn verify_directory(folder: &OpenPath) -> Result<()> {
    let current = Metadata::read(&folder.file)?;
    current.ensure_safe(true)?;
    ensure!(
        current.directory()
            && current.same_identity(folder.metadata)
            && current.attributes == folder.metadata.attributes,
        "{CHANGED}"
    );
    Ok(())
}

fn entry_at(file: File, parent: &OpenPath, name: &str) -> Result<OpenPath> {
    let metadata = Metadata::read(&file)?;
    let opened = OpenPath::from_checked_handle(file, Some(parent), metadata)?;
    ensure!(
        fold(name) == fold(file_name(&opened.path)?) && metadata.volume == parent.metadata.volume,
        "{CHANGED}"
    );
    Ok(opened)
}

fn remove_contents(root: &OpenPath, budget: &mut Budget) -> Result<()> {
    let mut frames = vec![(None::<OpenPath>, fs::read_dir(&root.path).map_err(safe_io)?)];
    while !frames.is_empty() {
        budget.check()?;
        if let Some(next) = frames.last_mut().unwrap().1.next() {
            budget.entry()?;
            let name = next.map_err(safe_io)?.file_name();
            let name = name.to_str().ok_or_else(|| anyhow::anyhow!(CHANGED))?;
            valid_name(name)?;
            let parent = frames.last().unwrap().0.as_ref().unwrap_or(root);
            verify_directory(parent)?;
            let file = open_handle(&parent.path.join(name), false, false)?;
            let entry = entry_at(file, parent, name)?;
            if entry.metadata.directory()
                && entry.metadata.attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
            {
                let directory = entry.pin_directory(Some(parent))?;
                let entries = fs::read_dir(&directory.path).map_err(safe_io)?;
                frames.push((Some(directory), entries));
            } else {
                let expected = entry.metadata;
                drop(entry);
                unlink(parent, name, expected)?;
            }
        } else if let (Some(directory), _) = frames.pop().unwrap() {
            let name = file_name(&directory.path)?.to_owned();
            let expected = directory.metadata;
            drop(directory);
            let parent = frames.last().unwrap().0.as_ref().unwrap_or(root);
            unlink(parent, &name, expected)?;
        }
    }
    Ok(())
}

fn unlink(parent: &OpenPath, name: &str, expected: Metadata) -> Result<()> {
    verify_directory(parent)?;
    let readonly = expected.attributes & FILE_ATTRIBUTE_READONLY != 0;
    ensure!(
        !readonly
            || !expected.directory()
                && expected.links == 1
                && expected.attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "{CLEANUP}"
    );
    let access = DELETE | FILE_READ_ATTRIBUTES | if readonly { FILE_WRITE_ATTRIBUTES } else { 0 };
    let file = OpenOptions::new()
        .access_mode(access)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent.path.join(name))
        .map_err(safe_io)?;
    let entry = entry_at(file, parent, name)?;
    let current = entry.metadata;
    ensure!(
        current.same_identity(expected)
            && current.attributes == expected.attributes
            && current.links == expected.links,
        "{CHANGED}"
    );
    mutation_policy(&entry.key)?;
    if readonly {
        let disposition = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        };
        ensure!(
            unsafe {
                SetFileInformationByHandle(
                    entry.file.as_raw_handle(),
                    FileDispositionInfoEx,
                    (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
                    size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
                )
            } != 0,
            "{CLEANUP}"
        );
    } else {
        mark_delete(&entry.file).map_err(safe_io)?;
    }
    Ok(())
}
