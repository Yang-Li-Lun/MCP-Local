use std::{
    collections::VecDeque,
    io::{self, Write},
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, ser::Formatter};

use crate::{budget::Budget, filesystem::Root, formats::known_binary, settings::fold};

const RANGE_BUDGET: usize = 512 * 1024 - 32 * 1024;
const CHAR_LIMIT: usize = 24_000;
const PREVIEW_LIMIT: usize = 600;
const SEARCH_CHUNK_BYTES: usize = 65_536;

struct SpacedJson;

impl Formatter for SpacedJson {
    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.begin_array_value(writer, first)
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        writer.write_all(b": ")
    }
}

// Document offsets and search output budgets use the same spaced JSON representation.
pub(crate) fn spaced_json(value: &impl Serialize) -> Result<String> {
    let mut output = Vec::new();
    value.serialize(&mut serde_json::Serializer::with_formatter(
        &mut output,
        SpacedJson,
    ))?;
    Ok(String::from_utf8(output)?)
}

fn line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\x0b' | '\x0c' | '\x1c'..='\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

pub fn lines(text: &str) -> impl Iterator<Item = &str> {
    let mut remaining = text;
    std::iter::from_fn(move || {
        if remaining.is_empty() {
            return None;
        }
        if let Some(index) = remaining.find(line_break) {
            let (line, separator) = remaining.split_at(index);
            remaining = if let Some(rest) = separator.strip_prefix("\r\n") {
                rest
            } else {
                &separator[separator.chars().next().unwrap().len_utf8()..]
            };
            Some(line)
        } else {
            Some(std::mem::take(&mut remaining))
        }
    })
}

pub fn decode(data: &[u8]) -> Result<&str> {
    ensure!(
        !known_binary(data) && !data.contains(&0),
        "檔案不是支援的文字格式。"
    );
    let bytes = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data);
    std::str::from_utf8(bytes).map_err(|_| anyhow::anyhow!("僅支援 UTF-8 文字。"))
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub start_line: u64,
    pub line_count: u64,
}

struct Window {
    range: Range,
    rendered: String,
    chars: usize,
    returned: u64,
    full: bool,
}

pub fn ranges(root: &Root, path: &str, ranges: Vec<Range>, budget: &mut Budget) -> Result<Value> {
    let file = root.open(path, true)?;
    ensure!(
        root.is_text(&file.relative) && !file.metadata().directory(),
        "只支援允許的純文字與程式碼格式。"
    );
    ensure!(
        file.metadata().size <= root.settings.max_file_bytes as u64,
        "檔案超過設定容量上限。"
    );
    let data = file.read(0, root.settings.max_file_bytes, budget)?;
    let mut windows: Vec<_> = ranges
        .into_iter()
        .map(|range| Window {
            range,
            rendered: String::new(),
            chars: 0,
            returned: 0,
            full: false,
        })
        .collect();
    let mut total_lines = 0;
    for (index, text) in lines(decode(&data)?).enumerate() {
        if index % 512 == 0 {
            budget.check()?;
        }
        let number = index as u64 + 1;
        total_lines = number;
        for window in &mut windows {
            let Some(offset) = number.checked_sub(window.range.start_line) else {
                continue;
            };
            if window.full || offset >= window.range.line_count {
                continue;
            }
            let prefix = format!("{number}: ");
            let chars = prefix.len() + text.chars().take(CHAR_LIMIT + 1).count();
            ensure!(
                chars <= CHAR_LIMIT,
                "單行內容過長。請先將此檔案格式化或分割。"
            );
            let newline = !window.rendered.is_empty();
            let cost = chars + usize::from(newline);
            if window.chars + cost > CHAR_LIMIT {
                window.full = true;
            } else {
                if newline {
                    window.rendered.push('\n');
                }
                window.rendered.push_str(&prefix);
                window.rendered.push_str(text);
                window.chars += cost;
                window.returned += 1;
            }
        }
    }
    let mut used = 1024 + windows.len() * 600;
    let mut truncated = false;
    let mut results = Vec::with_capacity(windows.len());
    for (index, mut window) in windows.into_iter().enumerate() {
        let start = window.range.start_line;
        let mut next = start
            .checked_add(window.returned)
            .filter(|line| window.returned != 0 && *line <= total_lines);
        let cost = serde_json::to_string(&window.rendered)?.len();
        if used + cost > RANGE_BUDGET {
            window.rendered.clear();
            window.returned = 0;
            next = Some(start).filter(|line| *line <= total_lines);
            truncated = true;
        } else {
            used += cost;
        }
        truncated |= window.full;
        results.push(json!({
            "index": index, "start_line": start, "returned_lines": window.returned,
            "content": window.rendered, "next_start_line": next
        }));
    }
    Ok(
        json!({"path": file.relative, "total_lines": total_lines, "ranges": results, "truncated": truncated}),
    )
}

