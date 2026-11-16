use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use tree_sitter::Node;

use crate::document::Document;
use crate::util;

const SOURCE: &str = "dtsls";
const MAX_DIAGNOSTICS: usize = 100;

/// Collect syntax errors (ERROR nodes and missing tokens) from the parse tree.
pub fn compute(doc: &Document) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    collect(doc.tree.root_node(), doc, &mut out);
    out
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
        if let Some(child) = node.child(i) {
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
        let valid = glob_dir(&format!("{manifest}/testdata/valid"));
        assert!(!broken.is_empty(), "no broken fixtures found");
        assert!(!valid.is_empty(), "no valid fixtures found");
        for path in broken {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                !diagnostics_for(&text).is_empty(),
                "broken fixture not detected: {path}"
            );
        }
        for path in valid {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                diagnostics_for(&text).is_empty(),
                "valid fixture has errors: {path}"
            );
        }
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
