use std::collections::{HashMap, HashSet};

use anyhow::{Result, ensure};
use percent_encoding::percent_decode_str;
use roxmltree::Node;
use serde_json::json;

use super::{INVALID, Page, spaced_json, tag, xml};
use crate::archives::{Zip, safe_name};

pub(super) fn zip_kind(zip: &Zip<'_>, exclusions: &[String]) -> Result<&'static str> {
    if !zip.contains("mimetype") {
        return Ok("ZIP");
    }
    let mime = zip.member("mimetype", exclusions)?;
    ensure!(mime.is_ascii(), "{INVALID}");
    Ok(match mime.as_slice() {
        b"application/vnd.oasis.opendocument.text" if zip.contains("content.xml") => "ODT",
        b"application/vnd.oasis.opendocument.spreadsheet" if zip.contains("content.xml") => "ODS",
        b"application/vnd.oasis.opendocument.presentation" if zip.contains("content.xml") => "ODP",
        b"application/epub+zip" if zip.contains("META-INF/container.xml") => "EPUB",
        _ => "ZIP",
    })
}

fn passive(node: Node<'_, '_>) -> bool {
    !matches!(
        tag(node).to_ascii_lowercase().as_str(),
        "script" | "scripts" | "style" | "head" | "object" | "iframe" | "binary-data"
    )
}

fn visible_elements<'a, 'input>(node: Node<'a, 'input>) -> impl Iterator<Item = Node<'a, 'input>> {
    node.descendants()
        .filter(|child| child.is_element() && child.ancestors().all(passive))
}

fn passive_text(node: Node<'_, '_>) -> String {
    fn append(node: Node<'_, '_>, output: &mut String) {
        if node.is_text() {
            if let Some(text) = node.text() {
                output.push_str(text);
            }
        } else if passive(node) {
            for child in node.children() {
                append(child, output);
            }
        }
    }
    let mut output = String::new();
    append(node, &mut output);
    output
}

fn attribute<'a>(node: Node<'a, '_>, name: &str) -> &'a str {
    node.attributes()
        .find(|attribute| attribute.name() == name)
        .map_or("", |attr| attr.value())
}

fn repeat(node: Node<'_, '_>, name: &str, maximum: usize) -> Result<usize> {
    let amount = node
        .attributes()
        .find(|attr| attr.name() == name)
        .map_or(Ok(1), |attr| attr.value().trim().parse::<usize>())?;
    ensure!(
        (1..=maximum).contains(&amount),
        "DOCUMENT_LIMIT：OpenDocument 重複項目超限。"
    );
    Ok(amount)
}

pub(super) fn odf(
    zip: &Zip<'_>,
    exclusions: &[String],
    format: &str,
    page: &mut Page,
) -> Result<()> {
    if zip.contains("META-INF/manifest.xml") {
        let bytes = zip.member("META-INF/manifest.xml", exclusions)?;
        ensure!(
            !xml(&bytes)?
                .descendants()
                .any(|node| tag(node) == "encryption-data"),
            "DOCUMENT_ENCRYPTED：不讀取加密 OpenDocument。"
        );
    }
    let bytes = zip.member("content.xml", exclusions)?;
    let doc = xml(&bytes)?;
    let root = doc.root_element();
    ensure!(tag(root) == "document-content", "{INVALID}");
    match format {
        "ODT" => {
            for (index, node) in visible_elements(root)
                .filter(|node| matches!(tag(*node), "p" | "h"))
                .enumerate()
            {
                page.add(
                    json!({"member":"content.xml", "paragraph":index + 1}),
                    &passive_text(node),
                )?;
            }
        }
        "ODP" => {
            for (index, node) in visible_elements(root)
                .filter(|node| tag(*node) == "page")
                .enumerate()
            {
                let text = visible_elements(node)
                    .filter(|child| matches!(tag(*child), "p" | "h"))
                    .map(passive_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                page.add(json!({"member":"content.xml", "slide":index + 1}), &text)?;
            }
        }
        "ODS" => {
            for table in visible_elements(root).filter(|child| tag(*child) == "table") {
                let name = attribute(table, "name");
                ensure!(
                    name.chars().count() <= 256,
                    "DOCUMENT_LIMIT：工作表名稱超限。"
                );
                let mut row_index = 0;
                for row in visible_elements(table).filter(|child| tag(*child) == "table-row") {
                    let rows = repeat(row, "number-rows-repeated", 1000)?;
                    let mut items = Vec::new();
                    let mut columns = 0;
                    for cell in row
                        .children()
                        .filter(|node| matches!(tag(*node), "table-cell" | "covered-table-cell"))
                    {
                        let amount = repeat(cell, "number-columns-repeated", 256)?;
                        columns += amount;
                        ensure!(
                            columns <= 256,
                            "DOCUMENT_LIMIT：OpenDocument 重複欄位超限。"
                        );
                        items.push((
                            amount,
                            json!({
                                "value_type":attribute(cell, "value-type"),
                                "value":attribute(cell, "value"),
                                "date_value":attribute(cell, "date-value"),
                                "boolean_value":attribute(cell, "boolean-value"),
                                "formula":attribute(cell, "formula"),
                                "text":passive_text(cell),
                            }),
                        ));
                    }
                    let values: Vec<_> = items
                        .iter()
                        .flat_map(|(amount, item)| std::iter::repeat_n(item, *amount))
                        .collect();
                    let row = spaced_json(&values)?;
                    for _ in 0..rows {
                        row_index += 1;
                        page.add(
                            json!({"member":"content.xml", "sheet":name, "row":row_index}),
                            &row,
                        )?;
                    }
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn epub_path(base: &str, href: &str) -> Result<String> {
    let resource = href.split('#').next().unwrap_or("");
    ensure!(
        !resource.starts_with('/') && !resource.contains(['?', ':']),
        "DOCUMENT_EXTERNAL：EPUB 必要資源不可以是外部網址。"
    );
    let decoded = percent_decode_str(resource).decode_utf8()?;
    ensure!(!decoded.is_empty(), "{INVALID}");
    let name = format!("{base}{decoded}");
    safe_name(&name)?;
    Ok(name)
}

pub(super) fn epub(zip: &Zip<'_>, exclusions: &[String], page: &mut Page) -> Result<()> {
    ensure!(
        !zip.contains("META-INF/encryption.xml"),
        "DOCUMENT_ENCRYPTED：不讀取加密／混淆 EPUB。"
    );
    let bytes = zip.member("META-INF/container.xml", exclusions)?;
    let container = xml(&bytes)?;
    let mut paths = container
        .descendants()
        .filter(|node| tag(*node) == "rootfile");
    let root = paths.next().ok_or_else(|| anyhow::anyhow!("{INVALID}"))?;
    ensure!(
        tag(container.root_element()) == "container" && paths.next().is_none(),
        "{INVALID}"
    );
    let path = epub_path("", attribute(root, "full-path"))?;
    let bytes = zip.member(&path, exclusions)?;
    let package = xml(&bytes)?;
    ensure!(tag(package.root_element()) == "package", "{INVALID}");
    let base = path.rfind('/').map_or("", |index| &path[..=index]);
    let mut items = HashMap::new();
    for item in package.descendants().filter(|node| tag(*node) == "item") {
        let key = attribute(item, "id");
        ensure!(
            !key.is_empty() && items.insert(key, item).is_none(),
            "{INVALID}"
        );
    }
    let mut seen = HashSet::new();
    for (chapter, reference) in package
        .descendants()
        .filter(|node| tag(*node) == "itemref")
        .enumerate()
    {
        let key = attribute(reference, "idref");
        let item = items.get(key).ok_or_else(|| anyhow::anyhow!("{INVALID}"))?;
        ensure!(
            seen.insert(key) && attribute(*item, "media-type") == "application/xhtml+xml",
            "{INVALID}"
        );
        let target = epub_path(base, attribute(*item, "href"))?;
        let bytes = zip.member(&target, exclusions)?;
        let tree = xml(&bytes)?;
        ensure!(tag(tree.root_element()) == "html", "{INVALID}");
        for (index, body) in visible_elements(tree.root_element())
            .filter(|node| tag(*node) == "body")
            .enumerate()
        {
            page.add(
                json!({"member":target, "chapter":chapter + 1, "body":index + 1}),
                &passive_text(body),
            )?;
        }
    }
    ensure!(!seen.is_empty(), "{INVALID}");
    Ok(())
}
