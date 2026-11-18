use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
use tree_sitter::Node;

use crate::bindings::BindingIndex;
use crate::document::Document;
use crate::status::FeatureIssue;
use crate::{index, util};

const SOURCE: &str = "dtsls";
const MAX_DIAGNOSTICS: usize = 500;
const MAX_BINDING_DIAGNOSTICS_PER_NODE: usize = 12;

const CAT_SYNTAX: u8 = 1 << 0;
const CAT_DUPLICATES: u8 = 1 << 1;
const CAT_INCLUDES: u8 = 1 << 2;
const CAT_BINDINGS: u8 = 1 << 3;
const CAT_ALL: u8 = CAT_SYNTAX | CAT_DUPLICATES | CAT_INCLUDES | CAT_BINDINGS;

/// Node-kind ids resolved once: comparing ids beats comparing kind strings on
/// every node of every traversal.
struct Kinds {
    source_file: u16,
    subnode: u16,
    devicetree: u16,
    property: u16,
    prop_data: u16,
    preproc_include: u16,
    preproc_string_literal: u16,
    system_lib_string: u16,
    string_literal: u16,
    /// containers whose direct subnodes may collide (`node_def`, `#if` bodies)
    containers: [u16; 3],
    /// `#else`/`#elif` subtrees are parsed but conditional; duplicates in
    /// them would be false positives
    alternatives: HashSet<u16>,
}

fn kinds() -> &'static Kinds {
    static KINDS: OnceLock<Kinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let language: tree_sitter::Language = tree_sitter_devicetree2::LANGUAGE.into();
        let id = |kind: &str| language.id_for_node_kind(kind, true);
        let mut alternatives = HashSet::new();
        for kind_id in 0..language.node_kind_count() as u16 {
            if let Some(kind) = language.node_kind_for_id(kind_id)
                && (kind.contains("else") || kind.contains("elif"))
            {
                alternatives.insert(kind_id);
            }
        }
        Kinds {
            source_file: id("source_file"),
            subnode: id("subnode"),
            devicetree: id("devicetree"),
            property: id("property"),
            prop_data: id("prop_data"),
            preproc_include: id("preproc_include"),
            preproc_string_literal: id("preproc_string_literal"),
            system_lib_string: id("system_lib_string"),
            string_literal: id("string_literal"),
            containers: [
                id("node_def"),
                id("preproc_if_in_node_def"),
                id("preproc_ifdef_in_node_def"),
            ],
            alternatives,
        }
    })
}

struct DiagCtx<'a> {
    doc: &'a Document,
    file_path: Option<&'a Path>,
    include_dirs: &'a [PathBuf],
    bindings: &'a BindingIndex,
    cats: u8,
    out: Vec<Diagnostic>,
}

impl DiagCtx<'_> {
    fn push(&mut self, range: Range, severity: DiagnosticSeverity, message: String) {
        self.out.push(Diagnostic {
            range,
            severity: Some(severity),
            source: Some(SOURCE.to_string()),
            message,
            ..Default::default()
        });
    }
}

/// Syntax errors, duplicate nodes, unresolved includes and binding violations
/// in one traversal, sorted by position. Whole-feature issues (`status`) are
/// reported at the top of the file.
pub fn all(
    doc: &Document,
    file_path: Option<&Path>,
    include_dirs: &[PathBuf],
    bindings: &BindingIndex,
    status: &[FeatureIssue],
) -> Vec<Diagnostic> {
    run(doc, file_path, include_dirs, bindings, status, CAT_ALL)
}

