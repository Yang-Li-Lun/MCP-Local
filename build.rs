#[cfg(feature = "desktop")]
#[path = "ui/fluent_style.rs"]
mod fluent_style;

fn main() {
    build_identity();
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock = std::fs::read_to_string("Cargo.lock").expect("read Cargo.lock");
    for (name, variable) in [
        ("rmcp", "MCP_SDK_VERSION"),
        ("image", "MCP_IMAGE_VERSION"),
        ("lopdf", "MCP_PDF_VERSION"),
        ("lofty", "MCP_AUDIO_VERSION"),
        ("yaml-rust2", "MCP_YAML_VERSION"),
    ] {
        let mut versions = lock.split("[[package]]").skip(1).filter_map(|package| {
            let mut fields = package.trim_start().lines();
            let package = fields
                .next()?
                .strip_prefix("name = \"")?
                .strip_suffix('"')?;
            let version = fields
                .next()?
                .strip_prefix("version = \"")?
                .strip_suffix('"')?;
            (package == name).then_some(version)
        });
        let version = versions
            .next()
            .unwrap_or_else(|| panic!("{name} is missing from Cargo.lock"));
        assert!(
            versions.next().is_none(),
            "multiple {name} versions in Cargo.lock"
        );
        println!("cargo:rustc-env={variable}={version}");
    }
    #[cfg(feature = "desktop")]
    fluent_style::compile();
}

fn build_identity() {
    use sha2::{Digest, Sha256};
    use std::{
        path::{Path, PathBuf},
        process::Command,
    };
    fn collect(path: &Path, files: &mut Vec<PathBuf>) {
        let metadata = path.symlink_metadata().expect("source metadata");
        assert!(!metadata.file_type().is_symlink(), "source symlink");
        if metadata.is_dir() {
            for entry in path.read_dir().expect("source directory") {
                collect(&entry.expect("source entry").path(), files);
            }
        } else if metadata.is_file() {
            files.push(path.to_owned());
        }
    }
    let mut files = Vec::new();
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        ".cargo",
        "src",
        "ui",
        "assets",
        "contracts",
        "vendor",
    ] {
        println!("cargo:rerun-if-changed={path}");
        collect(Path::new(path), &mut files);
    }
    files.sort();
    let mut hash = Sha256::new();
    for path in files {
        let name = path.to_str().expect("UTF-8 source path").replace('\\', "/");
        let bytes = std::fs::read(&path).expect("source bytes");
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    println!("cargo:rustc-env=MCP_SOURCE_SHA256={:x}", hash.finalize());
    println!(
        "cargo:rustc-env=MCP_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
    let compiler = Command::new(std::env::var_os("RUSTC").unwrap())
        .arg("--version")
        .output()
        .expect("compiler identity");
    assert!(compiler.status.success());
    println!(
        "cargo:rustc-env=MCP_BUILD_COMPILER={}",
        String::from_utf8(compiler.stdout).unwrap().trim()
    );
}
