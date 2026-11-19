use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rustc_hash::FxHashMap;
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
    subnode: u16,
    devicetree: u16,
    property: u16,
    prop_data: u16,
    preproc_include: u16,
    preproc_string_literal: u16,
    system_lib_string: u16,
    string_literal: u16,
    node_def: u16,
    /// `foo:` tokens on nodes and properties
    label: u16,
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
            subnode: id("subnode"),
            devicetree: id("devicetree"),
            property: id("property"),
            prop_data: id("prop_data"),
            preproc_include: id("preproc_include"),
            preproc_string_literal: id("preproc_string_literal"),
            system_lib_string: id("system_lib_string"),
            string_literal: id("string_literal"),
            node_def: id("node_def"),
            label: id("label"),
            containers: [
                id("node_def"),
                id("preproc_if_in_node_def"),
                id("preproc_ifdef_in_node_def"),
            ],
            alternatives,
        }
    })
}

/// Diagnostic before position conversion: byte offsets only. Subtree results
/// are cached in this form so they can be re-based when the subtree moves.
#[derive(Clone)]
struct RawDiag {
    start: u32,
    end: u32,
    severity: DiagnosticSeverity,
    message: String,
}

impl RawDiag {
    fn shifted(&self, base: u32) -> RawDiag {
        RawDiag {
            start: self.start + base,
            end: self.end + base,
            severity: self.severity,
            message: self.message.clone(),
        }
    }

