// DocumentSymbol::deprecated is required by lsp-types 0.94 but marked deprecated.
#![allow(deprecated)]

use std::ops::Range;

use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};
use tree_sitter::Node;

use crate::document::Document;
use crate::util;

pub fn document_symbols(doc: &Document) -> Vec<DocumentSymbol> {
    let root = doc.tree.root_node();
    let mut symbols = Vec::new();
    for i in 0..root.child_count() {
        if let Some(child) = root.child(i) {
            collect(child, doc, "", &mut symbols);
        }
    }
    symbols
}

fn byte_range(node: Node) -> Range<usize> {
    node.start_byte()..node.end_byte()
}

fn position_range(doc: &Document, node: Node) -> tower_lsp::lsp_types::Range {
    tower_lsp::lsp_types::Range {
        start: util::byte_to_position(&doc.rope, node.start_byte()),
        end: util::byte_to_position(&doc.rope, node.end_byte()),
    }
}

fn collect(node: Node, doc: &Document, path: &str, out: &mut Vec<DocumentSymbol>) {
    match node.kind() {
        "node" => {
            let name = match node.child_by_field_name("name") {
                Some(name_node) => doc.slice(byte_range(name_node)).to_string(),
                None => "/".to_string(),
            };
            let mut unit_address = None;
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i)
                    && child.kind() == "unit_address" {
                        unit_address = Some(doc.slice(byte_range(child)).to_string());
                    }
            }
            let display = match &unit_address {
                Some(addr) => format!("{name}@{addr}"),
                None => name.clone(),
            };
            let child_path = if name == "/" {
                String::from("/")
            } else if path.ends_with('/') {
                format!("{path}{display}")
            } else {
                format!("{path}/{display}")
            };
            let detail = if child_path.is_empty() {
                "/".to_string()
            } else {
                child_path.clone()
            };

            let mut children = Vec::new();
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    match child.kind() {
                        "node" | "property" | "preproc_def" | "preproc_function_def"
                        | "memory_reservation" => collect(child, doc, &child_path, &mut children),
                        _ => {}
                    }
                }
            }

            out.push(DocumentSymbol {
                name: display,
                detail: Some(detail),
                kind: SymbolKind::OBJECT,
                range: position_range(doc, node),
                selection_range: position_range(doc, node),
                children: Some(children),
                deprecated: None,
                tags: None,
            });
        }
        "property" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return;
            };
            let mut cursor = node.walk();
            let mut parts: Vec<&str> = Vec::new();
            for value in node.children_by_field_name("value", &mut cursor) {
                let label = match value.kind() {
                    "string_literal" => "string",
                    "byte_string_literal" => "bytes",
                    "integer_cells" => "cells",
                    "reference" => "phandle",
                    "incbin" => "incbin",
                    _ => continue,
                };
                if !parts.contains(&label) {
                    parts.push(label);
                }
            }
            let detail = if parts.is_empty() {
                "boolean".to_string()
            } else {
                parts.join(", ")
            };
            out.push(DocumentSymbol {
                name: doc.slice(byte_range(name_node)).to_string(),
                detail: Some(detail),
                kind: SymbolKind::FIELD,
                range: position_range(doc, node),
                selection_range: position_range(doc, name_node),
                children: Some(Vec::new()),
                deprecated: None,
                tags: None,
            });
        }
        "preproc_def" | "preproc_function_def" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return;
            };
            let detail = match node.kind() {
                "preproc_function_def" => "#define (function-like macro)",
                _ => "#define",
            };
            out.push(DocumentSymbol {
                name: format!("#define {}", doc.slice(byte_range(name_node))),
                detail: Some(detail.to_string()),
                kind: SymbolKind::CONSTANT,
                range: position_range(doc, node),
                selection_range: position_range(doc, name_node),
                children: Some(Vec::new()),
                deprecated: None,
                tags: None,
            });
        }
        "memory_reservation" => {
            out.push(DocumentSymbol {
                name: "/memreserve/".to_string(),
                detail: Some("reserved memory".to_string()),
                kind: SymbolKind::VARIABLE,
                range: position_range(doc, node),
                selection_range: position_range(doc, node),
                children: Some(Vec::new()),
                deprecated: None,
                tags: None,
            });
        }
        _ => {}
    }
}
