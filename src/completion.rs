use std::collections::HashMap;

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, InsertTextFormat, Range, TextEdit,
};
use tree_sitter::Node;

use crate::analysis;
use crate::document::Document;
use crate::workspace::DefineMap;
use crate::util;

/// label -> (node path, defining file name). An empty file name means the
/// label is defined in the file the index entry belongs to.
#[cfg_attr(not(test), allow(dead_code))]
pub type LabelMap = HashMap<String, (String, String)>;

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
    let cursor = cursor.min(doc.rope.len_bytes());
    let line = doc.rope.byte_to_line(cursor);
    let line_start = doc.rope.line_to_byte(line);
    let mut start = cursor;
    while start > line_start {
        let b = doc.rope.byte(start - 1);
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == cursor {
        // no word typed: right after `&` offer labels, right after `<` macros
        if cursor > 0 {
            match doc.rope.byte(cursor - 1) {
                b'&' => return Some(Context::Labels(String::new())),
                b'<' => return Some(Context::Defines(String::new())),
                _ => {}
            }
        }
        return None;
    }
    let word = doc.slice(start..cursor).to_string();
    if start > 0 && doc.rope.byte(start - 1) == b'&' {
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
    let cursor = cursor.min(doc.rope.len_bytes());
    let line_start = doc.rope.line_to_byte(doc.rope.byte_to_line(cursor));
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
    let node = doc.tree.root_node().descendant_for_byte_range(byte, byte)?;
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
    let name = util::node_text(doc, name);
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
    let (open, block_end) = enclosing_brace(text.as_bytes(), cursor)?;
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
/// (the closing brace, or EOF when it is missing). Runs over a whole 200 KB
/// file: the special bytes are collected with vectorized scans first so the
/// per-token work is a handful of comparisons.
fn enclosing_brace(bytes: &[u8], cursor: usize) -> Option<(usize, usize)> {
    let len = bytes.len();
    let opens: Vec<usize> = memchr::memchr_iter(b'{', bytes).collect();
    let closes: Vec<usize> = memchr::memchr_iter(b'}', bytes).collect();
    let quotes: Vec<usize> = memchr::memchr_iter(b'"', bytes).collect();
    let slashes: Vec<usize> = memchr::memchr_iter(b'/', bytes).collect();
    let (mut oi, mut ci, mut qi, mut si) = (0, 0, 0, 0);
    let mut stack: Vec<usize> = Vec::new();
    let mut open: Option<usize> = None;
    let mut target_depth = 0;
    loop {
        let mut next = len;
        if oi < opens.len() {
            next = next.min(opens[oi]);
        }
        if ci < closes.len() {
            next = next.min(closes[ci]);
        }
        if qi < quotes.len() {
            next = next.min(quotes[qi]);
        }
        if si < slashes.len() {
            next = next.min(slashes[si]);
        }
        if next >= len {
            break;
        }
        // record the stack as of the cursor before a token at/after it
        if open.is_none() && next >= cursor {
            open = stack.last().copied();
            if open.is_none() {
                return None;
            }
            target_depth = stack.len();
        }
        if oi < opens.len() && opens[oi] == next {
            stack.push(next);
            oi += 1;
        } else if ci < closes.len() && closes[ci] == next {
            stack.pop();
            ci += 1;
            if open.is_some() && stack.len() < target_depth {
                return Some((open.unwrap(), next));
            }
        } else if qi < quotes.len() && quotes[qi] == next {
            // skip the string: its closing quote is the next unescaped one
            let mut string_end = len;
            qi += 1;
            while qi < quotes.len() {
                let quote = quotes[qi];
                qi += 1;
                if !escaped(bytes, quote) {
                    string_end = quote;
                    break;
                }
            }
            // slashes and braces inside the string are not syntax
            while oi < opens.len() && opens[oi] < string_end {
                oi += 1;
            }
            while ci < closes.len() && closes[ci] < string_end {
                ci += 1;
            }
            while si < slashes.len() && slashes[si] < string_end {
                si += 1;
            }
        } else if si < slashes.len() && slashes[si] == next {
            let slash = next;
            si += 1;
            let skip_to = if slash + 1 < len && bytes[slash + 1] == b'*' {
                memchr::memmem::find(&bytes[slash + 2..], b"*/")
                    .map(|offset| slash + 2 + offset + 2)
                    .unwrap_or(len)
            } else if slash + 1 < len && bytes[slash + 1] == b'/' {
                memchr::memchr(b'\n', &bytes[slash..])
                    .map(|offset| slash + offset + 1)
                    .unwrap_or(len)
            } else {
                slash + 1
            };
            while oi < opens.len() && opens[oi] < skip_to {
                oi += 1;
            }
            while ci < closes.len() && closes[ci] < skip_to {
                ci += 1;
            }
            while qi < quotes.len() && quotes[qi] < skip_to {
                qi += 1;
            }
            while si < slashes.len() && slashes[si] < skip_to {
                si += 1;
            }
        }
    }
    // no token at or after the cursor: the stack as of EOF is the answer
    if open.is_none() {
        open = Some(stack.last().copied()?);
    }
    open.map(|open| (open, len))
}

fn escaped(bytes: &[u8], index: usize) -> bool {
    let mut backslashes = 0;
    let mut i = index;
    while i > 0 && bytes[i - 1] == b'\\' {
        backslashes += 1;
        i -= 1;
    }
    backslashes % 2 == 1
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
    indent_of(&text[line_start..])
}

fn indent_of(line: &str) -> usize {
    let mut indent = 0;
    for c in line.chars() {
        match c {
            '\t' => indent += 8 - (indent % 8),
            ' ' => indent += 1,
            _ => break,
        }
    }
    indent
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
            && indent_of(line) > open_indent;
        if looks_like_header {
            found = Some((node_name_from_header(trimmed), indent_of(line), line_end));
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
        if !line.trim().is_empty() && indent_of(line) <= indent {
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
        let text = util::node_text(doc, node);
        if text.trim_end_matches(':') != label {
            return;
        }
        let owner = match node.parent() {
            Some(parent) if parent.kind() == "property" => {
                util::owning_node(parent).unwrap_or(parent)
            }
            Some(parent) => parent,
            None => node,
        };
        found = Some(Range {
            start: util::byte_to_position(&doc.rope, owner.start_byte()),
            end: util::byte_to_position(&doc.rope, owner.end_byte()),
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
        let Some(label_ref) = util::find_child(node, "label_ref") else {
            return;
        };
        let text = util::node_text(doc, label_ref);
        if text.trim_start_matches('&') != label {
            return;
        }
        out.push(Range {
            start: util::byte_to_position(&doc.rope, node.start_byte()),
            end: util::byte_to_position(&doc.rope, node.end_byte()),
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
                let name = util::node_text(doc, name).to_string();
                let value = node
                    .child_by_field_name("value")
                    .map(|value| summarize_value(util::node_text(doc, value)))
                    .unwrap_or_default();
                defines.insert(name, (value, String::new()));
            }
        }
        "preproc_undef" => {
            if let Some(name) = node.child_by_field_name("name") {
                defines.remove(util::node_text(doc, name));
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

#[cfg_attr(not(test), allow(dead_code))]
pub fn items_from_index(labels: &LabelMap, prefix: &str) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = labels
        .iter()
        .filter(|(label, _)| label.starts_with(prefix))
        .map(|(label, (path, file))| {
            let detail = detail(path.as_str(), file.as_str());
            item(label, &detail, CompletionItemKind::VARIABLE)
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
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
    let analysis = analysis::analyze(doc);
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
    let cursor = cursor.min(doc.rope.len_bytes());
    let line = doc.rope.byte_to_line(cursor);
    let line_start = doc.rope.line_to_byte(line);
    let mut start = cursor;
    while start > line_start {
        let b = doc.rope.byte(start - 1);
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            start -= 1;
        } else {
            break;
        }
    }
    let mut end = cursor;
    while end < doc.rope.len_bytes() {
        let b = doc.rope.byte(end);
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
    if start > 0 && doc.rope.byte(start - 1) == b'&' {
        return Some(word);
    }
    if doc.slice(end..(end + 1).min(doc.rope.len_bytes())) == ":" {
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
            patterns: Vec::new(),
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
            patterns: Vec::new(),
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
    fn items_from_index_are_filtered_and_tagged() {
        let mut labels = LabelMap::new();
        labels.insert("uart0".to_string(), ("/serial@1000".to_string(), String::new()));
        labels.insert(
            "intc".to_string(),
            ("/interrupt-controller@1000".to_string(), "soc.dtsi".to_string()),
        );
        let items = items_from_index(&labels, "u");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "uart0");
        assert_eq!(items[0].detail.as_deref(), Some("/serial@1000"));
        let all = items_from_index(&labels, "");
        assert_eq!(all.len(), 2);
        assert!(all[0].detail.as_deref().unwrap().contains("soc.dtsi"));
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

