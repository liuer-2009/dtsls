use tower_lsp::lsp_types::TextDocumentContentChangeEvent;
use tree_sitter::{InputEdit, Parser, Tree};

use crate::model;

pub struct Document {
    pub text: String,
    /// byte offset where each line starts (`line_starts[0] == 0`)
    pub line_starts: Vec<usize>,
    pub tree: Tree,
    pub version: i32,
}

impl Document {
    pub fn open(text: &str, version: i32) -> Self {
        let mut doc = Self {
            text: text.to_string(),
            line_starts: Vec::new(),
            tree: Self::parse(text, None).unwrap_or_else(Self::empty_tree),
            version,
        };
        doc.rebuild_line_starts();
        doc
    }

    fn empty_tree() -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_devicetree2::LANGUAGE.into())
            .expect("devicetree grammar should load");
        parser.parse("", None).expect("parsing empty text always succeeds")
    }

    fn parse(text: &str, old_tree: Option<&Tree>) -> Option<Tree> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_devicetree2::LANGUAGE.into())
            .ok()?;
        parser.parse(text, old_tree)
    }

    fn rebuild_line_starts(&mut self) {
        self.line_starts.clear();
        self.line_starts.push(0);
        for (i, b) in self.text.bytes().enumerate() {
            if b == b'\n' {
                self.line_starts.push(i + 1);
            }
        }
    }

    pub fn apply_changes(
        &mut self,
        changes: &[TextDocumentContentChangeEvent],
        version: i32,
    ) {
        for change in changes {
            match change.range {
                Some(range) => {
                    let start_byte = model::position_to_byte(self, range.start);
                    let old_end_byte = model::position_to_byte(self, range.end);
                    let start_point = model::byte_to_point(self, start_byte);
                    let old_end_point = model::byte_to_point(self, old_end_byte);

                    self.text
                        .replace_range(start_byte..old_end_byte, &change.text);

                    let new_end_byte = start_byte + change.text.len();
                    self.rebuild_line_starts();
                    let new_end_point = model::byte_to_point(self, new_end_byte);
                    self.tree.edit(&InputEdit {
                        start_byte,
                        old_end_byte,
                        new_end_byte,
                        start_position: start_point,
                        old_end_position: old_end_point,
                        new_end_position: new_end_point,
                    });
                    if let Some(tree) = Self::parse(&self.text, Some(&self.tree)) {
                        self.tree = tree;
                    }
                }
                None => {
                    self.text = change.text.clone();
                    self.rebuild_line_starts();
                    self.tree = Self::parse(&self.text, None).unwrap_or_else(Self::empty_tree);
                }
            }
        }
        self.version = version;
    }

    /// Replace the whole text, reusing the previous tree. The common prefix
    /// and suffix are kept, so a localized edit (the usual case for a
    /// preprocessed unit) reparses incrementally instead of from scratch.
    pub fn update_text(&mut self, new_text: &str) -> bool {
        if self.text == new_text {
            return false;
        }
        let old = &self.text;
        let max = old.len().min(new_text.len());
        let mut prefix = 0;
        while prefix < max && old.as_bytes()[prefix] == new_text.as_bytes()[prefix] {
            prefix += 1;
        }
        while prefix > 0 && (!old.is_char_boundary(prefix) || !new_text.is_char_boundary(prefix)) {
            prefix -= 1;
        }
        let mut suffix = 0;
        while suffix < max - prefix
            && old.as_bytes()[old.len() - 1 - suffix] == new_text.as_bytes()[new_text.len() - 1 - suffix]
        {
            suffix += 1;
        }
        while suffix > 0
            && (!old.is_char_boundary(old.len() - suffix)
                || !new_text.is_char_boundary(new_text.len() - suffix))
        {
            suffix -= 1;
        }
        let start_byte = prefix;
        let old_end_byte = old.len() - suffix;
        let new_end_byte = new_text.len() - suffix;
        let start_point = model::byte_to_point(self, start_byte);
        let old_end_point = model::byte_to_point(self, old_end_byte);
        self.text
            .replace_range(start_byte..old_end_byte, &new_text[prefix..new_end_byte]);
        self.rebuild_line_starts();
        let new_end_point = model::byte_to_point(self, new_end_byte);
        self.tree.edit(&InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_position: start_point,
            old_end_position: old_end_point,
            new_end_position: new_end_point,
        });
        if let Some(tree) = Self::parse(&self.text, Some(&self.tree)) {
            self.tree = tree;
        }
        true
    }

    /// Byte-range text slice.
    pub fn slice(&self, range: std::ops::Range<usize>) -> &str {
        self.text.get(range).unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
/dts-v1/;

#define SOC_BASE 0x1000
#include <dt-bindings/interrupt-controller/arm-gic.h>

/ {
    #address-cells = <1>;

    intc: interrupt-controller@SOC_BASE {
        compatible = "arm,gic";
        interrupts = <GIC_SPI 0 4>;
        /* block comment
           spanning lines */
    };
};

/ {
    /delete-node/ nonexistent;
};

&{/soc/uart@1000} {
    status = "okay";
};
"#;

    fn count_errors(node: tree_sitter::Node) -> usize {
        let mut count = usize::from(node.is_error() || node.is_missing());
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                count += count_errors(child);
            }
        }
        count
    }

    #[test]
    fn parses_kernel_style_syntax_without_errors() {
        let doc = Document::open(SAMPLE, 0);
        assert_eq!(count_errors(doc.tree.root_node()), 0);
    }

    #[test]
    fn incremental_edit_keeps_tree_in_sync() {
        let mut doc = Document::open(SAMPLE, 0);
        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position { line: 7, character: 4 },
                end: tower_lsp::lsp_types::Position { line: 7, character: 4 },
            }),
            range_length: None,
            text: "/* x */ ".to_string(),
        }];
        doc.apply_changes(&changes, 1);
        assert_eq!(count_errors(doc.tree.root_node()), 0);
        assert!(doc.text.contains("/* x */ #address-cells"));
    }

    /// Requires a Linux kernel tree: run with `cargo test -- --ignored`
    #[test]
    #[ignore]
    fn parses_real_kernel_dtsi() {
        let path = "/linux-7.2.2/arch/arm64/boot/dts/qcom/lemans.dtsi";
        let Ok(text) = std::fs::read_to_string(path) else {
            panic!("kernel tree not found at {path}");
        };
        let doc = Document::open(&text, 0);
        assert_eq!(count_errors(doc.tree.root_node()), 0);
    }
}