pub enum ReadResponse {
    File,
    HostFile,
    Batch,
}

pub fn read(
    root: &Root,
    path: &str,
    range: Range,
    budget: &mut Budget,
    response: ReadResponse,
) -> Result<Value> {
    let mut result = ranges(root, path, vec![range], budget)?;
    let mut row = result["ranges"][0].take();
    let mut output = json!({
        "path": result["path"], "total_lines": result["total_lines"],
        "start_line": row["start_line"], "next_start_line": row["next_start_line"]
    });
    let field = match response {
        ReadResponse::File => "text",
        ReadResponse::HostFile | ReadResponse::Batch => "content",
    };
    output[field] = row["content"].take();
    if matches!(response, ReadResponse::Batch) {
        output["returned_lines"] = row["returned_lines"].take();
        output["has_more"] = json!(!row["next_start_line"].is_null());
    }
    Ok(output)
}

pub fn batch(root: &Root, files: &[Value], maximum: usize, budget: &mut Budget) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        path: String,
        #[serde(default = "default_start")]
        start_line: u64,
        #[serde(default = "default_count")]
        line_count: u64,
    }
    fn default_start() -> u64 {
        1
    }
    fn default_count() -> u64 {
        200
    }
    let mut requests = Vec::with_capacity(files.len());
    for value in files {
        budget.check()?;
        let request = serde_json::from_value::<Request>(value.clone())
            .ok()
            .and_then(|request| {
                if request.start_line == 0 || !(1..=400).contains(&request.line_count) {
                    return None;
                }
                let access = root.open(&request.path, false).ok()?;
                (root.is_text(&access.relative) && !access.metadata().directory()).then_some((
                    access.relative,
                    Range {
                        start_line: request.start_line,
                        line_count: request.line_count,
                    },
                ))
            });
        requests.push(request);
    }
    let mut used = 1024;
    let mut truncated = false;
    let mut results = Vec::with_capacity(files.len());
    for (index, request) in requests.into_iter().enumerate() {
        budget.check()?;
        let skip_capacity =
            || json!({"index": index, "skipped_reason": "已達本次總輸出容量上限。"});
        let mut row = if truncated {
            skip_capacity()
        } else if let Some((path, range)) = request {
            match read(root, &path, range, budget, ReadResponse::Batch) {
                Ok(row) => row,
                Err(_) => {
                    budget.check()?;
                    json!({"path": path, "skipped_reason": "檔案無法讀取、編碼或容量不符合限制。"})
                }
            }
        } else {
            json!({"skipped_reason": "路徑不存在或未通過安全驗證。"})
        };
        row["index"] = json!(index);
        let rendered = serde_json::to_string_pretty(&row)?;
        let mut cost =
            rendered.len() + 4 * (rendered.bytes().filter(|&b| b == b'\n').count() + 1) + 2;
        if used + cost + (files.len() - index - 1) * 512 > maximum {
            truncated = true;
            row = skip_capacity();
            cost = 512;
        }
        used += cost;
        results.push(row);
    }
    Ok(json!({"files": results, "truncated": truncated}))
}

pub fn validate_queries(queries: &[&str], filenames: bool) -> Result<()> {
    ensure!(
        queries.iter().all(|q| {
            !q.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
                .is_empty()
                && if filenames {
                    !q.contains(|c| c < ' ')
                } else {
                    !q.contains(line_break)
                }
        }),
        "請提供非空的單行字面搜尋詞。"
    );
    Ok(())
}

