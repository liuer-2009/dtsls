use std::collections::HashMap;

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, InsertTextFormat, Range, TextEdit,
};
use tree_sitter::Node;

use crate::document::Document;
use crate::unit::DefineMap;
use crate::model;

/// What kind of completion the cursor is in.
#[derive(Debug, PartialEq, Eq)]
pub enum Context {
    /// cursor directly follows `&` (plus a typed prefix)
    Labels(String),
    /// bare ALL_CAPS word (macro convention), or `<` just typed
    Defines(String),
    /// lowercase word at a property-name position inside a node body
    Properties(String),
    /// inside `compatible = "…"`: vendor stage (no comma yet) or device
    /// stage (after `vendor,`)
    Compatible {
        prefix: String,
        vendor: Option<String>,
    },
}

/// Classify the completion context at the cursor. Lowercase words only offer
/// property names, and only when the line so far cannot be a value.
pub fn completion_context(doc: &Document, cursor: usize) -> Option<Context> {
    if let Some(context) = compatible_context(doc, cursor) {
        return Some(context);
    }
    let cursor = cursor.min(doc.text.len());
    let line_start = model::line_start_of(&doc.text, cursor);
    let mut start = cursor;
    while start > line_start {
        let b = doc.text.as_bytes()[start - 1];
        // `#` is a legal first character of dt property names (`#clock-cells`)
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'#' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == cursor {
        // no word typed: right after `&` offer labels, right after `<` macros
        if cursor > 0 {
            match doc.text.as_bytes()[cursor - 1] {
                b'&' => return Some(Context::Labels(String::new())),
                b'<' => return Some(Context::Defines(String::new())),
                _ => {}
            }
        }
        return None;
    }
    let word = doc.slice(start..cursor).to_string();
    if start > 0 && doc.text.as_bytes()[start - 1] == b'&' {
        return Some(Context::Labels(word));
    }
    let macro_case = word.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if macro_case {
        return Some(Context::Defines(word));
    }
    // property-name position: everything before the word on this line is
    // whitespace (a value would contain `=` / `<` / `,` / `&` / `"`)
    let head = doc.slice(line_start..start);
    if head.chars().all(|c| c == ' ' || c == '\t') && !word.is_empty() {
        return Some(Context::Properties(word));
    }
    None
}

/// Inside a `compatible = "…"` string: vendor stage (no comma yet) or device
/// stage (after `vendor,`). Works on the raw line so it also fires while the
/// string is still unterminated (everything is mid-typing).
fn compatible_context(doc: &Document, cursor: usize) -> Option<Context> {
    let cursor = cursor.min(doc.text.len());
    let line_start = model::line_start_of(&doc.text, cursor);
    let head = doc.slice(line_start..cursor);
    // an odd number of quotes means the cursor sits inside a string
    if head.matches('"').count() % 2 == 0 {
        return None;
    }
    let quote = head.rfind('"')?;
    let before = &head[..quote];
    // the string must be the value of a `compatible` property
    let equals = before.rfind('=')?;
    let property = before[..equals].trim().split_whitespace().next_back()?;
    if property != "compatible" {
        return None;
    }
    // endpoint/port nodes are plumbing and never carry compatible
    if in_endpoint_or_port(doc, cursor) {
        return None;
    }
    let content = &head[quote + 1..];
    Some(match content.split_once(',') {
        Some((vendor, prefix)) => Context::Compatible {
            prefix: prefix.to_string(),
            vendor: Some(vendor.to_string()),
        },
        None => Context::Compatible {
            prefix: content.to_string(),
            vendor: None,
        },
    })
}

/// Enclosing `subnode` or root `devicetree` of the cursor position.
pub fn enclosing_node(doc: &Document, byte: usize) -> Option<Node<'_>> {
    // a zero-width lookup at a line end or inside whitespace falls back to the
    // root, so probe the byte before the cursor
    let node = doc
        .tree
        .root_node()
        .descendant_for_byte_range(byte.saturating_sub(1), byte)?;
    let mut current = Some(node);
    while let Some(node) = current {
        match node.kind() {
            "subnode" | "devicetree" => return Some(node),
            _ => {}
        }
        current = node.parent();
    }
    None
}

/// `endpoint` / `port` nodes (and their `@addr` variants).
pub fn is_endpoint_or_port(doc: &Document, node: Node<'_>) -> bool {
    let Some(name) = node.child_by_field_name("name") else {
        return false;
    };
    let name = model::node_text(doc, name);
    let base = name.split('@').next().unwrap_or(name);
    base == "endpoint" || base == "port"
}

/// Whether the node enclosing the cursor is `endpoint`/`port` plumbing.
/// Falls back to the raw text when syntax errors collapse the tree into a
/// single ERROR node — the normal state while a node is half-written.
pub fn in_endpoint_or_port(doc: &Document, byte: usize) -> bool {
    if let Some(node) = enclosing_node(doc, byte) {
        if node.kind() == "subnode" {
            return is_endpoint_or_port(doc, node);
        }
    }
    let Some(header) = text_header(doc, byte) else {
        return false;
    };
    let base = header.name.split('@').next().unwrap_or(&header.name);
    base == "endpoint" || base == "port"
}

/// A node read straight from the text. Needed because tree-sitter's error
/// recovery only inserts `MISSING` nodes while the surrounding skeleton still
/// matches the grammar; a half-written node (no `;`, no closing brace) makes
/// the whole region a single ERROR node with no `subnode`/`node_def` inside.
pub struct TextNode {
    pub name: String,
    pub compatibles: Vec<String>,
    pub properties: Vec<String>,
}

