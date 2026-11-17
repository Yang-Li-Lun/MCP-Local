use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    budget::{Budget, MAX_OUTPUT},
    filesystem::{Metadata, Root, missing},
    listing::{hash_file, hex},
};

const TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ROWS: usize = 10_000;
const SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CHANGES: usize = 500;

struct Record {
    metadata: Metadata,
    digest: Option<String>,
}

struct Inventory {
    rows: BTreeMap<String, Record>,
    bytes: usize,
}

#[derive(Default, Serialize)]
struct Stats {
    bytes_read: usize,
    hashed_files: usize,
    reused_hashes: usize,
    scanned_entries: usize,
    skipped_entries: usize,
}

fn join(base: &str, path: &str) -> String {
    match (base, path) {
        (_, ".") => base.to_owned(),
        (".", _) => path.to_owned(),
        _ => format!("{base}/{path}"),
    }
}

struct InventoryBuilder<'a> {
    root: &'a Root,
    base: &'a str,
    previous: Option<&'a Inventory>,
    force: bool,
    result: Inventory,
    stats: Stats,
}

impl InventoryBuilder<'_> {
    fn add(&mut self, path: &str, budget: &mut Budget) -> Result<bool> {
        budget.check()?;
        let key = if path == self.base {
            "."
        } else if self.base == "." {
            path
        } else {
            path.strip_prefix(self.base)
                .and_then(|suffix| suffix.strip_prefix('/'))
                .ok_or_else(|| anyhow::anyhow!("SCAN_CHANGED：目錄已變更。"))?
        };
        let access = self.root.open(path, false)?;
        let metadata = access.metadata();
        let is_dir = metadata.directory();
        let digest = if is_dir {
            None
        } else {
            ensure!(
                self.stats.hashed_files + self.stats.reused_hashes
                    < self.root.settings.max_scan_files,
                "SCAN_LIMIT：請縮小範圍。"
            );
            let previous = self
                .previous
                .and_then(|previous| previous.rows.get(key))
                .filter(|record| !self.force && metadata.same_content(record.metadata));
            if let Some(previous) = previous {
                let handle = self.root.open(path, true)?;
                ensure!(
                    metadata.same_content(handle.metadata()),
                    "SCAN_CHANGED：檔案已變更。"
                );
                handle.verify()?;
                self.stats.reused_hashes += 1;
                previous.digest.clone()
            } else {
                let (digest, _) = hash_file(self.root, path, &mut self.stats.bytes_read, budget)?;
                self.stats.hashed_files += 1;
                Some(digest)
            }
        };
        self.result.bytes += 640 + 4 * key.len();
        ensure!(
            self.result.rows.len() < MAX_ROWS && self.result.bytes <= SNAPSHOT_BYTES,
            "SNAPSHOT_LIMIT：請縮小範圍。"
        );
        ensure!(
            self.result
                .rows
                .insert(key.to_owned(), Record { metadata, digest })
                .is_none(),
            "SCAN_CHANGED：掃描期間項目已變更。"
        );
        Ok(is_dir)
    }
}

fn inventory(
    root: &Root,
    base: &str,
    previous: Option<&Inventory>,
    force: bool,
    budget: &mut Budget,
) -> Result<(Inventory, Stats)> {
    let mut builder = InventoryBuilder {
        root,
        base,
        previous,
        force,
        result: Inventory {
            rows: BTreeMap::new(),
            bytes: 1024,
        },
        stats: Stats::default(),
    };
    if builder.add(base, budget)? {
        let mut pending = vec![base.to_owned()];
        while let Some(folder) = pending.pop() {
            let scan = root.visit(&folder, true, true, budget, |_, entry, budget, _| {
                if builder.add(&entry.path, budget)? {
                    pending.push(entry.path);
                }
                Ok(true)
            })?;
            builder.stats.scanned_entries += scan.scanned_entries;
            builder.stats.skipped_entries += scan.skipped_entries;
            ensure!(
                !scan.truncated && builder.stats.scanned_entries <= root.settings.max_scan_entries,
                "SCAN_LIMIT：清冊不完整，請縮小範圍。"
            );
        }
    }
    for (key, row) in &builder.result.rows {
        budget.check()?;
        let current = root.open(&join(base, key), false)?;
        ensure!(
            row.metadata.same_content(current.metadata()),
            "SCAN_CHANGED：項目在掃描期間變更，請重試。"
        );
    }
    if let Some(previous) = previous {
        for key in previous
            .rows
            .keys()
            .filter(|key| !builder.result.rows.contains_key(*key))
        {
            budget.check()?;
            match root.open(&join(base, key), false) {
                Err(error) if missing(&error) => continue,
                Err(_) => bail!("BASELINE_SCOPE_CHANGED：先前項目已無法安全驗證，請建立新基準。"),
                Ok(_) => bail!("SCAN_INCOMPLETE：先前項目未被完整列舉，請建立新基準。"),
            }
        }
    }
    Ok((builder.result, builder.stats))
}