fn preview(line: &str) -> (&str, bool) {
    match line.char_indices().nth(PREVIEW_LIMIT) {
        Some((i, _)) => (&line[..i], true),
        None => (line, false),
    }
}

fn snippet(haystack: &str, position: usize, needle: &str) -> String {
    let before = (PREVIEW_LIMIT.saturating_sub(needle.chars().count()) / 2).min(120);
    let start = suffix_start(&haystack[..position], before);
    haystack[start..].chars().take(PREVIEW_LIMIT).collect()
}

fn suffix_start(text: &str, length: usize) -> usize {
    if length == 0 {
        return text.len();
    }
    text.char_indices()
        .rev()
        .nth(length - 1)
        .map_or(0, |(i, _)| i)
}

fn matches_in_line(
    line: &str,
    needles: &[String],
    needed: &[bool],
    truncated: bool,
    line_offset: usize,
    budget: &Budget,
) -> Result<Vec<Option<String>>> {
    if !truncated {
        let text = if line.is_ascii() {
            line.to_ascii_lowercase()
        } else {
            fold(line)
        };
        return Ok(needles
            .iter()
            .zip(needed)
            .map(|(needle, needed)| (*needed && text.contains(needle)).then(String::new))
            .collect());
    }
    let mut results = vec![None; needles.len()];
    let tail_lengths: Vec<_> = needles
        .iter()
        .map(|n| n.chars().count().saturating_sub(1).max(1))
        .collect();
    let max_tail = tail_lengths.iter().copied().max().unwrap_or(0);
    let mut folded = String::new();
    let mut start = 0;
    // Boundaries are measured from the file start, including its UTF-8 BOM.
    let mut chunk_end = SEARCH_CHUNK_BYTES - line_offset % SEARCH_CHUNK_BYTES;
    while start < line.len() {
        budget.check()?;
        let end = line.floor_char_boundary(chunk_end.min(line.len()));
        chunk_end += SEARCH_CHUNK_BYTES;
        if end == start {
            continue;
        }
        let tail_end = folded.len();
        folded.push_str(&fold(&line[start..end]));
        for (index, (needle, &tail_length)) in needles.iter().zip(&tail_lengths).enumerate() {
            if needed[index] && results[index].is_none() {
                let view = &folded[suffix_start(&folded[..tail_end], tail_length)..];
                if let Some(position) = view.find(needle) {
                    results[index] = Some(snippet(view, position, needle));
                }
            }
        }
        if results
            .iter()
            .zip(needed)
            .all(|(result, &need)| !need || result.is_some())
        {
            break;
        }
        folded.drain(..suffix_start(&folded, max_tail));
        start = end;
    }
    Ok(results)
}

#[derive(Clone, Copy)]
struct Preview<'a> {
    number: usize,
    text: &'a str,
    truncated: bool,
}

impl Preview<'_> {
    fn json(self) -> Value {
        json!({"line": self.number, "text": self.text, "line_truncated": self.truncated})
    }
}

fn search_snapshot(
    relative: &str,
    data: &[u8],
    needles: &[String],
    limits: &[usize],
    context: usize,
    budget: &Budget,
) -> Result<Option<Vec<Vec<Value>>>> {
    let Ok(source) = decode(data) else {
        return Ok(None);
    };
    let mut previous = VecDeque::<Preview>::with_capacity(context);
    let mut pending = Vec::<(usize, usize, usize)>::new();
    let mut groups = vec![Vec::<Value>::new(); needles.len()];
    let mut needed = vec![true; needles.len()];
    for (index, line) in lines(source).enumerate() {
        if index % 512 == 0 {
            budget.check()?;
        }
        if pending.is_empty() && !needed.contains(&true) {
            break;
        }
        let (preview, truncated) = preview(line);
        let row = Preview {
            number: index + 1,
            text: preview,
            truncated,
        };
        pending.retain(|&(group, row_index, until)| {
            groups[group][row_index]["context"]
                .as_array_mut()
                .unwrap()
                .push(row.json());
            index < until
        });
        let line_offset = line.as_ptr().addr() - data.as_ptr().addr();
        for (group, found) in
            matches_in_line(line, needles, &needed, truncated, line_offset, budget)?
                .into_iter()
                .enumerate()
        {
            let Some(found) = found else { continue };
            let mut matched = row.json();
            matched["path"] = json!(relative);
            if truncated {
                matched["match_snippet"] = json!(found);
                matched["snippet_format"] = json!("casefolded_text");
            }
            if context > 0 {
                matched["context"] = Value::Array(
                    previous
                        .iter()
                        .copied()
                        .chain([row])
                        .map(Preview::json)
                        .collect(),
                );
                pending.push((group, groups[group].len(), index + context));
            }
            groups[group].push(matched);
            needed[group] = groups[group].len() <= limits[group];
        }
        if context > 0 {
            if previous.len() == context {
                previous.pop_front();
            }
            previous.push_back(row);
        }
    }
    Ok(Some(groups))
}

