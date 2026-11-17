use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    budget::Budget,
    filesystem::{Entry, Root, Scan},
    formats,
    settings::fold,
    text,
};

const SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const INVALID_CURSOR: &str = "cursor 無效或已損壞，請從第一頁重新列出。";

struct Snapshot {
    rows: Vec<Entry>,
    scan: Scan,
    owner: String,
    scope: String,
    size: usize,
}

struct Cached {
    id: String,
    snapshot: Arc<Snapshot>,
    created: Instant,
    used: Instant,
}

pub struct Snapshots {
    cache: Mutex<VecDeque<Cached>>,
    key: [u8; 32],
}

impl Snapshots {
    pub fn new() -> Result<Self> {
        let mut key = [0; 32];
        getrandom::fill(&mut key)?;
        Ok(Self {
            cache: Mutex::new(VecDeque::new()),
            key,
        })
    }

    fn expire(cache: &mut VecDeque<Cached>, now: Instant) {
        cache.retain(|entry| {
            now - entry.used < Duration::from_secs(60)
                && now - entry.created < Duration::from_secs(300)
        });
    }

    fn store(&self, snapshot: Snapshot) -> Result<(String, Arc<Snapshot>, usize)> {
        let mut id = [0u8; 16];
        getrandom::fill(&mut id)?;
        let id = hex(&id);
        let snapshot = Arc::new(snapshot);
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("無法儲存分頁快照。"))?;
        let now = Instant::now();
        Self::expire(&mut cache, now);
        let owned = cache
            .iter()
            .filter(|entry| entry.snapshot.owner == snapshot.owner)
            .count();
        if owned >= 8 {
            let index = cache
                .iter()
                .position(|entry| entry.snapshot.owner == snapshot.owner)
                .unwrap();
            cache.remove(index);
        }
        while cache.len() >= 32
            || cache
                .iter()
                .map(|entry| entry.snapshot.rows.len())
                .sum::<usize>()
                + snapshot.rows.len()
                > 250_000
            || cache.iter().map(|entry| entry.snapshot.size).sum::<usize>() + snapshot.size
                > CACHE_BYTES
        {
            cache.pop_front();
        }
        cache.push_back(Cached {
            id: id.clone(),
            snapshot: snapshot.clone(),
            created: now,
            used: now,
        });
        Ok((id, snapshot, 0))
    }

    fn encode(&self, id: &str, offset: usize) -> Result<String> {
        // The signed ID locates a snapshot that owns the full query scope.
        // Putting a long Unicode path in the token can exceed the cursor limit.
        let data = serde_json::to_vec(&(2, id, offset))?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).unwrap();
        mac.update(&data);
        let mut token = mac.finalize().into_bytes().to_vec();
        token.extend(data);
        Ok(URL_SAFE.encode(token))
    }

    fn resume(
        &self,
        cursor: &str,
        owner: &str,
        scope: &str,
    ) -> Result<(String, Arc<Snapshot>, usize)> {
        ensure!(cursor.len() <= 16_000, "{INVALID_CURSOR}");
        let bytes = URL_SAFE
            .decode(cursor)
            .map_err(|_| anyhow::anyhow!(INVALID_CURSOR))?;
        let (signature, payload) = bytes
            .split_at_checked(32)
            .ok_or_else(|| anyhow::anyhow!(INVALID_CURSOR))?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).unwrap();
        mac.update(payload);
        mac.verify_slice(signature)
            .map_err(|_| anyhow::anyhow!(INVALID_CURSOR))?;
        let (version, id, offset): (u8, String, usize) =
            serde_json::from_slice(payload).map_err(|_| anyhow::anyhow!(INVALID_CURSOR))?;
        ensure!(version == 2, "{INVALID_CURSOR}");
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!(INVALID_CURSOR))?;
        let now = Instant::now();
        Self::expire(&mut cache, now);
        let index = cache
            .iter()
            .position(|entry| entry.id == id)
            .ok_or_else(|| anyhow::anyhow!("cursor 已失效，請從第一頁重新列出。"))?;
        let snapshot = &cache[index].snapshot;
        ensure!(
            snapshot.owner == owner && snapshot.scope == scope,
            "cursor 不屬於本次列舉條件。"
        );
        ensure!(offset < snapshot.rows.len(), "{INVALID_CURSOR}");
        let mut entry = cache.remove(index).unwrap();
        entry.used = now;
        let snapshot = entry.snapshot.clone();
        cache.push_back(entry);
        Ok((id, snapshot, offset))
    }

    pub fn list(
        &self,
        root: &Root,
        owner: &str,
        directory: &str,
        shallow: bool,
        limit: usize,
        cursor: Option<&str>,
        budget: &mut Budget,
    ) -> Result<Value> {
        let started = Instant::now();
        let directory = root.open(directory, false)?.relative;
        let scope = format!("list:{shallow}:{directory}");
        let (id, snapshot, offset) = if let Some(cursor) = cursor {
            self.resume(cursor, owner, &scope)?
        } else {
            let mut rows = Vec::new();
            let mut size = 1024;
            let scan = root.visit(&directory, shallow, false, budget, |_, entry, _, scan| {
                size += 512 + 4 * (entry.path.len() + entry.name.len());
                if size > SNAPSHOT_BYTES {
                    size = SNAPSHOT_BYTES;
                    scan.truncate("SNAPSHOT_BYTES");
                    return Ok(false);
                }
                rows.push(entry);
                if rows.len() == 100_000 {
                    scan.truncated = true;
                    return Ok(false);
                }
                Ok(true)
            })?;
            rows.sort_by_cached_key(|entry| (fold(&entry.path), entry.path.clone()));
            self.store(Snapshot {
                rows,
                scan,
                owner: owner.into(),
                scope: scope.clone(),
                size,
            })?
        };
        let end = (offset + limit).min(snapshot.rows.len());
        let has_more = end < snapshot.rows.len();
        let next = has_more.then(|| self.encode(&id, end)).transpose()?;
        let mut output = json!({
            "returned_count": end - offset, "has_more": has_more, "next_cursor": next,
            "scan_truncated": snapshot.scan.truncated, "truncated": has_more || snapshot.scan.truncated,
            "skipped_entries": snapshot.scan.skipped_entries, "scanned_entries": snapshot.scan.scanned_entries,
            "sorted_entries": snapshot.rows.len(), "elapsed_ms": elapsed_ms(started),
        });
        let page = &snapshot.rows[offset..end];
        if shallow {
            output["entries"] = json!(
                page.iter()
                    .map(|entry| json!({
                        "path": entry.path, "name": entry.name,
                        "type": if entry.metadata.directory() { "directory" } else { "file" }
                    }))
                    .collect::<Vec<_>>()
            );
        } else {
            output["files"] = json!(
                page.iter()
                    .map(|entry| entry.path.as_str())
                    .collect::<Vec<_>>()
            );
        }
        Ok(output)
    }

    pub fn query(
        &self,
        root: &Root,
        owner: &str,
        args: &Value,
        budget: &mut Budget,
    ) -> Result<Value> {
        let started = Instant::now();
        let directory = root
            .open(args["directory"].as_str().unwrap(), false)?
            .relative;
        let limit = args["limit"].as_u64().unwrap() as usize;
        let sort_by = args["sort_by"].as_str().unwrap();
        let name = args["name"].as_str();
        let extensions = args["extensions"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .map(|value| {
                        let value = value.as_str().unwrap();
                        ensure!(
                            !value.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0']),
                            "QUERY_EXTENSION：副檔名清單不合法。"
                        );
                        Ok(if value.is_empty() {
                            String::new()
                        } else {
                            format!(".{}", fold(value.trim_start_matches('.')))
                        })
                    })
                    .collect::<Result<std::collections::BTreeSet<_>>>()
            })
            .transpose()?;
        let size_range = (args["min_size"].as_u64(), args["max_size"].as_u64());
        let dates = ["created", "modified", "accessed"].map(|field| {
            (
                args[format!("{field}_after")].as_f64(),
                args[format!("{field}_before")].as_f64(),
            )
        });
        ensure!(
            !reversed(size_range) && dates.iter().all(|range| !reversed(*range)),
            "QUERY_RANGE：下限不可大於上限。"
        );
        let normalized = json!([
            directory,
            name,
            extensions,
            size_range,
            dates,
            sort_by,
            args["order"],
            args["include_format"],
            args["include_capabilities"],
            args["include_sha256"]
        ]);
        let scope = format!(
            "query:{}",
            hex(&Sha256::digest(serde_json::to_vec(&normalized)?))
        );
        let (id, snapshot, offset) = if let Some(cursor) = args["cursor"].as_str() {
            self.resume(cursor, owner, &scope)?
        } else {
            let folded_name = name.map(fold);
            let mut rows = Vec::new();
            let mut size = 1024;
            let scan = root.visit(&directory, false, true, budget, |_, entry, _, _| {
                let access = root.open(&entry.path, false)?;
                let meta = access.metadata();
                ensure!(
                    !meta.directory() && entry.metadata.same_content(meta),
                    "QUERY_CHANGED：掃描期間檔案已變更。"
                );
                if folded_name
                    .as_ref()
                    .is_some_and(|needle| !fold(&entry.name).contains(needle))
                    || extensions
                        .as_ref()
                        .is_some_and(|set| !set.contains(&formats::extension(&entry.name)))
                    || !in_range(meta.size, size_range)
                    || ![meta.created, meta.modified, meta.accessed]
                        .into_iter()
                        .zip(dates)
                        .all(|(value, range)| in_range(formats::unix_seconds(value), range))
                {
                    return Ok(true);
                }
                size += 1024 + 6 * (entry.path.len() + entry.name.len());
                ensure!(
                    size <= SNAPSHOT_BYTES && rows.len() < 100_000,
                    "QUERY_SNAPSHOT_LIMIT：請縮小範圍。"
                );
                rows.push(Entry {
                    metadata: meta,
                    ..entry
                });
                Ok(true)
            })?;
            ensure!(
                !scan.truncated,
                "QUERY_SCAN_LIMIT：掃描未完成，不能回報全域排序，請縮小範圍。"
            );
            rows.sort_by_cached_key(|entry| {
                let value = match sort_by {
                    "size" => i128::from(entry.metadata.size),
                    "created_time" => i128::from(entry.metadata.created),
                    "modified_time" => i128::from(entry.metadata.modified),
                    "accessed_time" => i128::from(entry.metadata.accessed),
                    _ => 0,
                };
                (
                    value,
                    if sort_by == "name" {
                        fold(&entry.name)
                    } else {
                        String::new()
                    },
                    fold(&entry.path),
                    entry.path.clone(),
                )
            });
            if args["order"] == "desc" {
                rows.reverse();
            }
            self.store(Snapshot {
                rows,
                scan,
                owner: owner.into(),
                scope: scope.clone(),
                size,
            })?
        };
        let mut files = Vec::new();
        let mut bytes = 0;
        let end = (offset + limit).min(snapshot.rows.len());
        budget.restrict_read_bytes(root.settings.max_scan_bytes);
        for item in &snapshot.rows[offset..end] {
            budget.check()?;
            let file = root.open(&item.path, false)?;
            ensure!(
                item.metadata.same_content(file.metadata()),
                "QUERY_CHANGED：分頁檔案已變更，請從第一頁重新查詢。"
            );
            let mut row = formats::basic_metadata(&item.path, item.metadata);
            // Windows path-based stat adds these bits by suffix. Handle-based file_info does not.
            if matches!(
                row["extension"].as_str(),
                Some(".exe" | ".com" | ".bat" | ".cmd")
            ) {
                let mode = &mut row["attributes"]["mode"];
                *mode = json!(mode.as_u64().unwrap() | 0o111);
            }
            if args["include_format"] == true || args["include_capabilities"] == true {
                let info = formats::info(root, &item.path, budget)?;
                if args["include_format"] == true {
                    for field in ["format", "mime_type", "detection", "container_error"] {
                        row[field] = info[field].clone();
                    }
                }
                if args["include_capabilities"] == true {
                    row["capabilities"] = info["capabilities"].clone();
                }
            }
            if args["include_sha256"] == true {
                row["sha256"] = json!(hash_file(root, &item.path, &mut bytes, budget)?.0);
            }
            ensure!(
                item.metadata
                    .same_content(root.open(&item.path, false)?.metadata()),
                "QUERY_CHANGED：讀取期間檔案已變更。"
            );
            files.push(row);
        }
        let more = end < snapshot.rows.len();
        let next = more.then(|| self.encode(&id, end)).transpose()?;
        Ok(json!({
            "files": files, "returned_count": files.len(), "matched_count": snapshot.rows.len(),
            "has_more": more, "next_cursor": next, "scan_complete": true, "scan_truncated": false,
            "truncated": more, "skipped_entries": snapshot.scan.skipped_entries,
            "scanned_entries": snapshot.scan.scanned_entries, "metadata_snapshot": true,
            "elapsed_ms": elapsed_ms(started)
        }))
    }
}

