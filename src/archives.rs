use std::{collections::BTreeSet, io::Read, ops::Range};

use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::{Decompress, FlushDecompress, Status, read::MultiGzDecoder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{filesystem::valid_name, formats::sniff, listing::hex, settings::fold};

const MAX_ENTRIES: usize = 4096;
const MAX_MEMBER: usize = 8 * 1024 * 1024;
const MAX_EXPANDED: usize = 128 * 1024 * 1024;
const MAX_GZIP: usize = 32 * 1024 * 1024;
const MAX_RATIO: usize = 200;
const INVALID: &str = "ARCHIVE_INVALID：封存結構或成員界線無效。";
const PATH_ERROR: &str = "ARCHIVE_PATH：封存含不安全或含糊路徑。";
const LIMIT: &str = "ARCHIVE_LIMIT：項目數、容量或壓縮比例超限。";
const LINK: &str = "ARCHIVE_LINK：拒絕重複名稱、連結及特殊項目。";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    pub start: usize,
    pub limit: usize,
    pub member_path: Option<String>,
    pub offset: usize,
    pub length: usize,
    pub excluded_names: Vec<String>,
}

struct Member {
    name: String,
    range: Range<usize>,
    size: usize,
    directory: bool,
    hidden: bool,
    deflate: bool,
    crc: Option<u32>,
}

pub struct Zip<'a> {
    source: &'a [u8],
    members: Vec<Member>,
    hidden_dirs: BTreeSet<String>,
}

pub fn safe_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.chars().count() <= 1024 && !name.contains('\\'),
        "{PATH_ERROR}"
    );
    for part in name.trim_end_matches('/').split('/') {
        valid_name(part).map_err(|_| anyhow::anyhow!(PATH_ERROR))?;
    }
    Ok(())
}

pub(crate) fn excluded(name: &str, names: &[String]) -> bool {
    name.split('/')
        .any(|part| part.starts_with('.') || names.binary_search(&fold(part)).is_ok())
}

fn word(data: &[u8], start: usize) -> usize {
    u16::from_le_bytes(data[start..start + 2].try_into().unwrap()) as usize
}

fn dword(data: &[u8], start: usize) -> usize {
    u32::from_le_bytes(data[start..start + 4].try_into().unwrap()) as usize
}

fn zip_name(bytes: &[u8], utf8: bool) -> Result<String> {
    let name = if utf8 {
        std::str::from_utf8(bytes)?.to_owned()
    } else {
        const CP437: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■ ";
        let high: Vec<_> = CP437.chars().collect();
        bytes
            .iter()
            .map(|&b| {
                if b < 128 {
                    char::from(b)
                } else {
                    high[usize::from(b - 128)]
                }
            })
            .collect()
    };
    safe_name(&name)?;
    Ok(name)
}

