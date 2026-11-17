use std::{path::PathBuf, sync::Mutex};

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    budget::Budget,
    filesystem::{
        Root,
        control::{LockedFile, ROLLBACK_FAILED, Target},
    },
    listing::{Snapshots, hex},
    security::channel::Mode,
    text::{self, Range},
};

pub const WRITE_MAXIMUM: usize = 2 * 1024 * 1024;
const WRITE_LIMIT: &str = "WRITE_LIMIT：寫入結果超過 2 MiB 或讀取設定上限。";
const CONFLICT: &str = "CONTENT_CONFLICT：必須提供目前檔案的 SHA-256。";
const CHANGED: &str = "CONTENT_CONFLICT：SHA-256 已變更。";
const FILE_MISSING: &str = "FILE_MISSING：請使用 create 模式建立新檔。";
const CREATE_EXISTS: &str = "FILE_EXISTS：建立模式不可覆寫檔案。";
const MOVE_EXISTS: &str = "FILE_EXISTS：移動不覆寫現有路徑。";
const BOM: &[u8] = b"\xef\xbb\xbf";

pub struct ControlledFiles {
    mode: Mode,
    closed: Mutex<bool>,
}

pub fn handles(tool: &str) -> bool {
    matches!(
        tool,
        "write_file"
            | "edit_block"
            | "create_directory"
            | "move_file"
            | "delete_file"
            | "read_host_file"
            | "list_host_directory"
    )
}

