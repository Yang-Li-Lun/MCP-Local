use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Result, bail, ensure};
use roxmltree::{Document, Node, ParsingOptions};
use serde::{
    Deserialize, Serialize,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value, json};

use crate::{
    archives::{Zip, excluded, safe_name},
    formats::sniff,
    text::{self, spaced_json},
};

mod containers;
mod mail;
mod pdf;
mod sqlite;
mod yaml;

const MAX_UNITS: usize = 100_000;
const MAX_PAGE: usize = 128 * 1024;
const INVALID: &str = "DOCUMENT_FORMAT：文件不符合支援的格式或結構。";
const STRUCTURE: &str = "DOCUMENT_STRUCTURE_LIMIT：節點、深度或來源位置超限。";
pub const TEXT_FORMATS: &[&str] = &[
    "CSV", "TSV", "JSON", "JSONL", "XML", "YAML", "TOML", "INI", "EML", "MBOX", "SRT", "VTT",
];
pub const ZIP_DOCUMENT_FORMATS: &[&str] = &["DOCX", "XLSX", "PPTX", "ODT", "ODS", "ODP", "EPUB"];

pub fn sqlite_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(sqlite::available)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    pub start: usize,
    pub limit: usize,
    pub format_hint: Option<String>,
    pub table: Option<String>,
    pub excluded_names: Vec<String>,
}

struct Page {
    start: usize,
    limit: usize,
    rows: Vec<Value>,
    count: usize,
    used: usize,
    limit_hit: bool,
    more: Option<usize>,
}

impl Page {
    fn new(start: usize, limit: usize) -> Self {
        Self {
            start,
            limit,
            rows: Vec::new(),
            count: 0,
            used: 0,
            limit_hit: false,
            more: None,
        }
    }

    fn add(&mut self, location: Value, text: &str) -> Result<()> {
        let mut remaining = text;
        let mut offset = 0;
        loop {
            ensure!(
                self.count < MAX_UNITS,
                "DOCUMENT_UNITS_LIMIT：文件片段數超限。"
            );
            let end = remaining
                .char_indices()
                .nth(2000)
                .map_or(remaining.len(), |(i, _)| i);
            if self.count >= self.start && self.more.is_none() {
                if self.rows.len() >= self.limit {
                    self.more = Some(self.count);
                } else {
                    let row = json!({"index": self.count, "location": location, "char_offset": offset, "text": &remaining[..end]});
                    let bytes = spaced_json(&row)?.len();
                    ensure!(
                        bytes <= MAX_PAGE,
                        "DOCUMENT_OUTPUT_LIMIT：單一來源位置超過分頁預算。"
                    );
                    if self.used + bytes > MAX_PAGE {
                        self.limit_hit = true;
                        self.more = Some(self.count);
                    } else {
                        self.rows.push(row);
                        self.used += bytes;
                    }
                }
            }
            self.count += 1;
            remaining = &remaining[end..];
            if remaining.is_empty() {
                break;
            }
            offset += 2000;
        }
        Ok(())
    }

    fn result(self, format: &str) -> Value {
        json!({
            "format": format, "unit": "text segment (zero-based index)",
            "units": self.rows, "returned_count": self.rows.len(), "next_start": self.more,
            "has_more": self.more.is_some(), "total_units": self.more.is_none().then_some(self.count),
            "response_limited": self.limit_hit
        })
    }
}

struct JsonCheck<'a> {
    depth: usize,
    nodes: &'a mut usize,
}

impl<'de> DeserializeSeed<'de> for JsonCheck<'_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        *self.nodes += 1;
        if self.depth > 64 || *self.nodes > MAX_UNITS {
            return Err(de::Error::custom(STRUCTURE));
        }
        decoder.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for JsonCheck<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("JSON")
    }
    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<(), E> {
        if value.is_finite() {
            Ok(())
        } else {
            Err(E::custom("non-finite number"))
        }
    }
    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut array: A) -> Result<(), A::Error> {
        while array
            .next_element_seed(JsonCheck {
                depth: self.depth + 1,
                nodes: self.nodes,
            })?
            .is_some()
        {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<(), A::Error> {
        let mut keys = BTreeSet::new();
        while let Some(key) = entries.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON key"));
            }
            entries.next_value_seed(JsonCheck {
                depth: self.depth + 1,
                nodes: self.nodes,
            })?;
        }
        Ok(())
    }
}