fn run(
    doc: &Document,
    file_path: Option<&Path>,
    include_dirs: &[PathBuf],
    bindings: &BindingIndex,
    status: &[FeatureIssue],
    cats: u8,
) -> Vec<Diagnostic> {
    // without a populated binding index, binding checks are pointless work —
    // but the feature notices must still get through
    let cats = if cats & CAT_BINDINGS != 0 && bindings.is_empty() {
        cats & !CAT_BINDINGS
    } else {
        cats
    };
    if cats == 0 && status.is_empty() {
        return Vec::new();
    }
    let mut ctx = DiagCtx {
        doc,
        file_path,
        include_dirs,
        bindings,
        cats,
        out: Vec::new(),
    };
    // one synthetic diagnostic per unusable/degraded feature, right at the
    // top of the file; the message shape is `feature: state — detail`
    let top = Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: 0,
            character: 0,
        },
    };
    for issue in status {
        ctx.push(top.clone(), issue.severity, issue.message());
    }
    if cats != 0 {
        walk(doc.tree.root_node(), &mut ctx, false);
    }
    let mut out = ctx.out;
    out.sort_by_key(|d| (d.range.start, d.range.end));
    out
}

fn walk(node: Node, ctx: &mut DiagCtx, in_alternative: bool) {
    if ctx.out.len() >= MAX_DIAGNOSTICS {
        return;
    }
    let kind = node.kind_id();
    if ctx.cats & CAT_SYNTAX != 0 {
        if node.is_error() {
            let range = error_range(ctx.doc, node);
            ctx.push(
                range,
                DiagnosticSeverity::ERROR,
                format!("syntax error near {}", snippet(ctx.doc, node)),
            );
            // the ERROR node covers the whole malformed region; recursing
            // would only produce noise about the same span
            return;
        }
        if node.is_missing() {
            let range = node_range(ctx.doc, node);
            ctx.push(
                range,
                DiagnosticSeverity::ERROR,
                format!("missing '{}'", node.kind()),
            );
            return;
        }
    }
    if ctx.cats & CAT_BINDINGS != 0 && kind == kinds().subnode {
        check_binding_node(node, ctx);
    }
    if ctx.cats & CAT_DUPLICATES != 0 && !in_alternative {
        if kind == kinds().source_file {
            check_duplicate_roots(node, ctx);
        }
        if kinds().containers.contains(&kind) {
            check_duplicate_children(node, ctx);
        }
    }
    if ctx.cats & CAT_INCLUDES != 0 && kind == kinds().preproc_include {
        check_include_node(node, ctx);
    }
    let child_alternative = in_alternative || kinds().alternatives.contains(&kind);
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            walk(child, ctx, child_alternative);
        }
    }
}

#[cfg(test)]
pub fn compute(doc: &Document) -> Vec<Diagnostic> {
    run(doc, None, &[], &BindingIndex::default(), &[], CAT_SYNTAX)
}

#[cfg(test)]
pub fn semantic(doc: &Document) -> Vec<Diagnostic> {
    run(doc, None, &[], &BindingIndex::default(), &[], CAT_DUPLICATES)
}

#[cfg(test)]
pub fn unresolved_includes(
    doc: &Document,
    file_path: Option<&Path>,
    include_dirs: &[PathBuf],
) -> Vec<Diagnostic> {
    run(
        doc,
        file_path,
        include_dirs,
        &BindingIndex::default(),
        &[],
        CAT_INCLUDES,
    )
}

/// Test-only view of the binding checks.
#[cfg(test)]
pub fn binding_diagnostics(
    doc: &Document,
    bindings: &BindingIndex,
    file_path: Option<&Path>,
) -> Vec<Diagnostic> {
    run(doc, file_path, &[], bindings, &[], CAT_BINDINGS)
}