impl<'a> Zip<'a> {
    pub fn parse(source: &'a [u8]) -> Result<Self> {
        let lower = source.len().saturating_sub(65_557);
        let end = source[lower..]
            .windows(4)
            .rposition(|window| window == b"PK\x05\x06")
            .map(|position| position + lower)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        let eocd = source
            .get(end..end + 22)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        let count = word(eocd, 10);
        let central = dword(eocd, 16);
        let central_size = dword(eocd, 12);
        ensure!(
            count <= MAX_ENTRIES
                && word(eocd, 8) == count
                && word(eocd, 4) == 0
                && word(eocd, 6) == 0
                && central_size <= 4 * 1024 * 1024
                && central + central_size == end
                && end + 22 + word(eocd, 20) == source.len(),
            "{LIMIT}"
        );
        let mut cursor = central;
        let mut spans = Vec::with_capacity(count);
        let mut members = Vec::with_capacity(count);
        let mut seen = BTreeSet::new();
        let mut total = 0usize;
        for _ in 0..count {
            let record = source[..end]
                .get(cursor..cursor + 46)
                .ok_or_else(|| anyhow::anyhow!(INVALID))?;
            ensure!(&record[..4] == b"PK\x01\x02", "{INVALID}");
            let (flags, method, crc, compressed, size) = (
                word(record, 8),
                word(record, 10),
                dword(record, 16),
                dword(record, 20),
                dword(record, 24),
            );
            ensure!(
                flags & 0x41 == 0 && matches!(method, 0 | 8),
                "ARCHIVE_METHOD：不支援加密或此壓縮方法。"
            );
            let attr = dword(record, 38);
            let mode = (attr >> 16) & 0xf000;
            ensure!(
                matches!(mode, 0 | 0x8000 | 0x4000) && attr & 0x400 == 0,
                "{LINK}"
            );
            ensure!(word(record, 34) == 0, "{INVALID}");
            total += size;
            ensure!(
                size <= MAX_MEMBER
                    && total <= MAX_EXPANDED
                    && size <= compressed.max(1) * MAX_RATIO,
                "{LIMIT}"
            );
            let (name_len, extra_len, comment_len) =
                (word(record, 28), word(record, 30), word(record, 32));
            let bytes = source[..end]
                .get(cursor + 46..cursor + 46 + name_len)
                .ok_or_else(|| anyhow::anyhow!(INVALID))?;
            let name = zip_name(bytes, flags & 0x800 != 0)?;
            ensure!(seen.insert(fold(name.trim_end_matches('/'))), "{LINK}");
            cursor += 46 + name_len + extra_len + comment_len;
            let local = dword(record, 42);
            let local_record = source[..central]
                .get(local..local + 30)
                .ok_or_else(|| anyhow::anyhow!(INVALID))?;
            ensure!(
                &local_record[..4] == b"PK\x03\x04"
                    && word(local_record, 6) == flags
                    && word(local_record, 8) == method,
                "{INVALID}"
            );
            let local_name_len = word(local_record, 26);
            let local_name = source[..central].get(local + 30..local + 30 + local_name_len);
            ensure!(local_name == Some(bytes), "{INVALID}");
            let begin = local + 30 + local_name_len + word(local_record, 28);
            let finish = begin + compressed;
            ensure!(finish <= central, "{INVALID}");
            let mut span_end = finish;
            let descriptor = if flags & 8 != 0 {
                let begin = finish
                    + if source.get(finish..finish + 4) == Some(b"PK\x07\x08") {
                        4
                    } else {
                        0
                    };
                span_end = begin + 12;
                source[..central]
                    .get(begin..span_end)
                    .ok_or_else(|| anyhow::anyhow!(INVALID))?
            } else {
                &local_record[14..26]
            };
            ensure!(
                (
                    dword(descriptor, 0),
                    dword(descriptor, 4),
                    dword(descriptor, 8)
                ) == (crc, compressed, size),
                "{INVALID}"
            );
            spans.push((local, span_end));
            let directory = name.ends_with('/') || attr & 0x10 != 0 || mode == 0x4000;
            members.push(Member {
                name,
                range: begin..finish,
                size,
                directory,
                hidden: attr & 2 != 0,
                deflate: method == 8,
                crc: Some(crc as u32),
            });
        }
        ensure!(cursor == end, "{INVALID}");
        spans.sort_unstable();
        ensure!(
            spans.windows(2).all(|pair| pair[0].1 <= pair[1].0),
            "ARCHIVE_OVERLAP：成員重疊。"
        );
        let hidden_dirs = members
            .iter()
            .filter(|member| member.hidden && member.directory)
            .map(|member| fold(member.name.trim_end_matches('/')))
            .collect();
        Ok(Self {
            source,
            members,
            hidden_dirs,
        })
    }

    fn hidden(&self, member: &Member) -> bool {
        if member.hidden {
            return true;
        }
        let name = fold(member.name.trim_end_matches('/'));
        name.match_indices('/')
            .any(|(index, _)| self.hidden_dirs.contains(&name[..index]))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.members.iter().any(|member| member.name == name)
    }