fn strict_json(text: &str) -> Result<Value> {
    let mut decoder = serde_json::Deserializer::from_str(text);
    JsonCheck {
        depth: 0,
        nodes: &mut 0,
    }
    .deserialize(&mut decoder)?;
    decoder.end()?;
    let value = serde_json::from_str(text)?;
    Ok(value)
}

fn leaves(value: &Value, base: Map<String, Value>, page: &mut Page) -> Result<()> {
    leaves_with_dates(value, base, page, &HashSet::new())
}

fn pointer_key(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn leaves_with_dates(
    value: &Value,
    base: Map<String, Value>,
    page: &mut Page,
    dates: &HashSet<String>,
) -> Result<()> {
    let mut nodes = 0;
    let mut pending = vec![(String::new(), value, 0)];
    while let Some((pointer, value, depth)) = pending.pop() {
        nodes += 1;
        ensure!(
            nodes <= MAX_UNITS && depth <= 64 && pointer.chars().count() <= 4096,
            "{STRUCTURE}"
        );
        match value {
            Value::Object(object) if !object.is_empty() => {
                ensure!(
                    nodes + pending.len() + object.len() <= MAX_UNITS,
                    "{STRUCTURE}"
                );
                pending.extend(object.iter().rev().map(|(key, value)| {
                    let key = pointer_key(key);
                    (format!("{pointer}/{key}"), value, depth + 1)
                }));
            }
            Value::Array(items) if !items.is_empty() => {
                ensure!(
                    nodes + pending.len() + items.len() <= MAX_UNITS,
                    "{STRUCTURE}"
                );
                pending.extend(
                    items
                        .iter()
                        .enumerate()
                        .rev()
                        .map(|(index, value)| (format!("{pointer}/{index}"), value, depth + 1)),
                );
            }
            _ => {
                let kind = match value {
                    Value::Object(_) => "object",
                    Value::Array(_) => "array",
                    Value::String(_) if dates.contains(&pointer) => "datetime",
                    Value::String(_) => "string",
                    Value::Number(number) if !number.to_string().contains(['.', 'e', 'E']) => {
                        "integer"
                    }
                    Value::Number(number) => {
                        ensure!(
                            number.as_f64().is_some_and(f64::is_finite),
                            "DOCUMENT_NUMBER：非有限數字。"
                        );
                        "number"
                    }
                    Value::Bool(_) => "boolean",
                    Value::Null => "null",
                };
                let mut location = base.clone();
                location.insert("pointer".into(), json!(pointer));
                location.insert("value_type".into(), json!(kind));
                page.add(Value::Object(location), &serde_json::to_string(value)?)?;
            }
        }
    }
    Ok(())
}

fn toml_json(
    value: toml::Value,
    pointer: &str,
    depth: usize,
    count: &mut usize,
    dates: &mut HashSet<String>,
) -> Result<Value> {
    *count += 1;
    ensure!(
        *count <= MAX_UNITS && depth <= 64 && pointer.chars().count() <= 4096,
        "{STRUCTURE}"
    );
    Ok(match value {
        toml::Value::Array(items) => Value::Array(
            items
                .into_iter()
                .enumerate()
                .map(|(index, item)| {
                    toml_json(item, &format!("{pointer}/{index}"), depth + 1, count, dates)
                })
                .collect::<Result<_>>()?,
        ),
        toml::Value::Table(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, value)| {
                    let path = format!("{pointer}/{}", pointer_key(&key));
                    Ok((key, toml_json(value, &path, depth + 1, count, dates)?))
                })
                .collect::<Result<_>>()?,
        ),
        toml::Value::Datetime(datetime) => {
            let mut rendered = datetime
                .date
                .map_or_else(String::new, |date| date.to_string());
            if let Some(time) = datetime.time {
                if datetime.date.is_some() {
                    rendered.push(' ');
                }
                rendered.push_str(&format!(
                    "{:02}:{:02}:{:02}",
                    time.hour,
                    time.minute,
                    time.second.ok_or_else(|| anyhow::anyhow!(INVALID))?
                ));
                let micros = time.nanosecond.unwrap_or(0) / 1000;
                if micros > 0 {
                    rendered.push_str(&format!(".{micros:06}"));
                }
            }
            if let Some(offset) = datetime.offset {
                let offset = offset.to_string();
                rendered.push_str(if matches!(offset.as_str(), "Z" | "-00:00") {
                    "+00:00"
                } else {
                    &offset
                });
            }
            dates.insert(pointer.to_owned());
            Value::String(rendered)
        }
        toml::Value::Float(number) => {
            ensure!(number.is_finite(), "DOCUMENT_NUMBER：非有限數字。");
            json!(number)
        }
        simple => serde_json::to_value(simple)?,
    })
}