impl ControlledFiles {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            closed: Mutex::new(false),
        }
    }

    pub fn close(&self) {
        if let Ok(mut closed) = self.closed.lock() {
            *closed = true;
        }
    }

    pub fn dispatch(
        &self,
        tool: &str,
        args: &Value,
        root: &Root,
        root_id: &str,
        snapshots: &Snapshots,
        budget: &mut Budget,
        authorize: &dyn Fn() -> Result<()>,
    ) -> Result<Value> {
        ensure!(
            self.mode != Mode::ReadOnly,
            "READ_ONLY_MODE：唯讀模式不允許使用控制工具。"
        );
        let mut closed = self
            .closed
            .lock()
            .map_err(|_| anyhow::anyhow!("CONTROL_CLOSED：寫入授權已撤銷。"))?;
        ensure!(!*closed, "CONTROL_CLOSED：寫入授權已撤銷，請使用目前連線。");
        budget.check()?;
        authorize()?;
        let result = self.apply(tool, args, root, root_id, snapshots, budget, authorize);
        if result
            .as_ref()
            .is_err_and(|error| error.to_string() == ROLLBACK_FAILED)
        {
            *closed = true;
        }
        result
    }

    fn apply(
        &self,
        tool: &str,
        args: &Value,
        root: &Root,
        root_id: &str,
        snapshots: &Snapshots,
        budget: &mut Budget,
        authorize: &dyn Fn() -> Result<()>,
    ) -> Result<Value> {
        let required = |name: &str| {
            args[name]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("工具參數不符合契約。"))
        };
        let maximum = root.settings.max_file_bytes.min(WRITE_MAXIMUM);
        let sha256 = args["expected_sha256"].as_str();
        let file = || self.target(root, required("path")?);
        let check_mutation = |budget: &Budget| {
            authorize()?;
            budget.check()
        };
        match tool {
            "write_file" => {
                let text = required("content")?;
                ensure!(!text.contains('\0'), "WRITE_INVALID：文字包含 NUL。");
                let target = file()?;
                ensure!(
                    root.is_text(target.file.relative()),
                    "只允許寫入已授權的文字副檔名。"
                );
                let bytes = text.as_bytes();
                match required("mode")? {
                    "create" => {
                        target.file.require_absent(CREATE_EXISTS)?;
                        if sha256.is_some() {
                            bail!("{FILE_MISSING}");
                        }
                        ensure!(bytes.len() <= maximum, "{WRITE_LIMIT}");
                        check_mutation(budget)?;
                        target.file.create_file()?.commit_created(bytes)?;
                        Ok(receipt(&target.label, bytes))
                    }
                    mode @ ("rewrite" | "append") => {
                        let mut existing = target
                            .file
                            .existing(true, false)?
                            .ok_or_else(|| anyhow::anyhow!(FILE_MISSING))?;
                        let original = matching(&mut existing, sha256, CONFLICT, maximum, budget)?;
                        let combined;
                        let bytes = if mode == "append" {
                            std::str::from_utf8(original.strip_prefix(BOM).unwrap_or(&original))
                                .map_err(|_| {
                                    anyhow::anyhow!("CONTROL_IO：來源不是有效的 UTF-8。")
                                })?;
                            ensure!(original.len() + bytes.len() <= maximum, "{WRITE_LIMIT}");
                            combined = [original.as_slice(), bytes].concat();
                            &combined[..]
                        } else {
                            ensure!(bytes.len() <= maximum, "{WRITE_LIMIT}");
                            bytes
                        };
                        target.file.verify_location()?;
                        check_mutation(budget)?;
                        existing.rewrite(bytes, &original)?;
                        Ok(receipt(&target.label, bytes))
                    }
                    _ => bail!("WRITE_INVALID：寫入模式無效。"),
                }
            }
            "edit_block" => {
                let target = file()?;
                ensure!(
                    root.is_text(target.file.relative()),
                    "只允許編輯已授權的文字副檔名。"
                );
                let mut existing = target
                    .file
                    .existing(true, false)?
                    .ok_or_else(|| anyhow::anyhow!(FILE_MISSING))?;
                let original = matching(&mut existing, sha256, CHANGED, maximum, budget)?;
                let (prefix, payload) = if let Some(rest) = original.strip_prefix(BOM) {
                    (BOM, rest)
                } else {
                    (&[][..], original.as_slice())
                };
                let payload = std::str::from_utf8(payload)
                    .map_err(|_| anyhow::anyhow!("CONTROL_IO：來源不是有效的 UTF-8。"))?;
                let expected = args["expected_replacements"]
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| anyhow::anyhow!("EDIT_INVALID"))?;
                let replaced = edit_block(
                    payload,
                    required("old_string")?,
                    required("new_string")?,
                    expected,
                    maximum - prefix.len(),
                )?;
                let bytes = [prefix, replaced.as_bytes()].concat();
                target.file.verify_location()?;
                check_mutation(budget)?;
                existing.rewrite(&bytes, &original)?;
                let mut result = receipt(&target.label, &bytes);
                result["replacements"] = expected.into();
                Ok(result)
            }
            "create_directory" => {
                let target = file()?;
                check_mutation(budget)?;
                target.file.create_directory()?;
                Ok(json!({"path":target.label,"created":true}))
            }
            "move_file" => {
                let source = self.target(root, required("source")?)?;
                let destination = self.target(root, required("destination")?)?;
                destination.file.require_absent(MOVE_EXISTS)?;
                let mut existing = source
                    .file
                    .existing(false, true)?
                    .ok_or_else(|| anyhow::anyhow!(FILE_MISSING))?;
                matching(&mut existing, sha256, CHANGED, maximum, budget)?;
                source.file.verify_location()?;
                check_mutation(budget)?;
                existing.move_to(&destination.file)?;
                Ok(json!({"source":source.label,"destination":destination.label,"sha256":sha256}))
            }
            "delete_file" => {
                let target = file()?;
                let mut existing = target
                    .file
                    .existing(false, true)?
                    .ok_or_else(|| anyhow::anyhow!(FILE_MISSING))?;
                matching(&mut existing, sha256, CHANGED, maximum, budget)?;
                target.file.verify_location()?;
                check_mutation(budget)?;
                existing.delete()?;
                Ok(json!({"path":target.label,"deleted":true,"sha256":sha256}))
            }
            "read_host_file" => {
                ensure!(
                    self.mode == Mode::HostControl,
                    "HOST_MODE_REQUIRED：需要主機控制模式。"
                );
                let path = root.host_absolute(required("path")?)?;
                let (reader, name) = host_file(root, &path)?;
                let range = Range {
                    start_line: args["start_line"].as_u64().unwrap_or(1),
                    line_count: args["line_count"].as_u64().unwrap_or(200),
                };
                let mut result =
                    text::read(&reader, &name, range, budget, text::ReadResponse::HostFile)?;
                result["path"] = label(path)?.into();
                Ok(result)
            }
            "list_host_directory" => {
                ensure!(
                    self.mode == Mode::HostControl,
                    "HOST_MODE_REQUIRED：需要主機控制模式。"
                );
                let directory = root.host_absolute(required("directory")?)?;
                let reader = root.host_reader(&directory)?;
                let metadata = reader.open(".", false)?.metadata();
                let label = label(directory)?;
                let owner = format!(
                    "host:{root_id}:{}:{}:{}",
                    metadata.volume,
                    metadata.index,
                    label.to_lowercase()
                );
                let limit = args["limit"].as_u64().unwrap_or(200) as usize;
                let mut result = snapshots.list(
                    &reader,
                    &owner,
                    ".",
                    true,
                    limit,
                    args["cursor"].as_str(),
                    budget,
                )?;
                result["directory"] = label.into();
                Ok(result)
            }
            _ => bail!("工具未註冊。"),
        }
    }

    fn target(&self, root: &Root, path: &str) -> Result<LabeledTarget> {
        if self.mode == Mode::HostControl {
            let path = root.host_absolute(path)?;
            let (reader, name) = host_file(root, &path)?;
            Ok(LabeledTarget {
                file: reader.control_target(&name)?,
                label: label(path)?,
            })
        } else {
            let file = root.control_target(path)?;
            Ok(LabeledTarget {
                label: file.relative().into(),
                file,
            })
        }
    }
}

