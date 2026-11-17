use std::path::Path;

use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    budget::Budget,
    documents,
    filesystem::{Metadata, Root},
    listing::hex,
    media,
    parser::{self, Operation},
    text,
};

pub fn sniff(data: &[u8]) -> (&'static str, &'static str) {
    let signatures: &[(&[u8], &str, &str)] = &[
        (b"%PDF-", "PDF", "application/pdf"),
        (b"\x89PNG\r\n\x1a\n", "PNG", "image/png"),
        (b"\xff\xd8\xff", "JPEG", "image/jpeg"),
        (b"GIF87a", "GIF", "image/gif"),
        (b"GIF89a", "GIF", "image/gif"),
        (b"BM", "BMP", "image/bmp"),
        (b"II*\0", "TIFF", "image/tiff"),
        (b"MM\0*", "TIFF", "image/tiff"),
        (b"PK\x03\x04", "ZIP", "application/zip"),
        (b"PK\x05\x06", "ZIP", "application/zip"),
        (b"\x1f\x8b", "GZIP", "application/gzip"),
        (b"7z\xbc\xaf\x27\x1c", "7Z", "application/x-7z-compressed"),
        (b"Rar!\x1a\x07", "RAR", "application/vnd.rar"),
        (b"\xfd7zXZ\0", "XZ", "application/x-xz"),
        (b"BZh", "BZIP2", "application/x-bzip2"),
        (b"fLaC", "FLAC", "audio/flac"),
        (b"OggS", "OGG", "application/ogg"),
        (b"ID3", "MP3", "audio/mpeg"),
        (b"\x1aE\xdf\xa3", "MATROSKA", "video/x-matroska"),
        (
            b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1",
            "OLE",
            "application/x-ole-storage",
        ),
        (
            b"MZ",
            "PE/DOS",
            "application/vnd.microsoft.portable-executable",
        ),
        (b"\x7fELF", "ELF", "application/x-elf"),
        (b"SQLite format 3\0", "SQLITE", "application/vnd.sqlite3"),
        (b"\0\0\x01\0", "ICO", "image/x-icon"),
        (b".snd", "AU", "audio/basic"),
    ];
    for &(prefix, name, mime) in signatures {
        if data.starts_with(prefix) {
            return (name, mime);
        }
    }
    if data.len() >= 12 && data.starts_with(b"RIFF") {
        return match &data[8..12] {
            b"WEBP" => ("WEBP", "image/webp"),
            b"WAVE" => ("WAV", "audio/wav"),
            b"AVI " => ("AVI", "video/x-msvideo"),
            _ => ("RIFF", "application/octet-stream"),
        };
    }
    if matches!(data, [b'P', b'1'..=b'6', b' ' | b'\t' | b'\r' | b'\n', ..]) {
        return ("PPM", "image/x-portable-anymap");
    }
    if data.len() >= 12 && data.starts_with(b"FORM") && matches!(&data[8..12], b"AIFF" | b"AIFC") {
        return ("AIFF", "audio/aiff");
    }
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        let end = (u32::from_be_bytes(data[..4].try_into().unwrap()) as usize)
            .min(data.len())
            .min(4096);
        let brands = data.get(16..end).unwrap_or_default().chunks_exact(4);
        if std::iter::once(&data[8..12])
            .chain(brands)
            .any(|brand| matches!(brand, b"avif" | b"avis"))
        {
            return ("AVIF", "image/avif");
        }
        if matches!(&data[8..12], b"heic" | b"heix" | b"mif1" | b"msf1") {
            return ("HEIF", "image/heif");
        }
        return ("MP4", "video/mp4");
    }
    if data.get(257..262) == Some(b"ustar") {
        return ("TAR", "application/x-tar");
    }
    if matches!(data, [255, byte, ..] if byte & 0xe0 == 0xe0) {
        return ("MPEG_AUDIO", "audio/mpeg");
    }
    ("UNKNOWN", "application/octet-stream")
}

pub fn known_binary(data: &[u8]) -> bool {
    !matches!(sniff(data).0, "UNKNOWN" | "PE/DOS" | "BMP" | "MPEG_AUDIO")
}

pub fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map_or_else(String::new, |value| format!(".{}", value.to_lowercase()))
}

pub fn unix_ns(ticks: i64) -> i128 {
    (i128::from(ticks) - 116_444_736_000_000_000) * 100
}

pub fn unix_seconds(ticks: i64) -> f64 {
    const NANOS_PER_SECOND: i128 = 1_000_000_000;
    let nanos = unix_ns(ticks);
    nanos.div_euclid(NANOS_PER_SECOND) as f64 + nanos.rem_euclid(NANOS_PER_SECOND) as f64 * 1e-9
}