fn source_text(data: &[u8]) -> Result<&str> {
    ensure!(!data.contains(&0), "{INVALID}");
    Ok(std::str::from_utf8(
        data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data),
    )?)
}

fn xml(source: &[u8]) -> Result<Document<'_>> {
    let mut options = ParsingOptions::default();
    options.nodes_limit = 200_000;
    let doc = Document::parse_with_options(source_text(source)?, options)?;
    ensure!(
        doc.descendants()
            .all(|node| node.ancestors().take(66).count() < 66),
        "DOCUMENT_XML_LIMIT：XML 深度超限。"
    );
    Ok(doc)
}

fn tag<'a, 'input>(node: Node<'a, 'input>) -> &'input str {
    node.tag_name().name()
}

fn qualified(node: Node<'_, '_>) -> String {
    let name = node.tag_name();
    name.namespace().map_or_else(
        || name.name().to_owned(),
        |namespace| format!("{{{namespace}}}{}", name.name()),
    )
}

fn xml_text<'a, 'input: 'a>(nodes: impl Iterator<Item = Node<'a, 'input>>) -> Option<String> {
    // Non-element markup does not interrupt an element's text or its tail.
    let mut chunks = nodes
        .take_while(|node| !node.is_element())
        .filter(|node| node.is_text())
        .filter_map(|node| node.text());
    let mut value = chunks.next()?.to_owned();
    for chunk in chunks {
        value.push_str(chunk);
    }
    Some(value)
}

fn xml_units(node: Node<'_, '_>, page: &mut Page) -> Result<()> {
    let mut pending = vec![(node, String::new(), 1)];
    while let Some((node, parent, index)) = pending.pop() {
        let name = qualified(node);
        let path = format!("{parent}/{name}[{index}]");
        ensure!(
            path.chars().count() <= 4096 && page.count < MAX_UNITS,
            "{STRUCTURE}"
        );
        let attributes: Map<String, Value> = node
            .attributes()
            .map(|attr| {
                let name = attr.namespace().map_or_else(
                    || attr.name().to_owned(),
                    |ns| format!("{{{ns}}}{}", attr.name()),
                );
                (name, json!(attr.value()))
            })
            .collect();
        let text = xml_text(node.children());
        let tail = xml_text(std::iter::successors(node.next_sibling(), |sibling| {
            sibling.next_sibling()
        }));
        page.add(
            json!({"element": path}),
            &spaced_json(&json!({
                "tag": name, "attributes": attributes, "text": text, "tail": tail,
            }))?,
        )?;
        let children: Vec<_> = node
            .children()
            .filter(Node::is_element)
            .enumerate()
            .collect();
        pending.extend(
            children
                .into_iter()
                .rev()
                .map(|(i, child)| (child, path.clone(), i + 1)),
        );
    }
    Ok(())
}

