use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens,
};
use tree_sitter::Node;

use crate::model::{self, Analysis};
use crate::document::Document;

pub const LEGEND_TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::COMMENT,
    SemanticTokenType::STRING,
    SemanticTokenType::NUMBER,
    SemanticTokenType::KEYWORD,
    SemanticTokenType::MACRO,
    SemanticTokenType::PROPERTY,
    SemanticTokenType::CLASS,
    SemanticTokenType::ENUM,
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::DECORATOR,
];

pub const LEGEND_TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION,
    SemanticTokenModifier::DEPRECATED,
];

const COMMENT: u32 = 0;
const STRING: u32 = 1;
const NUMBER: u32 = 2;
const MACRO: u32 = 4;
const PROPERTY: u32 = 5;
const CLASS: u32 = 6;
const ENUM: u32 = 7;
const ENUM_MEMBER: u32 = 8;
const DECORATOR: u32 = 9;

const DECLARATION: u32 = 1 << 0;
const DEPRECATED: u32 = 1 << 1;

struct RawToken {
    line: u32,
    character: u32,
    length: u32,
    token_type: u32,
    token_modifiers: u32,
}

pub fn semantic_tokens(doc: &Document) -> SemanticTokens {
    let analysis = model::analyze(doc);
    let mut raw = Vec::new();
    walk(doc.tree.root_node(), doc, &analysis, &mut raw);
    raw.sort_by_key(|t| (t.line, t.character));

    let mut data = Vec::with_capacity(raw.len());
    let mut prev_line = 0;
    let mut prev_character = 0;
    for token in raw {
        let delta_line = token.line - prev_line;
        let delta_start = if delta_line == 0 {
            token.character - prev_character
        } else {
            token.character
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: token.length,
            token_type: token.token_type,
            token_modifiers_bitset: token.token_modifiers,
        });
        prev_line = token.line;
        prev_character = token.character;
    }

    SemanticTokens {
        result_id: None,
        data,
    }
}

/// Push a token span, splitting tokens that cross line boundaries (block comments, strings).
fn push_span(
    out: &mut Vec<RawToken>,
    doc: &Document,
    start: usize,
    end: usize,
    token_type: u32,
    token_modifiers: u32,
) {
    let mut s = start;
    while s < end {
        let byte = s.min(doc.text.len());
        let line = doc.line_starts.partition_point(|&line_start| line_start <= byte) - 1;
        let line_end_byte = doc
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(doc.text.len());
        let piece_end = end.min(line_end_byte);
        let piece = doc.slice(s..piece_end).trim_end_matches(['\r', '\n']);
        let length: u32 = piece.chars().map(|c| c.len_utf16() as u32).sum();
        if length > 0 {
            let character = model::byte_to_position(doc, s).character;
            out.push(RawToken {
                line: line as u32,
                character,
                length,
                token_type,
                token_modifiers,
            });
        }
        if piece_end <= s {
            break;
        }
        s = piece_end;
    }
}

/// Color the leading directive keyword of a node (`/dts-v1/`, `#ifdef`, ...).
fn directive(
    out: &mut Vec<RawToken>,
    doc: &Document,
    node: Node,
    token_type: u32,
    token_modifiers: u32,
) {
    let head_end = (node.start_byte() + 24).min(node.end_byte());
    let head = doc.slice(node.start_byte()..head_end);
    if let Some(word) = head.split_whitespace().next() {
        let word = word.trim_end_matches(';');
        if !word.is_empty() {
            push_span(
                out,
                doc,
                node.start_byte(),
                node.start_byte() + word.len(),
                token_type,
                token_modifiers,
            );
        }
    }
}

/// Emit a `name@address` token as two tokens: the name part and the hex unit
/// address as a number. The `@` itself stays uncolored.
fn push_name_and_address(
    out: &mut Vec<RawToken>,
    doc: &Document,
    name: Node,
    token_type: u32,
    token_modifiers: u32,
) {
    let text = model::node_text(doc, name);
    match text.find('@') {
        Some(at) => {
            let at_byte = name.start_byte() + at;
            push_span(out, doc, name.start_byte(), at_byte, token_type, token_modifiers);
            push_span(
                out,
                doc,
                at_byte + 1,
                name.end_byte(),
                NUMBER,
                token_modifiers,
            );
        }
        None => push_span(
            out,
            doc,
            name.start_byte(),
            name.end_byte(),
            token_type,
            token_modifiers,
        ),
    }
}

fn recurse(
    node: Node,
    doc: &Document,
    analysis: &Analysis,
    out: &mut Vec<RawToken>,
    skip: Option<usize>,
) {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            if Some(child.id()) == skip {
                continue;
            }
            walk(child, doc, analysis, out);
        }
    }
}