pub fn basic_metadata(path: &str, metadata: Metadata) -> Value {
    let mode = 0o100444
        | if metadata.attributes & 1 == 0 {
            0o222
        } else {
            0
        };
    json!({
        "path": path, "name": path.rsplit('/').next().unwrap_or(path),
        "extension": extension(path), "size": metadata.size,
        "time_unit": "Unix seconds UTC",
        "created_time": unix_seconds(metadata.created), "created_time_ns": unix_ns(metadata.created),
        "modified_time": unix_seconds(metadata.modified), "modified_time_ns": unix_ns(metadata.modified),
        "accessed_time": unix_seconds(metadata.accessed), "accessed_time_ns": unix_ns(metadata.accessed),
        "attributes": {"mode": mode, "windows": metadata.attributes, "links": metadata.links},
        "links": metadata.links
    })
}

pub fn limits() -> Value {
    let sqlite = documents::sqlite_available();
    let mut document_formats = vec!["PDF"];
    document_formats.extend_from_slice(documents::ZIP_DOCUMENT_FORMATS);
    document_formats.extend(["EML", "MBOX", "SRT", "VTT"]);
    let mut structured_formats = vec!["CSV", "TSV", "JSON", "JSONL", "XML", "YAML", "INI", "TOML"];
    if sqlite {
        structured_formats.push("SQLITE (Windows deserialize/limits API required)");
    }
    let mut limits = json!({
        "max_source_bytes": parser::MAX_SOURCE, "source_policy": "min(root.max_file_bytes, root.max_scan_bytes, max_source_bytes)",
        "max_path_chars": 4096, "max_root_id_chars": 32, "header_bytes": 4096, "max_range_bytes": 16384,
        "parser_timeout_seconds": 10, "parser_cpu_seconds": 10, "parser_memory_bytes": 512 * 1024 * 1024,
        "max_response_bytes": 256 * 1024, "max_units_per_call": 100, "max_unit_chars": 2000,
        "max_document_page_bytes": 128 * 1024, "max_document_units": 100_000, "max_start": 100_000,
        "max_archive_entries": 4096, "max_archive_member_bytes": 8 * 1024 * 1024,
        "max_archive_expanded_bytes": 128 * 1024 * 1024, "max_compression_ratio": 200, "max_archive_name_chars": 1024,
        "max_zip_directory_bytes": 4 * 1024 * 1024, "max_gzip_expanded_bytes": 32 * 1024 * 1024,
        "max_xml_nodes": 200_000, "max_xml_depth": 64, "max_structured_nodes": 100_000,
        "max_structured_depth": 64, "max_columns": 256, "max_csv_field_chars": 128 * 1024,
        "metadata_for_oversized_files": true, "structured_formats": structured_formats,
        "document_formats": document_formats, "media_formats": media::FORMATS,
        "source_sha256": true, "continuation": "expected_sha256 optionally pins the source snapshot",
        "archive_member_read": true, "toml_available": true, "sqlite_available": sqlite,
        "parser_network": false, "parser_source_paths": false
    });
    let mut format_limits = json!({
        "max_pdf_pages": 2000, "max_pdf_stream_bytes": 8 * 1024 * 1024,
        "max_pdf_page_tree_nodes": 4000, "max_pdf_page_tree_depth": 64, "max_pdf_xform_invocations": 1000,
        "max_sqlite_value_bytes": 1024 * 1024, "max_sqlite_tables": 1024, "max_sqlite_vm_steps": 20_000_000,
        "max_mail_messages": 1000, "max_mime_parts": 1000, "max_mime_depth": 32,
        "yaml_loader": "yaml-rust2 event parser; no anchors, aliases, explicit tags or merge",
        "max_media_tracks": 32, "max_media_boxes": 10_000, "max_media_tags": 32,
        "max_ebml_elements": 10_000, "ebml_unknown_sizes": "Segment only; unknown-size Cluster rejected",
        "max_media_tag_key_chars": 128, "max_media_tag_values": 4, "max_media_tag_value_chars": 1024,
        "max_mp4_timing_entries": 100_000, "max_avi_list_depth": 8
    });
    limits
        .as_object_mut()
        .unwrap()
        .append(format_limits.as_object_mut().unwrap());
    limits
}

