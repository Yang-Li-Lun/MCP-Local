use std::{borrow::Cow, cell::RefCell, sync::LazyLock};

use anyhow::{Context, Result, ensure};
use html5ever::{
    TokenizerResult,
    tokenizer::{
        BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, states::RawKind,
    },
};
use mail_parser::{HeaderForm, MessageParser, PartKind};
use regex::{Regex, bytes::Regex as BytesRegex};
use serde_json::{Map, json};
use sha2::{Digest, Sha256};

use super::{MAX_UNITS, Page, spaced_json};
use crate::listing::hex;

const MAX_MESSAGE: usize = 8 * 1024 * 1024;
const MAX_PARTS: usize = 1000;
const MAX_MESSAGES: usize = 1000;
const MAX_MIME_DEPTH: usize = 32;
const INVALID: &str = "DOCUMENT_MAIL：郵件 header、MIME 或編碼無效。";
const LIMIT: &str = "DOCUMENT_MAIL_LIMIT：MIME 深度、郵件項目數或大小超限。";
const CHARSET: &str = "DOCUMENT_MAIL_CHARSET：郵件文字編碼無效或不支援。";

static ENVELOPE: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"(?m-u)^From [^\s]+ [^\r\n]+[0-9]{4}\r?$").unwrap());
static ESCAPED_FROM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^>(>*From )").unwrap());

#[derive(Default)]
struct Html {
    hidden: Vec<html5ever::LocalName>,
    tags: usize,
    limited: bool,
    text: String,
}

struct HtmlSink(RefCell<Html>);

impl TokenSink for HtmlSink {
    type Handle = ();