/// Validate one node against its binding: missing required properties and
/// (for strict, fully resolved bindings) unknown properties are errors; a
/// `compatible` with no matching binding is a warning.
fn check_binding_node(node: Node, ctx: &mut DiagCtx) {
    let (Some(name_node), Some(compatibles)) = (
        node.child_by_field_name("name"),
        compatible_strings(ctx.doc, node),
    ) else {
        return;
    };
    let Some(binding) = ctx.bindings.lookup(&compatibles) else {
        ctx.push(
            node_range(ctx.doc, name_node),
            DiagnosticSeverity::WARNING,
            format!(
                "no binding found for compatible \"{}\"",
                compatibles.join("\", \"")
            ),
        );
        return;
    };
    let file = binding
        .uri
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or("")
        .to_string();
    let node_name = util::node_text(ctx.doc, name_node);
    let mut count = 0;
    for required in &binding.required {
        if node_property(node, required, ctx.doc).is_none() {
            ctx.push(
                node_range(ctx.doc, name_node),
                DiagnosticSeverity::ERROR,
                format!(
                    "missing required property '{required}' for node \
                     '{node_name}' (binding: {file})"
                ),
            );
            count += 1;
        }
    }
    if binding.strict && binding.refs_complete {
        let body = util::node_def_of(node).unwrap_or(node);
        for i in 0..body.child_count() {
            let Some(child) = body.child(i as u32) else { continue };
            if child.kind_id() != kinds().property {
                continue;
            }
            let Some(prop_name) = child.child_by_field_name("name") else {
                continue;
            };
            let name = util::node_text(ctx.doc, prop_name);
            if ctx.bindings.is_property_allowed(name, &binding) {
                continue;
            }
            ctx.push(
                node_range(ctx.doc, prop_name),
                DiagnosticSeverity::ERROR,
                format!("property '{name}' is not allowed by binding {file} (node '{node_name}')"),
            );
            count += 1;
            if count >= MAX_BINDING_DIAGNOSTICS_PER_NODE {
                break;
            }
        }
    }
}

