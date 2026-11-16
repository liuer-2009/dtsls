use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens,
};
use tree_sitter::Node;

use crate::document::Document;
use crate::util;

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
];

pub const LEGEND_TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[SemanticTokenModifier::DECLARATION];

const COMMENT: u32 = 0;
const STRING: u32 = 1;
const NUMBER: u32 = 2;
const KEYWORD: u32 = 3;
const MACRO: u32 = 4;
const PROPERTY: u32 = 5;
const CLASS: u32 = 6;
const ENUM: u32 = 7;
const ENUM_MEMBER: u32 = 8;

const DECLARATION: u32 = 1 << 0;

struct RawToken {
    line: u32,
    character: u32,
    length: u32,
    token_type: u32,
    token_modifiers: u32,
}

pub fn semantic_tokens(doc: &Document) -> SemanticTokens {
    let mut raw = Vec::new();
    walk(doc.tree.root_node(), doc, &mut raw);
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
        let byte = s.min(doc.rope.len_bytes());
        let line = doc.rope.byte_to_line(byte);
        let line_end_byte = if line + 1 < doc.rope.len_lines() {
            doc.rope.line_to_byte(line + 1)
        } else {
            doc.rope.len_bytes()
        };
        let piece_end = end.min(line_end_byte);
        let piece = doc.slice(s..piece_end).trim_end_matches(['\r', '\n']);
        let length: u32 = piece.chars().map(|c| c.len_utf16() as u32).sum();
        if length > 0 {
            let character = util::byte_to_position(&doc.rope, s).character;
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

fn directive(out: &mut Vec<RawToken>, doc: &Document, node: Node, token_type: u32) {
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
                0,
            );
        }
    }
}

fn recurse(node: Node, doc: &Document, out: &mut Vec<RawToken>, skip: Option<usize>) {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if Some(child.id()) == skip {
                continue;
            }
            walk(child, doc, out);
        }
    }
}

fn walk(node: Node, doc: &Document, out: &mut Vec<RawToken>) {
    match node.kind() {
        "comment" => push_span(out, doc, node.start_byte(), node.end_byte(), COMMENT, 0),
        "string_literal" | "byte_string_literal" | "system_lib_string" => {
            push_span(out, doc, node.start_byte(), node.end_byte(), STRING, 0)
        }
        "integer_literal" | "unit_address" => {
            push_span(out, doc, node.start_byte(), node.end_byte(), NUMBER, 0)
        }
        "reference" => push_span(out, doc, node.start_byte(), node.end_byte(), ENUM_MEMBER, 0),
        "property" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_span(out, doc, name.start_byte(), name.end_byte(), PROPERTY, 0);
            }
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i)
                    && child.kind() == "/bits/" {
                        push_span(out, doc, child.start_byte(), child.end_byte(), KEYWORD, 0);
                        break;
                    }
            }
            recurse(node, doc, out, None);
        }
        "node" => {
            for label in node
                .children_by_field_name("label", &mut node.walk())
            {
                push_span(out, doc, label.start_byte(), label.end_byte(), ENUM, DECLARATION);
            }
            let mut skip = None;
            if let Some(name) = node.child_by_field_name("name") {
                if name.kind() == "reference" {
                    push_span(out, doc, name.start_byte(), name.end_byte(), ENUM_MEMBER, 0);
                    skip = Some(name.id());
                } else {
                    push_span(out, doc, name.start_byte(), name.end_byte(), CLASS, 0);
                }
            }
            recurse(node, doc, out, skip);
        }
        "preproc_include" => {
            directive(out, doc, node, MACRO);
            recurse(node, doc, out, None);
        }
        "dtsi_include" => {
            directive(out, doc, node, KEYWORD);
            recurse(node, doc, out, None);
        }
        "preproc_def" | "preproc_function_def" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_span(out, doc, name.start_byte(), name.end_byte(), MACRO, 0);
            }
        }
        "preproc_undef" => {
            directive(out, doc, node, MACRO);
            if let Some(name) = node.child_by_field_name("name") {
                push_span(out, doc, name.start_byte(), name.end_byte(), MACRO, 0);
            }
        }
        "preproc_if" | "preproc_ifdef" | "preproc_elif" | "preproc_elifdef" | "preproc_else" => {
            directive(out, doc, node, MACRO);
            recurse(node, doc, out, None);
        }
        "file_version" | "plugin" => directive(out, doc, node, KEYWORD),
        "memory_reservation" => {
            directive(out, doc, node, KEYWORD);
            recurse(node, doc, out, None);
        }
        "delete_node" => {
            directive(out, doc, node, KEYWORD);
            recurse(node, doc, out, None);
        }
        "delete_property" => {
            directive(out, doc, node, KEYWORD);
            if let Some(name) = node.child_by_field_name("name") {
                push_span(out, doc, name.start_byte(), name.end_byte(), PROPERTY, 0);
            }
        }
        "omit_if_no_ref" => {
            directive(out, doc, node, KEYWORD);
            recurse(node, doc, out, None);
        }
        "incbin" => {
            directive(out, doc, node, KEYWORD);
            recurse(node, doc, out, None);
        }
        _ => recurse(node, doc, out, None),
    }
}
