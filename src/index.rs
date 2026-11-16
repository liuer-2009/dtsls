use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::RwLock;

use tower_lsp::lsp_types::{Range, Url};
use walkdir::WalkDir;

use crate::analysis;
use crate::completion::LabelMap;
use crate::document::Document;
use crate::util;

/// A label definition: where to jump (file + node span) and the node's path.
#[derive(Clone, Debug)]
pub struct LabelDef {
    pub range: Range,
    /// Node path, e.g. `/soc/serial@1000`; rename (M2) reports this as context.
    #[allow(dead_code)]
    pub path: String,
}

/// A `&label` usage somewhere in the workspace.
#[derive(Clone, Debug)]
pub struct LabelRef {
    pub label: String,
    pub range: Range,
}

/// Per-file contribution table. Built off the request path; requests only
/// ever read finished entries.
pub struct FileIndex {
    pub version: i32,
    /// label -> definition (this file only)
    pub labels: HashMap<String, LabelDef>,
    /// every `&label` usage in this file
    pub refs: Vec<LabelRef>,
    /// resolved quoted `#include` targets
    pub includes: Vec<Url>,
    /// label -> (node path, file name) for completion (own + includes merged)
    pub completion_labels: LabelMap,
}

#[derive(Default)]
pub struct Index {
    files: RwLock<HashMap<Url, FileIndex>>,
}

impl Index {
    pub fn with_file<R>(&self, uri: &Url, f: impl FnOnce(Option<&FileIndex>) -> R) -> R {
        let files = self.files.read().unwrap();
        f(files.get(uri))
    }

    pub fn insert(&self, uri: Url, entry: FileIndex) {
        let mut files = self.files.write().unwrap();
        match files.get(&uri) {
            // Live buffers carry versions >= 1; never let a disk scan (v0)
            // or a stale rebuild overwrite them.
            Some(existing) if existing.version > entry.version => {}
            _ => {
                files.insert(uri, entry);
            }
        }
    }

    pub fn has(&self, uri: &Url) -> bool {
        self.files.read().unwrap().contains_key(uri)
    }

    pub fn includes_of(&self, uri: &Url) -> Vec<Url> {
        self.with_file(uri, |fi| fi.map(|f| f.includes.clone()).unwrap_or_default())
    }

    pub fn all_label_defs(&self, label: &str) -> Vec<(Url, LabelDef)> {
        let files = self.files.read().unwrap();
        let mut out = Vec::new();
        for (uri, fi) in files.iter() {
            if let Some(def) = fi.labels.get(label) {
                out.push((uri.clone(), def.clone()));
            }
        }
        out
    }

    pub fn label_references(&self, label: &str, cap: usize) -> Vec<(Url, Range)> {
        let files = self.files.read().unwrap();
        let mut out = Vec::new();
        for (uri, fi) in files.iter() {
            for r in &fi.refs {
                if r.label == label {
                    out.push((uri.clone(), r.range));
                    if out.len() >= cap {
                        return out;
                    }
                }
            }
        }
        out
    }
}

/// Build one file's contribution table and recursively its quoted includes.
/// Disk-parsed entries carry version 0; live buffers are rebuilt by the
/// request loop with their document version.
pub fn build_and_insert(
    index: &Index,
    uri: &Url,
    text: &str,
    version: i32,
    roots: &[PathBuf],
    depth: usize,
) {
    let mut visited = HashSet::new();
    build_recursive(index, uri, text, version, roots, &mut visited, depth);
}

fn build_recursive(
    index: &Index,
    uri: &Url,
    text: &str,
    version: i32,
    roots: &[PathBuf],
    visited: &mut HashSet<Url>,
    depth: usize,
) {
    if depth > 8 || !visited.insert(uri.clone()) {
        return;
    }
    let doc = Document::open(text, 0);
    let mut labels = HashMap::new();
    let mut refs = Vec::new();
    let mut raw_includes = Vec::new();
    let mut completion = LabelMap::new();
    walk_file(&doc, &mut labels, &mut refs, &mut raw_includes, &mut completion);

    let mut includes = Vec::new();
    if depth <= 8 {
        let file_dir = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_default();
        for relative in &raw_includes {
            for base in std::iter::once(&file_dir).chain(roots.iter()) {
                let candidate = base.join(relative);
                let Ok(child_text) = std::fs::read_to_string(&candidate) else {
                    continue;
                };
                let Ok(child_uri) = Url::from_file_path(&candidate) else {
                    continue;
                };
                build_recursive(index, &child_uri, &child_text, 0, roots, visited, depth + 1);
                // Merge the include's labels into completion; tag the include's
                // own labels (empty file name) with its file name.
                index.with_file(&child_uri, |fi| {
                    if let Some(fi) = fi {
                        for (label, (path, file)) in &fi.completion_labels {
                            let file = if file.is_empty() {
                                candidate
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            } else {
                                file.clone()
                            };
                            completion.entry(label.clone()).or_insert((path.clone(), file));
                        }
                    }
                });
                includes.push(child_uri);
                break;
            }
        }
    }

    index.insert(
        uri.clone(),
        FileIndex {
            version,
            labels,
            refs,
            includes,
            completion_labels: completion,
        },
    );
}

fn walk_file(
    doc: &Document,
    labels: &mut HashMap<String, LabelDef>,
    refs: &mut Vec<LabelRef>,
    raw_includes: &mut Vec<String>,
    completion: &mut LabelMap,
) {
    walk_node(doc, doc.tree.root_node(), labels, refs, raw_includes, completion);
}

