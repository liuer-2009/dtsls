// SPDX-License-Identifier: AGPL-3.0-only
//! The one toolkit for walking DTS trees and raw text.
//!
//! Every module (diagnostics, completion, hints, phandle checks, unit
//! collection) reads the tree through these helpers, so node lookup, path
//! computation, reference resolution and cell reading exist exactly once.

use std::collections::{HashMap, HashSet};

use tower_lsp::lsp_types::Position;
use tree_sitter::Node;

use crate::document::Document;

// ---------------------------------------------------------------- node basics

/// Text of a syntax node.
pub fn node_text<'a>(doc: &'a Document, node: Node) -> &'a str {
    doc.slice(node.start_byte()..node.end_byte())
}

pub fn find_child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    (0..node.child_count()).find_map(|i| node.child(i as u32).filter(|child| child.kind() == kind))
}

/// First descendant (depth-first) with the given kind.
pub fn find_descendant<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }
    (0..node.child_count()).find_map(|i| {
        let child = node.child(i as u32)?;
        find_descendant(child, kind)
    })
}

/// The `{ ... }` body of a `subnode` or `devicetree`.
pub fn node_def_of<'a>(node: Node<'a>) -> Option<Node<'a>> {
    find_child(node, "node_def")
}

/// Body of a node: a `node_def` itself, or the one under a `devicetree`.
pub fn body_of<'a>(node: Node<'a>) -> Option<Node<'a>> {
    match node.kind() {
        "devicetree" => find_child(node, "node_def"),
        "node_def" => Some(node),
        _ => node_def_of(node),
    }
}

/// The subnode (or root devicetree) that directly contains this property.
pub fn owning_node(prop: Node) -> Option<Node> {
    let mut cur = prop.parent();
    while let Some(n) = cur {
        if matches!(n.kind(), "subnode" | "devicetree") {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

/// Property named `name` directly inside a node's body. `/delete-property/`
/// children are skipped: they remove a property, they do not define one.
pub fn find_property<'a>(doc: &Document, node: Node<'a>, name: &str) -> Option<Node<'a>> {
    let body = match node.kind() {
        "subnode" | "devicetree" => body_of(node)?,
        _ => node,
    };
    (0..body.child_count()).find_map(|i| {
        let child = body.child(i as u32)?;
        if child.kind() != "property" || find_child(child, "delete_property").is_some() {
            return None;
        }
        child
            .child_by_field_name("name")
            .is_some_and(|n| node_text(doc, n) == name)
            .then_some(child)
    })
}

// ---------------------------------------------------------------- positions

/// Convert an LSP position (UTF-16 line/character) to a byte offset.
/// Characters past the end of the line clamp to the line break.
pub fn position_to_byte(doc: &Document, pos: Position) -> usize {
    let line_idx = (pos.line as usize).min(doc.line_starts.len() - 1);
    let line_start = doc.line_starts[line_idx];
    let line_end = doc
        .line_starts
        .get(line_idx + 1)
        .copied()
        .unwrap_or(doc.text.len());
    let mut utf16 = 0usize;
    let mut byte = line_start;
    for c in doc.text[line_start..line_end].chars() {
        if utf16 >= pos.character as usize {
            break;
        }
        utf16 += c.len_utf16();
        byte += c.len_utf8();
    }
    byte
}

/// Convert a byte offset to an LSP position (UTF-16 line/character).
pub fn byte_to_position(doc: &Document, byte: usize) -> Position {
    let byte = byte.min(doc.text.len());
    let line_idx = doc.line_starts.partition_point(|&start| start <= byte) - 1;
    let character: usize = doc.text[doc.line_starts[line_idx]..byte]
        .chars()
        .map(char::len_utf16)
        .sum();
    Position {
        line: line_idx as u32,
        character: character as u32,
    }
}

/// Convert a byte offset to a tree-sitter point (row = line, column = byte offset within line).
pub fn byte_to_point(doc: &Document, byte: usize) -> tree_sitter::Point {
    let byte = byte.min(doc.text.len());
    let line_idx = doc.line_starts.partition_point(|&start| start <= byte) - 1;
    tree_sitter::Point {
        row: line_idx,
        column: byte - doc.line_starts[line_idx],
    }
}

/// Byte offset where the line containing `byte` starts.
pub fn line_start_of(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())]
        .rfind('\n')
        .map_or(0, |pos| pos + 1)
}