    fn rebase(&self, base: u32) -> RawDiag {
        RawDiag {
            start: self.start - base,
            end: self.end - base,
            severity: self.severity,
            message: self.message.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    file: u64,
    text: u64,
    len: u32,
    cats: u8,
}

const CACHE_LIMIT: usize = 200_000;

/// Per-subtree result cache: a keystroke only changes one small subtree, so
/// everything else is served from here. Keyed by the subtree's text, which
/// also survives the positions of the whole file shifting.
/// One memoized subtree: its diagnostics plus the labels it defines (byte
/// ranges relative to the subtree start; labels never span a subtree unit, so
/// nested units are not double counted).
struct CachedUnit {
    diags: Vec<RawDiag>,
    labels: Vec<(u32, u32)>,
}

#[derive(Default)]
pub struct DiagCache {
    entries: dashmap::DashMap<CacheKey, std::sync::Arc<CachedUnit>>,
    generation: std::sync::atomic::AtomicU64,
}

impl DiagCache {
    fn sync(&self, bindings: &BindingIndex) {
        use std::sync::atomic::Ordering;
        let current = bindings.generation();
        let previous = self.generation.load(Ordering::Relaxed);
        if previous != current {
            // binding index grew (background scan) or was re-scanned
            if previous != 0 {
                self.entries.clear();
            }
            self.generation.store(current, Ordering::Relaxed);
        }
        if self.entries.len() > CACHE_LIMIT {
            self.entries.clear();
        }
    }

    fn get(&self, key: &CacheKey) -> Option<std::sync::Arc<CachedUnit>> {
        self.entries.get(key).map(|entry| entry.clone())
    }

    fn insert(&self, key: CacheKey, value: CachedUnit) {
        self.entries.insert(key, std::sync::Arc::new(value));
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut hasher = rustc_hash::FxHasher::default();
    hasher.write(bytes);
    hasher.finish()
}

/// Everything needed to check one subtree, minus the output vector.
struct CheckCtx<'a> {
    doc: &'a Document,
    file_path: Option<&'a Path>,
    include_dirs: &'a [PathBuf],
    bindings: &'a BindingIndex,
    cache: Option<&'a DiagCache>,
    file_hash: u64,
    cats: u8,
}

/// Syntax errors, duplicate nodes, unresolved includes and binding violations
/// sorted by position. Whole-feature issues (`status`) are reported at the top
/// of the file.
/// Uncached entry point (tests and one-off callers).
#[cfg_attr(not(test), allow(dead_code))]
pub fn all(
    doc: &Document,
    file_path: Option<&Path>,
    include_dirs: &[PathBuf],
    bindings: &BindingIndex,
    status: &[FeatureIssue],
) -> Vec<Diagnostic> {
    run(doc, file_path, include_dirs, bindings, status, CAT_ALL, None)
}

/// Same, reusing per-subtree results across edits.
pub fn all_cached(
    doc: &Document,
    file_path: Option<&Path>,
    include_dirs: &[PathBuf],
    bindings: &BindingIndex,
    status: &[FeatureIssue],
    cache: &DiagCache,
) -> Vec<Diagnostic> {
    let cached = run(
        doc,
        file_path,
        include_dirs,
        bindings,
        status,
        CAT_ALL,
        Some(cache),
    );
    if std::env::var_os("DTSLS_VERIFY_CACHE").is_some() {
        let full = run(doc, file_path, include_dirs, bindings, status, CAT_ALL, None);
        if cached != full {
            tracing::error!(
                cached = cached.len(),
                full = full.len(),
                "diagnostics cache diverged from a full recompute"
            );
        }
    }
    cached
}

fn run(
    doc: &Document,
    file_path: Option<&Path>,
    include_dirs: &[PathBuf],
    bindings: &BindingIndex,
    status: &[FeatureIssue],
    cats: u8,
    cache: Option<&DiagCache>,
) -> Vec<Diagnostic> {
    // without a populated binding index, binding checks are pointless work —
    // but the feature notices must still get through
    let cats = if cats & CAT_BINDINGS != 0 && bindings.is_empty() {
        cats & !CAT_BINDINGS
    } else {
        cats
    };
    if let Some(cache) = cache {
        cache.sync(bindings);
    }
    if cats == 0 && status.is_empty() {
        return Vec::new();
    }
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
    let mut out: Vec<Diagnostic> = Vec::new();
    for issue in status {
        out.push(Diagnostic {
            range: top.clone(),
            severity: Some(issue.severity),
            source: Some(SOURCE.to_string()),
            message: issue.message(),
            ..Default::default()
        });
    }
    if cats == 0 {
        return out;
    }
    let root = doc.tree.root_node();
    // error recovery is the only source of syntax diagnostics, and
    // `has_error` is a flag: clean files skip the whole scan
    if cats & CAT_SYNTAX != 0 && root.has_error() {
        collect_errors(doc, root, &mut out);
    }
    let ctx = CheckCtx {
        doc,
        file_path,
        include_dirs,
        bindings,
        cache,
        file_hash: hash_bytes(
            file_path
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default()
                .as_bytes(),
        ),
        cats,
    };
    let mut raws: Vec<RawDiag> = Vec::new();
    let mut labels: Vec<(u32, u32)> = Vec::new();
    if cats & CAT_DUPLICATES != 0 {
        check_duplicate_roots(root, doc, &mut out);
    }
    check_node(
        &ctx,
        root,
        false,
        &mut raws,
        (cats & CAT_DUPLICATES != 0).then_some(&mut labels),
    );
    if cats & CAT_DUPLICATES != 0 && !labels.is_empty() {
        labels.sort_unstable();
        let mut seen: FxHashMap<&str, ()> = FxHashMap::default();
        for (start, end) in labels {
            let (start, end) = (start as usize, (end as usize).min(doc.text.len()));
            if start >= end {
                continue;
            }
            // the label token includes its trailing ':'
            let name = &doc.text[start..end.saturating_sub(1)];
            if seen.insert(name, ()).is_some() {
                raws.push(RawDiag {
                    start: start as u32,
                    end: end as u32,
                    severity: DiagnosticSeverity::ERROR,
                    message: format!("duplicate label '{name}'"),
                });
            }
        }
    }
    out.extend(raws.iter().map(|raw| Diagnostic {
        range: Range {
            start: util::byte_to_position(&doc.rope, raw.start as usize),
            end: util::byte_to_position(&doc.rope, raw.end as usize),
        },
        severity: Some(raw.severity),
        source: Some(SOURCE.to_string()),
        message: raw.message.clone(),
        ..Default::default()
    }));
    out.sort_by_key(|d| (d.range.start, d.range.end));
    out.truncate(MAX_DIAGNOSTICS);
    out
}

/// Check one structural node and everything below it. Property subtrees are
/// skipped: they can never contain subnodes, duplicates or includes.
fn check_node(
    ctx: &CheckCtx,
    node: Node,
    in_alternative: bool,
    out: &mut Vec<RawDiag>,
    mut labels: Option<&mut Vec<(u32, u32)>>,
) {
    let k = kinds();
    let kind = node.kind_id();
    if kind == k.property {
        return;
    }
    if node.is_error() || node.is_missing() {
        // malformed region: its contents are not trustworthy
        return;
    }
    let alternative = in_alternative || k.alternatives.contains(&kind);
    if kind == k.subnode {
        check_subnode(ctx, node, alternative, out, labels);
        return;
    }
    if kind == k.preproc_include {
        if ctx.cats & CAT_INCLUDES != 0 {
            include_into(ctx, node, out);
        }
        return;
    }
    if k.containers.contains(&kind) && ctx.cats & CAT_DUPLICATES != 0 && !alternative {
        duplicate_children_into(ctx.doc, node, out);
    }
    // labels on the root node and on overlays are not covered by a subnode
    // unit, so collect them here
    if !alternative
        && ctx.cats & CAT_DUPLICATES != 0
        && (kind == k.devicetree
            || kind == k.node_def
            || node.kind() == "reference"
            || k.containers.contains(&kind))
        && let Some(dst) = labels.as_deref_mut()
    {
        container_labels(ctx.doc, node, dst);
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.is_named() {
                check_node(ctx, child, alternative, out, labels.as_deref_mut());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// A subnode's diagnostics depend only on its own text, so the whole subtree
/// result is memoized (except inside `#else`/`#elif`, where duplicate checks
/// must not run).
fn check_subnode(
    ctx: &CheckCtx,
    node: Node,
    in_alternative: bool,
    out: &mut Vec<RawDiag>,
    labels: Option<&mut Vec<(u32, u32)>>,
) {
    let Some(cache) = ctx.cache.filter(|_| !in_alternative) else {
        check_subnode_uncached(ctx, node, in_alternative, out);
        // no cache in this run (or an `#else` branch): labels still count,
        // but duplicates inside alternatives are not reported
        if !in_alternative
            && ctx.cats & CAT_DUPLICATES != 0
            && let Some(dst) = labels
        {
            subtree_labels(node, dst);
        }
        return;
    };
    let base = node.start_byte() as u32;
    let text = &ctx.doc.text[node.start_byte()..node.end_byte()];
    let key = CacheKey {
        file: ctx.file_hash,
        text: hash_bytes(text.as_bytes()),
        len: text.len() as u32,
        cats: ctx.cats,
    };
    if let Some(hit) = cache.get(&key) {
        out.extend(hit.diags.iter().map(|raw| raw.shifted(base)));
        if let Some(dst) = labels {
            dst.extend(hit.labels.iter().map(|&(start, end)| (start + base, end + base)));
        }
        return;
    }
    let mut local = Vec::new();
    check_subnode_uncached(ctx, node, in_alternative, &mut local);
    // labels of the whole subtree: nested units are not visited once this
    // entry is reused, so they belong to the outermost reachable unit
    let mut abs_labels = Vec::new();
    if ctx.cats & CAT_DUPLICATES != 0 {
        subtree_labels(node, &mut abs_labels);
    }
    let relative = abs_labels
        .iter()
        .map(|&(start, end)| (start - base, end - base))
        .collect();
    cache.insert(
        key,
        CachedUnit {
            diags: local.iter().map(|raw| raw.rebase(base)).collect(),
            labels: relative,
        },
    );
    out.extend(local);
    if let Some(dst) = labels {
        dst.extend(abs_labels);
    }
}

/// Labels defined in a subtree, skipping `#else`/`#elif` branches (we cannot
/// know which branch the preprocessor keeps, so they never report duplicates).
fn subtree_labels(node: Node, out: &mut Vec<(u32, u32)>) {
    let k = kinds();
    let kind = node.kind_id();
    if k.alternatives.contains(&kind) {
        return;
    }
    if kind == k.label {
        out.push((node.start_byte() as u32, node.end_byte() as u32));
        return;
    }
    if node.is_error() || node.is_missing() {
        return;
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.is_named() && child.kind_id() != k.prop_data {
                subtree_labels(child, out);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Labels attached to a container that is not inside a subnode (the root node,
/// overlay bodies) plus the labels of its direct properties.
fn container_labels(_doc: &Document, node: Node, out: &mut Vec<(u32, u32)>) {
    let k = kinds();
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        let child = cursor.node();
        if child.kind_id() == k.label {
            out.push((child.start_byte() as u32, child.end_byte() as u32));
        } else if child.kind_id() == k.property {
            let mut prop_cursor = child.walk();
            if prop_cursor.goto_first_child() {
                loop {
                    let kind_child = prop_cursor.node();
                    if kind_child.kind_id() == k.label {
                        out.push((kind_child.start_byte() as u32, kind_child.end_byte() as u32));
                    }
                    if !prop_cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn check_subnode_uncached(
    ctx: &CheckCtx,
    node: Node,
    in_alternative: bool,
    out: &mut Vec<RawDiag>,
) {
    if ctx.cats & CAT_BINDINGS != 0 {
        check_binding_node(node, ctx.doc, ctx.bindings, out);
    }
    if let Some(body) = util::node_def_of(node) {
        check_node(ctx, body, in_alternative, out, None);
    }
}

/// Walk only the parts of the tree that carry errors: every node knows
/// whether its subtree has one, so clean subtrees are never entered.
fn collect_errors(doc: &Document, root: Node, out: &mut Vec<Diagnostic>) {
    let mut cursor = root.walk();
    collect_errors_node(doc, &mut cursor, out);
}

fn collect_errors_node(
    doc: &Document,
    cursor: &mut tree_sitter::TreeCursor<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let node = cursor.node();
    if node.is_error() {
        out.push(Diagnostic {
            range: error_range(doc, node),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some(SOURCE.to_string()),
            message: format!("syntax error near {}", snippet(doc, node)),
            ..Default::default()
        });
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
    if cursor.goto_first_child() {
        loop {
            if cursor.node().has_error() {
                collect_errors_node(doc, cursor, out);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

#[cfg(test)]
pub fn compute(doc: &Document) -> Vec<Diagnostic> {
    run(doc, None, &[], &BindingIndex::default(), &[], CAT_SYNTAX, None)
}

#[cfg(test)]
pub fn semantic(doc: &Document) -> Vec<Diagnostic> {
    run(doc, None, &[], &BindingIndex::default(), &[], CAT_DUPLICATES, None)
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
        None,
    )
}

/// Test-only view of the binding checks.
#[cfg(test)]
pub fn binding_diagnostics(
    doc: &Document,
    bindings: &BindingIndex,
    file_path: Option<&Path>,
) -> Vec<Diagnostic> {
    run(doc, file_path, &[], bindings, &[], CAT_BINDINGS, None)
}

/// Validate one node against its binding: missing required properties and
/// (for strict, fully resolved bindings) unknown properties are errors; a
/// `compatible` with no matching binding is a warning.
fn check_binding_node(
    node: Node,
    doc: &Document,
    bindings: &BindingIndex,
    out: &mut Vec<RawDiag>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    // one pass over the body: property names, plus child node names because a
    // required entry may be a child node (`required: [ports]`)
    let k = kinds();
    let body = util::node_def_of(node).unwrap_or(node);
    let mut props: Vec<(Node, &str)> = Vec::with_capacity(body.child_count() / 2);
    let mut child_names: Vec<&str> = Vec::new();
    let mut compatible = None;
    let mut cursor = body.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind_id() == k.property
                && let Some(prop_name) = child.child_by_field_name("name")
            {
                let name = util::node_text(doc, prop_name);
                if name == "compatible" {
                    compatible = Some(child);
                }
                props.push((child, name));
            } else if child.kind_id() == k.subnode
                && let Some(child_name) = child.child_by_field_name("name")
            {
                // exact name: `required: [port@0]` must not match `port@1`
                child_names.push(util::node_text(doc, child_name));
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    let has = |name: &str| {
        props.iter().any(|(_, prop)| *prop == name) || child_names.contains(&name)
    };
    let Some(compatible) = compatible else {
        return;
    };
    let Some(compatibles) = compatible_strings_of(doc, compatible) else {
        return;
    };
    let name_start = name_node.start_byte() as u32;
    let name_end = name_node.end_byte() as u32;
    let mut push = |start: u32, end: u32, severity: DiagnosticSeverity, message: String| {
        out.push(RawDiag {
            start,
            end,
            severity,
            message,
        });
    };
    let Some(binding) = bindings.lookup(&compatibles) else {
        push(
            name_start,
            name_end,
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
        .unwrap_or("");
    let node_name = util::node_text(doc, name_node);
    let mut count = 0;
    for required in &binding.required {
        if has(required) {
            continue;
        }
        push(
            name_start,
            name_end,
            DiagnosticSeverity::ERROR,
            format!(
                "missing required property '{required}' for node \
                 '{node_name}' (binding: {file})"
            ),
        );
        count += 1;
    }
    for group in &binding.required_any {
        if group.iter().any(|name| has(name)) {
            continue;
        }
        push(
            name_start,
            name_end,
            DiagnosticSeverity::ERROR,
            format!(
                "missing required property '{}' (or '{}') for node \
                 '{node_name}' (binding: {file})",
                group[0],
                group.get(1).map(String::as_str).unwrap_or("")
            ),
        );
        count += 1;
    }
    if binding.strict && binding.refs_complete {
        for (prop_node, name) in &props {
            if bindings.is_property_allowed(name, &binding) {
                continue;
            }
            push(
                prop_node.start_byte() as u32,
                prop_node.end_byte() as u32,
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
    compatible_strings_of(doc, prop)
}

/// `compatible` strings of an already-located property node.
fn compatible_strings_of(doc: &Document, prop: Node) -> Option<Vec<String>> {
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

fn include_into(ctx: &CheckCtx, node: Node, out: &mut Vec<RawDiag>) {
    let doc = ctx.doc;
    let Some(path_node) = node.child_by_field_name("path") else {
        return;
    };
    let kind = path_node.kind_id();
    let raw = util::node_text(doc, path_node);
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
    if resolved.is_some() {
        return;
    }
    out.push(RawDiag {
        start: node.start_byte() as u32,
        end: node.end_byte() as u32,
        severity: DiagnosticSeverity::ERROR,
        message: format!(
            "unresolved #include \"{target}\" — no include dir matches; \
             open the workspace at the kernel root or set \
             initializationOptions.includePaths"
        ),
    });
}

/// Multiple `/ { }` roots in one file are silently merged by dtc.
fn check_duplicate_roots(node: Node, doc: &Document, out: &mut Vec<Diagnostic>) {
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

/// Flag sibling nodes that define the same name (dtc would silently merge
/// them) or that claim the same unit address under one parent.
fn duplicate_children_into(doc: &Document, node: Node, out: &mut Vec<RawDiag>) {
    let mut seen_names: FxHashMap<&str, ()> = FxHashMap::default();
    let mut seen_addresses: FxHashMap<&str, &str> = FxHashMap::default();
    let mut seen_properties: FxHashMap<&str, ()> = FxHashMap::default();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind_id() == kinds().property
                && let Some(name_node) = child.child_by_field_name("name")
            {
                // dtc is strict about this: duplicate_property_names is an error
                let name = util::node_text(doc, name_node);
                if seen_properties.insert(name, ()).is_some() {
                    out.push(RawDiag {
                        start: name_node.start_byte() as u32,
                        end: name_node.end_byte() as u32,
                        severity: DiagnosticSeverity::ERROR,
                        message: format!("duplicate property '{name}'"),
                    });
                }
            } else if child.kind_id() == kinds().subnode
                && let Some(name_node) = child.child_by_field_name("name")
            {
                let full = util::node_text(doc, name_node);
                let (name, address) = match full.split_once('@') {
                    Some((name, addr)) => (name, Some(addr)),
                    None => (full, None),
                };
                if seen_names.insert(full, ()).is_some() {
                    out.push(RawDiag {
                        start: name_node.start_byte() as u32,
                        end: name_node.end_byte() as u32,
                        severity: DiagnosticSeverity::ERROR,
                        message: format!("duplicate node '{full}'"),
                    });
                }
                if let Some(addr) = address {
                    match seen_addresses.get(addr) {
                        Some(first_name) if *first_name != name => {
                            // Legitimate on some SoCs (e.g. Qualcomm GENI: one
                            // serial engine, several protocol nodes per
                            // address), so this is a warning, not an error.
                            out.push(RawDiag {
                                start: name_node.start_byte() as u32,
                                end: name_node.end_byte() as u32,
                                severity: DiagnosticSeverity::WARNING,
                                message: format!(
                                    "node '{full}' shares address with '{first_name}@{addr}'"
                                ),
                            });
                        }
                        None => {
                            seen_addresses.insert(addr, name);
                        }
                        _ => {}
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
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
    // scan the mirrored text directly: ropey random access is O(log n) and
    // an unterminated file makes this region span everything up to EOF
    let bytes = doc.text.as_bytes();
    let start_byte = start_byte.min(bytes.len());
    let line_start = doc.text[..start_byte].rfind('\n').map_or(0, |pos| pos + 1);
    let mut stack: Vec<(usize, usize)> = Vec::new(); // (byte, indent width)
    let mut suspects: Vec<usize> = Vec::new();
    let mut in_comment = false;
    let mut line_begin = line_start;

    while line_begin < bytes.len() {
        let line_end = doc.text[line_begin..]
            .find('\n')
            .map_or(bytes.len(), |pos| line_begin + pos);

        let mut indent = 0;
        let mut byte = line_begin;
        while byte < line_end {
            match bytes[byte] {
                b'\t' => indent += 8 - (indent % 8),
                b' ' => indent += 1,
                _ => break,
            }
            byte += 1;
        }

        let mut in_string = false;
        while byte < line_end {
            let b = bytes[byte];
            if in_comment {
                if b == b'*' && byte + 1 < line_end && bytes[byte + 1] == b'/' {
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
                    b'/' if byte + 1 < line_end && bytes[byte + 1] == b'*' => {
                        in_comment = true;
                        byte += 1;
                    }
                    b'/' if byte + 1 < line_end && bytes[byte + 1] == b'/' => break,
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
        line_begin = line_end + 1;
    }

    suspects.extend(stack.into_iter().map(|(pos, _)| pos));
    suspects.into_iter().max()
}

fn snippet(doc: &Document, node: Node) -> String {
    let start = node.start_byte();
    let end = (start + 80).min(node.end_byte()).min(doc.text.len());
    let text = &doc.text[start..end];
    let first_line = text.lines().next().unwrap_or("");
    let short: String = first_line.chars().take(32).collect();
    format!("{short:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::scan;
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
    #[ignore]
    fn bench_diagnostics() {
        let path = Path::new("/linux-7.2.2/arch/arm64/boot/dts/qcom/sm8650.dtsi");
        let text = std::fs::read_to_string(path).unwrap();
        let doc = Document::open(&text, 1);
        let includes = vec![PathBuf::from("/linux-7.2.2/include")];
        let iterations = 200;

        let bindings = BindingIndex::default();
        let started = std::time::Instant::now();
        scan(
            &bindings,
            Path::new("/linux-7.2.2/Documentation/devicetree/bindings"),
        );
        println!(
            "bindings: {} scanned in {:?}",
            bindings.len(),
            started.elapsed()
        );

        let empty = BindingIndex::default();
        for (label, index, cats) in [
            ("walk only (no checks)", &empty, 0u8),
            ("syntax", &empty, CAT_SYNTAX),
            ("duplicates", &empty, CAT_DUPLICATES),
            ("includes", &empty, CAT_INCLUDES),
            ("bindings (empty index)", &empty, CAT_BINDINGS),
            ("bindings (kernel index)", &bindings, CAT_BINDINGS),
            ("all (kernel index)", &bindings, CAT_ALL),
        ] {
            let diags = run(&doc, Some(path), &includes, index, &[], cats, None);
            let started = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(run(&doc, Some(path), &includes, index, &[], cats, None));
            }
            println!(
                "{label}: {:?}/iter, {} diagnostics",
                started.elapsed() / iterations,
                diags.len()
            );
        }
    }

    #[test]
    fn cached_diagnostics_match_full_recompute() {
        use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, VersionedTextDocumentIdentifier};
        let text = "/dts-v1/;\n\n/ {\n    lbl: widget@1000 {\n        compatible = \"acme,widget\";\n        reg = <0x0 0x1000>;\n        bogus = <1>;\n        reg = <0x1 0x1000>;\n    };\n\n    lbl: widget@2000 {\n        compatible = \"acme,widget\";\n        reg = <0x0 0x2000>;\n    };\n};\n";
        let mut doc = Document::open(text, 0);
        let cache = DiagCache::default();
        let bindings = BindingIndex::default();
        bindings.insert_for_test(crate::bindings::Binding {
            uri: tower_lsp::lsp_types::Url::parse("file:///widget.yaml").unwrap(),
            singles: vec!["acme,widget".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "reg", "clock-names"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            required: vec!["compatible".to_string(), "reg".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: Vec::new(),
        });
        let check = |doc: &Document| {
            let cached = all_cached(doc, None, &[], &bindings, &[], &cache);
            let full = all(doc, None, &[], &bindings, &[]);
            assert_eq!(cached, full, "cached diagnostics diverged from a full recompute");
            cached
        };
        let initial = check(&doc);
        assert!(
            initial.iter().any(|d| d.message.contains("not allowed")),
            "diags: {initial:?}"
        );

        let edits: [(&str, VersionedTextDocumentIdentifier, TextDocumentContentChangeEvent); 3] = {
            let change = |line: u32, character: u32, text: &str| TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position { line, character },
                    end: Position { line, character },
                }),
                range_length: None,
                text: text.to_string(),
            };
            let id = |version| VersionedTextDocumentIdentifier {
                uri: tower_lsp::lsp_types::Url::parse("file:///t.dts").unwrap(),
                version,
            };
            [
                ("insert property", id(1), change(6, 0, "        clock-names = \"a\";\n")),
                ("insert duplicate node", id(2), change(9, 0, "    widget@1000 {\n    };\n\n")),
                ("remove required", id(3), change(4, 0, "")),
            ]
        };
        for (label, id, edit) in edits {
            doc.apply_changes(
                &[TextDocumentContentChangeEvent {
                    range: edit.range,
                    range_length: None,
                    text: edit.text,
                }],
                id.version,
            );
            let diags = check(&doc);
            println!("{label}: {} diagnostics", diags.len());
        }
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

    fn irq_binding() -> crate::bindings::Binding {
        crate::bindings::Binding {
            uri: tower_lsp::lsp_types::Url::parse("file:///irq.yaml").unwrap(),
            singles: vec!["acme,irq".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "interrupts", "interrupts-extended"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            required: vec!["compatible".to_string()],
            required_any: vec![vec![
                "interrupts".to_string(),
                "interrupts-extended".to_string(),
            ]],
            strict: true,
            refs_complete: true,
            patterns: Vec::new(),
        }
    }

    #[test]
    fn interrupts_extended_satisfies_required_interrupts() {
        // dt-schema rewrites `required: [interrupts]` into "interrupts or
        // interrupts-extended"
        for (label, interrupts) in [
            (
                "interrupts",
                "        interrupts = <0 5 4>;\n",
            ),
            (
                "interrupts-extended",
                "        interrupts-extended = <&intc 0 5 4>;\n",
            ),
        ] {
            let text = format!(
                "/dts-v1/;\n\n/ {{\n    irq@1000 {{\n        compatible = \"acme,irq\";\n{interrupts}    }};\n}};\n"
            );
            let doc = Document::open(&text, 0);
            let bindings = BindingIndex::default();
            bindings.insert_for_test(irq_binding());
            let diags = binding_diagnostics(&doc, &bindings, None);
            assert!(diags.is_empty(), "{label}: {diags:?}");
        }

        // neither form: one error naming both alternatives
        let text = "/dts-v1/;\n\n/ {\n    irq@1000 {\n        compatible = \"acme,irq\";\n    };\n};\n";
        let doc = Document::open(text, 0);
        let bindings = BindingIndex::default();
        bindings.insert_for_test(irq_binding());
        let diags = binding_diagnostics(&doc, &bindings, None);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("'interrupts' (or 'interrupts-extended')"));
    }

    #[test]
    fn child_node_satisfies_a_required_entry() {
        let text = "/dts-v1/;\n\n/ {\n    disp@2000 {\n        compatible = \"acme,disp\";\n        ports {\n            port {\n            };\n        };\n    };\n};\n";
        let doc = Document::open(text, 0);
        let bindings = BindingIndex::default();
        bindings.insert_for_test(crate::bindings::Binding {
            uri: tower_lsp::lsp_types::Url::parse("file:///disp.yaml").unwrap(),
            singles: vec!["acme,disp".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "ports"].into_iter().map(str::to_string).collect(),
            required: vec!["compatible".to_string(), "ports".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: Vec::new(),
        });
        let diags = binding_diagnostics(&doc, &bindings, None);
        assert!(diags.is_empty(), "{diags:?}");

        let text = "/dts-v1/;\n\n/ {\n    disp@2000 {\n        compatible = \"acme,disp\";\n    };\n};\n";
        let doc = Document::open(text, 0);
        let bindings = BindingIndex::default();
        bindings.insert_for_test(crate::bindings::Binding {
            uri: tower_lsp::lsp_types::Url::parse("file:///disp.yaml").unwrap(),
            singles: vec!["acme,disp".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "ports"].into_iter().map(str::to_string).collect(),
            required: vec!["compatible".to_string(), "ports".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: Vec::new(),
        });
        let diags = binding_diagnostics(&doc, &bindings, None);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("missing required property 'ports'"));
    }

    #[test]
    fn duplicate_properties_are_errors() {
        let text = "/dts-v1/;\n\n/ {\n    node@1 {\n        reg = <1>;\n        reg = <2>;\n    };\n};\n";
        let diags = semantic(&Document::open(text, 0));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("duplicate property 'reg'"));
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));

        // same name in different nodes is fine
        let text = "/dts-v1/;\n\n/ {\n    a {\n        reg = <1>;\n    };\n    b {\n        reg = <2>;\n    };\n};\n";
        assert!(semantic(&Document::open(text, 0)).is_empty());
    }

    #[test]
    fn duplicate_labels_are_errors() {
        // node labels
        let text = "/dts-v1/;\n\n/ {\n    a: node@1 {\n    };\n    a: node@2 {\n    };\n};\n";
        let diags = semantic(&Document::open(text, 0));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("duplicate label 'a'"));
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));

        // property labels
        let text = "/dts-v1/;\n\n/ {\n    node {\n        l: p = <1>;\n        l: q = <2>;\n    };\n};\n";
        let diags = semantic(&Document::open(text, 0));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("duplicate label 'l'"));

        // distinct labels are fine
        let text = "/dts-v1/;\n\n/ {\n    a: node@1 {\n    };\n    b: node@2 {\n    };\n};\n";
        assert!(semantic(&Document::open(text, 0)).is_empty());
    }

    #[test]
    fn duplicate_labels_in_alternative_branches_are_ignored() {
        let text = "/dts-v1/;\n\n/ {\n    node {\n#ifdef A\n        l: p = <1>;\n#else\n        l: q = <2>;\n#endif\n    };\n};\n";
        let diags = semantic(&Document::open(text, 0));
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn addressed_child_names_match_exactly() {
        let bindings = BindingIndex::default();
        bindings.insert_for_test(crate::bindings::Binding {
            uri: tower_lsp::lsp_types::Url::parse("file:///tcon.yaml").unwrap(),
            singles: vec!["acme,tcon".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "ports"].into_iter().map(str::to_string).collect(),
            required: vec!["compatible".to_string(), "ports".to_string()],
            required_any: Vec::new(),
            strict: false,
            refs_complete: true,
            patterns: Vec::new(),
        });
        let text = "/dts-v1/;\n\n/ {\n    tcon@1000 {\n        compatible = \"acme,tcon\";\n        ports {\n            port@0 {\n            };\n            port {\n            };\n        };\n    };\n};\n";
        let doc = Document::open(text, 0);
        let diags = binding_diagnostics(&doc, &bindings, None);
        assert!(diags.is_empty(), "{diags:?}");
    }
}

