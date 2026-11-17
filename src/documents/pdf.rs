use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

use anyhow::{Context, Result, bail, ensure};
use lopdf::{
    DecompressError, Dictionary, Document, Encoding, LoadOptions, Object, ObjectId, Stream,
    content::Content, dictionary,
};
use serde_json::json;

use super::Page;

const MAX_STREAM: usize = 8 * 1024 * 1024;
const MAX_PAGES: usize = 2000;
const MAX_TREE_NODES: usize = 4000;
const MAX_FORMS: usize = 1000;
const MAX_DEPTH: usize = 64;
const INVALID: &str = "DOCUMENT_PDF：PDF 結構損毀或使用不支援的內容。";
const LIMIT: &str = "DOCUMENT_STREAM_LIMIT：PDF 展開量、深度或節點數超限。";
static DEFAULT_FONT: LazyLock<Dictionary> = LazyLock::new(|| dictionary! { "Type" => "Font" });

fn optional_dict<'a>(
    dictionary: &'a Dictionary,
    key: &[u8],
    document: &'a Document,
) -> Result<Option<&'a Dictionary>> {
    if !dictionary.has(key) {
        return Ok(None);
    }
    Ok(Some(dictionary.get_deref(key, document)?.as_dict()?))
}

fn checked_stream<'a>(stream: &'a Stream, document: &Document) -> Result<Cow<'a, Stream>> {
    ensure!(!stream.dict.has(b"F"), "{INVALID}");
    let Ok(filter) = stream.dict.get(b"Filter") else {
        return Ok(Cow::Borrowed(stream));
    };
    let (reference, resolved) = document.dereference(filter)?;
    let normalized = match resolved {
        Object::Name(_) if reference.is_none() => return Ok(Cow::Borrowed(stream)),
        Object::Name(_) => resolved.clone(),
        Object::Array(filters) => {
            ensure!(filters.len() <= Stream::MAX_FILTERS, "{LIMIT}");
            if reference.is_none() && filters.iter().all(|item| matches!(item, Object::Name(_))) {
                return Ok(Cow::Borrowed(stream));
            }
            Object::Array(
                filters
                    .iter()
                    .map(|item| {
                        let (_, filter) = document.dereference(item)?;
                        Ok(Object::Name(filter.as_name()?.to_vec()))
                    })
                    .collect::<Result<Vec<_>>>()?,
            )
        }
        _ => bail!("{INVALID}"),
    };
    let mut stream = stream.clone();
    stream.dict.set("Filter", normalized);
    Ok(Cow::Owned(stream))
}

fn stream_data(stream: &Stream, document: &Document, maximum: usize) -> Result<Vec<u8>> {
    let stream = checked_stream(stream, document)?;
    match stream.get_plain_content_with_limit(maximum) {
        Ok(data) => Ok(data),
        Err(lopdf::Error::Decompress(DecompressError::MemoryLimitExceeded { .. })) => {
            bail!("{LIMIT}")
        }
        Err(_) => bail!("{INVALID}"),
    }
}

fn page_data(document: &Document, page: ObjectId) -> Result<Vec<u8>> {
    let page = document.get_dictionary(page)?;
    if !page.has(b"Contents") {
        return Ok(Vec::new());
    }
    let (_, contents) = document.dereference(page.get(b"Contents")?)?;
    let streams = match contents {
        Object::Array(streams) => streams.as_slice(),
        Object::Stream(_) => std::slice::from_ref(contents),
        Object::Null => return Ok(Vec::new()),
        _ => bail!("{INVALID}"),
    };
    let mut data = Vec::new();
    for item in streams {
        let (_, stream) = document.dereference(item)?;
        if matches!(stream, Object::Null) {
            continue;
        }
        let separator = usize::from(!data.is_empty());
        let remaining = MAX_STREAM
            .checked_sub(data.len() + separator)
            .context(LIMIT)?;
        let decoded = stream_data(stream.as_stream()?, document, remaining)?;
        if !decoded.is_empty() {
            if separator != 0 {
                data.push(b'\n');
            }
            data.extend_from_slice(&decoded);
        }
    }
    Ok(data)
}

struct FontEncoding<'a> {
    primary: Encoding<'a>,
    fallback: Option<Encoding<'a>>,
}