fn elapsed_ms(start: Instant) -> f64 {
    (start.elapsed().as_secs_f64() * 1_000_000.).round() / 1000.
}

fn reversed<T: PartialOrd>((min, max): (Option<T>, Option<T>)) -> bool {
    min.zip(max).is_some_and(|(min, max)| min > max)
}

fn in_range<T: PartialOrd>(value: T, (min, max): (Option<T>, Option<T>)) -> bool {
    min.is_none_or(|min| min <= value) && max.is_none_or(|max| value <= max)
}

pub fn find(
    root: &Root,
    dir: &str,
    queries: &[&str],
    limit: usize,
    budget: &mut Budget,
) -> Result<Value> {
    text::validate_queries(queries, true)?;
    let folded: Vec<_> = queries.iter().map(|query| fold(query)).collect();
    let mut groups = vec![Vec::new(); queries.len()];
    let (mut bytes, mut count) = (16_384, 0);
    let scan = root.visit(dir, false, false, budget, |_, entry, _, scan| {
        let path = fold(&entry.path);
        for (needle, matches) in folded.iter().zip(&mut groups) {
            if matches.len() >= limit || !path.contains(needle) {
                continue;
            }
            let row = json!({"path": entry.path, "name": entry.name});
            let size = serde_json::to_string_pretty(&row)?.len() + 64;
            if count == 200 || bytes + size > 100 * 1024 {
                scan.truncated = true;
                return Ok(false);
            }
            matches.push(row);
            count += 1;
            bytes += size;
        }
        if count == 200 || groups.iter().all(|matches| matches.len() == limit) {
            scan.truncated = true;
            return Ok(false);
        }
        Ok(true)
    })?;
    let mut truncated = scan.truncated;
    let results: Vec<_> = queries
        .iter()
        .zip(groups)
        .map(|(query, matches)| {
            let group_truncated = scan.truncated || matches.len() == limit;
            truncated |= group_truncated;
            json!({"query": query, "matches": matches, "truncated": group_truncated})
        })
        .collect();
    let mut result = json!({
        "results": results, "truncated": truncated,
        "scanned_entries": scan.scanned_entries, "skipped_entries": scan.skipped_entries
    });
    if let Some(reason) = scan.reason {
        result["truncation_reason"] = json!(reason);
    }
    Ok(result)
}