fn diff(left: &Inventory, right: &Inventory, budget: &Budget) -> Result<Value> {
    let keys = left
        .rows
        .keys()
        .chain(right.rows.keys())
        .collect::<BTreeSet<_>>();
    let mut counts = [0; 4];
    let mut items = Vec::with_capacity(keys.len());
    for key in keys {
        budget.check()?;
        let (kind, row) = match (left.rows.get(key), right.rows.get(key)) {
            (None, Some(row)) => (0, row),
            (Some(row), None) => (1, row),
            (Some(left), Some(right)) => (if left.digest == right.digest { 3 } else { 2 }, right),
            (None, None) => unreachable!(),
        };
        counts[kind] += 1;
        items.push((
            kind,
            key,
            if row.metadata.directory() {
                "directory"
            } else {
                "file"
            },
        ));
    }
    items.sort_by_key(|(kind, _, _)| *kind == 3);
    let mut changes = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for (kind, path, entry_type) in items.iter().take(MAX_CHANGES) {
        changes[*kind].push(json!({"path": path, "type": entry_type}));
    }
    Ok(json!({
        "counts": {"added": counts[0], "deleted": counts[1], "modified": counts[2], "same": counts[3]},
        "changes": {"added": changes[0], "deleted": changes[1], "modified": changes[2], "same": changes[3]},
        "returned_count": items.len().min(MAX_CHANGES), "details_truncated": items.len() > MAX_CHANGES
    }))
}

pub fn compare(
    left: &Root,
    left_path: &str,
    right: &Root,
    right_path: &str,
    budget: &mut Budget,
) -> Result<Value> {
    let left_path = left.open(left_path, false)?.relative;
    let right_path = right.open(right_path, false)?.relative;
    let (before, left_stats) = inventory(left, &left_path, None, true, budget)?;
    let (after, right_stats) = inventory(right, &right_path, None, true, budget)?;
    let mut result = diff(&before, &after, budget)?;
    result["left_stats"] = json!(left_stats);
    result["right_stats"] = json!(right_stats);
    result["comparison"] = json!("raw_bytes_sha256");
    result["scope"] = json!("allowed_utf8_files_and_directories");
    Ok(result)
}

struct Baseline {
    id: String,
    root: String,
    directory: String,
    created: Instant,
    inventory: Arc<Inventory>,
}

#[derive(Default)]
pub struct Baselines(Mutex<VecDeque<Baseline>>);

impl Baselines {
    pub fn status(
        &self,
        root: &Root,
        owner: &str,
        directory: &str,
        baseline: Option<&str>,
        force: bool,
        budget: &mut Budget,
    ) -> Result<Value> {
        let access = root.open(directory, false)?;
        ensure!(access.metadata().directory(), "專案路徑必須為相對目錄。");
        let directory = access.relative;
        let previous = if let Some(id) = baseline {
            let mut cache = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("無法讀取基準快照。"))?;
            cache.retain(|entry| entry.created.elapsed() < TTL);
            Some(
                cache
                    .iter()
                    .find(|entry| {
                        entry.id == id && entry.root == owner && entry.directory == directory
                    })
                    .ok_or_else(|| anyhow::anyhow!("BASELINE_INVALID：基準已失效或不屬於此範圍。"))?
                    .inventory
                    .clone(),
            )
        } else {
            None
        };
        let (inventory, stats) = inventory(root, &directory, previous.as_deref(), force, budget)?;
        let mut result = diff(
            previous.as_deref().unwrap_or(&inventory),
            &inventory,
            budget,
        )?;
        result
            .as_object_mut()
            .unwrap()
            .extend(serde_json::to_value(stats)?.as_object().unwrap().clone());
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes)?;
        let id = hex(&bytes);
        result["baseline_id"] = json!(id);
        result["baseline_created"] = json!(baseline.is_none());
        result["compared_to"] = json!(baseline);
        result["cache_mode"] = json!(if force {
            "full_hash"
        } else {
            "checked_metadata"
        });
        result["atomic_snapshot"] = json!(false);
        result["expires_after_seconds"] = json!(TTL.as_secs());
        result["scope"] = json!("allowed_utf8_files_and_directories");
        ensure!(
            serde_json::to_string_pretty(&result)?.len() <= MAX_OUTPUT,
            "OUTPUT_LIMIT：回傳資料超過 2 MiB。"
        );
        budget.check()?;
        let mut cache = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("無法儲存基準快照。"))?;
        cache.retain(|entry| entry.created.elapsed() < TTL);
        if cache.len() >= 8 {
            cache.pop_front();
        }
        while cache
            .iter()
            .map(|entry| entry.inventory.bytes)
            .sum::<usize>()
            + inventory.bytes
            > CACHE_BYTES
        {
            cache.pop_front();
        }
        cache.push_back(Baseline {
            id,
            root: owner.into(),
            directory,
            created: Instant::now(),
            inventory: Arc::new(inventory),
        });
        Ok(result)
    }
}