fn detected(text: &str) -> &str {
    let trimmed = text.trim_start();
    if text.starts_with("WEBVTT") {
        return "VTT";
    }
    let mut first = text::lines(text);
    if first
        .next()
        .is_some_and(|line| !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit()))
        && first
            .next()
            .is_some_and(|line| line.contains(" --> ") && line.contains(','))
    {
        return "SRT";
    }
    if trimmed.starts_with('<') {
        return "XML";
    }
    if trimmed.starts_with("---\n")
        || trimmed.starts_with("---\r\n")
        || trimmed.starts_with("%YAML ")
    {
        return "YAML";
    }
    if text.starts_with("From ") {
        return "MBOX";
    }
    if text::lines(text)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':').map(|(name, _)| name))
        .any(|name| {
            ["from", "subject", "mime-version"]
                .iter()
                .any(|known| name.eq_ignore_ascii_case(known))
        })
    {
        return "EML";
    }
    if trimmed.starts_with(['{', '[']) {
        if strict_json(text).is_ok() {
            return "JSON";
        }
        if text::lines(text).count() > 1
            && text::lines(text)
                .all(|line| line.trim().is_empty() || line.trim_start().starts_with(['{', '[']))
        {
            return "JSONL";
        }
        return "JSON";
    }
    for (separator, format) in [('\t', "TSV"), (',', "CSV")] {
        let mut records = Rows::new(text, separator);
        if let (Ok(Some((a, _))), Ok(Some((b, _)))) = (records.next_row(), records.next_row()) {
            if a.len() == b.len() && (2..=256).contains(&a.len()) {
                return format;
            }
        }
    }
    "UTF-8"
}

struct Rows<'a> {
    remaining: std::iter::Peekable<std::str::Chars<'a>>,
    separator: char,
    line: usize,
}

impl<'a> Rows<'a> {
    fn new(text: &'a str, separator: char) -> Self {
        Self {
            remaining: text.chars().peekable(),
            separator,
            line: 1,
        }
    }

    fn next_row(&mut self) -> Result<Option<(Vec<String>, usize)>> {
        if self.remaining.peek().is_none() {
            return Ok(None);
        }
        let mut row = Vec::new();
        let mut field = String::new();
        let (mut length, mut quoted, mut closed, mut started) = (0, false, false, false);
        while let Some(character) = self.remaining.next() {
            let newline = matches!(character, '\r' | '\n');
            if quoted {
                if character == '"' {
                    if self.remaining.next_if_eq(&'"').is_none() {
                        quoted = false;
                        closed = true;
                        continue;
                    }
                } else if newline {
                    if character == '\r' && self.remaining.next_if_eq(&'\n').is_some() {
                        field.push('\r');
                        field.push('\n');
                        length += 2;
                        self.line += 1;
                        ensure!(length <= 128 * 1024, "DOCUMENT_ROWS：欄位超限。");
                        continue;
                    }
                    self.line += 1;
                }
            } else if character == self.separator || newline {
                if started || !row.is_empty() || !newline {
                    row.push(std::mem::take(&mut field));
                    ensure!(row.len() <= 256, "DOCUMENT_COLUMNS：欄數超限。");
                }
                if newline {
                    if character == '\r' {
                        self.remaining.next_if_eq(&'\n');
                    }
                    self.line += 1;
                    return Ok(Some((row, self.line - 1)));
                }
                started = false;
                closed = false;
                length = 0;
                continue;
            } else if !started && character == '"' {
                started = true;
                quoted = true;
                continue;
            } else {
                ensure!(!closed, "{INVALID}");
            }
            started = true;
            field.push(character);
            length += 1;
            ensure!(length <= 128 * 1024, "DOCUMENT_ROWS：欄位超限。");
        }
        ensure!(!quoted && row.len() < 256, "{INVALID}");
        row.push(field);
        Ok(Some((row, self.line)))
    }
}

