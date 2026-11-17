use std::{collections::HashMap, fmt::Write};

use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86,
    FOLDERID_Windows,
};

use super::*;

const MAX_TOOLCHAINS: usize = 16;
const MAX_TREE_ENTRIES: usize = 200_000;
const MAPPING_PATH: &str = "VM_MAPPING_PATH：映射需要無連結的本機絕對目錄。";
const MAPPING_ALIAS: &str = "VM_MAPPING_ALIAS：映射樹包含重新解析點、向外硬連結或變更中的檔案。";
const MAPPING_LIMIT: &str = "VM_MAPPING_LIMIT：映射樹超過檢查上限。";
const MAPPING_PROTECTED: &str =
    "VM_MAPPING_PROTECTED：映射不得涵蓋程式狀態、憑證或受保護的執行環境。";
const TOOLCHAIN_SCOPE: &str = "VM_TOOLCHAIN_SCOPE：不可映射磁碟或整個使用者／系統目錄。";
const TOOLCHAIN_CREDENTIALS: &str = "VM_TOOLCHAIN_CREDENTIALS：工具鏈不可含主機憑證或設定。";
const CREDENTIAL_NAMES: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    ".kube",
    ".docker",
    ".config",
    ".git",
    ".gitconfig",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".env",
    "credentials",
    "credentials.json",
    "credentials.toml",
    "api-key.dpapi",
    "id_rsa",
    "id_ed25519",
];

struct KnownFolders {
    program_files: PathBuf,
    program_files_x86: PathBuf,
    local_app_data: PathBuf,
    profile: PathBuf,
    windows: PathBuf,
    system_drive: PathBuf,
}

impl KnownFolders {
    fn read() -> Result<Self> {
        let windows = known_folder(&FOLDERID_Windows)?;
        let (drive, _) = absolute_parts(&windows)?;
        Ok(Self {
            program_files: known_folder(&FOLDERID_ProgramFiles)?,
            program_files_x86: known_folder(&FOLDERID_ProgramFilesX86)?,
            local_app_data: known_folder(&FOLDERID_LocalAppData)?,
            profile: known_folder(&FOLDERID_Profile)?,
            windows,
            system_drive: windows_path(&drive)?.into(),
        })
    }

    fn paths(&self) -> [&Path; 5] {
        [
            &self.program_files,
            &self.program_files_x86,
            &self.local_app_data,
            &self.profile,
            &self.windows,
        ]
    }
}

struct Mapping {
    folder: Vec<Arc<OpenPath>>,
    root_id: Option<String>,
}

impl Mapping {
    fn root(&self) -> &OpenPath {
        self.folder.last().unwrap()
    }

    fn read_only(&self) -> bool {
        self.root_id.is_none()
    }
}

pub struct Plan {
    mappings: Vec<Mapping>,
    folders: KnownFolders,
}

impl Plan {
    pub fn discover(roots: &[(&str, &Root)], budget: &mut Budget) -> Result<Self> {
        let preferred: Vec<_> = env::var_os("PATH")
            .iter()
            .flat_map(env::split_paths)
            .collect();
        Self::from_folders(roots, KnownFolders::read()?, &preferred, budget)
    }