// ------------------------------------------------------- paths & references

/// A `&label` or `&{/path}` reference in source form.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RefKind<'a> {
    Label(&'a str),
    Path(&'a str),
}

/// What a `reference` node names.
pub fn reference_of<'a>(doc: &'a Document, node: Node<'a>) -> Option<RefKind<'a>> {
    if let Some(label_ref) = find_child(node, "label_ref") {
        return node_text(doc, label_ref).strip_prefix('&').map(RefKind::Label);
    }
    let path_ref = find_child(node, "path_ref")?;
    Some(RefKind::Path(node_text(doc, path_ref)))
}

/// Display path of a node without fragment resolution: a label inside
/// `&intc { dev { } }` shows up as `/&intc/dev`.
pub fn fallback_path(doc: &Document, mut node: Node) -> String {
    let mut parts: Vec<String> = Vec::new();
    loop {
        match node.kind() {
            "subnode" => {
                if let Some(name) = node.child_by_field_name("name") {
                    parts.push(node_text(doc, name).to_string());
                }
            }
            "devicetree" => {
                if let Some(reference) = find_child(node, "reference") {
                    parts.push(node_text(doc, reference).to_string());
                }
                break;
            }
            _ => {}
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => break,
        }
    }
    parts.reverse();
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

/// The merged view of one document: labels, canonical paths (`&label { }`
/// fragments map onto the nodes they extend, so the whole document looks like
/// one tree), reference resolution and path lookup. Paths are memoized.
pub struct Tree<'a> {
    doc: &'a Document,
    labels: HashMap<&'a str, Node<'a>>,
    paths: HashMap<usize, String>,
}

impl<'a> Tree<'a> {
    pub fn new(doc: &'a Document) -> Self {
        let mut labels = HashMap::new();
        collect_labels(doc.tree.root_node(), doc, &mut labels);
        Self {
            doc,
            labels,
            paths: HashMap::new(),
        }
    }

    pub fn label(&self, name: &str) -> Option<Node<'a>> {
        self.labels.get(name).copied()
    }

    /// All labels as `(name, labeled node)`.
    pub fn labels(&self) -> impl Iterator<Item = (&'a str, Node<'a>)> {
        self.labels.iter().map(|(name, node)| (*name, *node))
    }

    /// The root body: the first `devicetree` without a reference, else a
    /// top-level `node_def`.
    pub fn root(&self) -> Option<Node<'a>> {
        let root = self.doc.tree.root_node();
        (0..root.child_count()).find_map(|i| {
            let child = root.child(i as u32)?;
            match child.kind() {
                "devicetree" if find_child(child, "reference").is_none() => Some(child),
                "node_def" => Some(child),
                _ => None,
            }
        })
    }

    /// Resolve a reference node to the node it names.
    pub fn resolve(&self, reference: Node<'a>) -> Option<Node<'a>> {
        match reference_of(self.doc, reference)? {
            RefKind::Label(label) => self.label(label),
            RefKind::Path(path) => self.resolve_path(path),
        }
    }

    /// Resolve `&{/soc/foo}` from the root.
    pub fn resolve_path(&self, path_ref: &str) -> Option<Node<'a>> {
        self.node_at(path_ref.strip_prefix("&{")?.strip_suffix('}')?)
    }

    /// Node at a canonical path like `/soc/serial@1000`.
    pub fn node_at(&self, path: &str) -> Option<Node<'a>> {
        let mut current = self.root()?;
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            let body = body_of(current)?;
            current = (0..body.child_count())
                .find_map(|i| {
                    let candidate = body.child(i as u32)?;
                    (candidate.kind() == "subnode"
                        && candidate
                            .child_by_field_name("name")
                            .is_some_and(|n| node_text(self.doc, n) == segment))
                    .then_some(candidate)
                })?;
        }
        Some(current)
    }

    /// The node that encloses `node` once fragments are merged: a fragment's
    /// parent is the node its `&label` / `&{/path}` resolves to.
    pub fn parent_of(&self, node: Node<'a>) -> Option<Node<'a>> {
        let owning = owning_node(node)?;
        match owning.kind() {
            "devicetree" => match find_child(owning, "reference") {
                Some(reference) => self.resolve(reference),
                None => Some(owning),
            },
            _ => Some(owning),
        }
    }

    /// Canonical path of a node. Returns `None` when a fragment on the way
    /// cannot be resolved.
    pub fn path_of(&mut self, node: Node<'a>) -> Option<String> {
        if let Some(path) = self.paths.get(&node.id()) {
            return Some(path.clone());
        }
        let path = match node.kind() {
            "subnode" => {
                let name = node_text(self.doc, node.child_by_field_name("name")?).to_string();
                match self.parent_of(node) {
                    Some(parent) => Some(format!("{}/{}", self.path_of(parent)?, name)),
                    None => Some(format!("/{name}")),
                }
            }
            "devicetree" => match find_child(node, "reference") {
                Some(reference) => self.path_of(self.resolve(reference)?),
                None => Some(String::new()),
            },
            _ => return None,
        };
        if let Some(path) = &path {
            self.paths.insert(node.id(), path.clone());
        }
        path
    }

    /// Canonical path for display, falling back to the textual `&intc/dev`
    /// form when a fragment cannot be resolved.
    pub fn display_path(&mut self, node: Node<'a>) -> String {
        if self.chain_is_whole(node) {
            self.path_of(node).unwrap_or_else(|| fallback_path(self.doc, node))
        } else {
            fallback_path(self.doc, node)
        }
    }

    /// Whether every `&label { }` on the way from `node` to the root resolves.
    pub fn chain_is_whole(&self, node: Node<'a>) -> bool {
        let mut current = Some(node);
        while let Some(node) = current {
            let Some(owning) = owning_node(node) else {
                return true;
            };
            if owning.kind() == "devicetree"
                && let Some(reference) = find_child(owning, "reference")
                && self.resolve(reference).is_none()
            {
                return false;
            }
            current = self.parent_of(node);
        }
        true
    }
}