fn ini(source: &str) -> Result<Value> {
    let mut sections = Map::new();
    let mut name = String::from("DEFAULT");
    sections.insert(name.clone(), json!({}));
    for line in text::lines(source) {
        let content = line.trim();
        if content.is_empty() || content.starts_with(['#', ';']) {
            continue;
        }
        if let Some(section) = content
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            name = section.into();
            ensure!(
                !name.is_empty() && !sections.contains_key(&name),
                "{INVALID}"
            );
            sections.insert(name.clone(), json!({}));
            continue;
        }
        let (key, value) = content
            .split_once([':', '='])
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        let key = key.trim_end().to_owned();
        ensure!(
            !key.is_empty() && sections[&name].get(&key).is_none(),
            "{INVALID}"
        );
        sections[&name][key] = json!(value.trim_start());
    }
    let defaults = sections["DEFAULT"].as_object().unwrap().clone();
    for (name, section) in &mut sections {
        if name == "DEFAULT" {
            continue;
        }
        for (key, value) in &defaults {
            section
                .as_object_mut()
                .unwrap()
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }
    Ok(Value::Object(sections))
}

fn timestamp(source: &str, hours_required: bool) -> Result<f64> {
    let (clock, millis) = source
        .rsplit_once(['.', ','])
        .ok_or_else(|| anyhow::anyhow!(INVALID))?;
    ensure!(
        millis.len() == 3 && millis.bytes().all(|b| b.is_ascii_digit()),
        "{INVALID}"
    );
    let parts: Vec<_> = clock.split(':').collect();
    ensure!(
        matches!(parts.len(), 2 | 3) && (!hours_required || parts.len() == 3),
        "{INVALID}"
    );
    let last = parts.len() - 1;
    ensure!(
        parts[last].len() == 2 && parts[last - 1].len() == 2,
        "{INVALID}"
    );
    ensure!(
        parts
            .iter()
            .all(|part| part.bytes().all(|b| b.is_ascii_digit())),
        "{INVALID}"
    );
    let seconds = parts[last].parse::<u8>()?;
    let minutes = parts[last - 1].parse::<u8>()?;
    ensure!(seconds <= 59 && minutes <= 59, "{INVALID}");
    let hours = if parts.len() == 3 {
        ensure!(parts[0].len() >= 2, "{INVALID}");
        parts[0].parse::<u32>()?
    } else {
        0
    };
    Ok(f64::from(hours) * 3600.
        + f64::from(minutes) * 60.
        + f64::from(seconds)
        + f64::from(millis.parse::<u16>()?) / 1000.)
}

fn subtitles(source: &str, format: &str, page: &mut Page) -> Result<()> {
    let srt = format == "SRT";
    let mut groups: Vec<Vec<&str>> = Vec::new();
    let mut group = Vec::new();
    for line in text::lines(source.trim()) {
        if line.trim_matches([' ', '\t']).is_empty() {
            if !group.is_empty() {
                groups.push(std::mem::take(&mut group));
            }
        } else {
            group.push(line);
        }
    }
    if !group.is_empty() {
        groups.push(group);
    }
    let mut groups = groups.into_iter();
    if !srt {
        let header = groups.next().ok_or_else(|| anyhow::anyhow!(INVALID))?;
        ensure!(
            header.len() == 1
                && (header[0] == "WEBVTT"
                    || header[0]
                        .strip_prefix("WEBVTT")
                        .is_some_and(|text| text.starts_with([' ', '\t']))),
            "{INVALID}"
        );
    }
    for (index, block) in groups.enumerate() {
        ensure!(index < MAX_UNITS, "{STRUCTURE}");
        let mut lines = block.into_iter();
        let first = lines.next().unwrap();
        if !srt && (first == "NOTE" || first.starts_with("NOTE ")) {
            continue;
        }
        let (label, time) = if first.contains("-->") {
            ("", first)
        } else {
            (first, lines.next().ok_or_else(|| anyhow::anyhow!(INVALID))?)
        };
        ensure!(
            !srt || (!label.is_empty() && label.bytes().all(|b| b.is_ascii_digit())),
            "{INVALID}"
        );
        let (start, end) = time
            .split_once(" --> ")
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        let (end, settings) = end
            .split_once([' ', '\t'])
            .map_or((end, ""), |(end, settings)| {
                (end, settings.trim_start_matches([' ', '\t']))
            });
        let start = timestamp(start, srt)?;
        let end = timestamp(end, srt)?;
        ensure!(
            end >= start && label.chars().count() <= 256 && settings.chars().count() <= 256,
            "{INVALID}"
        );
        page.add(
            json!({"cue": index + 1, "label": label, "start_seconds": start,
            "end_seconds": end, "settings": settings}),
            &lines.collect::<Vec<_>>().join("\n"),
        )?;
    }
    Ok(())
}

