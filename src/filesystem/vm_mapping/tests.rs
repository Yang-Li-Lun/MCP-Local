use tempfile::{Builder, TempDir};
use tokio_util::sync::CancellationToken;

use super::*;

struct Fixture {
    directory: TempDir,
    shared: PathBuf,
    root: Root,
}

impl Fixture {
    fn new() -> Self {
        let directory = Builder::new().prefix("mcp-mapping-").tempdir().unwrap();
        let shared = directory.path().join("project & docs");
        fs::create_dir(&shared).unwrap();
        let root = Root::new(&shared, ReaderSettings::default()).unwrap();
        for folder in [
            "programs",
            "programs-x86",
            "local",
            "profile",
            "windows",
            "system-drive",
        ] {
            fs::create_dir(directory.path().join(folder)).unwrap();
        }
        Self {
            directory,
            shared,
            root,
        }
    }

    fn folders(&self) -> KnownFolders {
        let base = self.directory.path();
        KnownFolders {
            program_files: base.join("programs"),
            program_files_x86: base.join("programs-x86"),
            local_app_data: base.join("local"),
            profile: base.join("profile"),
            windows: base.join("windows"),
            system_drive: base.join("system-drive"),
        }
    }

    fn install(&self, relative: &str) -> PathBuf {
        let path = self.directory.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"synthetic fixture; never executed").unwrap();
        path
    }

    fn plan(&self, preferred: &[PathBuf]) -> Result<Plan> {
        Plan::from_folders(
            &[("project", &self.root)],
            self.folders(),
            preferred,
            &mut budget(),
        )
    }
}

fn budget() -> Budget {
    Budget::new(CancellationToken::new())
}

#[test]
fn discovers_fixed_installations_and_generates_isolated_xml() {
    let fixture = Fixture::new();
    for path in [
        "programs/Python313/python.exe",
        "programs/Git/cmd/git.exe",
        "programs/nodejs/node.exe",
        "programs/Go/bin/go.exe",
        "programs-x86/CMake/bin/cmake.exe",
        "local/Programs/LLVM/bin/clang.exe",
        "system-drive/msys64/ucrt64/bin/gcc.exe",
        "profile/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin/rustc.exe",
        "profile/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin/cargo.exe",
    ] {
        fixture.install(path);
    }
    let unknown = fixture.install("untrusted/bin/git.exe");
    let preferred = fixture.folders().program_files.join("nodejs");
    let plan = fixture
        .plan(&[unknown.parent().unwrap().into(), preferred.clone()])
        .unwrap();
    assert_eq!(
        plan.guest_roots().collect::<Vec<_>>(),
        vec![("project", guest_folder(0))]
    );
    assert_eq!(
        plan.guest_toolchains().collect::<Vec<_>>(),
        (1..=8).map(guest_folder).collect::<Vec<_>>()
    );
    let xml = plan.configuration(&mut budget()).unwrap();
    let parsed = roxmltree::Document::parse(&xml).unwrap();
    let config = parsed.root_element();
    assert_eq!(config.tag_name().name(), "Configuration");
    for name in [
        "Networking",
        "ClipboardRedirection",
        "AudioInput",
        "VideoInput",
        "PrinterRedirection",
        "vGPU",
    ] {
        assert_eq!(
            config
                .children()
                .find(|node| node.has_tag_name(name))
                .unwrap()
                .text(),
            Some("Disable")
        );
    }
    assert!(
        !config
            .children()
            .any(|node| node.has_tag_name("LogonCommand"))
    );
    let mounts: Vec<_> = config
        .descendants()
        .filter(|node| node.has_tag_name("MappedFolder"))
        .collect();
    assert_eq!(mounts.len(), 9);
    for (index, entry) in mounts.into_iter().enumerate() {
        let text = |tag| {
            entry
                .children()
                .find(|node| node.has_tag_name(tag))
                .unwrap()
                .text()
                .unwrap()
        };
        assert_eq!(text("SandboxFolder"), guest_folder(index));
        assert_eq!(text("ReadOnly"), if index == 0 { "false" } else { "true" });
        assert!(!text("HostFolder").contains("untrusted"));
        if index == 0 {
            assert_eq!(text("HostFolder"), fixture.shared.to_str().unwrap());
        }
        if index == 1 {
            assert_eq!(text("HostFolder"), preferred.to_str().unwrap());
        }
    }
    assert!(xml.contains("project &amp; docs"));
}

#[test]
fn revalidation_rejects_hardlinks_that_cross_mapping_boundaries() {
    let fixture = Fixture::new();
    let git = fixture.install("programs/Git/cmd/git.exe");
    fs::hard_link(&git, git.with_file_name("git-alias.exe")).unwrap();
    let plan = fixture.plan(&[]).unwrap();
    assert_eq!(plan.guest_toolchains().count(), 1);
    plan.configuration(&mut budget()).unwrap();

    let crossing = fixture.shared.join("writable-alias.exe");
    fs::hard_link(&git, &crossing).unwrap();
    assert!(
        plan.configuration(&mut budget())
            .unwrap_err()
            .to_string()
            .contains("VM_MAPPING_ALIAS")
    );
    fs::remove_file(crossing).unwrap();

    let outside = fixture.install("outside/untouched.txt");
    fs::hard_link(&outside, fixture.shared.join("outside-alias.txt")).unwrap();
    assert!(
        plan.configuration(&mut budget())
            .unwrap_err()
            .to_string()
            .contains("VM_MAPPING_ALIAS")
    );
    assert_eq!(
        fs::read(outside).unwrap(),
        b"synthetic fixture; never executed"
    );
}

