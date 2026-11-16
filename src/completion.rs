use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, CompletionList};
use tree_sitter::Node;

use crate::analysis;
use crate::document::Document;
use crate::util;

/// label -> (node path, defining file name). An empty file name means the
/// label is defined in the file the index entry belongs to.
pub type LabelMap = HashMap<String, (String, String)>;

/// The word being typed, if it directly follows a `&` — the label reference
/// completion context. Anything else returns None so the handler stays cheap.
pub fn label_prefix(doc: &Document, cursor: usize) -> Option<String> {
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
    if start == 0 || doc.rope.byte(start - 1) != b'&' {
        return None;
    }
    Some(doc.slice(start..cursor).to_string())
}

/// Items straight from the background index: no tree walk, no IO. This is the
/// steady-state path and must stay well under a millisecond.
pub fn items_from_index(labels: &LabelMap, prefix: &str) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = labels
        .iter()
        .filter(|(label, _)| label.starts_with(prefix))
        .map(|(label, (path, file))| {
            let detail = detail(path.as_str(), file.as_str());
            item(label, &detail)
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
        .map(|(label, path)| item(label, path))
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

fn detail(path: &str, file: &str) -> String {
    if file.is_empty() {
        path.to_string()
    } else {
        format!("{path} — {file}")
    }
}

fn item(label: &str, detail: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::VARIABLE),
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

/// Recursively collect `label -> (node path, file name)` for a file and its
/// quoted `#include`s. Runs on a worker thread; the completion handler only
/// ever reads the finished map.
pub fn build_file_labels(
    path: &Path,
    text: &str,
    roots: &[PathBuf],
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) -> LabelMap {
    let mut out = HashMap::new();
    if depth > 8 || !visited.insert(path.to_path_buf()) {
        return out;
    }
    let doc = Document::open(text, 0);

    // Labels defined in this file: empty file name marks them as "own".
    let analysis = analysis::analyze(&doc);
    for (label, node_path) in &analysis.labels {
        out.entry(label.clone())
            .or_insert_with(|| (node_path.clone(), String::new()));
    }

    collect_includes(doc.tree.root_node(), &doc, path, roots, visited, depth, &mut out);
    out
}

fn collect_includes<'a>(
    node: Node<'a>,
    doc: &'a Document,
    file_path: &Path,
    roots: &[PathBuf],
    visited: &mut HashSet<PathBuf>,
    depth: usize,
    out: &mut LabelMap,
) {
    if node.kind() == "preproc_include"
        && let Some(path) = node.child_by_field_name("path")
        && path.kind() == "preproc_string_literal"
    {
        // Only quoted includes are resolvable without knowing the compiler's
        // include dirs; `<...>` ones are skipped for now.
        let raw = util::node_text(doc, path);
        let relative = raw.trim_matches('"');
        let dir = file_path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        for base in std::iter::once(dir).chain(roots.iter().cloned()) {
            let candidate = base.join(relative);
            let Ok(included) = std::fs::read_to_string(&candidate) else {
                continue;
            };
            let included_file = candidate
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let labels = build_file_labels(&candidate, &included, roots, visited, depth + 1);
            for (label, (node_path, file)) in labels {
                // Tag the include's own labels with its file name.
                let file = if file.is_empty() {
                    included_file.clone()
                } else {
                    file
                };
                out.entry(label).or_insert((node_path, file));
            }
            break;
        }
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            collect_includes(child, doc, file_path, roots, visited, depth, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn prefix_at(text: &str, needle_end: &str) -> Option<String> {
        let doc = Document::open(text, 0);
        let byte = text.find(needle_end).unwrap() + needle_end.len();
        label_prefix(&doc, byte)
    }

    #[test]
    fn prefix_after_ampersand() {
        assert_eq!(
            prefix_at("/ { a = <&int>;\n};\n", "&int"),
            Some("int".to_string())
        );
        assert_eq!(prefix_at("/ { a = <&>;\n};\n", "&"), Some(String::new()));
        assert_eq!(prefix_at("/ { a = <1>;\n};\n", "a"), None);
    }

    #[test]
    fn ampersand_must_directly_precede_the_word() {
        // A word not directly after & (cursor after a space or a digit) is not
        // label completion context.
        assert_eq!(prefix_at("/ { a = &x + &y;\n};\n", "+ "), None);
        assert_eq!(prefix_at("/ { a = &x + &y;\n};\n", "&x"), Some("x".into()));
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
    fn include_graph_is_walked_for_labels() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let board = Path::new(&manifest).join("testdata/includes/board.dts");
        let text = std::fs::read_to_string(&board).unwrap();
        let mut visited = HashSet::new();
        let labels = build_file_labels(&board, &text, &[], &mut visited, 0);
        assert_eq!(
            labels.get("board_label").map(|(path, file)| (path.as_str(), file.as_str())),
            Some(("/node", ""))
        );
        assert_eq!(
            labels.get("uart0").map(|(path, file)| (path.as_str(), file.as_str())),
            Some(("/soc/serial@1000", "soc.dtsi"))
        );
    }
}
