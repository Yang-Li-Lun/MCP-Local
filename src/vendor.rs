use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{filesystem::PrivateDirectory, listing::hex};

pub const CLIENT: &str = "tunnel-client.exe";
const COMPANION: &str = "cloudflared.exe";
const METADATA: &str = "cloudflared-manifest.json";
const PINS: &str = include_str!("../vendor/tunnel-client-windows-x64.json");

#[derive(Deserialize)]
struct Pin {
    bytes: u64,
    sha256: String,
}

#[derive(Deserialize)]
struct Manifest {
    files: BTreeMap<String, Pin>,
}

pub struct Vendor {
    _directory: PrivateDirectory,
    _files: [File; 3],
    pub client: PathBuf,
    pub companion: PathBuf,
}

impl Vendor {
    pub fn open(executable: &Path) -> Result<Self> {
        let parent = executable.parent().context("找不到通道程式目錄。")?;
        let manifest: Manifest =
            serde_json::from_str(PINS).context("無法載入內建通道完整性設定。")?;
        Self::verify(&parent.join("third_party").join("tunnel-client"), &manifest)
    }

    fn verify(path: &Path, manifest: &Manifest) -> Result<Self> {
        let directory = PrivateDirectory::open(path, false)
            .and_then(PrivateDirectory::pin)
            .context("無法開啟並固定安全的官方通道程式目錄。")?;
        let verify = |name: &str| -> Result<(File, PathBuf)> {
            let pin = manifest
                .files
                .get(name)
                .context("內建通道完整性設定缺少必要檔案。")?;
            let (mut file, path) = directory
                .regular(name)
                .with_context(|| format!("找不到安全的官方通道檔案：{name}。"))?;
            let mut digest = Sha256::new();
            let bytes = io::copy(&mut (&mut file).take(pin.bytes + 1), &mut digest)?;
            ensure!(
                bytes == pin.bytes && hex(&digest.finalize()) == pin.sha256,
                "官方通道檔案完整性不符：{name}。"
            );
            Ok((file, path))
        };
        let (client_file, client) = verify(CLIENT)?;
        let (companion_file, companion) = verify(COMPANION)?;
        let (metadata, _) = verify(METADATA)?;
        Ok(Self {
            _directory: directory,
            _files: [client_file, companion_file, metadata],
            client,
            companion,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_and_pins_both_executables_and_the_companion_manifest() {
        let fixture = tempfile::Builder::new()
            .prefix("mcp-vendor-")
            .tempdir()
            .unwrap();
        let path = fixture.path().join("vendor");
        std::fs::create_dir(&path).unwrap();
        let files = [CLIENT, COMPANION, METADATA]
            .into_iter()
            .map(|name| {
                std::fs::write(path.join(name), name).unwrap();
                let pin = Pin {
                    bytes: name.len() as u64,
                    sha256: hex(&Sha256::digest(name.as_bytes())),
                };
                (name.to_owned(), pin)
            })
            .collect();
        let manifest = Manifest { files };
        let vendor = Vendor::verify(&path, &manifest).unwrap();
        assert_eq!(vendor.client.file_name().unwrap(), CLIENT);
        assert_eq!(vendor.companion.file_name().unwrap(), COMPANION);
        let shifted = fixture.path().join("shifted");
        assert!(std::fs::rename(&path, &shifted).is_err());
        for name in [CLIENT, COMPANION, METADATA] {
            assert!(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(path.join(name))
                    .is_err()
            );
        }
        drop(vendor);
        std::fs::rename(&path, &shifted).unwrap();
        std::fs::rename(&shifted, &path).unwrap();
        for name in [COMPANION, METADATA] {
            std::fs::write(path.join(name), vec![0; name.len()]).unwrap();
            let reason = Vendor::verify(&path, &manifest).err().unwrap();
            assert!(reason.to_string().contains(&format!("完整性不符：{name}")));
            std::fs::write(path.join(name), name).unwrap();
        }
        std::fs::remove_file(path.join(COMPANION)).unwrap();
        assert!(Vendor::verify(&path, &manifest).is_err());
        std::fs::hard_link(path.join(CLIENT), path.join(COMPANION)).unwrap();
        assert!(Vendor::verify(&path, &manifest).is_err());
    }
}