    pub fn member(&self, name: &str, exclusions: &[String]) -> Result<Vec<u8>> {
        safe_name(name)?;
        ensure!(
            !excluded(name, exclusions),
            "ARCHIVE_EXCLUDED：成員已排除。"
        );
        let member = self
            .members
            .iter()
            .find(|member| member.name == name && !member.directory && !self.hidden(member))
            .ok_or_else(|| anyhow::anyhow!("ARCHIVE_MEMBER：找不到可讀取的一般成員。"))?;
        let data = &self.source[member.range.clone()];
        let result = if member.deflate {
            let mut expanded = vec![0; member.size + 1];
            let mut decoder = Decompress::new(false);
            let status = decoder.decompress(data, &mut expanded, FlushDecompress::Finish)?;
            ensure!(
                status == Status::StreamEnd
                    && decoder.total_in() == data.len() as u64
                    && decoder.total_out() == member.size as u64,
                "{INVALID}"
            );
            expanded.truncate(member.size);
            expanded
        } else {
            ensure!(data.len() == member.size, "{INVALID}");
            data.to_vec()
        };
        ensure!(
            Some(crc32fast::hash(&result)) == member.crc,
            "ARCHIVE_CRC：成員 CRC 不符。"
        );
        Ok(result)
    }
}

fn tar_oct(bytes: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(bytes)?.trim_matches([' ', '\0']);
    if text.is_empty() {
        return Ok(0);
    }
    usize::from_str_radix(text, 8).map_err(|_| anyhow::anyhow!(INVALID))
}

fn tar_path(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes.split(|&byte| byte == 0).next().unwrap()).map_err(Into::into)
}

#[derive(Default)]
struct TarMetadata<'a> {
    path: Option<&'a str>,
    size: Option<usize>,
}

impl<'a> TarMetadata<'a> {
    fn apply_pax(&mut self, mut bytes: &'a [u8]) -> Result<()> {
        while !bytes.is_empty() {
            if bytes[0] == 0 {
                ensure!(bytes.iter().all(|&byte| byte == 0), "{INVALID}");
                break;
            }
            // A record's decimal length includes its own digits and the newline.
            let separator = bytes
                .iter()
                .position(|byte| !byte.is_ascii_digit())
                .ok_or_else(|| anyhow::anyhow!(INVALID))?;
            ensure!(separator > 0 && bytes[separator] == b' ', "{INVALID}");
            let length = std::str::from_utf8(&bytes[..separator])?
                .parse::<usize>()
                .map_err(|_| anyhow::anyhow!(INVALID))?;
            ensure!(
                length > separator + 3 && length <= bytes.len() && bytes[length - 1] == b'\n',
                "{INVALID}"
            );
            let record = &bytes[separator + 1..length - 1];
            let equals = record
                .iter()
                .position(|&byte| byte == b'=')
                .ok_or_else(|| anyhow::anyhow!(INVALID))?;
            let key = &record[..equals];
            let value = &record[equals + 1..];
            ensure!(!key.is_empty() && !key.contains(&0), "{INVALID}");
            match key {
                b"path" => {
                    let path = std::str::from_utf8(value)?;
                    safe_name(path)?;
                    self.path = Some(path.trim_end_matches('/'));
                }
                b"size" => {
                    ensure!(
                        !value.is_empty() && value.iter().all(u8::is_ascii_digit),
                        "{INVALID}"
                    );
                    self.size = Some(
                        std::str::from_utf8(value)?
                            .parse()
                            .map_err(|_| anyhow::anyhow!(INVALID))?,
                    );
                }
                b"SCHILY.realsize" => bail!("{LINK}"),
                b"SCHILY.filetype" if value == b"sparse" => bail!("{LINK}"),
                key if key.starts_with(b"GNU.sparse.") => bail!("{LINK}"),
                _ => {}
            }
            bytes = &bytes[length..];
        }
        Ok(())
    }
}

