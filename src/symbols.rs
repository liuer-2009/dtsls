// SPDX-License-Identifier: AGPL-3.0-only
// DocumentSymbol::deprecated is required by lsp-types 0.94 but marked deprecated.
#![allow(deprecated)]

use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};
use tree_sitter::Node;

use crate::document::Document;
use crate::model;

pub fn document_symbols(doc: &Document) -> Vec<DocumentSymbol> {
    let root = doc.tree.root_node();
    let mut symbols = Vec::new();
    for i in 0..root.child_count() {
        let Some(child) = root.child(i as u32) else { continue };
        match child.kind() {
            "devicetree" => symbols.push(devicetree_symbol(child, doc)),
            "memreserve" => symbols.push(memreserve_symbol(child, doc)),
            "preproc_def" | "preproc_function_def" => {
                if let Some(symbol) = define_symbol(child, doc) {
                    symbols.push(symbol);
                }
            }
            _ => {}
        }
    }
    symbols
}

fn position_range(doc: &Document, node: Node) -> tower_lsp::lsp_types::Range {
    tower_lsp::lsp_types::Range {
        start: model::byte_to_position(doc, node.start_byte()),
        end: model::byte_to_position(doc, node.end_byte()),
    }
}

/// `/ { ... }` (root) or `&label { ... }` (same-file extension).
fn devicetree_symbol(devicetree: Node, doc: &Document) -> DocumentSymbol {
    let reference = model::find_child(devicetree, "reference");
    let (name, child_path) = match reference {
        Some(reference) => {
            let name = model::node_text(doc, reference).to_string();
            let path = name.clone();
            (name, path)
        }
        None => ("/".to_string(), "/".to_string()),
    };
    let detail = child_path.clone();

    let mut children = Vec::new();
    if let Some(body) = model::node_def_of(devicetree) {
        collect_block(body, doc, &child_path, &mut children);
    }

    DocumentSymbol {
        name,
        detail: Some(detail),
        kind: SymbolKind::OBJECT,
        range: position_range(doc, devicetree),
        selection_range: position_range(doc, reference.unwrap_or(devicetree)),
        children: Some(children),
        tags: None,
        deprecated: None,
    }
}

fn subnode_symbol(subnode: Node, doc: &Document, path: &str, out: &mut Vec<DocumentSymbol>) {
    let Some(name_node) = subnode.child_by_field_name("name") else {
        // `omit_node` wraps a real subnode; `delete_node` statements are not
        // outline-worthy.
        if let Some(omit) = model::find_child(subnode, "omit_node")
            && let Some(inner) = model::find_child(omit, "subnode") {
                subnode_symbol(inner, doc, path, out);
            }
        return;
    };
    let name = model::node_text(doc, name_node).to_string();
    let child_path = if path.ends_with('/') {
        format!("{path}{name}")
    } else {
        format!("{path}/{name}")
    };
    let detail = child_path.clone();

    let mut children = Vec::new();
    if let Some(body) = model::node_def_of(subnode) {
        collect_block(body, doc, &child_path, &mut children);
    }

    out.push(DocumentSymbol {
        name,
        detail: Some(detail),
        kind: SymbolKind::OBJECT,
        range: position_range(doc, subnode),
        selection_range: position_range(doc, name_node),
        children: Some(children),
        tags: None,
        deprecated: None,
    });
}

/// Collect symbols from a node_def body (or a #if container within one).
fn collect_block(container: Node, doc: &Document, path: &str, out: &mut Vec<DocumentSymbol>) {
    for i in 0..container.child_count() {
        let Some(child) = container.child(i as u32) else { continue };
        match child.kind() {
            "property" => property_symbol(child, doc, out),
            "subnode" => subnode_symbol(child, doc, path, out),
            "preproc_if_in_node_def" | "preproc_ifdef_in_node_def" => {
                for j in 0..child.child_count() {
                    if let Some(inner) = child.child(j as u32) {
                        if inner.kind().contains("else") || inner.kind().contains("elif") {
                            continue;
                        }
                        collect_block(inner, doc, path, out);
                    }
                }
            }
            "preproc_def" | "preproc_function_def" => {
                if let Some(symbol) = define_symbol(child, doc) {
                    out.push(symbol);
                }
            }
            _ => {}
        }
    }
}

fn property_symbol(property: Node, doc: &Document, out: &mut Vec<DocumentSymbol>) {
    if model::find_child(property, "delete_property").is_some() {
        return;
    }
    let Some(name_node) = property.child_by_field_name("name") else {
        return;
    };
    out.push(DocumentSymbol {
        name: model::node_text(doc, name_node).to_string(),
        detail: Some(value_summary(property)),
        kind: SymbolKind::FIELD,
        range: position_range(doc, property),
        selection_range: position_range(doc, name_node),
        children: Some(Vec::new()),
        tags: None,
        deprecated: None,
    });
}

fn value_summary(property: Node) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for i in 0..property.child_count() {
        let Some(data) = property.child(i as u32) else { continue };
        if data.kind() != "prop_data" {
            continue;
        }
        for j in 0..data.child_count() {
            let Some(value) = data.child(j as u32) else { continue };
            let label = match value.kind() {
                "bits" => "/bits/",
                "array" => "cells",
                "string_literal" => "string",
                "bytestring" => "bytes",
                "reference" => "phandle",
                "incbin" => "incbin",
                "preproc_identifier" | "preproc_function_call" => "macro",
                _ => continue,
            };
            if !parts.contains(&label) {
                parts.push(label);
            }
        }
    }
    if parts.is_empty() {
        "boolean".to_string()
    } else {
        parts.join(", ")
    }
}

fn define_symbol(define: Node, doc: &Document) -> Option<DocumentSymbol> {
    let name_node = define.child_by_field_name("name")?;
    let detail = match define.kind() {
        "preproc_function_def" => "#define (function-like macro)",
        _ => "#define",
    };
    Some(DocumentSymbol {
        name: format!("#define {}", model::node_text(doc, name_node)),
        detail: Some(detail.to_string()),
        kind: SymbolKind::CONSTANT,
        range: position_range(doc, define),
        selection_range: position_range(doc, name_node),
        children: Some(Vec::new()),
        tags: None,
        deprecated: None,
    })
}

fn memreserve_symbol(memreserve: Node, doc: &Document) -> DocumentSymbol {
    DocumentSymbol {
        name: "/memreserve/".to_string(),
        detail: Some("reserved memory".to_string()),
        kind: SymbolKind::VARIABLE,
        range: position_range(doc, memreserve),
        selection_range: position_range(doc, memreserve),
        children: Some(Vec::new()),
        tags: None,
        deprecated: None,
    }
}