impl TextNode {
    /// `endpoint`/`port` plumbing never carries `compatible`.
    pub fn is_endpoint_or_port(&self) -> bool {
        let base = self.name.split('@').next().unwrap_or(&self.name);
        base == "endpoint" || base == "port"
    }
}

struct TextHeader {
    name: String,
    content_start: usize,
    content_end: usize,
}

/// `compatible` strings and property names of the node around `byte`, from
/// the raw text.
pub fn text_node(doc: &Document, byte: usize) -> Option<TextNode> {
    let header = text_header(doc, byte)?;
    let (properties, compatibles) =
        block_properties(&doc.text, header.content_start, header.content_end);
    Some(TextNode {
        name: header.name,
        compatibles,
        properties,
    })
}

fn text_header(doc: &Document, byte: usize) -> Option<TextHeader> {
    let text = &doc.text;
    let cursor = byte.min(text.len());
    let (open, block_end) = enclosing_brace(text, cursor)?;
    let mut name = header_name(text, open);
    let mut content_start = open + 1;
    let mut content_end = block_end;
    let open_indent = line_indent_at(text, open);
    if let Some((braceless, indent, line_end)) = braceless_header(text, open, cursor, open_indent) {
        name = braceless;
        content_start = line_end;
        content_end = indent_bound(text, line_end, indent, block_end);
    }
    Some(TextHeader {
        name,
        content_start,
        content_end,
    })
}

/// Innermost brace still open at `cursor`, plus the byte where its block ends
/// (the closing brace, or EOF when it is missing).
fn enclosing_brace(text: &str, cursor: usize) -> Option<(usize, usize)> {
    let mut stack: Vec<usize> = Vec::new();
    let mut open: Option<(usize, usize)> = None; // (brace byte, stack depth)
    for (offset, byte) in model::brace_offsets(text, 0) {
        if open.is_none() && offset >= cursor {
            let brace = *stack.last()?;
            open = Some((brace, stack.len()));
        }
        if byte == b'{' {
            stack.push(offset);
        } else if let Some((_, depth)) = open {
            stack.pop();
            if stack.len() < depth {
                return Some((open.unwrap().0, offset));
            }
        } else {
            stack.pop();
        }
    }
    let (brace, _) = open.or_else(|| stack.last().map(|&b| (b, stack.len())))?;
    Some((brace, text.len()))
}

fn header_name(text: &str, brace: usize) -> String {
    let line_start = text[..brace].rfind('\n').map_or(0, |pos| pos + 1);
    let before = text[line_start..brace].trim();
    let raw = if before.is_empty() {
        // `{` on its own line: the header is the previous non-empty line
        text[..line_start]
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
    } else {
        before
    };
    node_name_from_header(raw)
}

fn node_name_from_header(header: &str) -> String {
    // drop labels (`label: node@addr`) and the trailing markers
    header.rsplit(':').next().unwrap_or(header).trim().to_string()
}

fn line_indent_at(text: &str, byte: usize) -> usize {
    let line_start = text[..byte.min(text.len())]
        .rfind('\n')
        .map_or(0, |pos| pos + 1);
    model::indent_of(&text[line_start..])
}

/// A node header written without its `{` yet, deeper than the enclosing block.
fn braceless_header(
    text: &str,
    open: usize,
    cursor: usize,
    open_indent: usize,
) -> Option<(String, usize, usize)> {
    let mut pos = text[open..].find('\n').map(|offset| open + offset + 1)?;
    // the line being typed is a partial property, never a header
    let cursor_line_start = text[..cursor.min(text.len())]
        .rfind('\n')
        .map_or(0, |offset| offset + 1);
    let mut found = None;
    while pos < cursor_line_start {
        let line_end = text[pos..]
            .find('\n')
            .map_or(text.len(), |offset| pos + offset);
        let line = &text[pos..line_end];
        let trimmed = line.trim();
        let looks_like_header = !trimmed.is_empty()
            && !trimmed.contains('=')
            && !trimmed.contains(';')
            && !trimmed.contains('{')
            && !trimmed.contains('}')
            && !trimmed.starts_with('#')
            && trimmed
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '&' || c == '_' || c == '/')
            && model::indent_of(line) > open_indent;
        if looks_like_header {
            found = Some((node_name_from_header(trimmed), model::indent_of(line), line_end));
        }
        if line_end >= text.len() {
            break;
        }
        pos = line_end + 1;
    }
    found
}

/// End of a brace-less node: the first following line indented at or above
/// its header.
fn indent_bound(text: &str, start: usize, indent: usize, block_end: usize) -> usize {
    let mut pos = start;
    while pos < block_end {
        let line_end = text[pos..]
            .find('\n')
            .map_or(text.len(), |offset| pos + offset);
        let line = &text[pos..line_end];
        if !line.trim().is_empty() && model::indent_of(line) <= indent {
            return pos;
        }
        pos = line_end + 1;
    }
    block_end
}