pub fn info(root: &Root, path: &str, budget: &mut Budget) -> Result<Value> {
    let access = root.open(path, true)?;
    let metadata = access.metadata();
    let header = access.read(
        0,
        4096.min(root.settings.max_file_bytes)
            .min(root.settings.max_scan_bytes),
        budget,
    )?;
    let (signature, mime) = sniff(&header);
    let mut format = signature.to_owned();
    let mut detection = "signature/header; content not yet validated".to_owned();
    let mut container_error = None;
    if signature == "UNKNOWN"
        && metadata.size == header.len() as u64
        && text::decode(&header).is_ok()
    {
        format = "UTF-8".into();
        detection = "complete small-file UTF-8 validation (no NUL)".into();
    }
    let max = source_limit(root);
    if metadata.size <= max as u64
        && (signature == "ZIP" || signature == "UNKNOWN" && !header.contains(&0))
    {
        let mut source = header;
        if source.len() < metadata.size as usize {
            source.extend(access.read(source.len() as u64, max - source.len(), budget)?);
        }
        let operation = Operation::Identify {
            excluded_names: root.settings.excluded_names.clone(),
        };
        match parser::parse(operation, source, budget) {
            Ok(result) => {
                format = result["format"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("格式結果無效。"))?
                    .into();
                detection = result["detection"].as_str().unwrap_or(
                    "validated ZIP directory and container type; document content not yet validated"
                ).into();
            }
            Err(error) => {
                budget.check()?;
                container_error = error.to_string().split('：').next().map(str::to_owned);
            }
        }
    }
    let mime = match format.as_str() {
        "UTF-8" => "text/plain",
        "JSON" => "application/json",
        "JSONL" => "application/x-ndjson",
        "XML" => "application/xml",
        "YAML" => "application/yaml",
        "CSV" => "text/csv",
        "TSV" => "text/tab-separated-values",
        "EML" => "message/rfc822",
        "MBOX" => "application/mbox",
        "DOCX" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "XLSX" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "PPTX" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "ODT" => "application/vnd.oasis.opendocument.text",
        "ODS" => "application/vnd.oasis.opendocument.spreadsheet",
        "ODP" => "application/vnd.oasis.opendocument.presentation",
        "EPUB" => "application/epub+zip",
        _ => mime,
    };
    let within = metadata.size <= root.settings.max_file_bytes as u64;
    let mut capabilities = vec!["file_info"];
    if within {
        capabilities.push("read_binary");
        if metadata.size <= root.settings.max_scan_bytes as u64 {
            capabilities.push("hash_files");
        }
        if root.is_text(&access.relative) && matches!(format.as_str(), "UNKNOWN" | "UTF-8") {
            capabilities.push("read_file (UTF-8 validation required)");
        }
        if metadata.size <= max as u64 {
            if documents::TEXT_FORMATS.contains(&format.as_str())
                || documents::ZIP_DOCUMENT_FORMATS.contains(&format.as_str())
                || format == "PDF"
                || (format == "SQLITE" && documents::sqlite_available())
            {
                capabilities.push("read_document (container validation required)");
            } else if format == "UTF-8" {
                capabilities.push("read_document (format_hint and structure validation required)");
            }
            if matches!(format.as_str(), "ZIP" | "TAR" | "GZIP")
                || documents::ZIP_DOCUMENT_FORMATS.contains(&format.as_str())
            {
                capabilities.push("inspect_archive");
            }
            if media::supports(&format) {
                capabilities.push("inspect_media");
            }
        }
        if crate::images::expected(&access.relative) == Some(format.as_str())
            && metadata.size <= root.settings.max_scan_bytes.min(16 * 1024 * 1024) as u64
        {
            capabilities.push("read_image (static decode required)");
        }
    }
    let mut row = basic_metadata(&access.relative, metadata);
    row["format"] = json!(format);
    row["mime_type"] = json!(mime);
    row["detection"] = json!(detection);
    row["container_error"] = json!(container_error);
    row["capabilities"] = json!(capabilities);
    row["content_within_root_limit"] = json!(within);
    row["limits"] = limits();
    Ok(row)
}

fn source_limit(root: &Root) -> usize {
    parser::MAX_SOURCE
        .min(root.settings.max_file_bytes)
        .min(root.settings.max_scan_bytes)
}

pub fn inspect(
    root: &Root,
    path: &str,
    operation: Operation,
    expected: Option<&str>,
    budget: &mut Budget,
) -> Result<Value> {
    let file = root.open(path, true)?;
    let max = source_limit(root);
    ensure!(
        file.metadata().size <= max as u64,
        "FORMAT_SOURCE_LIMIT：來源超過解析上限；可改用 file_info 或 hash_files。"
    );
    let content = file.read(0, max, budget)?;
    let digest = hex(&Sha256::digest(&content));
    ensure!(
        expected.is_none_or(|expected| expected == digest),
        "FORMAT_SOURCE_CHANGED：來源 SHA-256 已變更，請重新開始分頁。"
    );
    let mut result = parser::parse(operation, content, budget)?;
    result["source_sha256"] = json!(digest);
    result["path"] = json!(path);
    Ok(result)
}

pub fn read_binary(
    root: &Root,
    path: &str,
    offset: u64,
    length: usize,
    budget: &mut Budget,
) -> Result<Value> {
    ensure!(
        length <= root.settings.max_scan_bytes,
        "BINARY_RANGE：超過 root 累計讀取限制。"
    );
    let access = root.open(path, true)?;
    let size = access.metadata().size;
    ensure!(
        size <= root.settings.max_file_bytes as u64,
        "檔案超過設定容量上限。"
    );
    let data = access.read(offset, length, budget)?;
    Ok(json!({
        "path": path, "offset": offset, "returned_bytes": data.len(), "size": size,
        "encoding": "base64", "data": STANDARD.encode(&data),
        "has_more": offset + (data.len() as u64) < size
    }))
}