    fn from_folders(
        roots: &[(&str, &Root)],
        folders: KnownFolders,
        preferred: &[PathBuf],
        budget: &mut Budget,
    ) -> Result<Self> {
        ensure!((1..=8).contains(&roots.len()), "{MAPPING_LIMIT}");
        let states = state_keys(&folders)?;
        let boundaries = boundary_keys(&folders)?;
        let mut mappings: Vec<Mapping> = Vec::with_capacity(roots.len() + MAX_TOOLCHAINS);
        for &(id, root) in roots {
            budget.check()?;
            local_drive(&root.path, false)?;
            let mapping = Mapping {
                folder: pin_directories(&root.open(".", false)?.chain)?,
                root_id: Some(id.into()),
            };
            guard_scope(&mapping, &states, &boundaries)?;
            ensure!(
                !mappings
                    .iter()
                    .any(|other| overlaps(&mapping.root().key, &other.root().key)),
                "VM_MAPPING_OVERLAP：映射不可重疊。"
            );
            mappings.push(mapping);
        }

        let mut candidates = installation_specs(&folders, budget)?;
        let preferred: Vec<_> = preferred
            .iter()
            .filter_map(|path| path_key(path).ok())
            .collect();
        candidates.sort_by_key(|candidate| {
            let executable = candidate.root.join(candidate.executable);
            executable
                .parent()
                .and_then(|path| path_key(path).ok())
                .and_then(|key| preferred.iter().position(|value| *value == key))
                .unwrap_or(usize::MAX)
        });
        for candidate in candidates {
            budget.check()?;
            if let Ok(mapping) = candidate.open(&states, &boundaries, &mappings, budget) {
                mappings.push(mapping);
            }
            if mappings.len() == roots.len() + MAX_TOOLCHAINS {
                break;
            }
        }
        budget.check()?;
        Ok(Self { mappings, folders })
    }

    pub fn configuration(&self, budget: &mut Budget) -> Result<String> {
        self.verify(budget)?;
        let mut xml = "<Configuration>".to_owned();
        for tag in [
            "Networking",
            "ClipboardRedirection",
            "AudioInput",
            "VideoInput",
            "PrinterRedirection",
            "vGPU",
        ] {
            write!(xml, "<{tag}>Disable</{tag}>")?;
        }
        xml.push_str("<MappedFolders>");
        for (index, mapping) in self.mappings.iter().enumerate() {
            let host = windows_path(&mapping.root().path)?;
            write!(
                xml,
                "<MappedFolder><HostFolder>{}</HostFolder><SandboxFolder>{}</SandboxFolder><ReadOnly>{}</ReadOnly></MappedFolder>",
                xml_text(host),
                guest_folder(index),
                mapping.read_only()
            )?;
        }
        budget.check()?;
        xml.push_str("</MappedFolders></Configuration>");
        Ok(xml)
    }

    fn verify(&self, budget: &mut Budget) -> Result<()> {
        let states = state_keys(&self.folders)?;
        let boundaries = boundary_keys(&self.folders)?;
        for mapping in &self.mappings {
            guard_scope(mapping, &states, &boundaries)?;
            for directory in &mapping.folder {
                let current = Metadata::read(&directory.file)?;
                ensure!(
                    current.directory()
                        && current.attributes == directory.metadata.attributes
                        && current.same_identity(directory.metadata),
                    "{MAPPING_PATH}"
                );
            }
            verify_tree(mapping, MAX_TREE_ENTRIES, budget)?;
        }
        budget.check()
    }

    fn spool_index(&self, root_id: &str, directory: &super::vm_spool::Directory) -> Result<usize> {
        self.mappings
            .iter()
            .position(|mapping| {
                mapping.root_id.as_deref() == Some(root_id) && directory.under(mapping.root())
            })
            .ok_or_else(|| anyhow::anyhow!("VM_SPOOL_SCOPE：暫存目錄與 VM 授權根不符。"))
    }

    pub fn guest_spool_path(
        &self,
        root_id: &str,
        directory: &super::vm_spool::Directory,
    ) -> Result<String> {
        let index = self.spool_index(root_id, directory)?;
        Ok(format!(r"{}\{}", guest_folder(index), directory.name()))
    }

    pub fn verify_spool(
        &self,
        root_id: &str,
        directory: &super::vm_spool::Directory,
        budget: &mut Budget,
    ) -> Result<()> {
        self.spool_index(root_id, directory)?;
        self.verify(budget)
    }

    #[cfg(test)]
    pub fn fixture(root_id: &str, root: &Root, base: &Path, budget: &mut Budget) -> Result<Self> {
        let folders = KnownFolders {
            program_files: base.join("programs"),
            program_files_x86: base.join("programs-x86"),
            local_app_data: base.join("local"),
            profile: base.join("profile"),
            windows: base.join("windows"),
            system_drive: base.join("system-drive"),
        };
        Self::from_folders(&[(root_id, root)], folders, &[], budget)
    }

