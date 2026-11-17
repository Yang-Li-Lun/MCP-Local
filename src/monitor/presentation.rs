use serde_json::{Map, Value, json};

use super::{Record, Status};

pub fn operation(tool: &str) -> &str {
    match tool {
        "read_file" => "讀取檔案",
        "read_host_file" => "讀取主機檔案",
        "read_files" => "批次讀取",
        "read_file_ranges" => "分段讀取",
        "read_image" => "讀取圖片",
        "read_binary" => "讀取二進位",
        "read_document" => "讀取文件",
        "start_process" => "執行指令",
        "read_process_output" => "讀取程序輸出",
        "list_directory" => "瀏覽目錄",
        "list_host_directory" => "瀏覽主機目錄",
        "list_files" => "列出檔案",
        "find_files" => "尋找檔案",
        "search_text" => "搜尋文字",
        "search_texts" => "批次搜尋文字",
        "query_files" => "查詢檔案",
        "project_context" => "讀取專案概覽",
        "write_file" => "寫入檔案",
        "edit_block" => "編輯檔案",
        "create_directory" => "建立目錄",
        "delete_file" => "刪除檔案",
        "move_file" => "移動檔案",
        "server_diagnostics" => "服務診斷",
        "file_info" => "檔案資訊",
        "workspace_info" => "工作區資訊",
        "list_projects" => "列出專案",
        "hash_files" => "計算檔案雜湊",
        "compare_paths" => "比較路徑",
        "project_status" => "專案狀態",
        "inspect_media" => "檢視影音資訊",
        "inspect_archive" => "檢視壓縮檔",
        "interact_with_process" => "傳送程序輸入",
        "list_sessions" => "列出執行工作階段",
        "force_terminate" => "終止程序",
        "close_session" => "關閉執行工作階段",
        "export_session_file" => "匯出工作階段檔案",
        _ => tool,
    }
}

pub fn shorten(text: &str, count: usize) -> String {
    let end = text
        .char_indices()
        .nth(count)
        .map_or(text.len(), |(at, _)| at);
    let mut result = String::with_capacity(end + 3);
    for ch in text[..end].chars() {
        result.push(match ch {
            '\n' => '↵',
            '\r' => '␍',
            ch if ch.is_control() => ' ',
            ch => ch,
        });
    }
    if end < text.len() {
        result.push('…');
    }
    result
}

pub fn preview(value: &Value, count: usize) -> String {
    match value {
        Value::Object(fields) => format!("{{…}} {} 個欄位", fields.len()),
        Value::Array(items) => format!("[…] {} 個項目", items.len()),
        Value::String(text) => shorten(text, count),
        other => shorten(&other.to_string(), count),
    }
}

pub fn arguments(record: &Record) -> Option<&Map<String, Value>> {
    record
        .request
        .as_ref()?
        .pointer("/params/arguments")?
        .as_object()
}

fn field(args: &Map<String, Value>, key: &str, fallback: &str) -> String {
    args.get(key)
        .map_or_else(|| fallback.to_owned(), |value| preview(value, 75))
}

