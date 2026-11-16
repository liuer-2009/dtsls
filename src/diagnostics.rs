use std::collections::HashMap;

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use tree_sitter::Node;

use crate::document::Document;
use crate::util;

const SOURCE: &str = "dtsls";
const MAX_DIAGNOSTICS: usize = 100;

/// Syntax errors plus semantic errors, sorted by position.
pub fn all(doc: &Document) -> Vec<Diagnostic> {
    let mut out = compute(doc);
    out.extend(semantic(doc));
    out.sort_by_key(|d| (d.range.start, d.range.end));
    out
}

/// Collect syntax errors (ERROR nodes and missing tokens) from the parse tree.
pub fn compute(doc: &Document) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    collect(doc.tree.root_node(), doc, &mut out);
    out
}

/// Semantic errors that the grammar alone cannot see.
pub fn semantic(doc: &Document) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    check_duplicates(doc.tree.root_node(), doc, &mut out);
    out
}

/// Flag sibling nodes that define the same name (dtc would silently merge
/// them) or that claim the same unit address under one parent.
fn check_duplicates(node: Node, doc: &Document, out: &mut Vec<Diagnostic>) {
    if out.len() >= MAX_DIAGNOSTICS {
        return;
    }
    if node.kind() == "source_file" {
        // Multiple `/ { }` roots in one file are silently merged by dtc.
        let mut seen_root = false;
        for i in 0..node.child_count() {
            let Some(child) = node.child(i as u32) else { continue };
            if child.kind() != "devicetree" {
                continue;
            }
            if util::find_child(child, "reference").is_some() {
                continue;
            }
            let range = Range {
                start: util::byte_to_position(&doc.rope, child.start_byte()),
                end: util::byte_to_position(&doc.rope, child.start_byte() + 1),
            };
            if seen_root {
                out.push(Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some(SOURCE.to_string()),
                    message: "duplicate node '/'".to_string(),
                    ..Default::default()
                });
            } else {
                seen_root = true;
            }
        }
    }
    // Node definitions live in node_def bodies, also directly inside #if blocks.
    if matches!(
        node.kind(),
        "node_def" | "preproc_if_in_node_def" | "preproc_ifdef_in_node_def"
    ) {
        let mut seen_names: HashMap<String, String> = HashMap::new();
        let mut seen_addresses: HashMap<String, String> = HashMap::new();
        for i in 0..node.child_count() {
            let Some(child) = node.child(i as u32) else { continue };
            if child.kind() != "subnode" {
                continue;
            }
            let Some(name_node) = child.child_by_field_name("name") else {
                continue;
            };
            let full = util::node_text(doc, name_node).to_string();
            let (name, address) = match full.split_once('@') {
                Some((name, addr)) => (name.to_string(), Some(addr.to_string())),
                None => (full.clone(), None),
            };
            if seen_names.contains_key(&full) {
                out.push(Diagnostic {
                    range: node_range(doc, name_node),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some(SOURCE.to_string()),
                    message: format!("duplicate node '{full}'"),
                    ..Default::default()
                });
            } else {
                seen_names.insert(full.clone(), name.clone());
            }
            if let Some(addr) = &address {
                match seen_addresses.get(addr) {
                    Some(first_name) if first_name != &name => {
                        // Legitimate on some SoCs (e.g. Qualcomm GENI: one
                        // serial engine, several protocol nodes per address),
                        // so this is a warning, not an error.
                        out.push(Diagnostic {
                            range: node_range(doc, name_node),
                            severity: Some(DiagnosticSeverity::WARNING),
                            source: Some(SOURCE.to_string()),
                            message: format!(
                                "node '{full}' shares address with '{}@{addr}'",
                                first_name
                            ),
                            ..Default::default()
                        });
                    }
                    None => {
                        seen_addresses.insert(addr.clone(), name.clone());
                    }
                    _ => {}
                }
            }
        }
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            // Both #ifdef branches are parsed; checking the alternative would
            // produce false duplicates, so only the primary branch is walked.
            if child.kind().contains("else") || child.kind().contains("elif") {
                continue;
            }
            check_duplicates(child, doc, out);
        }
    }
}

fn collect(node: Node, doc: &Document, out: &mut Vec<Diagnostic>) {
    if out.len() >= MAX_DIAGNOSTICS {
        return;
    }
    if node.is_error() {
        let mut range = node_range(doc, node);
        if range.end == range.start {
            range.end.character += 1;
        }
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some(SOURCE.to_string()),
            message: format!("syntax error near {}", snippet(doc, node)),
            ..Default::default()
        });
        // The ERROR node covers the whole malformed region; recursing would
        // only produce noise about the same span.
        return;
    }
    if node.is_missing() {
        out.push(Diagnostic {
            range: node_range(doc, node),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some(SOURCE.to_string()),
            message: format!("missing '{}'", node.kind()),
            ..Default::default()
        });
        return;
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            collect(child, doc, out);
        }
    }
}

fn node_range(doc: &Document, node: Node) -> Range {
    Range {
        start: util::byte_to_position(&doc.rope, node.start_byte()),
        end: util::byte_to_position(&doc.rope, node.end_byte()),
    }
}