    pub fn guest_roots(&self) -> impl Iterator<Item = (&str, String)> {
        self.mappings
            .iter()
            .enumerate()
            .filter_map(|(index, mapping)| {
                mapping
                    .root_id
                    .as_deref()
                    .map(|id| (id, guest_folder(index)))
            })
    }

    pub fn guest_toolchains(&self) -> impl Iterator<Item = String> {
        self.mappings
            .iter()
            .enumerate()
            .filter(|(_, mapping)| mapping.read_only())
            .map(|(index, _)| guest_folder(index))
    }
}

fn guest_folder(index: usize) -> String {
    format!(r"C:\MCP\mapping-{index}")
}

fn state_keys(folders: &KnownFolders) -> Result<Vec<String>> {
    let mut states = protected_paths()?.to_vec();
    for name in ["MCP-Local", "MCP-Local-Rust"] {
        states.push(protected_path(&folders.local_app_data.join(name))?);
    }
    Ok(states)
}

fn boundary_keys(folders: &KnownFolders) -> Result<Vec<String>> {
    folders.paths().into_iter().map(protected_path).collect()
}

fn overlaps(left: &str, right: &str) -> bool {
    within(left, right) || within(right, left)
}

fn local_drive(path: &Path, read_only: bool) -> Result<()> {
    let (anchor, parts) = absolute_parts(path).map_err(|_| anyhow::anyhow!(MAPPING_PATH))?;
    let Some(Component::Prefix(prefix)) = anchor.components().next() else {
        bail!("{MAPPING_PATH}");
    };
    let Prefix::VerbatimDisk(letter) = prefix.kind() else {
        bail!("{MAPPING_PATH}");
    };
    ensure!(
        !parts.is_empty(),
        "{}",
        if read_only {
            TOOLCHAIN_SCOPE
        } else {
            MAPPING_PROTECTED
        }
    );
    ensure!(local_disk(letter, read_only), "{MAPPING_PATH}");
    Ok(())
}