/// Top-level properties of a block: names in order, plus the `compatible`
/// strings. Statements may lack their terminating `;` (mid-typing).
fn block_properties(text: &str, start: usize, end: usize) -> (Vec<String>, Vec<String>) {
    let bytes = text.as_bytes();
    let end = end.min(bytes.len());
    let mut properties = Vec::new();
    let mut compatibles = Vec::new();
    let mut depth = 0i32;
    let mut in_comment = false;
    let mut in_string = false;
    let mut i = start;
    let mut chunk_start = start;
    while i < end {
        let b = bytes[i];
        if in_comment {
            if b == b'*' && i + 1 < end && bytes[i + 1] == b'/' {
                in_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_string {
            match b {
                b'\\' => {
                    i += 2;
                    continue;
                }
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        match b {
            b'/' if i + 1 < end && bytes[i + 1] == b'*' => {
                in_comment = true;
                i += 2;
            }
            b'/' if i + 1 < end && bytes[i + 1] == b'/' => {
                while i < end && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                in_string = true;
                i += 1;
            }
            b'{' => {
                if depth == 0 {
                    // nested node starts here: the chunk before it is either a
                    // property missing its `;` or the child node's header
                    take_chunk(
                        text,
                        chunk_start,
                        i,
                        &mut properties,
                        &mut compatibles,
                    );
                    if let Some(name) = child_node_name(&text[chunk_start..i]) {
                        properties.push(name);
                    }
                    chunk_start = i + 1;
                }
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    chunk_start = i;
                }
            }
            b';' if depth == 0 => {
                take_chunk(text, chunk_start, i, &mut properties, &mut compatibles);
                chunk_start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    take_chunk(text, chunk_start, end, &mut properties, &mut compatibles);
    (properties, compatibles)
}

/// One statement of a block: registers its property name, and the
/// `compatible` strings when it is the compatible property.
fn take_chunk(
    text: &str,
    from: usize,
    to: usize,
    properties: &mut Vec<String>,
    compatibles: &mut Vec<String>,
) {
    let chunk = &text[from..to];
    let Some(equals) = chunk.find('=') else {
        return;
    };
    let name = chunk[..equals].trim();
    let name = name.split_whitespace().next_back().unwrap_or("");
    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '#' || c == '_' || c == '-')
    {
        return;
    }
    properties.push(name.to_string());
    if name == "compatible" {
        collect_strings(&chunk[equals + 1..], compatibles);
    }
}

/// Node name of a `{`-header chunk (`label: ports@0` -> `ports`).
fn child_node_name(chunk: &str) -> Option<String> {
    let header = chunk.trim();
    let header = header.rsplit(':').next().unwrap_or(header).trim();
    let token = header.split_whitespace().next_back()?;
    let name = token.split('@').next().unwrap_or(token);
    let plausible = !name.is_empty()
        && !name.starts_with('&')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | ','));
    plausible.then(|| name.to_string())
}

fn collect_strings(value: &str, out: &mut Vec<String>) {
    let mut rest = value;
    while let Some(start) = rest.find('"') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('"') else {
            return;
        };
        if end > 0 {
            out.push(rest[..end].to_string());
        }
        rest = &rest[end + 1..];
    }
}

/// Labels defined in the live buffer, for the window before the first unit run.
fn walk_tree(node: Node, f: &mut impl FnMut(Node)) {
    f(node);
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            walk_tree(child, f);
        }
    }
}

/// Where a label is defined in this buffer (node span of its owner).
pub fn label_definition(doc: &Document, label: &str) -> Option<Range> {
    let mut found = None;
    walk_tree(doc.tree.root_node(), &mut |node| {
        if found.is_some() || node.kind() != "label" {
            return;
        }
        let text = model::node_text(doc, node);
        if text.trim_end_matches(':') != label {
            return;
        }
        let owner = match node.parent() {
            Some(parent) if parent.kind() == "property" => {
                model::owning_node(parent).unwrap_or(parent)
            }
            Some(parent) => parent,
            None => node,
        };
        found = Some(Range {
            start: model::byte_to_position(doc, owner.start_byte()),
            end: model::byte_to_position(doc, owner.end_byte()),
        });
    });
    found
}

/// `&label` uses in this buffer.
pub fn label_references(doc: &Document, label: &str) -> Vec<Range> {
    let mut out = Vec::new();
    walk_tree(doc.tree.root_node(), &mut |node| {
        if node.kind() != "reference" {
            return;
        }
        let Some(label_ref) = model::find_child(node, "label_ref") else {
            return;
        };
        let text = model::node_text(doc, label_ref);
        if text.trim_start_matches('&') != label {
            return;
        }
        out.push(Range {
            start: model::byte_to_position(doc, node.start_byte()),
            end: model::byte_to_position(doc, node.end_byte()),
        });
    });
    out
}

/// Macro completion from the live buffer (fallback before a unit is checked).
pub fn same_file_define_items(doc: &Document, prefix: &str) -> Vec<CompletionItem> {
    let mut defines: DefineMap = HashMap::new();
    walk_tree(doc.tree.root_node(), &mut |node| match node.kind() {
        "preproc_def" => {
            if let Some(name) = node.child_by_field_name("name") {
                let name = model::node_text(doc, name).to_string();
                let value = node
                    .child_by_field_name("value")
                    .map(|value| summarize_value(model::node_text(doc, value)))
                    .unwrap_or_default();
                defines.insert(name, (value, String::new()));
            }
        }
        "preproc_undef" => {
            if let Some(name) = node.child_by_field_name("name") {
                defines.remove(model::node_text(doc, name));
            }
        }
        _ => {}
    });
    items_from_defines(&defines, prefix)
}

fn summarize_value(value: &str) -> String {
    let collapsed: String = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(48).collect();
    if collapsed.chars().count() > 48 {
        out.push('…');
    }
    out
}