impl<'a> FontEncoding<'a> {
    fn new(font: &'a Dictionary, document: &'a Document) -> Result<Self> {
        if !font.has(b"ToUnicode") {
            return Ok(Self {
                primary: font.get_font_encoding_with_limit(document, MAX_STREAM)?,
                fallback: None,
            });
        }
        let cmap_id = font.get(b"ToUnicode")?.as_reference()?;
        let cmap = match checked_stream(document.get_object(cmap_id)?.as_stream()?, document)? {
            Cow::Borrowed(_) => Object::Reference(cmap_id),
            Cow::Owned(stream) => Object::Stream(stream),
        };
        // Ask lopdf for the CMap without the font's display encoding, which
        // otherwise takes precedence in its encoding lookup.
        let cmap_font = dictionary! { "Type" => "Font", "ToUnicode" => cmap };
        let primary = match cmap_font.get_font_encoding_with_limit(document, MAX_STREAM)? {
            Encoding::UnicodeMapEncoding(map) => Encoding::UnicodeMapEncoding(map),
            _ => bail!("{INVALID}"),
        };
        let fallback = if font.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Type0") {
            None
        } else {
            let base = if font.has(b"Encoding") {
                font
            } else {
                &DEFAULT_FONT
            };
            Some(base.get_font_encoding_with_limit(document, MAX_STREAM)?)
        };
        Ok(Self { primary, fallback })
    }

    fn write(&self, text: &mut String, bytes: &[u8]) -> Result<()> {
        if let (Encoding::UnicodeMapEncoding(map), Some(fallback)) = (&self.primary, &self.fallback)
        {
            for byte in bytes {
                if let Some(units) = map.get(u32::from(*byte), 1) {
                    text.extend(
                        char::decode_utf16(units)
                            .map(|ch| ch.unwrap_or(char::REPLACEMENT_CHARACTER)),
                    );
                } else {
                    fallback.write_to_string(std::slice::from_ref(byte), text)?;
                }
                ensure!(text.len() <= MAX_STREAM, "{LIMIT}");
            }
        } else {
            self.primary.write_to_string(bytes, text)?;
        }
        Ok(())
    }
}

fn show_text(text: &mut String, encoding: &FontEncoding<'_>, operand: &Object) -> Result<()> {
    match operand {
        Object::String(bytes, _) => {
            for chunk in bytes.chunks(MAX_STREAM) {
                encoding.write(text, chunk)?;
                ensure!(text.len() <= MAX_STREAM, "{LIMIT}");
            }
        }
        Object::Integer(spacing) if *spacing < -100 => space(text),
        Object::Real(spacing) if *spacing < -100. => space(text),
        Object::Integer(_) | Object::Real(_) => {}
        _ => bail!("{INVALID}"),
    }
    Ok(())
}

fn space(text: &mut String) {
    if text.ends_with(|ch: char| !ch.is_whitespace()) {
        text.push(' ');
    }
}

fn newline(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

struct Extraction<'a> {
    document: &'a Document,
    remaining: usize,
    forms: usize,
    ancestors: HashSet<ObjectId>,
    text: String,
}