#[test]
#[ignore = "requires Windows Developer Mode or symlink privilege"]
fn mapping_rejects_directory_symlinks_without_traversing_them() {
    let fixture = Fixture::new();
    let external = fixture.install("outside/untouched.txt");
    let alias = fixture.shared.join("outside-alias");
    std::os::windows::fs::symlink_dir(external.parent().unwrap(), &alias).unwrap();
    let plan = fixture.plan(&[]).unwrap();
    let result = plan.configuration(&mut budget());
    fs::remove_dir(&alias).unwrap();
    assert!(result.unwrap_err().to_string().contains("VM_MAPPING_ALIAS"));
    assert_eq!(
        fs::read(external).unwrap(),
        b"synthetic fixture; never executed"
    );
}

#[test]
fn checks_toolchain_credentials_again_without_reading_arbitrary_configurations() {
    let fixture = Fixture::new();
    fixture.install("programs/nodejs/node.exe");
    let npm = fixture.install("programs/nodejs/node_modules/npm/.npmrc");
    fs::write(&npm, br"prefix=${APPDATA}\npm").unwrap();
    let plan = fixture.plan(&[]).unwrap();
    assert_eq!(plan.guest_toolchains().count(), 1);
    plan.configuration(&mut budget()).unwrap();
    fs::write(&npm, b"# synthetic unrecognized config").unwrap();
    assert!(
        plan.configuration(&mut budget())
            .unwrap_err()
            .to_string()
            .contains("VM_TOOLCHAIN_CREDENTIALS")
    );
    assert_eq!(fixture.plan(&[]).unwrap().guest_toolchains().count(), 0);
    fs::write(&npm, b"").unwrap();
    plan.configuration(&mut budget()).unwrap();
    let unknown = fixture.install("programs/nodejs/example/credentials.toml");
    assert!(
        plan.configuration(&mut budget())
            .unwrap_err()
            .to_string()
            .contains("VM_TOOLCHAIN_CREDENTIALS")
    );
    assert_eq!(
        fs::read(unknown).unwrap(),
        b"synthetic fixture; never executed"
    );
}

#[test]
fn blocks_protected_overlapping_and_ambiguous_roots_and_pins_ancestors() {
    let fixture = Fixture::new();
    let settings = ReaderSettings::default();
    let folders = fixture.folders();
    let state = folders.local_app_data.join("MCP-Local");
    fs::create_dir(&state).unwrap();
    let protected = Root::new(&state, settings.clone()).unwrap();
    let overlap = Root::new(&fixture.shared, settings.clone()).unwrap();
    for (roots, code) in [
        (vec![("state", &protected)], "VM_MAPPING_PROTECTED"),
        (
            vec![("first", &fixture.root), ("second", &overlap)],
            "VM_MAPPING_OVERLAP",
        ),
    ] {
        let error = Plan::from_folders(&roots, fixture.folders(), &[], &mut budget())
            .err()
            .unwrap();
        assert!(error.to_string().contains(code), "{error}");
    }

    let expands = fixture
        .directory
        .path()
        .join("possible-%APPDATA%-expansion");
    fs::create_dir(&expands).unwrap();
    let candidate = Root::new(&expands, settings).unwrap();
    assert!(
        Plan::from_folders(
            &[("literal", &candidate)],
            fixture.folders(),
            &[],
            &mut budget()
        )
        .err()
        .unwrap()
        .to_string()
        .contains("VM_MAPPING_PATH")
    );
    let plan = fixture.plan(&[]).unwrap();
    let renamed = fixture.directory.path().join("renamed");
    assert!(fs::rename(&fixture.shared, &renamed).is_err());
    drop(plan);
    let target = fixture.root.control_target("before.txt").unwrap();
    assert!(fs::rename(&fixture.shared, &renamed).is_err());
    drop(target);
    fs::rename(&fixture.shared, &renamed).unwrap();
}

#[test]
fn known_folders_come_from_windows_and_mapping_respects_budgets() {
    let system = KnownFolders::read().unwrap();
    assert!(system.paths().into_iter().all(Path::is_absolute));
    assert!(system.system_drive.is_absolute());
    let fixture = Fixture::new();
    fs::write(fixture.shared.join("file.txt"), b"x").unwrap();
    let plan = fixture.plan(&[]).unwrap();
    assert!(
        verify_tree(&plan.mappings[0], 0, &mut budget())
            .unwrap_err()
            .to_string()
            .contains("VM_MAPPING_LIMIT")
    );
    let canceled = CancellationToken::new();
    canceled.cancel();
    assert!(
        plan.configuration(&mut Budget::new(canceled))
            .unwrap_err()
            .to_string()
            .contains("OPERATION_CANCELLED")
    );
}