fn zip_kind(zip: &Zip<'_>, exclusions: &[String]) -> Result<&'static str> {
    if !zip.contains("[Content_Types].xml") {
        return containers::zip_kind(zip, exclusions);
    }
    let bytes = zip.member("[Content_Types].xml", exclusions)?;
    let types = xml(&bytes)?;
    ensure!(tag(types.root_element()) == "Types", "{INVALID}");
    let candidates = [
        (
            "DOCX",
            "word/document.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        ),
        (
            "XLSX",
            "xl/workbook.xml",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
        ),
        (
            "PPTX",
            "ppt/presentation.xml",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        ),
    ];
    let matches: Vec<_> = candidates
        .into_iter()
        .filter(|(_, part, mime)| {
            zip.contains(part)
                && types.root_element().children().any(|node| {
                    node.attribute("PartName") == Some(&format!("/{part}"))
                        && node.attribute("ContentType") == Some(*mime)
                })
        })
        .collect();
    ensure!(matches.len() == 1, "{INVALID}");
    Ok(matches[0].0)
}

fn namespaced_id<'a>(node: Node<'a, '_>) -> Option<&'a str> {
    node.attributes()
        .find(|attr| attr.name() == "id" && attr.namespace().is_some())
        .map(|attr| attr.value())
}

fn relationships(
    zip: &Zip<'_>,
    location: &str,
    base: &str,
    exclusions: &[String],
) -> Result<HashMap<String, String>> {
    let bytes = zip.member(location, exclusions)?;
    let document = xml(&bytes)?;
    ensure!(tag(document.root_element()) == "Relationships", "{INVALID}");
    let mut links = HashMap::new();
    let absolute = format!("/{base}/");
    for node in document.root_element().children().filter(Node::is_element) {
        if node.attribute("TargetMode") == Some("External") {
            continue;
        }
        let target = node.attribute("Target").unwrap_or("");
        let path = if target.starts_with(&absolute) {
            target[1..].into()
        } else {
            format!("{base}/{target}")
        };
        safe_name(&path)?;
        if !excluded(&path, exclusions)
            && let Some(key) = node.attribute("Id")
        {
            links.insert(key.into(), path);
        }
    }
    Ok(links)
}

fn tagged_text(node: Node<'_, '_>) -> String {
    node.descendants()
        .filter(|element| tag(*element) == "t")
        .filter_map(|element| element.text())
        .collect()
}

fn office(data: &[u8], exclusions: &[String], page: &mut Page) -> Result<&'static str> {
    let zip = Zip::parse(data)?;
    let format = zip_kind(&zip, exclusions)?;
    match format {
        "DOCX" => {
            let bytes = zip.member("word/document.xml", exclusions)?;
            let doc = xml(&bytes)?;
            ensure!(tag(doc.root_element()) == "document", "{INVALID}");
            for (i, p) in doc
                .descendants()
                .filter(|node| tag(*node) == "p")
                .enumerate()
            {
                page.add(json!({"paragraph": i + 1}), &tagged_text(p))?;
            }
        }
        "XLSX" => {
            let strings = if zip.contains("xl/sharedStrings.xml") {
                let bytes = zip.member("xl/sharedStrings.xml", exclusions)?;
                xml(&bytes)?
                    .root_element()
                    .children()
                    .filter(|child| tag(*child) == "si")
                    .map(tagged_text)
                    .collect()
            } else {
                Vec::new()
            };
            let links = relationships(&zip, "xl/_rels/workbook.xml.rels", "xl", exclusions)?;
            let source = zip.member("xl/workbook.xml", exclusions)?;
            let book = xml(&source)?;
            ensure!(tag(book.root_element()) == "workbook", "{INVALID}");
            for sheet in book.descendants().filter(|node| tag(*node) == "sheet") {
                let Some(path) = sheet
                    .attributes()
                    .find(|attr| attr.name() == "id")
                    .and_then(|attr| links.get(attr.value()))
                else {
                    continue;
                };
                let data = zip.member(path, exclusions)?;
                let rows = xml(&data)?;
                ensure!(tag(rows.root_element()) == "worksheet", "{INVALID}");
                let sheet_name: String = sheet
                    .attribute("name")
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect();
                for row in rows.descendants().filter(|node| tag(*node) == "row") {
                    let mut cells = Vec::new();
                    for cell in row.children().filter(|node| tag(*node) == "c") {
                        let mut value = cell
                            .children()
                            .find(|node| tag(*node) == "v")
                            .and_then(|node| node.text())
                            .unwrap_or("")
                            .to_owned();
                        match cell.attribute("t") {
                            Some("s") => {
                                let index: usize = value.trim().parse()?;
                                value = strings
                                    .get(index)
                                    .ok_or_else(|| anyhow::anyhow!(INVALID))?
                                    .clone();
                            }
                            Some("inlineStr") => value = tagged_text(cell),
                            _ => {}
                        }
                        let formula = cell
                            .children()
                            .find(|node| tag(*node) == "f")
                            .and_then(|node| node.text());
                        cells.push(json!({"cell": cell.attribute("r"), "value": value, "formula": formula}));
                    }
                    page.add(json!({"sheet": sheet_name, "row": row.attribute("r").unwrap_or("").chars().take(20).collect::<String>()}),
                        &spaced_json(&cells)?)?;
                }
            }
        }
        "PPTX" => {
            let links = relationships(&zip, "ppt/_rels/presentation.xml.rels", "ppt", exclusions)?;
            let source = zip.member("ppt/presentation.xml", exclusions)?;
            let presentation = xml(&source)?;
            ensure!(
                tag(presentation.root_element()) == "presentation",
                "{INVALID}"
            );
            let mut slide = 0;
            for item in presentation
                .descendants()
                .filter(|node| tag(*node) == "sldId")
            {
                let Some(path) = namespaced_id(item).and_then(|id| links.get(id)) else {
                    continue;
                };
                let bytes = zip.member(path, exclusions)?;
                let tree = xml(&bytes)?;
                ensure!(tag(tree.root_element()) == "sld", "{INVALID}");
                slide += 1;
                let text = tree
                    .descendants()
                    .filter(|node| tag(*node) == "p")
                    .map(tagged_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                page.add(json!({"slide": slide}), &text)?;
            }
        }
        "ODT" | "ODS" | "ODP" => containers::odf(&zip, exclusions, format, page)?,
        "EPUB" => containers::epub(&zip, exclusions, page)?,
        _ => bail!("{INVALID}"),
    }
    Ok(format)
}

pub fn identify(data: &[u8], names: Vec<String>) -> Result<Value> {
    if sniff(data).0 == "ZIP" {
        return Ok(json!({"format": zip_kind(&Zip::parse(data)?, &names)?}));
    }
    let text = source_text(data)?;
    let format = detected(text);
    if format != "UTF-8" {
        read(
            data,
            Options {
                start: 0,
                limit: 1,
                format_hint: Some(format.into()),
                table: None,
                excluded_names: names,
            },
        )?;
    }
    Ok(
        json!({"format": format, "detection": "complete bounded UTF-8/structure validation; ambiguous formats require format_hint"}),
    )
}

pub fn read(data: &[u8], options: Options) -> Result<Value> {
    ensure!(
        options.start <= MAX_UNITS && (1..=100).contains(&options.limit),
        "FORMAT_RANGE：不支援的參數或分頁超限。"
    );
    let format = sniff(data).0;
    ensure!(
        options.table.is_none() || format == "SQLITE",
        "DOCUMENT_FORMAT：table 僅適用 SQLite。"
    );
    let mut page = Page::new(options.start, options.limit);
    if format == "SQLITE" && options.format_hint.is_none() {
        let columns = sqlite::read(data, options.table.as_deref(), &mut page)?;
        let mut output = page.result(format);
        output["columns"] = json!(columns);
        output["mode"] = json!(if options.table.is_some() {
            "rows"
        } else {
            "table schema"
        });
        output["order"] = json!("snapshot storage order");
        return Ok(output);
    }
    if matches!(format, "ZIP" | "PDF") && options.format_hint.is_none() {
        let pages = if format == "PDF" {
            Some(pdf::read(data, &mut page)?)
        } else {
            None
        };
        let format = if pages.is_some() {
            format
        } else {
            office(data, &options.excluded_names, &mut page)?
        };
        let mut output = page.result(format);
        output["external_resources"] = json!("never accessed");
        output["active_content"] = json!("not executed");
        if let Some(pages) = pages {
            output["pages"] = json!(pages);
        }
        return Ok(output);
    }
    ensure!(format == "UNKNOWN", "{INVALID}");
    let format = match options.format_hint.as_deref() {
        Some(hint) => hint,
        None => detected(source_text(data)?),
    };
    if matches!(format, "EML" | "MBOX") {
        let messages = mail::read(data, format, &mut page)?;
        let mut output = page.result(format);
        output["messages"] = json!(messages);
        return Ok(output);
    }
    let source = source_text(data)?;
    let mut metadata = Map::new();
    match format {
        "JSON" => leaves(&strict_json(source)?, Map::new(), &mut page)?,
        "JSONL" => {
            for (index, line) in text::lines(source).enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                leaves(
                    &strict_json(line)?,
                    Map::from_iter([("line".into(), json!(index + 1))]),
                    &mut page,
                )?;
            }
        }
        "CSV" | "TSV" => {
            let mut rows = Rows::new(source, if format == "CSV" { ',' } else { '\t' });
            let (header, mut previous) =
                rows.next_row()?.ok_or_else(|| anyhow::anyhow!(INVALID))?;
            ensure!(
                !header.is_empty() && header.iter().all(|c| c.chars().count() <= 256),
                "DOCUMENT_COLUMNS：欄名或欄數超限。"
            );
            let mut index = 0;
            while let Some((row, line)) = rows.next_row()? {
                ensure!(row.len() == header.len(), "DOCUMENT_ROWS：欄數不一致。");
                index += 1;
                page.add(
                    json!({"row": index, "line_start": previous + 1, "line_end": line}),
                    &spaced_json(&row)?,
                )?;
                previous = line;
            }
            metadata.insert("columns".into(), json!(header));
            metadata.insert("column_types".into(), json!(vec!["string"; header.len()]));
            metadata.insert("header_record".into(), json!(true));
        }
        "XML" => xml_units(xml(data)?.root_element(), &mut page)?,
        "YAML" => {
            let (value, dates) = yaml::parse(source)?;
            leaves_with_dates(&value, Map::new(), &mut page, &dates)?;
        }
        "INI" => {
            leaves(&ini(source)?, Map::new(), &mut page)?;
            metadata.insert("interpolation".into(), json!(false));
        }
        "TOML" => {
            let value: toml::Value = toml::from_str(source)?;
            let mut dates = HashSet::new();
            let value = toml_json(value, "", 0, &mut 0, &mut dates)?;
            leaves_with_dates(&value, Map::new(), &mut page, &dates)?;
        }
        "SRT" | "VTT" => subtitles(source, format, &mut page)?,
        _ => bail!("{INVALID}"),
    }
    let mut output = page.result(format);
    output.as_object_mut().unwrap().extend(metadata);
    Ok(output)
}
