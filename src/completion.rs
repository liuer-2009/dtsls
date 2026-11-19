use std::collections::HashMap;

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, InsertTextFormat, Range, TextEdit,
};
use tree_sitter::Node;

use crate::analysis;
use crate::document::Document;
use crate::index::DefineMap;
use crate::util;

/// label -> (node path, defining file name). An empty file name means the
/// label is defined in the file the index entry belongs to.
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
fn is_endpoint_or_port(doc: &Document, node: Node<'_>) -> bool {
    let Some(name) = node.child_by_field_name("name") else {
        return false;
    };
    let name = util::node_text(doc, name);
    let base = name.split('@').next().unwrap_or(name);
    base == "endpoint" || base == "port"
}

/// Whether the node enclosing the cursor is `endpoint`/`port` plumbing.
/// Falls back to a line scan when syntax errors collapse the tree into a
/// single ERROR node — the normal state while a string is being typed.
pub fn in_endpoint_or_port(doc: &Document, byte: usize) -> bool {
    match enclosing_node(doc, byte) {
        Some(node) if node.kind() == "subnode" => is_endpoint_or_port(doc, node),
        _ => match line_scanned_node_name(doc, byte) {
            Some(name) => {
                let base = name.split('@').next().unwrap_or(&name);
                base == "endpoint" || base == "port"
            }
            None => false,
        },
    }
}

/// Innermost open node name at the cursor line, from brace counting. Only
/// used to recognise `endpoint`/`port`, so comment/string noise is tolerable.
fn line_scanned_node_name(doc: &Document, byte: usize) -> Option<String> {
    let last_line = doc.rope.byte_to_line(byte.min(doc.rope.len_bytes()));
    let mut stack: Vec<String> = Vec::new();
    let mut buf = String::new();
    for line in 0..=last_line {
        let start = doc.rope.line_to_byte(line);
        let end = if line + 1 < doc.rope.len_lines() {
            doc.rope.line_to_byte(line + 1)
        } else {
            doc.rope.len_bytes()
        };
        buf.clear();
        buf.push_str(&doc.slice(start..end));
        let code = buf.split("//").next().unwrap_or("").trim();
        let opens = code.matches('{').count();
        let closes = code.matches('}').count();
        if opens > 0 {
            let name = code
                .split('{')
                .next()
                .unwrap_or("")
                .rsplit(':')
                .next()
                .unwrap_or("")
                .trim()
                .split_whitespace()
                .next_back()
                .unwrap_or("")
                .to_string();
            for _ in 0..opens {
                stack.push(name.clone());
            }
        }
        for _ in 0..closes {
            stack.pop();
        }
    }
    stack.pop()
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
    let mut items: Vec<CompletionItem> = binding
        .properties
        .iter()
        .chain(binding.required.iter())
        .filter(|name| !existing.contains(*name) && name.starts_with(prefix))
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
    items.dedup_by(|a, b| a.label == b.label);
    items
}

/// Items straight from the background index: no tree walk, no IO. This is the
/// steady-state path and must stay well under a millisecond.
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