/// Property-name item that expands to `compatible = ""` with the cursor
/// between the quotes.
pub fn compatible_property_item(prefix: &str) -> Option<CompletionItem> {
    if !prefix.is_empty() && !"compatible".starts_with(prefix) {
        return None;
    }
    Some(CompletionItem {
        label: "compatible".to_string(),
        kind: Some(CompletionItemKind::PROPERTY),
        detail: Some("add a compatible string".to_string()),
        insert_text: Some("compatible = \"$0\"".to_string()),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..Default::default()
    })
}

/// Vendor items; accepting one inserts `vendor,` so the device list follows.
pub fn vendor_completion_items(
    vendors: Vec<(String, Option<String>, usize)>,
    prefix: &str,
    range: &Range,
) -> Vec<CompletionItem> {
    vendors
        .into_iter()
        .filter(|(vendor, _, _)| vendor.starts_with(prefix))
        .map(|(vendor, name, count)| CompletionItem {
            label: vendor.clone(),
            kind: Some(CompletionItemKind::MODULE),
            detail: Some(match name {
                Some(name) => format!("{name} · {count} devices"),
                None => format!("vendor · {count} devices"),
            }),
            filter_text: Some(vendor.clone()),
            text_edit: Some(
                TextEdit {
                    range: range.clone(),
                    new_text: format!("{vendor},"),
                }
                .into(),
            ),
            ..Default::default()
        })
        .collect()
}

/// Compatibles that carry no vendor prefix (`operating-points-v2`, ...).
pub fn vendor_less_items(
    compatibles: Vec<String>,
    prefix: &str,
    range: &Range,
) -> Vec<CompletionItem> {
    compatibles
        .into_iter()
        .filter(|compatible| compatible.starts_with(prefix))
        .map(|compatible| CompletionItem {
            label: compatible.clone(),
            kind: Some(CompletionItemKind::CONSTANT),
            detail: Some("compatible without vendor".to_string()),
            filter_text: Some(compatible.clone()),
            text_edit: Some(
                TextEdit {
                    range: range.clone(),
                    new_text: compatible,
                }
                .into(),
            ),
            ..Default::default()
        })
        .collect()
}

/// Device items for a resolved vendor; labels are the full compatible strings.
pub fn device_completion_items(
    devices: Vec<(String, String)>,
    prefix: &str,
    range: &Range,
) -> Vec<CompletionItem> {
    devices
        .into_iter()
        .filter(|(compatible, _)| {
            compatible
                .split_once(',')
                .map(|(_, device)| device.starts_with(prefix))
                .unwrap_or(false)
        })
        .map(|(compatible, file)| CompletionItem {
            label: compatible.clone(),
            kind: Some(CompletionItemKind::CONSTANT),
            detail: Some(file),
            filter_text: Some(compatible.clone()),
            text_edit: Some(
                TextEdit {
                    range: range.clone(),
                    new_text: compatible,
                }
                .into(),
            ),
            ..Default::default()
        })
        .collect()
}

/// Property-name items from a node's binding. Required properties sort first;
/// properties the node already has are dropped.
pub fn items_from_binding(
    binding: &crate::bindings::Binding,
    existing: &std::collections::HashSet<String>,
    prefix: &str,
) -> Vec<CompletionItem> {
    let file = binding
        .uri
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or("")
        .to_string();
    // one item per property: required entries also live in `properties`, and
    // anything the node already has (property or child node) is dropped
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut items: Vec<CompletionItem> = binding
        .properties
        .iter()
        .chain(binding.required.iter())
        .filter(|name| !existing.contains(*name) && name.starts_with(prefix))
        .filter(|name| seen.insert(name.as_str()))
        .map(|name| {
            let required = binding.required.contains(name);
            CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some(if required {
                    format!("{file} (required)")
                } else {
                    file.clone()
                }),
                sort_text: Some(if required {
                    format!("0{name}")
                } else {
                    format!("1{name}")
                }),
                ..Default::default()
            }
        })
        .collect();
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    items
}

/// Items straight from the background index: no tree walk, no IO. This is the
/// steady-state path and must stay well under a millisecond.
/// One `&label` completion item (path, plus the defining file when foreign).
pub fn label_item(label: &str, path: &str, file: &str) -> CompletionItem {
    let detail = detail(path, file);
    item(label, &detail, CompletionItemKind::VARIABLE)
}

pub fn items_from_defines(defines: &DefineMap, prefix: &str) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = defines
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(name, (value, file))| {
            let detail = detail(value.as_str(), file.as_str());
            item(name, &detail, CompletionItemKind::CONSTANT)
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Fallback for the first completion right after opening a document, before
/// the background index has landed: walk the current tree synchronously.
pub fn same_file_items(doc: &Document) -> Vec<CompletionItem> {
    let analysis = model::analyze(doc);
    let mut items: Vec<CompletionItem> = analysis
        .labels
        .iter()
        .map(|(label, path)| item(label, path, CompletionItemKind::VARIABLE))
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Where is the cursor relative to a label? Reference context (`&label`)
/// or definition context (`label:`) both resolve to the label name.
pub fn label_at_cursor(doc: &Document, cursor: usize) -> Option<String> {
    let cursor = cursor.min(doc.text.len());
    let line_start = model::line_start_of(&doc.text, cursor);
    let mut start = cursor;
    while start > line_start {
        let b = doc.text.as_bytes()[start - 1];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            start -= 1;
        } else {
            break;
        }
    }
    let mut end = cursor;
    while end < doc.text.len() {
        let b = doc.text.as_bytes()[end];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            end += 1;
        } else {
            break;
        }
    }
    if start == end {
        return None;
    }
    let word = doc.slice(start..end).to_string();
    if start > 0 && doc.text.as_bytes()[start - 1] == b'&' {
        return Some(word);
    }
    if doc.slice(end..(end + 1).min(doc.text.len())) == ":" {
        return Some(word);
    }
    None
}