fn walk_node(
    doc: &Document,
    node: tree_sitter::Node,
    labels: &mut HashMap<String, LabelDef>,
    refs: &mut Vec<LabelRef>,
    raw_includes: &mut Vec<String>,
    completion: &mut LabelMap,
) {
    match node.kind() {
        "subnode" => {
            for i in 0..node.child_count() {
                let Some(child) = node.child(i as u32) else { continue };
                if child.kind() == "label" {
                    // Label token includes the trailing ':'.
                    let end = child.end_byte().saturating_sub(1);
                    let label = doc.slice(child.start_byte()..end).to_string();
                    let path = analysis::node_path(doc, node);
                    labels
                        .entry(label.clone())
                        .or_insert_with(|| LabelDef {
                            range: byte_range(doc, node),
                            path: path.clone(),
                        });
                    completion.entry(label).or_insert_with(|| (path, String::new()));
                }
            }
        }
        "reference" => {
            if let Some(label_ref) = util::find_child(node, "label_ref") {
                let label = util::node_text(doc, label_ref)
                    .strip_prefix('&')
                    .unwrap_or_default()
                    .to_string();
                if !label.is_empty() {
                    refs.push(LabelRef {
                        label,
                        range: byte_range(doc, node),
                    });
                }
            }
            // `&{/path}` refs are a separate lookup, not handled yet.
            return;
        }
        "preproc_include" => {
            if let Some(path) = node.child_by_field_name("path")
                && path.kind() == "preproc_string_literal"
            {
                raw_includes
                    .push(util::node_text(doc, path).trim_matches('"').to_string());
            }
        }
        _ => {}
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            walk_node(doc, child, labels, refs, raw_includes, completion);
        }
    }
}

fn byte_range(doc: &Document, node: tree_sitter::Node) -> Range {
    Range {
        start: util::byte_to_position(&doc.rope, node.start_byte()),
        end: util::byte_to_position(&doc.rope, node.end_byte()),
    }
}

/// One-shot background scan of the workspace roots. Skips open buffers and
/// already-indexed files; entries built here carry version 0.
pub fn scan_workspace(index: &Index, roots: &[PathBuf], skip: &HashSet<Url>) {
    let started = std::time::Instant::now();
    let mut targets: Vec<(Url, PathBuf)> = Vec::new();
    for root in roots {
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            match path.extension().and_then(|e| e.to_str()) {
                Some("dts") | Some("dtsi") | Some("dtso") | Some("overlay") => {}
                _ => continue,
            }
            let Ok(uri) = Url::from_file_path(path) else {
                continue;
            };
            if skip.contains(&uri) || index.has(&uri) {
                continue;
            }
            targets.push((uri, path.to_path_buf()));
        }
    }

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(targets.len().max(1));
    let chunk_size = targets.len().div_ceil(workers);
    let roots = roots.to_vec();
    std::thread::scope(|scope| {
        for chunk in targets.chunks(chunk_size) {
            let index = &index;
            let roots = &roots;
            scope.spawn(move || {
                let mut visited = HashSet::new();
                for (uri, path) in chunk {
                    let Ok(text) = std::fs::read_to_string(path) else {
                        continue;
                    };
                    build_recursive(index, uri, &text, 0, roots, &mut visited, 0);
                }
            });
        }
    });
    info!(
        files = targets.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "workspace index scan"
    );
}

/// Resolve a label from `from`'s point of view: own file first, then the
/// translation unit (transitive includes), then the whole workspace.
pub fn resolve_label(index: &Index, from: &Url, label: &str) -> Option<(Url, LabelDef)> {
    if let Some(def) = index.with_file(from, |fi| fi.and_then(|f| f.labels.get(label).cloned())) {
        return Some((from.clone(), def));
    }
    let mut visited = HashSet::from([from.clone()]);
    let mut queue = index.includes_of(from);
    while let Some(uri) = queue.pop() {
        if !visited.insert(uri.clone()) {
            continue;
        }
        if let Some(def) =
            index.with_file(&uri, |fi| fi.and_then(|f| f.labels.get(label).cloned()))
        {
            return Some((uri, def));
        }
        queue.extend(index.includes_of(&uri));
    }
    let mut all = index.all_label_defs(label);
    all.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    all.into_iter().next()
}

use tracing::info;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(name: &str) -> (Url, String) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/includes")
            .join(name);
        let text = std::fs::read_to_string(&path).unwrap();
        (Url::from_file_path(path).unwrap(), text)
    }

    #[test]
    fn goto_definition_across_includes() {
        let index = Index::default();
        let (board_uri, board_text) = fixture("board.dts");
        let (soc_uri, soc_text) = fixture("soc.dtsi");
        build_and_insert(&index, &soc_uri, &soc_text, 0, &[], 0);
        build_and_insert(&index, &board_uri, &board_text, 0, &[], 0);

        // own file
        let (uri, def) = resolve_label(&index, &board_uri, "board_label").unwrap();
        assert_eq!(uri, board_uri);
        assert_eq!(def.path, "/node");
        // via include edge
        let (uri, def) = resolve_label(&index, &board_uri, "uart0").unwrap();
        assert_eq!(uri, soc_uri);
        assert_eq!(def.path, "/soc/serial@1000");
        // unknown label
        assert!(resolve_label(&index, &board_uri, "nope").is_none());
    }

    #[test]
    fn references_are_collected() {
        let index = Index::default();
        let (board_uri, board_text) = fixture("board.dts");
        let (soc_uri, soc_text) = fixture("soc.dtsi");
        build_and_insert(&index, &soc_uri, &soc_text, 0, &[], 0);
        build_and_insert(&index, &board_uri, &board_text, 0, &[], 0);
        let refs = index.label_references("uart0", 100);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, board_uri);
    }
}