fn windows_path(path: &Path) -> Result<&str> {
    let path = path
        .to_str()
        .and_then(|path| path.strip_prefix(r"\\?\"))
        .ok_or_else(|| anyhow::anyhow!(MAPPING_PATH))?;
    // Sandbox can expand %NAME% in HostFolder values before opening the mapping.
    ensure!(
        path.as_bytes()
            .iter()
            .filter(|&&byte| byte == b'%')
            .take(2)
            .count()
            < 2,
        "{MAPPING_PATH}"
    );
    Ok(path)
}

fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn guard_scope(mapping: &Mapping, states: &[String], boundaries: &[String]) -> Result<()> {
    let root = mapping.root();
    local_drive(&root.path, mapping.read_only())?;
    windows_path(&root.path)?;
    ensure!(
        !states.iter().any(|state| overlaps(&root.key, state)),
        "{MAPPING_PROTECTED}"
    );
    if mapping.read_only() {
        ensure!(
            !boundaries
                .iter()
                .any(|boundary| within(boundary, &root.key)),
            "{TOOLCHAIN_SCOPE}"
        );
        for directory in mapping.folder.iter().skip(1) {
            ensure!(
                !credential(file_name(&directory.path)?),
                "{TOOLCHAIN_CREDENTIALS}"
            );
        }
    } else {
        control::mutation_policy(&root.key).map_err(|_| anyhow::anyhow!(MAPPING_PROTECTED))?;
    }
    Ok(())
}

fn credential(name: &str) -> bool {
    CREDENTIAL_NAMES.contains(&fold(name).as_str())
}

struct Candidate {
    root: PathBuf,
    executable: &'static str,
}

impl Candidate {
    fn open(
        &self,
        states: &[String],
        boundaries: &[String],
        mappings: &[Mapping],
        budget: &mut Budget,
    ) -> Result<Mapping> {
        local_drive(&self.root, true)?;
        let mapping = Mapping {
            folder: pin_directories(&open_root(&self.root)?)?,
            root_id: None,
        };
        guard_scope(&mapping, states, boundaries)?;
        ensure!(
            !mappings
                .iter()
                .any(|other| overlaps(&mapping.root().key, &other.root().key)),
            "VM_MAPPING_OVERLAP：映射不可重疊。"
        );
        executable(mapping.root(), self.executable)?;
        if self.executable == "bin/rustc.exe" {
            executable(mapping.root(), "bin/cargo.exe")?;
        }
        verify_tree(&mapping, MAX_TREE_ENTRIES, budget)?;
        Ok(mapping)
    }
}

fn executable(root: &OpenPath, relative: &str) -> Result<()> {
    let mut parent = None;
    for component in relative.split('/') {
        let folder = parent.as_ref().unwrap_or(root);
        let opened = open_entry(folder, component)?;
        parent = Some(opened);
    }
    ensure!(
        parent.is_some_and(|file| !file.metadata.directory()),
        "{MAPPING_PATH}"
    );
    Ok(())
}

fn installation_specs(folders: &KnownFolders, budget: &mut Budget) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for programs in [
        folders.program_files.clone(),
        folders.program_files_x86.clone(),
        folders.local_app_data.join("Programs"),
    ] {
        for (folder, executable) in [
            ("Git", "cmd/git.exe"),
            ("nodejs", "node.exe"),
            ("Go", "bin/go.exe"),
            ("CMake", "bin/cmake.exe"),
            ("LLVM", "bin/clang.exe"),
            ("mingw64", "bin/gcc.exe"),
        ] {
            candidates.push(Candidate {
                root: programs.join(folder),
                executable,
            });
        }
    }
    for base in [
        &folders.program_files,
        &folders.program_files_x86,
        &folders.local_app_data.join("Programs/Python"),
    ] {
        add_installations(
            base,
            "python.exe",
            |name| name.starts_with("Python3"),
            &mut candidates,
            budget,
        )?;
    }
    add_installations(
        &folders.profile.join(".rustup/toolchains"),
        "bin/rustc.exe",
        |name| name.contains("-pc-windows-"),
        &mut candidates,
        budget,
    )?;
    for (folder, executable) in [
        ("msys64/mingw64", "bin/gcc.exe"),
        ("msys64/ucrt64", "bin/gcc.exe"),
        ("mingw64", "bin/gcc.exe"),
        ("msys64/clang64", "bin/clang.exe"),
    ] {
        candidates.push(Candidate {
            root: folders.system_drive.join(folder),
            executable,
        });
    }
    Ok(candidates)
}

fn add_installations(
    base: &Path,
    executable: &'static str,
    matches: impl Fn(&str) -> bool,
    candidates: &mut Vec<Candidate>,
    budget: &mut Budget,
) -> Result<()> {
    // Check ancestors before opening the search directory, including a profile's .rustup.
    let Ok(chain) = open_root(base) else {
        return Ok(());
    };
    let Some(root) = chain.last() else {
        return Ok(());
    };
    let Ok(entries) = fs::read_dir(&root.path) else {
        return Ok(());
    };
    let mut found = Vec::new();
    for entry in entries {
        budget.entry()?;
        let Ok(entry) = entry else {
            return Ok(());
        };
        let name = entry.file_name();
        if name.to_str().is_some_and(&matches) {
            let index = found.partition_point(|current| current < &name);
            if index < MAX_TOOLCHAINS {
                found.insert(index, name);
                found.truncate(MAX_TOOLCHAINS);
            }
        }
    }
    candidates.extend(found.into_iter().map(|name| Candidate {
        root: base.join(name),
        executable,
    }));
    Ok(())
}

fn entry_handle(parent: &OpenPath, name: &str, contents: bool) -> Result<(File, Metadata)> {
    valid_name(name).map_err(|_| anyhow::anyhow!(MAPPING_ALIAS))?;
    let path = parent.path.join(name);
    let file = if contents {
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
            .map_err(safe_io)?
    } else {
        open_handle(&path, false, false)?
    };
    let metadata = Metadata::read(&file)?;
    ensure!(
        metadata.attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
            && metadata.volume == parent.metadata.volume
            && (metadata.directory() || metadata.links > 0),
        "{MAPPING_ALIAS}"
    );
    Ok((file, metadata))
}

fn open_entry(parent: &OpenPath, name: &str) -> Result<OpenPath> {
    let (file, metadata) = entry_handle(parent, name, false)?;
    let opened = OpenPath::from_checked_handle(file, Some(parent), metadata)?;
    ensure!(
        fold(file_name(&opened.path)?) == fold(name),
        "{MAPPING_ALIAS}"
    );
    Ok(opened)
}

fn verify_tree(mapping: &Mapping, limit: usize, budget: &mut Budget) -> Result<()> {
    let mut pending = vec![directory(Arc::clone(mapping.folder.last().unwrap()))?];
    let mut aliases = HashMap::new();
    let mut configs = Vec::new();
    let mut visited = 0;
    while let Some((parent, expected, entries)) = pending.last_mut() {
        budget.check()?;
        let Some(entry) = entries.next() else {
            ensure!(
                expected.same_content(Metadata::read(&parent.file)?),
                "{MAPPING_ALIAS}"
            );
            pending.pop();
            continue;
        };
        visited += 1;
        ensure!(visited <= limit, "{MAPPING_LIMIT}");
        let name = entry.map_err(safe_io)?.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!(MAPPING_ALIAS))?;
        let npm = mapping.read_only() && credential(name);
        if npm {
            let relative = parent
                .path
                .strip_prefix(&mapping.root().path)
                .map_err(|_| anyhow::anyhow!(MAPPING_ALIAS))?
                .join(name);
            ensure!(
                relative
                    .to_str()
                    .is_some_and(|relative| fold(relative) == r"node_modules\npm\.npmrc")
                    && configs.len() < MAX_TOOLCHAINS,
                "{TOOLCHAIN_CREDENTIALS}"
            );
        }
        let (file, metadata) = entry_handle(parent, name, npm)?;
        if metadata.directory() {
            ensure!(!npm, "{TOOLCHAIN_CREDENTIALS}");
            let opened = OpenPath::from_checked_handle(file, Some(parent), metadata)?;
            let pinned = opened.pin_directory(Some(parent))?;
            pending.push(directory(Arc::new(pinned))?);
        } else {
            if metadata.links > 1 {
                let (expected, count) = aliases
                    .entry((metadata.volume, metadata.index))
                    .or_insert((metadata.links, 0u32));
                *count += 1;
                ensure!(
                    *expected == metadata.links && *count <= *expected,
                    "{MAPPING_ALIAS}"
                );
            }
            if npm {
                configs.push((file, metadata));
            }
        }
    }
    ensure!(
        aliases
            .into_values()
            .all(|(expected, count)| expected == count),
        "{MAPPING_ALIAS}"
    );
    for (file, metadata) in configs {
        verify_npm(file, metadata, budget)?;
    }
    Ok(())
}

fn directory(opened: Arc<OpenPath>) -> Result<(Arc<OpenPath>, Metadata, fs::ReadDir)> {
    let current = Metadata::read(&opened.file)?;
    ensure!(
        current.directory()
            && current.attributes == opened.metadata.attributes
            && current.same_identity(opened.metadata),
        "{MAPPING_ALIAS}"
    );
    let entries = fs::read_dir(&opened.path).map_err(safe_io)?;
    Ok((opened, current, entries))
}

fn verify_npm(file: File, metadata: Metadata, budget: &mut Budget) -> Result<()> {
    ensure!(metadata.size <= 1024, "{TOOLCHAIN_CREDENTIALS}");
    budget.can_read(metadata.size as usize)?;
    let mut bytes = Vec::new();
    (&file)
        .take(1025)
        .read_to_end(&mut bytes)
        .map_err(safe_io)?;
    budget.read(bytes.len())?;
    ensure!(
        metadata.same_content(Metadata::read(&file)?)
            && bytes.len() == metadata.size as usize
            && matches!(
                bytes.trim_ascii(),
                b"" | br"prefix=${APPDATA}\npm" | b"prefix=${APPDATA}/npm"
            ),
        "{TOOLCHAIN_CREDENTIALS}"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