fn detail(path: &str, file: &str) -> String {
    if file.is_empty() {
        path.to_string()
    } else {
        format!("{path} — {file}")
    }
}

fn item(label: &str, detail: &str, kind: CompletionItemKind) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        detail: Some(detail.to_string()),
        ..Default::default()
    }
}

pub fn completion_list(items: Vec<CompletionItem>) -> CompletionList {
    CompletionList {
        // lists are never truncated, so the client should not re-request
        is_incomplete: false,
        items,
    }
}

/// What the node around `byte` looks like for completion: its binding, the
/// property names it already has, and whether it can still take a
/// `compatible`. The syntax tree is preferred; a half-written node makes
/// tree-sitter collapse the region into a single ERROR node, so the raw text
/// is used as a fallback.
struct NodeView {
    binding: Option<std::sync::Arc<crate::bindings::Binding>>,
    existing: std::collections::HashSet<String>,
    can_add_compatible: bool,
}

fn node_view(doc: &Document, byte: usize, bindings: &crate::bindings::BindingIndex) -> NodeView {
    let mut existing = std::collections::HashSet::new();
    let mut compatibles = None;
    let mut endpoint = None;
    let mut in_node = false;
    // A node starting on the cursor's own line is not the one being typed
    // inside: error recovery glues the partial line onto the *following*
    // node, so its compatible would win. Only nodes whose header is above the
    // cursor are trusted; the text reader handles the rest.
    let line_start = doc.text[..byte.min(doc.text.len())]
        .rfind('\n')
        .map_or(0, |pos| pos + 1);
    if let Some(node) = enclosing_node(doc, byte)
        && node.start_byte() < line_start
    {
        in_node = true;
        compatibles = crate::diagnostics::compatible_strings(doc, node);
        endpoint = Some(is_endpoint_or_port(doc, node));
        if let Some(body) = model::node_def_of(node) {
            for i in 0..body.child_count() {
                let Some(child) = body.child(i as u32) else { continue };
                if child.kind() == "property"
                    && let Some(name) = child.child_by_field_name("name")
                {
                    existing.insert(model::node_text(doc, name).to_string());
                } else if child.kind() == "subnode"
                    && let Some(name) = child.child_by_field_name("name")
                {
                    // child nodes (`ports`, `mdio`, ...) are declared under
                    // `properties:` too, so they count as already present
                    let name = model::node_text(doc, name);
                    existing.insert(name.split('@').next().unwrap_or(name).to_string());
                }
            }
        }
    }
    if compatibles.is_none() {
        // incomplete node: read compatible/properties from the text
        if let Some(text_node) = text_node(doc, byte) {
            in_node = true;
            endpoint.get_or_insert_with(|| text_node.is_endpoint_or_port());
            existing.extend(text_node.properties);
            if !text_node.compatibles.is_empty() {
                compatibles = Some(text_node.compatibles);
            }
        }
    }
    let endpoint = endpoint.unwrap_or_else(|| in_endpoint_or_port(doc, byte));
    let binding = compatibles.as_ref().and_then(|list| bindings.lookup(list));
    // the `compatible` snippet only makes sense inside a node — top-level
    // directives (`#include`, `/dts-v1/`) have no node context
    let can_add_compatible =
        in_node && !endpoint && compatibles.is_none() && !existing.contains("compatible");
    NodeView {
        binding,
        existing,
        can_add_compatible,
    }
}

