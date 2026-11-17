use std::{collections::BTreeSet, path::PathBuf};

use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const MIB: usize = 1024 * 1024;
const EXTENSIONS: &str = ".txt .md .rst .kt .kts .java .gradle .xml .json .yaml .yml .toml .ini .cfg .csv .tsv .py .js .jsx .ts .tsx .html .css .scss .c .h .cpp .hpp .cs .go .rs .sql .pine .sh .ps1 .bat .log .properties .dart";
const NAMES: &str = "readme license dockerfile makefile cmakelists.txt go.mod";
const EXCLUSIONS: &str = "node_modules build dist target __pycache__ venv local.properties gradle.properties credentials.json secrets.json secrets.yaml secrets.yml id_rsa id_ed25519";

pub fn fold(value: &str) -> String {
    caseless::default_case_fold_str(value)
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReaderSettings {
    pub max_file_bytes: usize,
    pub max_scan_files: usize,
    pub max_scan_entries: usize,
    pub max_scan_bytes: usize,
    pub extensions: Vec<String>,
    pub text_names: Vec<String>,
    pub excluded_names: Vec<String>,
}

fn words(value: &str) -> Vec<String> {
    value.split_ascii_whitespace().map(str::to_owned).collect()
}

impl Default for ReaderSettings {
    fn default() -> Self {
        Self {
            max_file_bytes: 2 * MIB,
            max_scan_files: 3000,
            max_scan_entries: 12_000,
            max_scan_bytes: 32 * MIB,
            extensions: words(EXTENSIONS),
            text_names: words(NAMES),
            excluded_names: words(EXCLUSIONS),
        }
    }
}

impl ReaderSettings {
    pub fn normalize(mut self) -> Result<Self> {
        for (value, maximum) in [
            (self.max_file_bytes, 64 * MIB),
            (self.max_scan_files, 100_000),
            (self.max_scan_entries, 300_000),
            (self.max_scan_bytes, 1024 * MIB),
        ] {
            ensure!((1..=maximum).contains(&value), "進階設定的數值超出範圍。");
        }
        self.extensions = normalize_names(self.extensions, true)?;
        self.text_names = normalize_names(self.text_names, false)?;
        self.excluded_names = normalize_names(self.excluded_names, false)?;
        ensure!(
            serde_json::to_string(&self)?.len()
                + 8
                + self.extensions.len()
                + self.text_names.len()
                + self.excluded_names.len()
                <= 12_000,
            "進階設定文字總量過大。"
        );
        Ok(self)
    }

    pub fn with_exclusions(&self, exclusions: Vec<String>) -> Result<Self> {
        let mut settings = self.clone();
        settings.excluded_names = normalize_names(exclusions, false)?
            .into_iter()
            .chain(self.excluded_names.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut settings = settings.normalize()?;
        let exclusions = settings.excluded_names.into_iter().chain(words(EXCLUSIONS));
        settings.excluded_names = exclusions.collect::<BTreeSet<_>>().into_iter().collect();
        Ok(settings)
    }
}

pub fn syntax_char(character: char) -> bool {
    character < ' ' || "\\/:*?\"<>|".contains(character)
}

fn normalize_names(entries: Vec<String>, extension: bool) -> Result<Vec<String>> {
    ensure!(entries.len() <= 128, "每個名稱清單最多 128 項。");
    let mut normalized = BTreeSet::new();
    for entry in entries {
        let entry = fold(entry.trim());
        ensure!(
            !entry.is_empty()
                && entry.chars().count() <= 80
                && entry != "."
                && entry != ".."
                && !entry.contains(syntax_char),
            "進階設定請填單一名稱，不使用路徑或萬用字元。"
        );
        if extension {
            let suffix = entry.trim_start_matches('.');
            ensure!(
                !suffix.is_empty()
                    && suffix
                        .chars()
                        .all(|c| c.is_alphanumeric() || "_+-".contains(c)),
                "副檔名格式不正確。"
            );
            normalized.insert(format!(".{suffix}"));
        } else {
            normalized.insert(entry);
        }
    }
    Ok(normalized.into_iter().collect())
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSettings {
    pub roots: Vec<RootSettings>,
    pub default_root: String,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RootSettings {
    pub id: String,
    pub name: Option<String>,
    pub path: PathBuf,
    #[serde(default)]
    pub excluded_names: Vec<String>,
}

impl WorkspaceSettings {
    pub fn single(path: PathBuf) -> Self {
        Self {
            roots: vec![RootSettings {
                id: "main".into(),
                name: None,
                path,
                excluded_names: vec![],
            }],
            default_root: "main".into(),
        }
    }
}

pub fn decode<T: DeserializeOwned>(encoded: &str, max: usize) -> Result<T> {
    ensure!(encoded.len() <= max, "設定快照超過容量上限。");
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| anyhow::anyhow!("設定快照無效。"))?;
    serde_json::from_slice(&decoded).map_err(|_| anyhow::anyhow!("設定快照無效。"))
}