struct LabeledTarget {
    file: Target,
    label: String,
}

fn label(path: PathBuf) -> Result<String> {
    path.into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("HOST_PATH：路徑不是有效 Unicode。"))
}

fn host_file(root: &Root, absolute: &std::path::Path) -> Result<(Root, String)> {
    let parent = absolute
        .parent()
        .ok_or_else(|| anyhow::anyhow!("不可操作根目錄。"))?;
    let name = absolute
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("HOST_PATH：需要檔案名稱。"))?;
    Ok((root.host_reader(parent)?, name.into()))
}

fn matching(
    file: &mut LockedFile,
    expected: Option<&str>,
    conflict: &str,
    maximum: usize,
    budget: &mut Budget,
) -> Result<Vec<u8>> {
    let content = file.read(maximum, budget)?;
    ensure!(
        expected.is_some_and(|expected| expected == hex(&Sha256::digest(&content))),
        "{conflict}"
    );
    Ok(content)
}

fn receipt(path: &str, bytes: &[u8]) -> Value {
    json!({"path":path,"bytes":bytes.len(),"sha256":hex(&Sha256::digest(bytes))})
}

fn edit_block(
    content: &str,
    old: &str,
    new: &str,
    expected: usize,
    maximum: usize,
) -> Result<String> {
    ensure!(
        !old.is_empty() && (1..=1000).contains(&expected),
        "EDIT_INVALID：搜尋文字不可為空，替換次數須為 1 至 1000。"
    );
    ensure!(
        ![content, old, new].iter().any(|text| text.contains('\0')),
        "不支援 NUL 文字。"
    );
    let ending = match content
        .bytes()
        .position(|byte| matches!(byte, b'\r' | b'\n'))
    {
        Some(index) if content.as_bytes()[index] == b'\n' => "\n",
        Some(index) if content.as_bytes().get(index + 1) != Some(&b'\n') => "\r",
        _ => "\r\n",
    };
    let normalize = |text: &str| {
        text.replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\n', ending)
    };
    let search = normalize(old);
    let count = if search.len() > content.len() {
        0
    } else {
        overlapping_count(content.as_bytes(), search.as_bytes())
    };
    ensure!(
        count == expected && content.matches(&search).count() == expected,
        "EDIT_MATCH_COUNT：預期 {expected} 次，找到 {count} 次，檔案未變更。"
    );
    let replacement = normalize(new);
    let length = content.len() - expected * search.len() + expected * replacement.len();
    ensure!(length <= maximum, "{WRITE_LIMIT}");
    Ok(content.replace(&search, &replacement))
}

// Byte matching is valid for UTF-8: a valid needle cannot start at a continuation byte.
fn overlapping_count(haystack: &[u8], needle: &[u8]) -> usize {
    let mut prefix = vec![0; needle.len()];
    let mut matched = 0;
    for index in 1..needle.len() {
        while matched > 0 && needle[matched] != needle[index] {
            matched = prefix[matched - 1];
        }
        if needle[matched] == needle[index] {
            matched += 1;
        }
        prefix[index] = matched;
    }
    matched = 0;
    let mut count = 0;
    for byte in haystack {
        while matched > 0 && needle[matched] != *byte {
            matched = prefix[matched - 1];
        }
        if needle[matched] == *byte {
            matched += 1;
        }
        if matched == needle.len() {
            count += 1;
            matched = prefix[matched - 1];
        }
    }
    count
}