/// Binding-based property completion for the node enclosing `byte`, plus the
/// `compatible` snippet when the node can still take one.
pub fn property_completion(
    doc: &Document,
    byte: usize,
    prefix: &str,
    bindings: &crate::bindings::BindingIndex,
) -> Vec<CompletionItem> {
    let view = node_view(doc, byte, bindings);
    let mut items = view
        .binding
        .as_ref()
        .map(|binding| items_from_binding(binding, &view.existing, prefix))
        .unwrap_or_default();
    if view.can_add_compatible
        && let Some(item) = compatible_property_item(prefix)
    {
        items.push(item);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn context_at(text: &str, needle_end: &str) -> Option<Context> {
        let doc = Document::open(text, 0);
        let byte = text.find(needle_end).unwrap() + needle_end.len();
        completion_context(&doc, byte)
    }

    #[test]
    fn compatible_vendor_stage() {
        let open = "/ {\n    node {\n        compatible = \"\n";
        let expect = Some(Context::Compatible {
            prefix: String::new(),
            vendor: None,
        });
        assert_eq!(context_at(open, "\""), expect);
        assert_eq!(
            context_at("/ {\n    node {\n        compatible = \"ti\n", "\"ti"),
            Some(Context::Compatible {
                prefix: "ti".to_string(),
                vendor: None,
            })
        );
    }

    #[test]
    fn compatible_device_stage() {
        assert_eq!(
            context_at("/ {\n    node {\n        compatible = \"ti,\n", "\"ti,"),
            Some(Context::Compatible {
                prefix: String::new(),
                vendor: Some("ti".to_string()),
            })
        );
        assert_eq!(
            context_at("/ {\n    node {\n        compatible = \"ti,am\n", "\"ti,am"),
            Some(Context::Compatible {
                prefix: "am".to_string(),
                vendor: Some("ti".to_string()),
            })
        );
        // second string of a multi-compatible stays in device context
        assert_eq!(
            context_at(
                "/ {\n    node {\n        compatible = \"ti,x\", \"\n",
                "\", \""
            ),
            Some(Context::Compatible {
                prefix: String::new(),
                vendor: None,
            })
        );
    }

    #[test]
    fn compatible_context_is_suppressed_for_endpoints_and_other_properties() {
        let endpoint = "/ {\n    ports {\n        port {\n            endpoint {\n                compatible = \"\n";
        assert_eq!(context_at(endpoint, "\""), None);
        assert_eq!(context_at("/ {\n    node {\n        status = \"ok\n", "\"ok"), None);
    }

    #[test]
    fn text_node_reads_half_written_nodes() {
        // no `;`, no closing braces at all
        let text = "/dts-v1/;\n\n/ {\n    node@1000 {\n        compatible = \"acme,widget\"\n        re\n";
        let byte = text.rfind("re\n").unwrap() + 2;
        let node = text_node(&Document::open(text, 0), byte).expect("no text node");
        assert_eq!(node.compatibles, vec!["acme,widget"]);
        assert!(node.properties.contains(&"compatible".to_string()));

        // nested node: the inner one wins, even when outer has a compatible
        let nested = "/dts-v1/;\n\n/ {\n    outer {\n        compatible = \"acme,widget\";\n        inner {\n            comp\n";
        let byte = nested.rfind("comp").unwrap() + 4;
        let node = text_node(&Document::open(nested, 0), byte).expect("no text node");
        assert!(node.compatibles.is_empty(), "compatibles: {:?}", node.compatibles);

        // only the `}` is missing
        let missing_close = "/dts-v1/;\n\n/ {\n    node@1000 {\n        compatible = \"acme,widget\";\n        reg = <0x1>;\n    };\n";
        let byte = missing_close.rfind("reg").unwrap() + 3;
        let node = text_node(&Document::open(missing_close, 0), byte).expect("no text node");
        assert_eq!(node.compatibles, vec!["acme,widget"]);
        assert!(node.properties.contains(&"reg".to_string()));
    }

    #[test]
    fn text_node_handles_allman_and_braceless_headers() {
        // `{` on its own line
        let allman = "/dts-v1/;\n\n/ {\n    node@1000\n    {\n        compatible = \"acme,widget\";\n        clo\n    };\n};\n";
        let byte = allman.rfind("clo").unwrap() + 3;
        let doc = Document::open(allman, 0);
        let node = text_node(&doc, byte).expect("no text node");
        assert_eq!(node.compatibles, vec!["acme,widget"]);
        assert!(!in_endpoint_or_port(&doc, byte));

        // header written but braces not typed yet
        let braceless = "/dts-v1/;\n\n/ {\n    node@1000\n        compatible = \"acme,widget\"\n        re\n";
        let byte = braceless.rfind("re\n").unwrap() + 2;
        let node = text_node(&Document::open(braceless, 0), byte).expect("no text node");
        assert_eq!(node.compatibles, vec!["acme,widget"]);

        // endpoint detection works from the text as well
        let endpoint = "/dts-v1/;\n\n/ {\n    ports {\n        port {\n            endpoint {\n                compatible = \"\n";
        let byte = endpoint.rfind('"').unwrap() + 1;
        let doc = Document::open(endpoint, 0);
        assert!(in_endpoint_or_port(&doc, byte));
    }

    #[test]
    fn text_node_ignores_syntax_lookalikes_in_strings_and_comments() {
        // slashes and braces inside a string, and a `}` inside a comment, must
        // not disturb brace matching for the following node
        let text = "/dts-v1/;\n\n/ {\n    a {\n        compatible = \"http://x/{y}\";\n        /* } */\n    };\n\n    b {\n        compatible = \"acme,widget\";\n        cl\n    };\n};\n";
        let byte = text.rfind("cl").unwrap() + 2;
        let node = text_node(&Document::open(text, 0), byte).expect("no text node");
        assert_eq!(node.compatibles, vec!["acme,widget"]);
        assert!(node.properties.contains(&"compatible".to_string()));

        let inside_a = text.find("/* } */").unwrap();
        let node = text_node(&Document::open(text, 0), inside_a).expect("no text node");
        assert_eq!(node.compatibles, vec!["http://x/{y}"]);
    }

    #[test]
    fn binding_items_are_unique() {
        use crate::bindings::Binding;
        use tower_lsp::lsp_types::Url;
        let binding = Binding {
            uri: Url::parse("file:///t.yaml").unwrap(),
            singles: vec!["acme,t".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "reg", "clocks"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            // `reg` is both allowed and required: must not produce two items
            required: vec!["compatible".to_string(), "reg".to_string()],
            required_any: Vec::new(),
            strict: false,
            refs_complete: true,
            patterns: regex::RegexSet::empty(),
        };
        let mut existing = std::collections::HashSet::new();
        let items = items_from_binding(&binding, &existing, "");
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["compatible", "reg", "clocks"], "items: {labels:?}");

        // properties the node already has are never suggested
        existing.insert("reg".to_string());
        let items = items_from_binding(&binding, &existing, "");
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["compatible", "clocks"], "items: {labels:?}");
    }

    #[test]
    fn compatible_property_item_is_a_snippet() {
        let Some(item) = compatible_property_item("") else {
            panic!("no item");
        };
        assert_eq!(item.label, "compatible");
        assert_eq!(item.insert_text.as_deref(), Some("compatible = \"$0\""));
        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert!(compatible_property_item("comp").is_some());
        assert!(compatible_property_item("reg").is_none());
    }

    #[test]
    fn labels_after_ampersand() {
        assert_eq!(
            context_at("/ { a = <&int>;\n};\n", "&int"),
            Some(Context::Labels("int".to_string()))
        );
        assert_eq!(
            context_at("/ { a = <&>;\n};\n", "&"),
            Some(Context::Labels(String::new()))
        );
    }

    #[test]
    fn defines_for_uppercase_words() {
        assert_eq!(
            context_at("/ { a = <GIC_SPI>;\n};\n", "GIC_SPI"),
            Some(Context::Defines("GIC_SPI".to_string()))
        );
        assert_eq!(
            context_at("/ { a = <GIC_>;\n};\n", "GIC_"),
            Some(Context::Defines("GIC_".to_string()))
        );
    }

    #[test]
    fn defines_after_fresh_angle_bracket() {
        assert_eq!(
            context_at("/ { a = <>;\n};\n", "<"),
            Some(Context::Defines(String::new()))
        );
    }

    #[test]
    fn lowercase_words_never_complete() {
        // mid-value lowercase words are not a completion context
        assert_eq!(context_at("/ { a = <1>;\n};\n", "a"), None);
        assert_eq!(context_at("/ { spi@1000 {\n};\n", "spi"), None);
        // node names in all caps would be completed as defines — acceptable,
        // node names are lowercase by convention.
    }

    #[test]
    fn property_position_detected() {
        let text = "/ {\n    node {\n        clo\n    };\n};\n";
        assert_eq!(
            context_at(text, "clo"),
            Some(Context::Properties("clo".to_string()))
        );
        // after `=` it is a value, not a property name
        assert_eq!(context_at("/ {\n    a = clo\n};\n", "clo"), None);
    }

    #[test]
    fn hash_prefixed_property_position_detected() {
        // `#` is a legal first character of dt property names
        let text = "/ {\n    node {\n        #address-cells = <1>;\n        #siz\n    };\n};\n";
        assert_eq!(
            context_at(text, "#siz"),
            Some(Context::Properties("#siz".to_string()))
        );
        // cursor right after `#`
        assert_eq!(
            context_at(text, "#"),
            Some(Context::Properties("#".to_string()))
        );
    }

    #[test]
    fn top_level_directive_gets_no_compatible_snippet() {
        // `#include <...>` lines have no node context: property completion
        // must not offer the `compatible` snippet there
        let text = "/dts-v1/;\n#include <dt-bindings/clock.h>\n#inc\n";
        let doc = Document::open(text, 0);
        let byte = text.rfind("#inc").unwrap() + 4;
        let items = property_completion(&doc, byte, "#inc", &crate::bindings::BindingIndex::default());
        assert!(items.is_empty(), "items: {items:?}");
    }

    #[test]
    fn hash_property_items_match_the_hash_prefix() {
        use crate::bindings::Binding;
        use tower_lsp::lsp_types::Url;
        let text = "/ {\n    node {\n        compatible = \"acme,widget\";\n        #siz\n    };\n};\n";
        let doc = Document::open(text, 0);
        let byte = text.rfind("#siz").unwrap() + 4;
        let bindings = crate::bindings::BindingIndex::default();
        bindings.insert_for_test(crate::bindings::Binding {
            uri: Url::parse("file:///w.yaml").unwrap(),
            singles: vec!["acme,widget".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "#size-cells"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            required: vec!["compatible".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: regex::RegexSet::empty(),
        });
        let items = property_completion(&doc, byte, "#siz", &bindings);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["#size-cells"], "items: {labels:?}");
    }

    #[test]
    fn binding_items_are_required_first_and_deduped() {
        use crate::bindings::Binding;
        use std::collections::HashSet;
        let binding = Binding {
            uri: tower_lsp::lsp_types::Url::parse("file:///b.yaml").unwrap(),
            singles: vec!["acme,x".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "clocks", "reg", "status_probe"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            required: vec!["compatible".to_string(), "reg".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: regex::RegexSet::empty(),
        };
        let existing = HashSet::from(["compatible".to_string()]);
        let items = items_from_binding(&binding, &existing, "");
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        // "reg" (required) first; "compatible" already present -> dropped
        assert_eq!(labels.first(), Some(&"reg"));
        assert!(!labels.contains(&"compatible"));
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn label_paths_are_full() {
        let text = "/dts-v1/;\n\n/ {\n    soc {\n        uart0: serial@1000 {\n            x;\n        };\n    };\n};\n";
        let doc = Document::open(text, 0);
        let items = same_file_items(&doc);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "uart0");
        assert_eq!(items[0].detail.as_deref(), Some("/soc/serial@1000"));
    }

    #[test]
    fn define_items_show_value_and_file() {
        let mut defines = DefineMap::new();
        defines.insert("GIC_SPI".to_string(), ("0".to_string(), String::new()));
        defines.insert(
            "IRQ_TYPE_EDGE_RISING".to_string(),
            ("1".to_string(), "irq.h".to_string()),
        );
        let items = items_from_defines(&defines, "GIC");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "GIC_SPI");
        assert_eq!(items[0].detail.as_deref(), Some("0"));
        assert_eq!(items[0].kind, Some(CompletionItemKind::CONSTANT));
        let all = items_from_defines(&defines, "");
        assert_eq!(all.len(), 2);
        let irq = all
            .iter()
            .find(|i| i.label == "IRQ_TYPE_EDGE_RISING")
            .unwrap();
        assert_eq!(irq.detail.as_deref(), Some("1 — irq.h"));
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use crate::bindings::Binding;
    use crate::bindings::BindingIndex;
    use tower_lsp::lsp_types::Url;

    fn widget_bindings() -> BindingIndex {
        let bindings = BindingIndex::default();
        bindings.insert_for_test(Binding {
            uri: Url::parse("file:///mmc.yaml").unwrap(),
            singles: vec!["acme,mmc".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "reg", "clocks", "clock-names"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            required: vec!["compatible".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: regex::RegexSet::empty(),
        });
        bindings
    }

    #[test]
    fn property_items_work_in_a_half_written_node() {
        // compatible accepted, but no `;` and no closing braces yet
        let text = "/dts-v1/;\n\n/ {\n    mmc@8804000 {\n        compatible = \"acme,mmc\"\n        clo\n";
        let doc = Document::open(text, 0);
        let bindings = widget_bindings();
        let byte = text.rfind("clo").unwrap() + 3;
        let items = property_completion(&doc, byte, "clo", &bindings);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["clock-names", "clocks"], "items: {labels:?}");
        // the node already has a compatible, so no snippet is offered
        assert!(!labels.contains(&"compatible"));
    }

    #[test]
    fn property_completion_offers_compatible_for_a_fresh_node() {
        let text = "/dts-v1/;\n\n/ {\n    node@1000 {\n        comp\n";
        let doc = Document::open(text, 0);
        let byte = text.rfind("comp").unwrap() + 4;
        let items = property_completion(&doc, byte, "comp", &BindingIndex::default());
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["compatible"]);
    }

    #[test]
    fn property_items_resolve_through_the_enclosing_node() {
        let text = "/dts-v1/;\n\n/ {\n    mmc@8804000 {\n        compatible = \"acme,mmc\";\n        reg = <0x1>;\n        clo\n    };\n};\n";
        let doc = Document::open(text, 0);
        let bindings = BindingIndex::default();
        bindings.insert_for_test(Binding {
            uri: Url::parse("file:///mmc.yaml").unwrap(),
            singles: vec!["acme,mmc".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "reg", "clocks", "clock-names"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            required: vec!["compatible".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: regex::RegexSet::empty(),
        });
        // cursor right after "clo" on its line
        let byte = text.find("clo").unwrap() + 3;
        let items = property_completion(&doc, byte, "clo", &bindings);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["clock-names", "clocks"], "items: {labels:?}");
    }

    fn nested_bindings() -> BindingIndex {
        let bindings = BindingIndex::default();
        let make = |single: &str, props: &[&str]| Binding {
            uri: Url::parse(&format!("file:///{single}.yaml")).unwrap(),
            singles: vec![single.to_string()],
            compounds: Vec::new(),
            properties: props.iter().map(|p| p.to_string()).collect(),
            required: vec!["compatible".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: regex::RegexSet::empty(),
        };
        bindings.insert_for_test(make("acme,parent", &["compatible", "parent-prop", "parent-extra"]));
        bindings.insert_for_test(make("acme,child", &["compatible", "child-prop"]));
        bindings
    }

    #[test]
    fn completion_above_a_node_uses_the_parent() {
        // cursor on the blank line between the parent's last property and the
        // child node header: the child's compatible must not leak in
        let text = "/dts-v1/;\n\n/ {\n    parent@1000 {\n        compatible = \"acme,parent\";\n        parent-prop = <1>;\n        p\n        child@2000 {\n            compatible = \"acme,child\";\n        };\n    };\n};\n";
        let doc = Document::open(text, 0);
        let byte = text.find("        p\n").unwrap() + 9;
        let items = property_completion(&doc, byte, "p", &nested_bindings());
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        // parent-prop is already written in the node, so only the other one
        assert_eq!(labels, vec!["parent-extra"], "items: {labels:?}");
    }

    #[test]
    fn completion_above_a_node_offers_compatible_for_the_parent() {
        let text = "/dts-v1/;\n\n/ {\n    parent@1000 {\n        reg = <1>;\n        c\n        child@2000 {\n            compatible = \"acme,child\";\n        };\n    };\n};\n";
        let doc = Document::open(text, 0);
        let byte = text.find("        c\n").unwrap() + 9;
        let items = property_completion(&doc, byte, "c", &nested_bindings());
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["compatible"], "items: {labels:?}");
    }

    #[test]
    fn completion_inside_a_node_still_uses_that_node() {
        let text = "/dts-v1/;\n\n/ {\n    parent@1000 {\n        compatible = \"acme,parent\";\n        child@2000 {\n            compatible = \"acme,child\";\n            c\n        };\n    };\n};\n";
        let doc = Document::open(text, 0);
        let byte = text.find("            c\n").unwrap() + 13;
        let items = property_completion(&doc, byte, "c", &nested_bindings());
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["child-prop"], "items: {labels:?}");
    }
}