fn tar_members(source: &[u8]) -> Result<Vec<Member>> {
    let mut cursor = 0;
    let mut members = Vec::new();
    let mut seen = BTreeSet::new();
    let mut total = 0;
    let mut global = TarMetadata::default();
    let mut pending = TarMetadata::default();
    let mut needs_member = false;
    // Extension records do not count as members, but still consume a header budget.
    for _ in 0..=MAX_ENTRIES * 3 {
        let header = source
            .get(cursor..cursor + 512)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        if header.iter().all(|&byte| byte == 0) {
            ensure!(!needs_member, "{INVALID}");
            return Ok(members);
        }
        let sum: usize = header
            .iter()
            .enumerate()
            .map(|(index, &byte)| {
                usize::from(if (148..156).contains(&index) {
                    b' '
                } else {
                    byte
                })
            })
            .sum();
        ensure!(tar_oct(&header[148..156])? == sum, "{INVALID}");
        let kind = header[156];
        let extended = matches!(kind, b'g' | b'x' | b'X' | b'L');
        ensure!(extended || matches!(kind, 0 | b'0' | b'5' | b'7'), "{LINK}");
        let physical_size = tar_oct(&header[124..136])?;
        let size = if extended {
            physical_size
        } else {
            pending.size.or(global.size).unwrap_or(physical_size)
        };
        let start = cursor + 512;
        ensure!(
            size <= MAX_MEMBER && start + size <= source.len(),
            "{LIMIT}"
        );
        cursor = start + size.div_ceil(512) * 512;
        if extended {
            let body = &source[start..start + size];
            needs_member = true;
            match kind {
                b'g' => global.apply_pax(body)?,
                b'L' => {
                    let path = std::str::from_utf8(body)?.trim_end_matches('\0');
                    safe_name(path)?;
                    pending.path = pending.path.or(Some(path));
                }
                _ => {
                    let mut metadata = TarMetadata::default();
                    metadata.apply_pax(body)?;
                    pending.path = pending.path.or(metadata.path);
                    pending.size = pending.size.or(metadata.size);
                }
            }
            continue;
        }
        let directory = kind == b'5';
        let path = if let Some(path) = pending.path.or(global.path) {
            path.to_owned()
        } else {
            let name = tar_path(&header[..100])?;
            let prefix = if &header[257..263] == b"ustar\0" {
                tar_path(&header[345..500])?
            } else {
                ""
            };
            if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            }
        };
        pending = TarMetadata::default();
        needs_member = false;
        let name = if directory {
            path.trim_end_matches('/').to_owned()
        } else {
            path
        };
        safe_name(&name)?;
        ensure!(seen.insert(fold(name.trim_end_matches('/'))), "{LINK}");
        total += size;
        ensure!(
            members.len() < MAX_ENTRIES && total <= MAX_EXPANDED,
            "{LIMIT}"
        );
        members.push(Member {
            name,
            range: start..start + size,
            size,
            directory,
            hidden: false,
            deflate: false,
            crc: None,
        });
    }
    bail!("{LIMIT}")
}