fn snippet(doc: &Document, node: Node) -> String {
    let text = doc.slice(node.start_byte()..node.end_byte());
    let first_line = text.lines().next().unwrap_or("");
    let short: String = first_line.chars().take(32).collect();
    format!("{short:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn diagnostics_for(text: &str) -> Vec<Diagnostic> {
        compute(&Document::open(text, 0))
    }

    #[test]
    fn valid_source_has_no_diagnostics() {
        let text = r#"
/dts-v1/;

/ {
    a = <1>;
    b = "str";
    node@100 {
        c;
    };
};
"#;
        assert!(diagnostics_for(text).is_empty());
    }

    #[test]
    fn unclosed_brace_is_reported() {
        let text = "/dts-v1/;\n\n/ {\n    a {\n        x = <1>;\n";
        let diags = diagnostics_for(text);
        assert!(!diags.is_empty());
    }

    #[test]
    fn missing_semicolon_is_reported() {
        let text = "/dts-v1/;\n\n/ {\n    a = <1>\n    b = <2>;\n};\n";
        let diags = diagnostics_for(text);
        assert!(!diags.is_empty());
    }

    #[test]
    fn unterminated_string_is_reported() {
        let text = "/dts-v1/;\n\n/ {\n    a = \"oops;\n};\n";
        assert!(!diagnostics_for(text).is_empty());
    }

    #[test]
    fn unclosed_cells_are_reported() {
        let text = "/dts-v1/;\n\n/ {\n    reg = <0x100 0x200;\n};\n";
        assert!(!diagnostics_for(text).is_empty());
    }

    #[test]
    fn fixture_directories_are_consistent() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let broken = glob_dir(&format!("{manifest}/testdata/broken"));
        let warning = glob_dir(&format!("{manifest}/testdata/warning"));
        let valid = glob_dir(&format!("{manifest}/testdata/valid"));
        assert!(!broken.is_empty(), "no broken fixtures found");
        assert!(!valid.is_empty(), "no valid fixtures found");
        for path in broken {
            let text = std::fs::read_to_string(&path).unwrap();
            let diags = all(&Document::open(&text, 0));
            assert!(!diags.is_empty(), "broken fixture not detected: {path}");
            assert!(
                diags
                    .iter()
                    .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
                "broken fixture has no error-severity diagnostic: {path}"
            );
        }
        for path in warning {
            let text = std::fs::read_to_string(&path).unwrap();
            let diags = all(&Document::open(&text, 0));
            assert!(!diags.is_empty(), "warning fixture not detected: {path}");
            assert!(
                diags
                    .iter()
                    .all(|d| d.severity != Some(DiagnosticSeverity::ERROR)),
                "warning fixture has an error diagnostic: {path}"
            );
        }
        for path in valid {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                all(&Document::open(&text, 0)).is_empty(),
                "valid fixture has errors: {path}"
            );
        }
    }

    #[test]
    fn duplicate_node_name_is_reported() {
        let text = "/dts-v1/;\n\n/ {\n    serial@2000 {\n        a;\n    };\n\n    serial@2000 {\n        b;\n    };\n};\n";
        let diags = semantic(&Document::open(text, 0));
        assert!(diags.iter().any(|d| d.message.contains("duplicate node 'serial@2000'")));
    }

    #[test]
    fn duplicate_address_with_different_name_is_reported() {
        let text = "/dts-v1/;\n\n/ {\n    serial@2000 {\n        a;\n    };\n\n    i2c@2000 {\n        b;\n    };\n};\n";
        let diags = semantic(&Document::open(text, 0));
        let warning = diags
            .iter()
            .find(|d| d.message.contains("shares address"))
            .expect("address conflict not reported");
        assert_eq!(warning.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn duplicate_node_severity_is_error() {
        let text = "/dts-v1/;\n\n/ {\n    serial@2000 {\n        a;\n    };\n\n    serial@2000 {\n        b;\n    };\n};\n";
        let diags = semantic(&Document::open(text, 0));
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("duplicate node") && d.severity == Some(DiagnosticSeverity::ERROR))
        );
    }

    #[test]
    fn same_name_different_address_is_fine() {
        let text = "/dts-v1/;\n\n/ {\n    serial@1000 {\n        a;\n    };\n\n    serial@2000 {\n        b;\n    };\n};\n";
        assert!(semantic(&Document::open(text, 0)).is_empty());
    }

    #[test]
    fn reference_extensions_are_not_duplicates() {
        let text = "/dts-v1/;\n\n/ {\n    intc: n@1000 {\n        a;\n    };\n};\n\n&intc {\n    b;\n};\n\n&intc {\n    c;\n};\n";
        assert!(semantic(&Document::open(text, 0)).is_empty());
    }

    #[test]
    fn duplicates_in_different_parents_are_fine() {
        let text = "/dts-v1/;\n\n/ {\n    bus1 {\n        dev@1000 {\n            a;\n        };\n    };\n\n    bus2 {\n        dev@1000 {\n            b;\n        };\n    };\n};\n";
        assert!(semantic(&Document::open(text, 0)).is_empty());
    }

    fn glob_dir(pattern: &str) -> Vec<String> {
        let dir = std::path::Path::new(pattern);
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "dts") {
                    out.push(entry.path().to_string_lossy().into_owned());
                }
            }
        }
        out.sort();
        out
    }
}