    fn process_token(&self, token: Token, _: u64) -> TokenSinkResult<Self::Handle> {
        let mut html = self.0.borrow_mut();
        if html.limited {
            return TokenSinkResult::Continue;
        }
        match token {
            Token::TagToken(tag) if tag.kind == TagKind::StartTag => {
                html.tags += 1;
                if html.tags > MAX_UNITS || html.hidden.len() > 64 {
                    html.limited = true;
                    return TokenSinkResult::Continue;
                }
                match tag.name.as_ref() {
                    "head" | "object" => html.hidden.push(tag.name),
                    "style" | "script" | "iframe" => {
                        let kind = if &*tag.name == "script" {
                            RawKind::ScriptData
                        } else {
                            RawKind::Rawtext
                        };
                        html.hidden.push(tag.name);
                        return TokenSinkResult::RawData(kind);
                    }
                    "p" | "br" | "div" | "tr" | "li" if html.hidden.is_empty() => {
                        html.text.push('\n')
                    }
                    _ => {}
                }
            }
            Token::TagToken(tag) if html.hidden.last() == Some(&tag.name) => {
                html.hidden.pop();
            }
            Token::CharacterTokens(text) if html.hidden.is_empty() => html.text.push_str(&text),
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

fn html_text(text: &str) -> Result<String> {
    let input = BufferQueue::default();
    input.push_back(text.into());
    let tokenizer = Tokenizer::new(HtmlSink(RefCell::new(Html::default())), Default::default());
    ensure!(
        matches!(tokenizer.feed(&input), TokenizerResult::Done),
        "{INVALID}"
    );
    tokenizer.end();
    let html = tokenizer.sink.0.into_inner();
    ensure!(!html.limited, "DOCUMENT_HTML_LIMIT：HTML 節點或深度超限。");
    Ok(html.text)
}

fn decode_text<'a>(payload: &'a [u8], label: &str) -> Result<Cow<'a, str>> {
    let text = match label.to_ascii_lowercase().as_str() {
        "ascii" | "us-ascii" => {
            ensure!(payload.is_ascii(), "{CHARSET}");
            Cow::Borrowed(std::str::from_utf8(payload)?)
        }
        "utf-8" => Cow::Borrowed(std::str::from_utf8(payload).context(CHARSET)?),
        "iso-8859-1" => Cow::Owned(payload.iter().copied().map(char::from).collect()),
        encoding
        @ ("windows-1252" | "big5" | "cp950" | "gb18030" | "shift_jis" | "iso-2022-jp") => {
            if encoding == "windows-1252" {
                ensure!(
                    !payload
                        .iter()
                        .any(|byte| matches!(byte, 0x81 | 0x8d | 0x8f | 0x90 | 0x9d)),
                    "{CHARSET}"
                );
            }
            encoding_rs::Encoding::for_label(encoding.as_bytes())
                .context(CHARSET)?
                .decode_without_bom_handling_and_without_replacement(payload)
                .context(CHARSET)?
        }
        _ => anyhow::bail!("{CHARSET}"),
    };
    ensure!(!text.contains('\0'), "{INVALID}");
    Ok(text)
}

fn message(
    parser: &MessageParser,
    data: &[u8],
    number: usize,
    offset: usize,
    mbox: bool,
    page: &mut Page,
) -> Result<()> {
    ensure!(data.len() <= MAX_MESSAGE, "{LIMIT}");
    let message = parser.parse(data).context(INVALID)?;
    let headers = message.headers();
    ensure!(
        ["From", "Subject", "MIME-Version"].iter().any(|name| {
            headers.get(*name).is_some_and(|header| {
                header
                    .raw_value()
                    .iter()
                    .any(|byte| !byte.is_ascii_whitespace())
            })
        }) && !headers.contains("Content-Length"),
        "{INVALID}"
    );
    let mut values = Map::new();
    for name in ["From", "To", "Cc", "Subject", "Date", "Message-ID"] {
        let value = headers
            .all(name)
            .next()
            .map(|header| {
                // Text form decodes encoded words while retaining address punctuation.
                header
                    .parse_as(HeaderForm::Text)
                    .value()
                    .as_text()
                    .unwrap_or_default()
                    .to_owned()
            })
            .unwrap_or_default();
        values.insert(name.into(), json!(value));
    }
    page.add(
        json!({"message":number,"byte_offset":offset,"section":"headers"}),
        &spaced_json(&values)?,
    )?;
    let mut pending = vec![(message.root_part(), "1".to_string(), 0, false)];
    let mut count = 0;
    while let Some((part, path, depth, digest_child)) = pending.pop() {
        count += 1;
        ensure!(
            count <= MAX_PARTS && depth <= MAX_MIME_DEPTH && part.flags().is_empty(),
            "{LIMIT}"
        );
        let content_type = part.content_type();
        let (kind, subtype) = content_type
            .as_ref()
            .and_then(|ct| Some((ct.ctype(), ct.subtype()?)))
            .unwrap_or(if digest_child {
                ("message", "rfc822")
            } else {
                ("text", "plain")
            });
        let declared = format!("{kind}/{subtype}").to_ascii_lowercase();
        let attachment = part
            .content_disposition()
            .is_some_and(|d| d.ctype().eq_ignore_ascii_case("attachment"));
        let multipart = matches!(part.kind(), PartKind::Multipart);
        ensure!(kind != "multipart" || multipart, "{INVALID}");
        if multipart && !attachment {
            let children = part.children().collect::<Vec<_>>();
            ensure!(
                !children.is_empty() && count + pending.len() + children.len() <= MAX_PARTS,
                "{LIMIT}"
            );
            for (index, child) in children.into_iter().enumerate().rev() {
                pending.push((
                    child,
                    format!("{path}.{}", index + 1),
                    depth + 1,
                    declared == "multipart/digest",
                ));
            }
            continue;
        }
        let location =
            json!({"message":number,"byte_offset":offset,"part":path,"content_type":declared});
        if multipart || matches!(part.kind(), PartKind::Message(_)) {
            page.add(
                location,
                &spaced_json(&json!({"attachment":true,"content":"nested message not traversed"}))?,
            )?;
            continue;
        }
        let (payload, problems) = part.decoded_checked();
        ensure!(problems.is_empty(), "{INVALID}");
        let text = if attachment || !matches!(declared.as_str(), "text/plain" | "text/html") {
            Cow::Owned(spaced_json(&json!({
                    "attachment":true,"filename":part.attachment_name(),
                    "size":payload.len(),"sha256":hex(&Sha256::digest(&payload))
            }))?)
        } else {
            let charset = content_type
                .and_then(|ct| ct.attribute("charset"))
                .unwrap_or("ascii");
            let decoded = decode_text(&payload, charset)?;
            let text = if mbox {
                ESCAPED_FROM.replace_all(&decoded, "${1}")
            } else {
                Cow::Borrowed(decoded.as_ref())
            };
            if declared == "text/html" {
                Cow::Owned(html_text(&text)?)
            } else {
                Cow::Owned(text.into_owned())
            }
        };
        page.add(location, &text)?;
    }
    Ok(())
}

pub(super) fn read(data: &[u8], format: &str, page: &mut Page) -> Result<usize> {
    let parser = MessageParser::default()
        .max_parts(MAX_PARTS)
        .max_depth(MAX_MIME_DEPTH + 1)
        .max_encoded_nesting(0);
    if format == "EML" {
        message(&parser, data, 1, 0, false, page)?;
        return Ok(1);
    }
    let mut envelopes = ENVELOPE.find_iter(data).peekable();
    ensure!(
        envelopes.peek().is_some_and(|m| m.start() == 0),
        "DOCUMENT_MBOX：MBOX envelope 無效。"
    );
    let mut count = 0;
    while let Some(envelope) = envelopes.next() {
        count += 1;
        ensure!(
            count <= MAX_MESSAGES && data.get(envelope.end()) == Some(&b'\n'),
            "{LIMIT}"
        );
        let offset = envelope.end() + 1;
        let end = envelopes.peek().map_or(data.len(), |next| next.start());
        message(&parser, &data[offset..end], count, offset, true, page)?;
    }
    Ok(count)
}