pub fn inspect(data: &[u8], options: Options) -> Result<Value> {
    ensure!(
        options.start <= 100_000
            && (1..=100).contains(&options.limit)
            && options.offset <= MAX_MEMBER
            && (1..=16_384).contains(&options.length),
        "FORMAT_RANGE：成員或分頁超限。"
    );
    let requested = options.member_path.as_deref();
    if let Some(requested) = requested {
        safe_name(requested)?;
        ensure!(
            options.start == 0 && options.limit == 50,
            "FORMAT_RANGE：成員模式不可同時指定目錄分頁。"
        );
        ensure!(
            !excluded(requested, &options.excluded_names),
            "ARCHIVE_EXCLUDED：成員已排除。"
        );
    } else {
        ensure!(
            options.offset == 0 && options.length == 4096,
            "FORMAT_RANGE：offset/length 需要 member_path。"
        );
    }
    let format = sniff(data).0;
    let mut rows = Vec::new();
    let mut total = 0;
    let mut skipped = 0;
    if format == "ZIP" {
        let zip = Zip::parse(data)?;
        if let Some(requested) = requested {
            let bytes = zip.member(requested, &options.excluded_names)?;
            return Ok(bytes_result("ZIP", requested, &bytes, &options));
        }
        for member in &zip.members {
            total += member.size;
            if excluded(&member.name, &options.excluded_names) || zip.hidden(member) {
                skipped += 1;
                continue;
            }
            rows.push(json!({
                "name": member.name, "size": member.size, "compressed_size": member.range.len(),
                "compression": if member.deflate { "deflate" } else { "stored" }, "directory": member.directory,
                "crc32": format!("{:08x}", member.crc.unwrap())
            }));
        }
        Ok(page("ZIP", rows, total, skipped, &options))
    } else {
        let (source, compression);
        let expanded;
        match format {
            "GZIP" => {
                let maximum = MAX_GZIP.min(data.len().saturating_mul(MAX_RATIO));
                expanded = {
                    let mut expanded = Vec::new();
                    MultiGzDecoder::new(data)
                        .take((maximum + 1) as u64)
                        .read_to_end(&mut expanded)?;
                    ensure!(expanded.len() <= maximum, "{LIMIT}");
                    expanded
                };
                source = expanded.as_slice();
                compression = "gzip";
            }
            "TAR" => {
                source = data;
                compression = "stored";
            }
            _ => bail!("ARCHIVE_FORMAT：只支援 ZIP、TAR、TAR.GZ。"),
        }
        let format = if format == "GZIP" { "TAR.GZ" } else { "TAR" };
        let members = tar_members(source)?;
        for member in members {
            total += member.size;
            if excluded(&member.name, &options.excluded_names) {
                skipped += 1;
                continue;
            }
            if let Some(requested) = requested {
                if requested == member.name && !member.directory {
                    return Ok(bytes_result(
                        format,
                        requested,
                        &source[member.range],
                        &options,
                    ));
                }
            }
            rows.push(
                json!({"name": member.name, "size": member.size, "directory": member.directory,
                "compressed_size": null, "compression": compression}),
            );
        }
        ensure!(
            requested.is_none(),
            "ARCHIVE_MEMBER：找不到可讀取的一般成員。"
        );
        Ok(page(format, rows, total, skipped, &options))
    }
}

fn bytes_result(format: &str, path: &str, contents: &[u8], options: &Options) -> Value {
    let end = (options.offset + options.length).min(contents.len());
    let bytes = &contents[options.offset.min(end)..end];
    let has_more = options.offset + bytes.len() < contents.len();
    json!({
        "format": format, "member_path": path, "size": contents.len(), "offset": options.offset,
        "returned_bytes": bytes.len(), "encoding": "base64", "data": STANDARD.encode(bytes),
        "sha256": hex(&Sha256::digest(contents)), "has_more": has_more,
        "next_offset": has_more.then_some(options.offset + bytes.len()), "member_contents_validated": true,
        "integrity": if format == "ZIP" { "CRC32 and exact length" } else { "TAR header checksum and bounds; TAR has no payload checksum" },
        "nested_archives": "opaque bytes; never recursively opened"
    })
}

fn page(format: &str, rows: Vec<Value>, total: usize, skipped: usize, options: &Options) -> Value {
    let count = rows.len();
    let mut size = 0;
    let page: Vec<_> = rows
        .into_iter()
        .skip(options.start)
        .take(options.limit)
        .take_while(|row| {
            size += serde_json::to_string(row).unwrap().len() + 16;
            size <= 128 * 1024
        })
        .collect();
    let end = options.start + page.len();
    let more = end < count;
    json!({
        "format": format, "entries": page, "returned_count": page.len(), "total_entries": count, "skipped_entries": skipped,
        "declared_expanded_bytes": total, "has_more": more, "next_start": more.then_some(end),
        "member_contents_validated": false, "nested_archives": "opaque entries; never recursively opened"
    })
}