fn walk(node: Node, doc: &Document, analysis: &Analysis, out: &mut Vec<RawToken>) {
    match node.kind() {
        "comment" => push_span(out, doc, node.start_byte(), node.end_byte(), COMMENT, 0),
        "string_literal" | "preproc_string_literal" | "system_lib_string" | "bytestring" => {
            push_span(out, doc, node.start_byte(), node.end_byte(), STRING, 0)
        }
        "int_literal" | "char_literal" | "preproc_number_literal" | "preproc_char_literal" => {
            push_span(out, doc, node.start_byte(), node.end_byte(), NUMBER, 0)
        }
        // Label tokens include the trailing ':'.
        "label" => push_span(
            out,
            doc,
            node.start_byte(),
            node.end_byte().saturating_sub(1),
            ENUM,
            DECLARATION,
        ),
        "reference" => push_span(out, doc, node.start_byte(), node.end_byte(), ENUM_MEMBER, 0),
        "preproc_identifier" | "preproc_function_call" => {
            push_span(out, doc, node.start_byte(), node.end_byte(), MACRO, 0)
        }
        "property" => {
            if let Some(delete) = model::find_child(node, "delete_property") {
                directive(out, doc, delete, DECORATOR, DEPRECATED);
                if let Some(name) = delete.child_by_field_name("name") {
                    push_span(out, doc, name.start_byte(), name.end_byte(), PROPERTY, DEPRECATED);
                }
                return;
            }
            let modifiers = if analysis.dead_properties.contains(&node.id()) {
                DEPRECATED
            } else {
                0
            };
            if let Some(name) = node.child_by_field_name("name") {
                push_span(out, doc, name.start_byte(), name.end_byte(), PROPERTY, modifiers);
            }
            recurse(node, doc, analysis, out, None);
        }
        "prop_data" | "array" | "int_binary_op" | "int_unary_op" | "int_ternary_op"
        | "int_parenthesized_expression" | "preproc_binary_expression"
        | "preproc_unary_expression" | "preproc_parenthesized_expression" => {
            recurse(node, doc, analysis, out, None)
        }
        "bits" => {
            // "/bits/" keyword only; the size literal is colored by recursion.
            push_span(out, doc, node.start_byte(), node.start_byte() + 6, DECORATOR, 0);
            recurse(node, doc, analysis, out, None);
        }
        "incbin" => {
            directive(out, doc, node, DECORATOR, 0);
            recurse(node, doc, analysis, out, None);
        }
        "header" => directive(out, doc, node, DECORATOR, 0),
        "memreserve" => {
            // Labels may precede the directive, so locate it in the text.
            let text = model::node_text(doc, node);
            if let Some(offset) = text.find("/memreserve/") {
                push_span(
                    out,
                    doc,
                    node.start_byte() + offset,
                    node.start_byte() + offset + 12,
                    DECORATOR,
                    0,
                );
            }
            recurse(node, doc, analysis, out, None);
        }
        "include" => {
            directive(out, doc, node, DECORATOR, 0);
            recurse(node, doc, analysis, out, None);
        }
        "subnode" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_name_and_address(out, doc, name, CLASS, 0);
            }
            recurse(node, doc, analysis, out, None);
        }
        "delete_node" => {
            directive(out, doc, node, DECORATOR, DEPRECATED);
            if let Some(name) = node.child_by_field_name("name") {
                push_name_and_address(out, doc, name, CLASS, DEPRECATED);
            }
        }
        "delete_node_ref" => {
            directive(out, doc, node, DECORATOR, DEPRECATED);
            if let Some(reference) = model::find_child(node, "reference") {
                push_span(
                    out,
                    doc,
                    reference.start_byte(),
                    reference.end_byte(),
                    ENUM_MEMBER,
                    DEPRECATED,
                );
            }
        }
        "omit_node" => {
            directive(out, doc, node, DECORATOR, 0);
            recurse(node, doc, analysis, out, None);
        }
        "omit_node_ref" => {
            directive(out, doc, node, DECORATOR, 0);
            recurse(node, doc, analysis, out, None);
        }
        "preproc_include" => {
            directive(out, doc, node, MACRO, 0);
            recurse(node, doc, analysis, out, None);
        }
        "preproc_def" => {
            directive(out, doc, node, MACRO, 0);
            if let Some(name) = node.child_by_field_name("name") {
                push_span(out, doc, name.start_byte(), name.end_byte(), MACRO, 0);
            }
        }
        "preproc_function_def" => {
            directive(out, doc, node, MACRO, 0);
            if let Some(name) = node.child_by_field_name("name") {
                push_span(out, doc, name.start_byte(), name.end_byte(), MACRO, 0);
            }
        }
        "preproc_undef" => {
            directive(out, doc, node, MACRO, 0);
            recurse(node, doc, analysis, out, None);
        }
        "preproc_call" => directive(out, doc, node, MACRO, 0),
        "preproc_if" | "preproc_ifdef" | "preproc_elif" | "preproc_elifdef" | "preproc_else"
        | "preproc_if_in_node_def" | "preproc_ifdef_in_node_def" | "preproc_elif_in_node_def"
        | "preproc_elifdef_in_node_def" | "preproc_else_in_node_def"
        | "preproc_ifdef_in_array" | "preproc_elif_in_array" | "preproc_elifdef_in_array"
        | "preproc_else_in_array" => {
            directive(out, doc, node, MACRO, 0);
            recurse(node, doc, analysis, out, None);
        }
        _ => recurse(node, doc, analysis, out, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    /// Decode relative semantic tokens back to absolute positions.
    fn decoded(doc: &Document) -> Vec<(u32, u32, u32, u32, u32)> {
        let tokens = semantic_tokens(doc);
        let mut out = Vec::new();
        let mut line = 0;
        let mut character = 0;
        for t in &tokens.data {
            line += t.delta_line;
            character = if t.delta_line == 0 {
                character + t.delta_start
            } else {
                t.delta_start
            };
            out.push((line, character, t.length, t.token_type, t.token_modifiers_bitset));
        }
        out
    }

    fn line_of(doc: &Document, needle: &str) -> usize {
        doc.text
            .lines()
            .position(|l| l.contains(needle))
            .expect("needle not found")
    }

    const OVERLAY: &str = "/dts-v1/;\n\n/ {\n    intc: interrupt-controller@1000 {\n        compatible = \"arm,gic\";\n        reg = <0x1000 0x100>;\n    };\n};\n\n&intc {\n    reg = <0x1000 0x200>;\n    status = \"okay\";\n};\n";

    #[test]
    fn overridden_base_property_is_marked_deprecated() {
        let doc = Document::open(OVERLAY, 0);
        let base_line = line_of(&doc, "0x1000 0x100") as u32;
        let ext_line = line_of(&doc, "0x1000 0x200") as u32;
        let tokens = decoded(&doc);

        let base_tokens: Vec<_> = tokens.iter().filter(|t| t.0 == base_line).collect();
        assert!(
            base_tokens.iter().any(|t| t.4 & DEPRECATED != 0),
            "base reg property should carry the deprecated modifier"
        );
        assert!(
            tokens
                .iter()
                .filter(|t| t.0 == ext_line)
                .all(|t| t.4 & DEPRECATED == 0),
            "extension reg property must not be deprecated"
        );
    }

    #[test]
    fn delete_property_target_is_marked_deprecated() {
        let doc = Document::open(
            "/dts-v1/;\n\n/ {\n    n {\n        a = <1>;\n        /delete-property/ a;\n    };\n};\n",
            0,
        );
        let delete_line = line_of(&doc, "/delete-property/") as u32;
        let tokens = decoded(&doc);
        let line_tokens: Vec<_> = tokens.iter().filter(|t| t.0 == delete_line).collect();
        assert!(
            line_tokens
                .iter()
                .any(|t| t.3 == DECORATOR && t.4 & DEPRECATED != 0)
        );
        assert!(
            line_tokens
                .iter()
                .any(|t| t.3 == PROPERTY && t.4 & DEPRECATED != 0)
        );
    }

    #[test]
    fn directives_use_decorator_token_type() {
        let doc = Document::open("/dts-v1/;\n\n/ {\n    a = <1>;\n};\n", 0);
        let tokens = decoded(&doc);
        assert!(tokens.iter().any(|t| t.3 == DECORATOR));
    }

    #[test]
    fn unit_address_is_highlighted_as_number() {
        let doc = Document::open(OVERLAY, 0);
        let node_line = line_of(&doc, "interrupt-controller@1000") as u32;
        let tokens = decoded(&doc);
        let line_tokens: Vec<_> = tokens.iter().filter(|t| t.0 == node_line).collect();
        let class = line_tokens.iter().find(|t| t.3 == CLASS).expect("no class token");
        let number = line_tokens.iter().find(|t| t.3 == NUMBER).expect("no number token");
        // The class token must stop before the '@', the number token starts
        // after it, leaving the '@' uncolored.
        assert_eq!(class.1 + class.2, number.1 - 1);
        assert_eq!(number.2, "1000".len() as u32);
    }

    #[test]
    fn references_are_emitted_as_enum_member_tokens() {
        let doc = Document::open(
            "/dts-v1/;\n\n/ {\n    a {\n        clocks = <&clk 0>;\n    };\n};\n",
            0,
        );
        let tokens = decoded(&doc);
        assert!(tokens.iter().any(|t| t.3 == ENUM_MEMBER));
    }
}
