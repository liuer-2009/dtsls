use ropey::Rope;
use tower_lsp::lsp_types::TextDocumentContentChangeEvent;
use tree_sitter::{InputEdit, Parser, Point, Tree};

use crate::util;

pub struct Document {
    pub rope: Rope,
    pub text: String,
    pub tree: Tree,
    pub version: i32,
}

impl Document {
    pub fn open(text: &str, version: i32) -> Self {
        let rope = Rope::from_str(text);
        let tree = Self::parse(&rope, None).unwrap_or_else(Self::empty_tree);
        Self {
            rope,
            text: text.to_string(),
            tree,
            version,
        }
    }

    fn empty_tree() -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_devicetree2::LANGUAGE.into())
            .expect("devicetree grammar should load");
        parser.parse("", None).expect("parsing empty text always succeeds")
    }

    /// Incremental-friendly parse over the rope, reusing the old tree when given.
    fn parse(rope: &Rope, old_tree: Option<&Tree>) -> Option<Tree> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_devicetree2::LANGUAGE.into())
            .ok()?;
        let mut chunk: (&str, usize) = ("", usize::MAX);
        let mut callback = |byte: usize, _point: Point| -> &[u8] {
            if byte >= rope.len_bytes() {
                return &[];
            }
            if !(byte >= chunk.1 && byte < chunk.1 + chunk.0.len()) {
                let (text, byte_start, _, _) = rope.chunk_at_byte(byte);
                chunk = (text, byte_start);
            }
            &chunk.0.as_bytes()[byte - chunk.1..]
        };
        parser.parse_with_options(&mut callback, old_tree, None)
    }

    pub fn apply_changes(
        &mut self,
        changes: &[TextDocumentContentChangeEvent],
        version: i32,
    ) {
        for change in changes {
            match change.range {
                Some(range) => {
                    let start_byte = util::position_to_byte(&self.rope, range.start);
                    let old_end_byte = util::position_to_byte(&self.rope, range.end);
                    let start_point = util::byte_to_point(&self.rope, start_byte);
                    let old_end_point = util::byte_to_point(&self.rope, old_end_byte);

                    let start_char = self.rope.byte_to_char(start_byte);
                    let old_end_char = self.rope.byte_to_char(old_end_byte);
                    self.rope.remove(start_char..old_end_char);
                    self.rope.insert(start_char, &change.text);
                    self.text.replace_range(start_byte..old_end_byte, &change.text);

                    let new_end_byte = start_byte + change.text.len();
                    let new_end_point = util::byte_to_point(&self.rope, new_end_byte);
                    self.tree.edit(&InputEdit {
                        start_byte,
                        old_end_byte,
                        new_end_byte,
                        start_position: start_point,
                        old_end_position: old_end_point,
                        new_end_position: new_end_point,
                    });
                    if let Some(tree) = Self::parse(&self.rope, Some(&self.tree)) {
                        self.tree = tree;
                    }
                }
                None => {
                    self.rope = Rope::from_str(&change.text);
                    self.text = change.text.clone();
                    self.tree = Self::parse(&self.rope, None).unwrap_or_else(Self::empty_tree);
                }
            }
        }
        self.version = version;
    }

    /// Byte-range text slice, from the mirrored plain-text copy.
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
