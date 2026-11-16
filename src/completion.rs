use std::collections::HashMap;

use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, CompletionList};

use crate::analysis;
use crate::document::Document;
use crate::index::DefineMap;

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
}

/// Classify the completion context at the cursor. Lowercase words (node and
/// property names) intentionally return None so the popup never nags there.
pub fn completion_context(doc: &Document, cursor: usize) -> Option<Context> {
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
    None
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
        assert_eq!(context_at("/ { a = <1>;\n};\n", "a"), None);
        assert_eq!(context_at("/ { spi@1000 {\n};\n", "spi"), None);
        // node names in all caps would be completed as defines — acceptable,
        // node names are lowercase by convention.
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
