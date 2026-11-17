use std::{collections::HashSet, ops::Range, sync::LazyLock};

use regex::{Regex, RegexBuilder};
use serde_json::Value;
use slint::{ModelRc, SharedString, StyledText, VecModel};

use super::{DetailPage, JsonField};
use crate::monitor::presentation::{preview, shorten};

const PAGE_CHARS: usize = 32 * 1024;
const MAX_CHILDREN: usize = 512;
const MAX_VISIBLE: usize = 4096;
const MAX_QUERY: usize = 200;
const LIGHT: [&str; 4] = ["#005A9E", "#146C38", "#8A3800", "#802080"];
const DARK: [&str; 4] = ["#79C0FF", "#A5D6A7", "#FFBA85", "#E5A6ED"];

static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?P<string>"(?:[^"\\]|\\.)*")|(?P<number>-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?)|\b(?:true|false|null)\b"#).unwrap()
});

#[derive(Default)]
pub(super) struct Reader {
    original: Option<Value>,
    scope: Option<String>,
    expanded: HashSet<String>,
    text: String,
    pages: Vec<usize>,
    tokens: Vec<(Range<usize>, usize)>,
    page: usize,
    query: String,
    hit: Option<Range<usize>>,
    feedback: String,
    rendered: Option<(usize, usize, SharedString, StyledText, StyledText)>,
}

impl Reader {
    pub(super) fn set_json(&mut self, value: Option<&Value>) {
        if self.original.as_ref() == value && !self.pages.is_empty() {
            return;
        }
        self.original = value.cloned();
        self.scope = None;
        self.expanded.clear();
        self.expanded.insert(String::new());
        let text = value
            .and_then(|value| serde_json::to_string_pretty(value).ok())
            .unwrap_or_default();
        self.reset_text(text, value.is_some());
    }

    pub(super) fn set_text(&mut self, text: String) {
        if self.text != text || self.pages.is_empty() {
            self.reset_text(text, false);
        }
    }

    fn reset_text(&mut self, text: String, json: bool) {
        self.tokens.clear();
        if json {
            self.tokens.extend(TOKENS.captures_iter(&text).map(|token| {
                let matched = token.get(0).unwrap();
                let kind = if token.name("string").is_some() {
                    usize::from(!text[matched.end()..].trim_start().starts_with(':'))
                } else if token.name("number").is_some() {
                    2
                } else {
                    3
                };
                (matched.range(), kind)
            }));
        }
        self.pages = vec![0];
        self.pages.extend(
            text.char_indices()
                .skip(PAGE_CHARS)
                .step_by(PAGE_CHARS)
                .map(|(index, _)| index),
        );
        self.text = text;
        self.page = 0;
        self.hit = None;
        self.feedback.clear();
        self.rendered = None;
    }

    pub(super) fn turn(&mut self, step: i32) {
        let next = self
            .page
            .saturating_add_signed(step as isize)
            .min(self.pages.len().saturating_sub(1));
        if self.page != next {
            self.page = next;
            self.hit = None;
            self.feedback.clear();
        }
    }

    pub(super) fn find(&mut self, term: &str, forward: bool) {
        let term = term.trim_end_matches('\0');
        if term != self.query {
            self.query = term.chars().take(MAX_QUERY).collect();
            self.hit = None;
        }
        if self.query.is_empty() {
            self.feedback.clear();
            return;
        }
        let literal = RegexBuilder::new(&regex::escape(&self.query))
            .case_insensitive(true)
            .build();
        self.hit = literal.ok().and_then(|literal| {
            if forward {
                let from = self.hit.as_ref().map_or(0, |hit| hit.end);
                literal
                    .find_at(&self.text, from)
                    .or_else(|| literal.find(&self.text))
            } else {
                let from = self.hit.as_ref().map_or(self.text.len(), |hit| hit.start);
                literal
                    .find_iter(&self.text[..from])
                    .last()
                    .or_else(|| literal.find_iter(&self.text).last())
            }
            .map(|found| found.range())
        });
        if let Some(hit) = &self.hit {
            self.page = self
                .pages
                .partition_point(|offset| *offset <= hit.start)
                .saturating_sub(1);
            self.feedback = format!("字元 {}", self.text[..hit.start].chars().count() + 1);
        } else {
            self.feedback = "找不到".into();
        }
    }

    pub(super) fn show(&mut self, notice: String, missing: bool) -> DetailPage {
        let start = self.pages.get(self.page).copied().unwrap_or(0);
        let end = self
            .pages
            .get(self.page + 1)
            .copied()
            .unwrap_or(self.text.len());
        let hit = self
            .hit
            .as_ref()
            .filter(|hit| (start..end).contains(&hit.start));
        let end = hit.map_or(end, |hit| end.max(hit.end));
        if self
            .rendered
            .as_ref()
            .is_none_or(|(a, b, ..)| (*a, *b) != (start, end))
        {
            let plain = self.text[start..end].into();
            let light = styled(&self.text, &self.tokens, start..end, &LIGHT);
            let dark = styled(&self.text, &self.tokens, start..end, &DARK);
            self.rendered = Some((start, end, plain, light, dark));
        }
        let (_, _, plain, light, dark) = self.rendered.as_ref().unwrap();
        DetailPage {
            plain: plain.clone(),
            light: light.clone(),
            dark: dark.clone(),
            page: self.page as i32,
            pages: self.pages.len() as i32,
            notice: notice.into(),
            missing,
            hit_start: hit.map_or(-1, |hit| (hit.start - start) as i32),
            hit_end: hit.map_or(-1, |hit| (hit.end - start) as i32),
            feedback: self.feedback.as_str().into(),
            scoped: self.scope.is_some(),
        }
    }