pub fn row(record: &Record) -> (String, String) {
    let Some(args) = arguments(record) else {
        return ("未擷取操作參數".into(), String::new());
    };
    let partial = !record.request_notes.is_empty();
    let missing = if partial {
        "未提供／未擷取"
    } else {
        "預設"
    };
    let fallback = |default: &'static str| if partial { missing } else { default };
    let root = format!("共享根：{}", field(args, "root_id", missing));
    let with_root = |scope: String| match (args.contains_key("root_id"), scope.is_empty()) {
        (true, true) => root.clone(),
        (true, false) => format!("{root} · {scope}"),
        _ => scope,
    };
    let tool = record.tool.as_str();
    match tool {
        "read_files" => {
            let Some(files) = args.get("files").and_then(Value::as_array) else {
                return ("未擷取批次目標".into(), format!("{root} · 讀取範圍未確認"));
            };
            let path = |value: &Value| match value {
                Value::Object(object) => object
                    .get("path")
                    .map_or_else(|| "未擷取".into(), |path| preview(path, 160)),
                other => preview(other, 160),
            };
            let target = files.first().map_or_else(|| "無已擷取目標".into(), path);
            let mut context = files.get(1).map_or_else(String::new, path);
            if files.len() > 2 {
                context.push_str(&format!(" · 另有 {} 個", files.len() - 2));
            }
            (target, with_root(context))
        }
        "read_file" | "read_host_file" | "read_file_ranges" | "read_binary" | "read_document"
        | "read_image" => {
            let scope = match tool {
                "read_file" | "read_host_file"
                    if !partial
                        && !args.contains_key("start_line")
                        && !args.contains_key("line_count") =>
                {
                    String::new()
                }
                "read_file" | "read_host_file" => format!(
                    "第 {} 行起 · {} 行",
                    field(args, "start_line", fallback("1")),
                    field(args, "line_count", fallback("200"))
                ),
                "read_file_ranges" => match args.get("ranges").and_then(Value::as_array) {
                    Some(ranges) => {
                        let mut scope = ranges
                            .iter()
                            .take(2)
                            .map(|entry| match entry.as_object() {
                                Some(range) => format!(
                                    "第 {} 行起／{} 行",
                                    field(range, "start_line", "未擷取"),
                                    field(range, "line_count", "未擷取")
                                ),
                                None => preview(entry, 50),
                            })
                            .collect::<Vec<_>>()
                            .join("、");
                        if ranges.len() > 2 {
                            scope.push('…');
                        }
                        scope
                    }
                    None => format!("範圍：{}", field(args, "ranges", "未擷取")),
                },
                "read_binary" => format!(
                    "位元組起點 {} · 長度 {}",
                    field(args, "offset", fallback("0")),
                    field(args, "length", fallback("4096"))
                ),
                "read_document" => format!(
                    "起點 {} · 上限 {}",
                    field(args, "start", fallback("0")),
                    field(args, "limit", fallback("50"))
                ),
                _ => "圖片".into(),
            };
            let target = args
                .get("path")
                .map_or_else(|| "未擷取路徑".into(), |value| preview(value, 320));
            (target, with_root(scope))
        }
        "list_directory"
        | "list_host_directory"
        | "list_files"
        | "find_files"
        | "search_text"
        | "search_texts"
        | "query_files"
        | "project_context" => {
            let conditions = args
                .iter()
                .filter(|(key, value)| {
                    !matches!(key.as_str(), "root_id" | "directory") && !value.is_null()
                })
                .take(4)
                .map(|(key, value)| format!("{}：{}", shorten(key, 30), preview(value, 65)))
                .collect::<Vec<_>>()
                .join(" · ");
            let scope = if tool == "list_host_directory" {
                "未提供／未擷取"
            } else {
                missing
            };
            let target = format!("範圍：{}", field(args, "directory", scope));
            let context = if conditions.is_empty() {
                format!("{root} · 條件：未指定")
            } else {
                format!("{root} · {conditions}")
            };
            (target, context)
        }
        "compare_paths" => (
            field(args, "left", "未擷取來源"),
            format!("↔ {}", field(args, "right", "未擷取比較目標")),
        ),
        "move_file" => (
            field(args, "source", "未擷取來源"),
            format!("→ {}", field(args, "destination", "未擷取目的地")),
        ),
        "start_process" => (
            field(args, "command", "未擷取指令"),
            format!("工作目錄：{}", field(args, "working_directory", missing)),
        ),
        _ => {
            let value = ["path", "session_id", "directory", "paths"]
                .iter()
                .find_map(|key| args.get(*key));
            let target = value.map_or_else(
                || {
                    args.iter()
                        .take(3)
                        .map(|(key, value)| format!("{}：{}", shorten(key, 35), preview(value, 68)))
                        .collect::<Vec<_>>()
                        .join(" · ")
                },
                |value| preview(value, 240),
            );
            (target, with_root(String::new()))
        }
    }
}

pub fn capture_warning(record: &Record) -> &'static str {
    if record.request.is_none() || record.response.is_none() && record.status != Status::Running {
        "內容缺失"
    } else if !record.request_notes.is_empty() || !record.response_notes.is_empty() {
        "部分內容"
    } else {
        "—"
    }
}

pub fn detail(record: &Record) -> String {
    let Some(args) = arguments(record) else {
        return "未擷取操作參數，請查看請求與擷取註記。".into();
    };
    let (target, scope) = row(record);
    let mut text = operation(&record.tool).to_string();
    for line in [target, scope] {
        if !line.is_empty() {
            text.push('\n');
            text.push_str(&line);
        }
    }
    if record.tool == "read_files" {
        text.push_str("\n各目標的讀取結果請查看回應。");
    }
    text.push_str("\n\n已擷取的提交參數\n");
    text.push_str(&serde_json::to_string_pretty(args).unwrap_or_default());
    text
}

pub fn export(record: &Record, lost: u64, evicted: u64) -> Value {
    let mut raw = json!({
        "id":record.id, "time":record.time,"tool": record.tool, "status": record.status,
        "access_mode":record.access_mode, "elapsed_ms": record.elapsed_ms,
        "request_notes":record.request_notes,"response_notes":record.response_notes,
    });
    if let Some(value) = &record.request {
        raw["request"] = value.clone();
    }
    if let Some(value) = &record.response {
        raw["response"] = value.clone();
    }
    json!({
        "format":"mcp-local-request-monitor-v1", "record":raw,
        "diagnostics":{
            "lost_events":lost, "evicted_records":evicted, "dropped_total":lost.saturating_add(evicted),
            "scope":"目前監控歷程的累計計數。遺失事件只計入可偵測的佇列覆寫與損毀。",
            "capture":"匯出保留的擷取內容與限制註記。"
        }
    })
}

pub fn contains(value: &Value, needle: &str) -> bool {
    use crate::settings::fold;
    match value {
        Value::Object(items) => items
            .iter()
            .any(|(key, value)| fold(key).contains(needle) || contains(value, needle)),
        Value::Array(items) => items.iter().any(|value| contains(value, needle)),
        Value::String(text) => fold(text).contains(needle),
        other => other.to_string().contains(needle),
    }
}