pub fn search(
    root: &Root,
    directory: &str,
    queries: &[&str],
    limit: usize,
    context: usize,
    multi: bool,
    budget: &mut Budget,
) -> Result<Value> {
    validate_queries(queries, false)?;
    let needles: Vec<_> = queries.iter().map(|query| fold(query)).collect();
    let mut groups = vec![Vec::new(); needles.len()];
    let mut group_truncated = vec![false; needles.len()];
    let (mut used, mut count, mut bytes, mut scanned_files) = (0, 0, 0, 0);
    let mut status = root.visit(
        directory,
        false,
        false,
        budget,
        |folder, entry, budget, scan| {
            if !root.is_text(&entry.path) {
                scan.skipped_entries += 1;
                return Ok(true);
            }
            let remaining = root.settings.max_scan_bytes.saturating_sub(bytes);
            if remaining == 0 {
                scan.truncated = true;
                return Ok(false);
            }
            let file = match root.open_entry(folder, &entry) {
                Ok(file) if file.metadata().size <= root.settings.max_file_bytes as u64 => file,
                _ => {
                    scan.skipped_entries += 1;
                    return Ok(true);
                }
            };
            let data = match file.read(0, remaining, budget) {
                Ok(data) => data,
                Err(_) => {
                    budget.check()?;
                    scan.skipped_entries += 1;
                    return Ok(true);
                }
            };
            bytes += data.len();
            if data.len() as u64 != file.metadata().size {
                scan.skipped_entries += 1;
                return Ok(true);
            }
            let limits: Vec<_> = groups.iter().map(|group| limit - group.len()).collect();
            let Some(candidates) =
                search_snapshot(&entry.path, &data, &needles, &limits, context, budget)?
            else {
                scan.skipped_entries += 1;
                return Ok(true);
            };
            scanned_files += 1;
            for (index, rows) in candidates.into_iter().enumerate() {
                for row in rows {
                    if groups[index].len() == limit {
                        group_truncated[index] = true;
                        break;
                    }
                    let encoded = spaced_json(&row)?;
                    let size = if multi {
                        encoded.len()
                    } else {
                        encoded.chars().count()
                    };
                    if count == 200 || used + size > 100_000 {
                        scan.truncated = true;
                        if !multi {
                            return Ok(false);
                        }
                        break;
                    }
                    used += size;
                    count += 1;
                    groups[index].push(row);
                }
            }
            if scan.truncated
                || (!multi && bytes == root.settings.max_scan_bytes)
                || groups.iter().all(|group| group.len() == limit)
            {
                scan.truncated = true;
                return Ok(false);
            }
            Ok(true)
        },
    )?;
    status.truncated |= bytes >= root.settings.max_scan_bytes;
    let mut output = json!({
        "scanned_files": scanned_files, "scanned_bytes": bytes, "skipped_entries": status.skipped_entries,
        "truncated": status.truncated || group_truncated.iter().any(|flag| *flag)
    });
    if status.scanned_entries > 0 {
        output["scanned_entries"] = json!(status.scanned_entries);
    }
    if let Some(reason) = status.reason {
        output["truncation_reason"] = json!(reason);
    }
    if multi {
        output["results"] = Value::Array(queries.iter().zip(groups).zip(group_truncated).map(
            |((&query, matches), truncated)| json!({"query": query, "matches": matches, "truncated": truncated || status.truncated}),
        ).collect());
    } else {
        output["matches"] = json!(groups.remove(0));
    }
    Ok(output)
}