    pub(super) fn source(&self) -> &str {
        &self.text
    }

    pub(super) fn change_field(&mut self, pointer: &str, action: i32) {
        let Some(original) = &self.original else {
            return;
        };
        let Some(value) = original.pointer(pointer) else {
            return;
        };
        match action {
            0 if value.is_array() || value.is_object() => {
                if !self.expanded.remove(pointer) && self.expanded.len() < MAX_VISIBLE {
                    self.expanded.insert(pointer.into());
                }
            }
            1 => {
                self.scope = (!pointer.is_empty()).then(|| pointer.to_owned());
                self.reset_text(
                    serde_json::to_string_pretty(value).unwrap_or_default(),
                    true,
                );
            }
            2 => {
                self.expanded.clear();
            }
            _ => {}
        }
    }

    pub(super) fn fields(&self) -> ModelRc<JsonField> {
        let mut rows = Vec::new();
        if let Some(value) = &self.original {
            self.children(&mut rows, "$", "", value, 0);
        }
        ModelRc::new(VecModel::from(rows))
    }

    fn children(
        &self,
        rows: &mut Vec<JsonField>,
        name: &str,
        pointer: &str,
        value: &Value,
        depth: i32,
    ) {
        if rows.len() >= MAX_VISIBLE {
            return;
        }
        let child_count = match value {
            Value::Object(fields) => fields.len(),
            Value::Array(items) => items.len(),
            _ => 0,
        };
        let expand = self.expanded.contains(pointer);
        rows.push(JsonField {
            name: shorten(name, 160).into(),
            pointer: pointer.into(),
            preview: preview(value, 100).into(),
            depth,
            expandable: child_count > 0,
            expanded: expand,
            notice: false,
        });
        if expand {
            match value {
                Value::Object(fields) => {
                    for (key, value) in fields.iter().take(MAX_CHILDREN) {
                        if rows.len() >= MAX_VISIBLE {
                            break;
                        }
                        let child =
                            format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                        self.children(rows, key, &child, value, depth + 1);
                    }
                }
                Value::Array(values) => {
                    for (index, value) in values.iter().take(MAX_CHILDREN).enumerate() {
                        if rows.len() >= MAX_VISIBLE {
                            break;
                        }
                        self.children(
                            rows,
                            &format!("[{index}]"),
                            &format!("{pointer}/{index}"),
                            value,
                            depth + 1,
                        );
                    }
                }
                _ => {}
            }
            if child_count > MAX_CHILDREN && rows.len() < MAX_VISIBLE {
                rows.push(JsonField {
                    name: format!(
                        "另有 {} 個欄位或項目，請查看 JSON",
                        child_count - MAX_CHILDREN
                    )
                    .into(),
                    depth: depth + 1,
                    notice: true,
                    ..Default::default()
                });
            }
        }
    }
}

fn styled(
    source: &str,
    tokens: &[(Range<usize>, usize)],
    page: Range<usize>,
    colors: &[&str; 4],
) -> StyledText {
    if tokens.is_empty() {
        return StyledText::from_plain_text(&source[page]);
    }
    let mut text = String::new();
    let mut from = page.start;
    let first = tokens.partition_point(|(token, _)| token.end <= page.start);
    for (token, kind) in &tokens[first..] {
        if token.start >= page.end {
            break;
        }
        let start = token.start.max(page.start);
        let end = token.end.min(page.end);
        escape_markdown(&mut text, &source[from..start]);
        text.push_str("<font color=\"");
        text.push_str(colors[*kind]);
        text.push_str("\">");
        escape_markdown(&mut text, &source[start..end]);
        text.push_str("</font>");
        from = end;
    }
    escape_markdown(&mut text, &source[from..page.end]);
    StyledText::from_markdown(&text).unwrap_or_else(|_| StyledText::from_plain_text(&source[page]))
}

fn escape_markdown(output: &mut String, text: &str) {
    use std::fmt::Write;
    for ch in text.chars() {
        match ch {
            '\n' => output.push_str("\\\n"),
            ch if ch.is_ascii_punctuation() || ch.is_ascii_whitespace() => {
                let _ = write!(output, "&#{};", u32::from(ch));
            }
            ch => output.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_unicode_across_pages_and_reads_escaped_field() {
        let mut reader = Reader::default();
        let value =
            json!({"a/b~c": "甲".repeat(PAGE_CHARS + 7) + "中文😀 needle", "next":"NEEDLE"});
        reader.set_json(Some(&value));
        reader.find("needle", true);
        let page = reader.show(String::new(), false);
        assert!(page.pages > 1 && page.page > 0);
        assert_eq!(
            &page.plain.as_str()[page.hit_start as usize..page.hit_end as usize],
            "needle"
        );
        reader.find("needle", true);
        assert!(reader.feedback.starts_with("字元"));
        reader.change_field("/a~1b~0c", 1);
        assert!(reader.scope.is_some() && reader.source().starts_with("\"甲"));
        reader.change_field("", 1);
        assert!(reader.scope.is_none() && reader.source().contains("\"next\""));
    }
}