pub fn hash_files(root: &Root, paths: &[&str], budget: &mut Budget) -> Result<Value> {
    let mut normalized = Vec::with_capacity(paths.len());
    for path in paths {
        normalized.push(root.open(path, false)?.relative);
    }
    let mut bytes = 0;
    let mut files = Vec::with_capacity(paths.len());
    for path in normalized {
        let (digest, size) = hash_file(root, &path, &mut bytes, budget)?;
        files.push(json!({"path": path, "sha256": digest, "size": size}));
    }
    Ok(json!({
        "algorithm": "SHA-256", "files": files, "bytes_read": bytes,
        "hashed_files": files.len(), "reused_hashes": 0, "scanned_entries": 0, "skipped_entries": 0
    }))
}

pub fn hash_file(
    root: &Root,
    path: &str,
    bytes: &mut usize,
    budget: &mut Budget,
) -> Result<(String, usize)> {
    let file = root.open(path, true)?;
    let size = file.metadata().size;
    ensure!(
        size <= root
            .settings
            .max_file_bytes
            .min(root.settings.max_scan_bytes.saturating_sub(*bytes)) as u64,
        "HASH_BYTES_LIMIT：已達讀取容量上限，請縮小範圍。"
    );
    let data = file.read(0, size as usize, budget)?;
    *bytes += data.len();
    Ok((hex(&Sha256::digest(&data)), data.len()))
}

pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(2 * bytes.len());
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
