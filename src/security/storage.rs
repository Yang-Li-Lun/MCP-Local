use std::{
    env,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    filesystem::{PrivateDirectory, missing, replace_atomic},
    listing::hex,
};

pub fn state_directory() -> Result<PathBuf> {
    let local = env::var_os("LOCALAPPDATA")
        .ok_or_else(|| anyhow::anyhow!("SECURITY_DIRECTORY：無法定位本機設定目錄。"))?;
    Ok(PathBuf::from(local).join("MCP-Local-Rust"))
}

pub fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn encoded(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    let json = serde_json::to_string(&value)?;
    let mut ascii = String::with_capacity(json.len());
    for ch in json.chars() {
        if ch < '\u{7f}' {
            ascii.push(ch);
        } else {
            for unit in ch.encode_utf16(&mut [0; 2]) {
                write!(&mut ascii, "\\u{unit:04x}").unwrap();
            }
        }
    }
    Ok(ascii.into_bytes())
}

pub fn directory(path: &Path) -> Result<Option<PrivateDirectory>> {
    match PrivateDirectory::open(path, false) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if missing(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn read(directory: &PrivateDirectory, name: &str, cap: usize) -> Result<Option<Vec<u8>>> {
    let file = match directory.regular(name) {
        Ok((file, _)) => file,
        Err(error) if missing(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take((cap + 1) as u64).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

struct StagedFile {
    path: PathBuf,
    file: Option<File>,
    remove: bool,
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        drop(self.file.take());
        if self.remove {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn transaction<T>(
    directory: &PrivateDirectory,
    name: &str,
    busy: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let lock = directory.path().join(name);
    let guard = OpenOptions::new().write(true).create_new(true).open(&lock);
    if guard
        .as_ref()
        .is_err_and(|error| error.kind() == io::ErrorKind::AlreadyExists)
    {
        anyhow::bail!("{busy}");
    }
    let mut guard = StagedFile {
        path: lock,
        file: Some(guard?),
        remove: true,
    };
    let result = operation();
    drop(guard.file.take());
    fs::remove_file(&guard.path)?;
    guard.remove = false;
    result
}

pub fn replace(directory: &PrivateDirectory, name: &str, bytes: &[u8]) -> Result<()> {
    let mut transaction = [0; 16];
    getrandom::fill(&mut transaction)?;
    let stage = directory
        .path()
        .join(format!("security-{}.tmp", hex(&transaction)));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stage)?;
    let mut cleanup = StagedFile {
        path: stage,
        file: Some(file),
        remove: true,
    };
    let file = cleanup.file.as_mut().unwrap();
    file.write_all(bytes)?;
    file.sync_all()?;
    let file = cleanup.file.take();
    drop(file);
    replace_atomic(&cleanup.path, &directory.path().join(name))?;
    cleanup.remove = false;
    Ok(())
}

pub fn parse_unique(bytes: &[u8]) -> Result<Value> {
    use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

    struct Unique(Value);
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visit;
            impl<'de> Visitor<'de> for Visit {
                type Value = Unique;

                fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                    formatter.write_str("unique JSON fields")
                }
                fn visit_bool<E: de::Error>(self, value: bool) -> Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_u64<E: de::Error>(self, value: u64) -> Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_i64<E: de::Error>(self, value: i64) -> Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_f64<E: de::Error>(self, value: f64) -> Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                    Ok(Unique(Value::Null))
                }
                fn visit_str<E: de::Error>(self, value: &str) -> Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_seq<S: SeqAccess<'de>>(self, mut rows: S) -> Result<Unique, S::Error> {
                    let mut values = Vec::new();
                    while let Some(Unique(value)) = rows.next_element()? {
                        values.push(value);
                    }
                    Ok(Unique(Value::Array(values)))
                }
                fn visit_map<M: MapAccess<'de>>(self, mut rows: M) -> Result<Unique, M::Error> {
                    let mut values = serde_json::Map::new();
                    while let Some((name, Unique(value))) = rows.next_entry::<String, Unique>()? {
                        if values.insert(name, value).is_some() {
                            return Err(de::Error::custom("duplicate JSON field"));
                        }
                    }
                    Ok(Unique(Value::Object(values)))
                }
            }

            deserializer.deserialize_any(Visit)
        }
    }

    let Unique(value) = serde_json::from_slice(bytes)?;
    ensure!(value.is_object(), "SECURITY_SCHEMA：需要物件資料。");
    Ok(value)
}