fn collect_labels<'a>(node: Node<'a>, doc: &'a Document, labels: &mut HashMap<&'a str, Node<'a>>) {
    if node.kind() == "subnode" {
        for i in 0..node.child_count() {
            if let Some(label) = node.child(i as u32)
                && label.kind() == "label"
            {
                // the label token includes its trailing ':'
                let name = node_text(doc, label).trim_end_matches(':').trim();
                if !name.is_empty() {
                    labels.entry(name).or_insert(node);
                }
            }
        }
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            collect_labels(child, doc, labels);
        }
    }
}

// ---------------------------------------------------------------- cells

/// One cell of a property value: a phandle reference or a number.
pub struct Cell<'a> {
    pub reference: Option<RefKind<'a>>,
    /// value when the text evaluates to a 32-bit integer
    pub value: Option<u32>,
    /// source text, for "not an integer" messages
    pub text: &'a str,
    pub start: usize,
    pub end: usize,
}

/// Flatten a property value into cells, skipping comments and `/bits/`
/// markers. One cell per value token: literals, unexpanded macro names and
/// integer expressions.
pub fn cells_of_property<'a>(doc: &'a Document, property: Node<'a>) -> Vec<Cell<'a>> {
    fn collect<'a>(doc: &'a Document, node: Node<'a>, out: &mut Vec<Cell<'a>>) {
        match node.kind() {
            "bits" => {}
            "property" | "prop_data" | "array" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i as u32) {
                        collect(doc, child, out);
                    }
                }
            }
            "reference" => out.push(Cell {
                reference: reference_of(doc, node),
                value: None,
                text: node_text(doc, node),
                start: node.start_byte(),
                end: node.end_byte(),
            }),
            "int_literal" | "integer" | "number" | "preproc_identifier" | "identifier"
            | "char_literal" | "string_literal" | "preproc_number_literal" | "preproc_call"
            | "int_unary_op" | "int_parenthesized_expression" | "int_binary_op" => {
                out.push(Cell {
                    reference: None,
                    value: eval_int(doc, node).map(|value| value as u32),
                    text: node_text(doc, node),
                    start: node.start_byte(),
                    end: node.end_byte(),
                })
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    collect(doc, property, &mut out);
    out
}