/// All `compatible` strings declared by this node's `compatible` property.
pub fn compatible_strings(doc: &Document, node: Node) -> Option<Vec<String>> {
    let prop = node_property(node, "compatible", doc)?;
    let k = kinds();
    let mut out = Vec::new();
    for i in 0..prop.child_count() {
        let Some(data) = prop.child(i as u32) else { continue };
        if data.kind_id() != k.prop_data {
            continue;
        }
        for j in 0..data.child_count() {
            let Some(child) = data.child(j as u32) else { continue };
            if child.kind_id() == k.string_literal {
                out.push(util::node_text(doc, child).trim_matches('"').to_string());
                break;
            }
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn node_property<'a>(node: Node<'a>, name: &str, doc: &Document) -> Option<Node<'a>> {
    // properties live inside the node's `{ ... }` body, not on the subnode
    let k = kinds();
    let container = if node.kind_id() == k.subnode || node.kind_id() == k.devicetree {
        util::node_def_of(node).unwrap_or(node)
    } else {
        node
    };
    for i in 0..container.child_count() {
        let Some(child) = container.child(i as u32) else { continue };
        if child.kind_id() != k.property {
            continue;
        }
        if child
            .child_by_field_name("name")
            .is_some_and(|n| util::node_text(doc, n) == name)
        {
            return Some(child);
        }
    }
    None
}

fn check_include_node(node: Node, ctx: &mut DiagCtx) {
    let Some(path_node) = node.child_by_field_name("path") else {
        return;
    };
    let kind = path_node.kind_id();
    let raw = util::node_text(ctx.doc, path_node);
    let (target, quoted) = if kind == kinds().preproc_string_literal {
        (raw.trim_matches('"'), true)
    } else if kind == kinds().system_lib_string {
        (raw.trim_matches(|c| c == '<' || c == '>'), false)
    } else {
        // `#include SOME_MACRO` — statically unresolvable, skip.
        return;
    };
    let resolved = index::resolve_include(
        ctx.file_path.and_then(Path::parent),
        target,
        quoted,
        ctx.include_dirs,
    );
    if resolved.is_none() {
        ctx.push(
            node_range(ctx.doc, node),
            DiagnosticSeverity::ERROR,
            format!(
                "unresolved #include \"{target}\" — no include dir matches; \
                 open the workspace at the kernel root or set \
                 initializationOptions.includePaths"
            ),
        );
    }
}

/// Multiple `/ { }` roots in one file are silently merged by dtc.
fn check_duplicate_roots(node: Node, ctx: &mut DiagCtx) {
    let mut seen_root = false;
    for i in 0..node.child_count() {
        let Some(child) = node.child(i as u32) else { continue };
        if child.kind_id() != kinds().devicetree {
            continue;
        }
        if util::find_child(child, "reference").is_some() {
            continue;
        }
        let range = Range {
            start: util::byte_to_position(&ctx.doc.rope, child.start_byte()),
            end: util::byte_to_position(&ctx.doc.rope, child.start_byte() + 1),
        };
        if seen_root {
            ctx.push(range, DiagnosticSeverity::ERROR, "duplicate node '/'".to_string());
        } else {
            seen_root = true;
        }
    }
}

/// Flag sibling nodes that define the same name (dtc would silently merge
/// them) or that claim the same unit address under one parent.
fn check_duplicate_children(node: Node, ctx: &mut DiagCtx) {
    let mut seen_names: HashMap<String, String> = HashMap::new();
    let mut seen_addresses: HashMap<String, String> = HashMap::new();
    for i in 0..node.child_count() {
        let Some(child) = node.child(i as u32) else { continue };
        if child.kind_id() != kinds().subnode {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let full = util::node_text(ctx.doc, name_node).to_string();
        let (name, address) = match full.split_once('@') {
            Some((name, addr)) => (name.to_string(), Some(addr.to_string())),
            None => (full.clone(), None),
        };
        if seen_names.contains_key(&full) {
            ctx.push(
                node_range(ctx.doc, name_node),
                DiagnosticSeverity::ERROR,
                format!("duplicate node '{full}'"),
            );
        } else {
            seen_names.insert(full.clone(), name.clone());
        }
        if let Some(addr) = &address {
            match seen_addresses.get(addr) {
                Some(first_name) if first_name != &name => {
                    // Legitimate on some SoCs (e.g. Qualcomm GENI: one serial
                    // engine, several protocol nodes per address), so this is
                    // a warning, not an error.
                    ctx.push(
                        node_range(ctx.doc, name_node),
                        DiagnosticSeverity::WARNING,
                        format!("node '{full}' shares address with '{first_name}@{addr}'"),
                    );
                }
                None => {
                    seen_addresses.insert(addr.clone(), name.clone());
                }
                _ => {}
            }
        }
    }
}

fn node_range(doc: &Document, node: Node) -> Range {
    Range {
        start: util::byte_to_position(&doc.rope, node.start_byte()),
        end: util::byte_to_position(&doc.rope, node.end_byte()),
    }
}

/// Range for an ERROR node. A malformed region can span everything up to EOF
/// (e.g. an unclosed `{`), and squiggling all of it is noise. Instead point at
/// the last unmatched `{` in the region — the likely culprit — or fall back to
/// the first line of the region.
fn error_range(doc: &Document, node: Node) -> Range {
    let start = node.start_byte();
    let end = node.end_byte();
    let start_pos = util::byte_to_position(&doc.rope, start);
    let end_pos = util::byte_to_position(&doc.rope, end);

    if end_pos.line > start_pos.line {
        if let Some(brace) = last_unmatched_open_brace(doc, start) {
            return line_range(doc, brace);
        }
        return line_range_at_line(doc, start_pos.line);
    }

    let mut range = Range {
        start: start_pos,
        end: end_pos,
    };
    if range.end == range.start {
        range.end.character += 1;
    }
    range
}

fn line_range(doc: &Document, byte: usize) -> Range {
    let line = doc.rope.byte_to_line(byte);
    line_range_at_line(doc, line as u32)
}

fn line_range_at_line(doc: &Document, line: u32) -> Range {
    let line_idx = line as usize;
    let start_byte = doc.rope.line_to_byte(line_idx);
    let mut end_byte = if line_idx + 1 < doc.rope.len_lines() {
        doc.rope.line_to_byte(line_idx + 1)
    } else {
        doc.rope.len_bytes()
    };
    while end_byte > start_byte
        && matches!(doc.rope.byte(end_byte - 1), b'\n' | b'\r' | b' ' | b'\t')
    {
        end_byte -= 1;
    }
    Range {
        start: util::byte_to_position(&doc.rope, start_byte),
        end: util::byte_to_position(&doc.rope, end_byte),
    }
}

/// Byte offset of the last unclosed `{` between the error and EOF, using
/// indentation-aware brace matching: a `}` only closes the topmost open brace
/// indented at or before its own column, so a `{` whose closer "stole" a
/// shallower one is the likely culprit. Plain greedy pairing would blame the
/// outermost node instead (everything below an unclosed `{` shifts one level).
fn last_unmatched_open_brace(doc: &Document, start_byte: usize) -> Option<usize> {
    let start_line = doc.rope.byte_to_line(start_byte);
    let mut stack: Vec<(usize, usize)> = Vec::new(); // (byte, indent width)
    let mut suspects: Vec<usize> = Vec::new();
    let mut in_comment = false;

    for line_idx in start_line..doc.rope.len_lines() {
        let line_start = doc.rope.line_to_byte(line_idx);
        let line_end = if line_idx + 1 < doc.rope.len_lines() {
            doc.rope.line_to_byte(line_idx + 1)
        } else {
            doc.rope.len_bytes()
        };

        let mut indent = 0;
        let mut byte = line_start;
        while byte < line_end {
            match doc.rope.byte(byte) {
                b'\t' => indent += 8 - (indent % 8),
                b' ' => indent += 1,
                _ => break,
            }
            byte += 1;
        }

        let mut in_string = false;
        while byte < line_end {
            let b = doc.rope.byte(byte);
            if in_comment {
                if b == b'*' && byte + 1 < line_end && doc.rope.byte(byte + 1) == b'/' {
                    in_comment = false;
                    byte += 1;
                }
            } else if in_string {
                match b {
                    b'\\' => byte += 1,
                    b'"' => in_string = false,
                    _ => {}
                }
            } else {
                match b {
                    b'/' if byte + 1 < line_end && doc.rope.byte(byte + 1) == b'*' => {
                        in_comment = true;
                        byte += 1;
                    }
                    b'/' if byte + 1 < line_end && doc.rope.byte(byte + 1) == b'/' => break,
                    b'"' => in_string = true,
                    b'{' => stack.push((byte, indent)),
                    b'}' => loop {
                        match stack.last() {
                            Some(&(_, open_indent)) if open_indent > indent => {
                                // This closer is too shallow for the open
                                // brace; the open brace is a suspect.
                                suspects.push(stack.pop().unwrap().0);
                            }
                            Some(&(_, open_indent)) if open_indent <= indent => {
                                stack.pop();
                                break;
                            }
                            _ => break,
                        }
                    },
                    _ => {}
                }
            }
            byte += 1;
        }
    }

    suspects.extend(stack.into_iter().map(|(pos, _)| pos));
    suspects.into_iter().max()
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
    fn unclosed_brace_points_at_the_brace_not_the_whole_file() {
        // A valid file, then a node opened without a closing brace right
        // before the root's `};` — the user's exact scenario.
        let mut text = String::from("/dts-v1/;\n\n/ {\n    a {\n        x = <1>;\n    };\n\n");
        for i in 0..50 {
            text.push_str(&format!("    filler{i} {{\n        y = <{i}>;\n    }};\n"));
        }
        text.push_str("};\n");
        let lines: Vec<&str> = text.lines().collect();
        let culprit_line = (lines.len() - 1) as u32; // 0-based line of the root's `};`
        let broken: Vec<&str> = lines[..lines.len() - 1]
            .iter()
            .copied()
            .chain(std::iter::once("    broken {"))
            .chain(std::iter::once("};"))
            .collect();
        let text = broken.join("\n") + "\n";

        let doc = Document::open(&text, 0);
        let diags = compute(&doc);
        let syntax: Vec<_> = diags
            .iter()
            .filter(|d| d.message.starts_with("syntax error"))
            .collect();
        assert!(!syntax.is_empty());
        // The squiggle must sit on the `broken {` line, not span to EOF.
        for d in syntax {
            assert_eq!(d.range.start.line, culprit_line, "range: {:?}", d.range);
        }
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
            let diags = all(&Document::open(&text, 0), None, &[], &BindingIndex::default(), &[]);
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
            let diags = all(&Document::open(&text, 0), None, &[], &BindingIndex::default(), &[]);
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
                all(&Document::open(&text, 0), None, &[], &BindingIndex::default(), &[])
                    .is_empty(),
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


    #[test]
    fn unresolved_include_is_reported() {
        let text = "/dts-v1/;\n\n#include <dt-bindings/nope.h>\n\n/ {\n    a;\n};\n";
        let doc = Document::open(text, 0);
        let diags = unresolved_includes(&doc, None, &[]);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(diags[0].message.contains("unresolved #include"));
    }

    #[test]
    fn quoted_include_resolves_relative_to_the_file() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/includes");
        let file = dir.join("board.dts");
        let text = std::fs::read_to_string(&file).unwrap();
        let doc = Document::open(&text, 0);
        assert!(unresolved_includes(&doc, Some(&file), &[]).is_empty());
    }

    #[test]
    fn angle_include_resolves_via_include_dirs() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/includes");
        let text = "/dts-v1/;\n\n#include <soc.dtsi>\n\n/ {\n    a;\n};\n";
        let doc = Document::open(text, 0);
        // no dirs -> error
        assert_eq!(unresolved_includes(&doc, None, &[]).len(), 1);
        // with the fixture dir as include dir -> clean
        assert!(unresolved_includes(&doc, None, &[dir]).is_empty());
    }

    #[test]
    fn binding_violations_are_errors_and_missing_binding_is_a_warning() {
        let bindings = BindingIndex::default();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/bindings");
        // build a tiny index by hand from the fixture files
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "yaml") {
                for binding in crate::bindings::parse_bindings(&path) {
                    bindings.insert_for_test(binding);
                }
            }
        }

        // missing required 'reg' + unknown property -> two errors
        let text = "/dts-v1/;\n\n/ {\n    widget@0 {\n        compatible = \"acme,widget\";\n        bogus-prop;\n    };\n};\n";
        let doc = Document::open(text, 0);
        let diags = binding_diagnostics(&doc, &bindings, None);
        assert!(diags.iter().any(|d| d.message.contains("missing required property 'reg'")));
        assert!(diags.iter().any(|d| d.message.contains("'bogus-prop' is not allowed")));
        assert!(diags.iter().all(|d| d.severity == Some(DiagnosticSeverity::ERROR)));

        // compatible with no binding -> warning only
        let text = "/dts-v1/;\n\n/ {\n    x@0 {\n        compatible = \"acme,unknown\";\n    };\n};\n";
        let doc = Document::open(text, 0);
        let diags = binding_diagnostics(&doc, &bindings, None);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::WARNING));
        assert!(diags[0].message.contains("no binding found"));

        // fully valid node -> clean
        let text = "/dts-v1/;\n\n/ {\n    widget@0 {\n        compatible = \"acme,widget\";\n        reg = <0 1>;\n        clocks = <&c 0>;\n        status_probe;\n    };\n};\n";
        let doc = Document::open(text, 0);
        assert!(binding_diagnostics(&doc, &bindings, None).is_empty());
    }

    #[test]
    fn feature_notices_are_emitted_at_the_top() {
        let issues = vec![
            FeatureIssue::unavailable("devicetree bindings", "no bindings dir"),
            FeatureIssue::degraded("dtschema", "pip install dtschema"),
        ];
        let text = "/dts-v1/;\n\n/ {\n    a;\n};\n";
        let doc = Document::open(text, 0);
        let diags = all(&doc, None, &[], &BindingIndex::default(), &issues);
        assert_eq!(diags.len(), 2, "diags: {diags:?}");
        assert_eq!(diags[0].range.start.line, 0);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(diags[0].message.starts_with("devicetree bindings: unavailable"));
        assert_eq!(diags[1].severity, Some(DiagnosticSeverity::WARNING));
        assert!(diags[1].message.starts_with("dtschema: degraded"));
    }
}