impl<'a> Extraction<'a> {
    fn stream(
        &mut self,
        data: &[u8],
        resources: Option<&'a Dictionary>,
        depth: usize,
    ) -> Result<()> {
        ensure!(
            depth <= MAX_DEPTH && data.len() <= self.remaining,
            "{LIMIT}"
        );
        self.remaining -= data.len();
        let document = self.document;
        let fonts = resources
            .map(|dict| optional_dict(dict, b"Font", document))
            .transpose()?
            .flatten();
        let mut encodings = HashMap::new();
        let mut active = None;
        let mut saved_fonts = Vec::new();
        let mut position = None;
        let operations = Content::decode(data)?;
        for operation in operations.operations {
            let operands = &operation.operands;
            match operation.operator.as_str() {
                "q" => {
                    ensure!(saved_fonts.len() < MAX_DEPTH, "{LIMIT}");
                    saved_fonts.push(active.clone());
                }
                "Q" => active = saved_fonts.pop().flatten(),
                "BT" => position = None,
                "Tf" => {
                    let name = operands.first().context(INVALID)?.as_name()?;
                    if !encodings.contains_key(name) {
                        let font = fonts
                            .context(INVALID)?
                            .get_deref(name, document)?
                            .as_dict()?;
                        encodings.insert(name.to_vec(), FontEncoding::new(font, document)?);
                    }
                    active = Some(name.to_vec());
                }
                "T*" | "TD" => newline(&mut self.text),
                "Td" if operands.get(1).context(INVALID)?.as_float()? != 0. => {
                    newline(&mut self.text)
                }
                "Tm" => {
                    let y = operands.get(5).context(INVALID)?.as_float()?;
                    if position.is_some_and(|previous: f32| (previous - y).abs() > 0.5) {
                        newline(&mut self.text);
                    }
                    position = Some(y);
                }
                "Tj" | "TJ" | "'" | "\"" => {
                    let encoding = active
                        .as_ref()
                        .and_then(|name| encodings.get(name))
                        .context(INVALID)?;
                    let operator = operation.operator.as_str();
                    if matches!(operator, "'" | "\"") {
                        newline(&mut self.text);
                    }
                    let operand = operands
                        .get(if operator == "\"" { 2 } else { 0 })
                        .context(INVALID)?;
                    if operator == "TJ" {
                        for part in operand.as_array()? {
                            show_text(&mut self.text, encoding, part)?;
                        }
                    } else {
                        show_text(&mut self.text, encoding, operand)?;
                    }
                }
                "Do" => {
                    let name = operands.first().context(INVALID)?.as_name()?;
                    let xobjects = resources
                        .context(INVALID)?
                        .get_deref(b"XObject", document)?
                        .as_dict()?;
                    let (id, object) = document.dereference(xobjects.get(name)?)?;
                    let form = object.as_stream()?;
                    if form.dict.get(b"Subtype")?.as_name()? == b"Form" {
                        self.forms += 1;
                        let id = id.context(INVALID)?;
                        ensure!(
                            self.forms <= MAX_FORMS && self.ancestors.insert(id),
                            "{LIMIT}"
                        );
                        ensure!(!form.dict.has(b"Ref"), "{INVALID}");
                        let local =
                            optional_dict(&form.dict, b"Resources", document)?.or(resources);
                        let data = stream_data(form, document, self.remaining)?;
                        newline(&mut self.text);
                        self.stream(&data, local, depth + 1)?;
                        newline(&mut self.text);
                        self.ancestors.remove(&id);
                    }
                }
                _ => {}
            }
            ensure!(self.text.len() <= MAX_STREAM, "{LIMIT}");
        }
        Ok(())
    }
}

pub(super) fn read(data: &[u8], page: &mut Page) -> Result<usize> {
    let document = Document::load_mem_with_options(
        data,
        LoadOptions {
            strict: true,
            max_decompressed_size: Some(MAX_STREAM),
            ..LoadOptions::default()
        },
    )
    .context(INVALID)?;
    ensure!(
        !document.is_encrypted() && !document.was_encrypted(),
        "DOCUMENT_ENCRYPTED：不讀取加密 PDF。"
    );
    let root = document.catalog()?.get(b"Pages")?.as_reference()?;
    let mut seen = HashSet::new();
    let mut pages = Vec::new();
    let mut pending = vec![(root, None, None, 0)];
    while let Some((id, parent, inherited, depth)) = pending.pop() {
        ensure!(
            depth <= MAX_DEPTH && seen.insert(id) && seen.len() <= MAX_TREE_NODES,
            "{LIMIT}"
        );
        let node = document.get_dictionary(id)?;
        if let Some(parent) = parent {
            ensure!(node.get(b"Parent")?.as_reference()? == parent, "{INVALID}");
        }
        let resources = optional_dict(node, b"Resources", &document)?.or(inherited);
        match node.get(b"Type")?.as_name()? {
            b"Pages" => {
                let children = node.get_deref(b"Kids", &document)?.as_array()?;
                ensure!(
                    pending.len() + children.len() + seen.len() <= MAX_TREE_NODES,
                    "{LIMIT}"
                );
                for child in children.iter().rev() {
                    pending.push((child.as_reference()?, Some(id), resources, depth + 1));
                }
            }
            b"Page" => {
                ensure!(
                    pages.len() < MAX_PAGES,
                    "DOCUMENT_PAGES_LIMIT：PDF 超過 2000 頁。"
                );
                pages.push((id, resources));
            }
            _ => bail!("{INVALID}"),
        }
    }
    for (index, (id, resources)) in pages.iter().enumerate() {
        let data = page_data(&document, *id)?;
        let mut extraction = Extraction {
            document: &document,
            remaining: MAX_STREAM,
            forms: 0,
            ancestors: HashSet::new(),
            text: String::new(),
        };
        extraction.stream(&data, *resources, 0)?;
        page.add(json!({"page": index + 1}), extraction.text.trim_end())?;
    }
    Ok(pages.len())
}