/// Value of a single-cell property such as `#clock-cells = <1>`.
pub fn property_u32(doc: &Document, property: Node<'_>) -> Option<u32> {
    let cells = cells_of_property(doc, property);
    (cells.len() == 1).then(|| cells[0].value).flatten()
}

/// Numeric value of a cell. dtc evaluates C constant expressions in cells, so
/// `(2 | 1)` and `((1 << 4) - 1) << 8 | 8` are ordinary cell values.
pub fn eval_int(doc: &Document, node: Node<'_>) -> Option<u64> {
    match node.kind() {
        "int_literal" | "integer" | "number" | "preproc_number_literal" => {
            parse_integer(node_text(doc, node))
        }
        "char_literal" => {
            let inner = node_text(doc, node).trim_matches('\'');
            let mut chars = inner.chars();
            let value = chars.next()?;
            (chars.next().is_none()).then_some(value as u64)
        }
        "int_parenthesized_expression" => eval_int(doc, node.named_child(0)?),
        "int_unary_op" => {
            let operand = eval_int(doc, node.named_child(0)?)?;
            match node.child(0)?.kind() {
                "~" => Some(!operand),
                "-" => Some(operand.wrapping_neg()),
                "+" => Some(operand),
                "!" => Some(u64::from(operand == 0)),
                _ => None,
            }
        }
        "int_binary_op" => {
            let left = eval_int(doc, node.named_child(0)?)?;
            let right = eval_int(doc, node.named_child(1)?)?;
            let operator = (0..node.child_count())
                .filter_map(|i| node.child(i as u32))
                .find(|child| !child.is_named())?;
            match node_text(doc, operator) {
                "|" => Some(left | right),
                "&" => Some(left & right),
                "^" => Some(left ^ right),
                "+" => Some(left.wrapping_add(right)),
                "-" => Some(left.wrapping_sub(right)),
                "*" => Some(left.wrapping_mul(right)),
                "/" => (right != 0).then(|| left / right),
                "%" => (right != 0).then(|| left % right),
                "<<" => (right < 64).then(|| left << right),
                ">>" => (right < 64).then(|| left >> right),
                "<" => Some(u64::from(left < right)),
                ">" => Some(u64::from(left > right)),
                "<=" => Some(u64::from(left <= right)),
                ">=" => Some(u64::from(left >= right)),
                "==" => Some(u64::from(left == right)),
                "!=" => Some(u64::from(left != right)),
                "&&" => Some(u64::from(left != 0 && right != 0)),
                "||" => Some(u64::from(left != 0 || right != 0)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn parse_integer(text: &str) -> Option<u64> {
    let text = text.trim().trim_end_matches(['u', 'U', 'l', 'L']);
    let (digits, radix) = match text.get(..2) {
        Some(prefix) if prefix.eq_ignore_ascii_case("0x") => (&text[2..], 16),
        Some(prefix) if prefix.eq_ignore_ascii_case("0b") => (&text[2..], 2),
        Some(prefix) if prefix.starts_with('0') && text.len() > 1 => (&text[1..], 8),
        _ => (text, 10),
    };
    u64::from_str_radix(digits, radix).ok()
}

// ------------------------------------------------------------ raw text scan

/// Indent width of a line, with tab stops of 8.
pub fn indent_of(line: &str) -> usize {
    let mut indent = 0;
    for c in line.chars() {
        match c {
            '\t' => indent += 8 - (indent % 8),
            ' ' => indent += 1,
            _ => break,
        }
    }
    indent
}

/// Offsets of `{` and `}` bytes outside strings and comments, from `from`
/// onward. The one scanner shared by completion (innermost open brace) and
/// syntax-error localization (last unmatched brace).
pub fn brace_offsets(text: &str, from: usize) -> Vec<(usize, u8)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = from.min(bytes.len());
    let mut in_block = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_block {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        match b {
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                in_block = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = text[i..].find('\n').map_or(bytes.len(), |pos| i + pos);
            }
            b'"' | b'\'' => {
                let quote = b;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                    } else {
                        let closed = bytes[i] == quote;
                        i += 1;
                        if closed {
                            break;
                        }
                    }
                }
            }
            b'{' | b'}' => {
                out.push((i, b));
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

// -------------------------------------------- property lifetime (dead props)

/// Property-lifetime analysis for a single document.
///
/// DTS semantics: within a merge scope, the last definition of a property
/// wins. A property dies when a later definition overrides it — either a
/// sibling in the same node, a property inside a same-file `&label { }` /
/// `&{/path} { }` extension of the node, or a following `/delete-property/`.
pub struct Analysis {
    /// Node ids of property syntax nodes whose value is dead.
    pub dead_properties: HashSet<usize>,
    /// Node labels with the full path of the labeled node, sorted by label.
    pub labels: Vec<(String, String)>,
}

/// (owner node id, property name) -> definitions in file order as (start byte, property node id).
type Contributions = HashMap<(usize, String), Vec<(usize, usize)>>;

pub fn analyze(doc: &Document) -> Analysis {
    let tree = Tree::new(doc);
    let mut contributions: Contributions = HashMap::new();
    let mut dead: HashSet<usize> = HashSet::new();
    record(doc.tree.root_node(), doc, &tree, &mut contributions, &mut dead);

    for mut defs in contributions.into_values() {
        if defs.len() < 2 {
            continue;
        }
        defs.sort_by_key(|(start, _)| *start);
        for (_, id) in &defs[..defs.len() - 1] {
            dead.insert(*id);
        }
    }
    let mut label_list: Vec<(String, String)> = tree
        .labels()
        .map(|(label, node)| (label.to_string(), fallback_path(doc, node)))
        .collect();
    label_list.sort();
    Analysis {
        dead_properties: dead,
        labels: label_list,
    }
}

fn record<'a>(
    node: Node<'a>,
    doc: &Document,
    tree: &Tree<'a>,
    contributions: &mut Contributions,
    dead: &mut HashSet<usize>,
) {
    match node.kind() {
        "devicetree" => {
            let owner = match find_child(node, "reference") {
                Some(reference) => tree.resolve(reference),
                None => Some(node),
            };
            if let (Some(body), Some(target)) = (node_def_of(node), owner) {
                record_block(body, target.id(), doc, contributions, dead);
            }
        }
        "subnode" => {
            if let Some(body) = node_def_of(node) {
                record_block(body, node.id(), doc, contributions, dead);
            }
        }
        _ => {}
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            record(child, doc, tree, contributions, dead);
        }
    }
}

fn record_block(
    container: Node<'_>,
    target_id: usize,
    doc: &Document,
    contributions: &mut Contributions,
    dead: &mut HashSet<usize>,
) {
    for i in 0..container.child_count() {
        let Some(child) = container.child(i as u32) else { continue };
        match child.kind() {
            "property" => {
                if let Some(delete) = find_child(child, "delete_property") {
                    if let Some(name) = delete.child_by_field_name("name") {
                        if let Some(defs) = contributions.get_mut(&(target_id, node_text(doc, name).to_string()))
                        {
                            for (start, id) in defs.iter() {
                                if *start < child.start_byte() {
                                    dead.insert(*id);
                                }
                            }
                        }
                    }
                } else if let Some(name) = child.child_by_field_name("name") {
                    contributions
                        .entry((target_id, node_text(doc, name).to_string()))
                        .or_default()
                        .push((child.start_byte(), child.id()));
                }
            }
            // Properties and subnodes may live under #if blocks; the primary
            // branch is always recorded, alternatives are skipped.
            "preproc_if_in_node_def" | "preproc_ifdef_in_node_def" => {
                for j in 0..child.child_count() {
                    if let Some(inner) = child.child(j as u32)
                        && !inner.kind().contains("else")
                        && !inner.kind().contains("elif")
                    {
                        record_block(inner, target_id, doc, contributions, dead);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    const DOC: &str = "/dts-v1/;\n\n/ {\n    #address-cells = <1>;\n\n    intc: interrupt-controller@1000 {\n        #interrupt-cells = <3>;\n    };\n\n    uart@2000 {\n        interrupt-parent = <&intc>;\n        interrupts = <GIC_SPI 0 4>;\n    };\n};\n\n&intc {\n    status = \"okay\";\n};\n";

    #[test]
    fn finds_properties_through_bodies() {
        let doc = Document::open(DOC, 0);
        let tree = Tree::new(&doc);
        let intc = tree.label("intc").unwrap();
        let prop = find_property(&doc, intc, "#interrupt-cells").unwrap();
        assert_eq!(property_u32(&doc, prop), Some(3));
        assert!(find_property(&doc, intc, "missing").is_none());
    }

    #[test]
    fn canonical_paths_merge_fragments() {
        let doc = Document::open(DOC, 0);
        let mut tree = Tree::new(&doc);
        let intc = tree.label("intc").unwrap();
        assert_eq!(tree.display_path(intc), "/interrupt-controller@1000");
    }

    #[test]
    fn unresolvable_fragments_fall_back_to_reference_text() {
        let doc = Document::open("/dts-v1/;\n&board { dev { }; };\n", 0);
        let fragment = find_child(doc.tree.root_node(), "devicetree").unwrap();
        let dev = find_child(find_child(fragment, "node_def").unwrap(), "subnode").unwrap();
        assert_eq!(Tree::new(&doc).display_path(dev), "/&board/dev");
    }

    #[test]
    fn path_lookup_walks_from_the_root() {
        let doc = Document::open(DOC, 0);
        let tree = Tree::new(&doc);
        let uart = tree.node_at("/uart@2000").unwrap();
        assert!(find_property(&doc, uart, "interrupt-parent").is_some());
        assert!(tree.node_at("/nope").is_none());
    }

    #[test]
    fn cells_evaluate_constant_expressions() {
        let doc = Document::open(
            "/dts-v1/;\n/ { n { reg = <((((1 << 4) - 1) << 8) | 8)>; }; };\n",
            0,
        );
        let node = Tree::new(&doc).node_at("/n").unwrap();
        let reg = find_property(&doc, node, "reg").unwrap();
        assert_eq!(property_u32(&doc, reg), Some((0xF << 8) | 8));
    }

    #[test]
    fn brace_scanner_skips_strings_and_comments() {
        let text = "a { /* } { */ x = \"http://{}\"; }; b { };";
        let braces: Vec<char> = brace_offsets(text, 0).iter().map(|(_, b)| *b as char).collect();
        assert_eq!(braces, vec!['{', '}', '{', '}']);
    }

    #[test]
    fn brace_offsets_start_from_offset() {
        let text = "{ a { } b { } }";
        assert_eq!(brace_offsets(text, 6).len(), 4);
    }

    #[test]
    fn analyze_marks_overridden_properties_dead() {
        let doc = Document::open(
            "/dts-v1/;\n/ { m: n { a = <1>; a = <2>; }; };\n&m { a = <3>; };\n",
            0,
        );
        let analysis = analyze(&doc);
        let props = property_spans(&doc, "a");
        assert_eq!(props.len(), 3);
        assert!(analysis.dead_properties.contains(&props[0].1));
        assert!(analysis.dead_properties.contains(&props[1].1));
        assert!(!analysis.dead_properties.contains(&props[2].1));
    }

    fn property_spans(doc: &Document, name: &str) -> Vec<(usize, usize)> {
        fn walk(node: Node, doc: &Document, name: &str, out: &mut Vec<(usize, usize)>) {
            if node.kind() == "property"
                && node.child_by_field_name("name").is_some_and(|n| node_text(doc, n) == name)
            {
                out.push((node.start_byte(), node.id()));
            }
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32) {
                    walk(child, doc, name, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(doc.tree.root_node(), doc, name, &mut out);
        out.sort_by_key(|(start, _)| *start);
        out
    }
}


