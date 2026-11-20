use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::request::{GotoDeclarationParams, GotoDeclarationResponse};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{debug, error, info, warn};

use crate::bindings::{self, BindingIndex};
use crate::completion;
use crate::document::Document;
use crate::hints;
use crate::workspace::{self, TuData, Workspace};

/// How many board contexts keep their unit and preprocessed snapshot. The
/// least recently used one is evicted beyond this (its files are re-analysed
/// when it is next used).
const MAX_CONTEXTS: usize = 8;
use crate::util;
use crate::status::FeatureIssue;
use crate::{diagnostics, semantic_tokens, symbols};

pub struct Backend {
    client: Client,
    documents: Arc<RwLock<HashMap<Url, Document>>>,
    root_dirs: RwLock<Vec<PathBuf>>,
    /// Boards, include edges and the last checked unit per root (the old
    /// workspace index, now derived from translation units).
    workspace: Arc<Workspace>,
    /// compatible -> devicetree binding, built in background at startup.
    bindings: Arc<BindingIndex>,
    /// Whole-feature conditions (unavailable = error, degraded = warning),
    /// reported at the top of every file.
    status: RwLock<Vec<FeatureIssue>>,
    /// Per-subtree diagnostic results, so a keystroke only rechecks the
    /// subtree it touched.
    diag_cache: Arc<diagnostics::DiagCache>,
    /// Last per-file diagnostics, so translation-unit results can be merged
    /// with them instead of replacing them.
    file_diags: Arc<RwLock<HashMap<Url, Vec<Diagnostic>>>>,
    /// Structural diagnostics produced by the unit covering each file.
    tu_file_diags: Arc<RwLock<HashMap<Url, Vec<Diagnostic>>>>,
    /// Coalesced translation-unit re-runs, per root.
    tu_pending: Arc<RwLock<HashMap<Url, u64>>>,
    /// Line of `compatible:` per binding file (go-to-binding target).
    binding_lines: Arc<RwLock<HashMap<Url, u32>>>,
    /// Board whose unit is the active analysis context (dts-lsp's
    /// `activeContext`): a file included by it is analysed in its context.
    active_board: RwLock<Option<PathBuf>>,
    /// Analysis context per open document (the board root, or the file itself
    /// when it is opened as a standalone dts).
    doc_context: Arc<RwLock<HashMap<Url, PathBuf>>>,
    /// Most recently opened/edited document: refreshed diagnostics for it are
    /// published first after the binding scan.
    last_active: Arc<RwLock<Option<Url>>>,
    /// Live-buffer labels per document version (analysing the tree on every
    /// completion request costs milliseconds on large files).
    same_file_labels: Arc<RwLock<HashMap<Url, (i32, Arc<Vec<CompletionItem>>)>>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(RwLock::new(HashMap::new())),
            root_dirs: RwLock::new(Vec::new()),
            workspace: Arc::new(Workspace::default()),
            bindings: Arc::new(BindingIndex::default()),
            status: RwLock::new(Vec::new()),
            diag_cache: Arc::new(diagnostics::DiagCache::default()),
            file_diags: Arc::new(RwLock::new(HashMap::new())),
            tu_file_diags: Arc::new(RwLock::new(HashMap::new())),
            tu_pending: Arc::new(RwLock::new(HashMap::new())),
            binding_lines: Arc::new(RwLock::new(HashMap::new())),
            active_board: RwLock::new(None),
            doc_context: Arc::new(RwLock::new(HashMap::new())),
            last_active: Arc::new(RwLock::new(None)),
            same_file_labels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Include dirs for diagnostics (file-relative resolution is separate).
    fn include_dirs(&self) -> Vec<PathBuf> {
        self.root_dirs.read().unwrap().clone()
    }

    /// Roots whose translation unit contains `uri`: a `.dts` is its own root,
    /// anything else is looked up through the reverse include edges (falling
    /// back to the file itself so a lone `.dtsi` still gets its closure
    /// checked).
    /// Context of a document: the board whose unit contains it, or the file
    /// itself when it was not opened inside a board (dts-lsp's ad-hoc
    /// context). A board opened later adopts standalone documents that are in
    /// its include closure.
    fn context_path(&self, uri: &Url) -> Option<PathBuf> {
        let path = uri.to_file_path().ok()?;
        if path.extension().and_then(|ext| ext.to_str()) == Some("dts") {
            return Some(path);
        }
        if let Some(board) = self.active_board.read().unwrap().clone()
            && self.workspace.closure_contains(&board, &path)
        {
            return Some(board);
        }
        Some(path)
    }

    /// Contexts that must not be evicted: every open document's context plus
    /// the active board.
    fn protected_contexts(&self) -> HashSet<PathBuf> {
        let mut protected: HashSet<PathBuf> = self
            .doc_context
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect();
        if let Some(board) = self.active_board.read().unwrap().clone() {
            protected.insert(board);
        }
        protected
    }

    fn prune_contexts(&self) {
        let protected = self.protected_contexts();
        self.workspace.prune(MAX_CONTEXTS, &protected);
    }

    /// Units to (re)check for an edit of `uri`: its own context, plus open
    /// boards that include it so their diagnostics stay fresh.
    fn analysis_roots(&self, uri: &Url) -> Vec<(Url, PathBuf)> {
        let mut roots: Vec<(Url, PathBuf)> = Vec::new();
        let context = self
            .doc_context
            .read()
            .unwrap()
            .get(uri)
            .cloned()
            .or_else(|| self.context_path(uri));
        if let Some(context) = context
            && let Ok(root_uri) = Url::from_file_path(&context)
        {
            roots.push((root_uri, context));
        }
        if let Some(path) = uri.to_file_path().ok() {
            let open = self.documents.read().unwrap();
            for root in self.workspace.roots_for(&path) {
                let Ok(root_uri) = Url::from_file_path(&root) else {
                    continue;
                };
                if roots.iter().any(|(uri, _)| *uri == root_uri) || !open.contains_key(&root_uri) {
                    continue;
                }
                roots.push((root_uri, root));
            }
        }
        roots.truncate(4);
        roots
    }

    /// The unit that covers `uri`, when one has been checked already.
    fn unit_for(&self, uri: &Url) -> Option<Arc<TuData>> {
        let context = self
            .doc_context
            .read()
            .unwrap()
            .get(uri)
            .cloned()
            .or_else(|| self.context_path(uri))?;
        let root_uri = Url::from_file_path(&context).ok()?;
        self.workspace.unit_for_uri(&root_uri)
    }

    /// `&label` completion. A checked unit is authoritative (it is what the
    /// preprocessor actually produced): labels expanded from macros are
    /// present and labels in disabled `#ifdef` branches are not. The live
    /// buffer is merged on top so a label just typed in this file is offered
    /// before the next run.
    fn same_file_label_items(&self, uri: &Url, doc: &Document) -> Arc<Vec<CompletionItem>> {
        if let Some((version, items)) = self.same_file_labels.read().unwrap().get(uri)
            && *version == doc.version
        {
            return Arc::clone(items);
        }
        let items = Arc::new(completion::same_file_items(doc));
        self.same_file_labels
            .write()
            .unwrap()
            .insert(uri.clone(), (doc.version, Arc::clone(&items)));
        items
    }

    fn label_items(&self, uri: &Url, doc: &Document, prefix: &str) -> Vec<CompletionItem> {
        if let Some(data) = self.unit_for(uri) {
            let current = uri.to_file_path().ok();
            let mut items: Vec<CompletionItem> = data
                .labels
                .iter()
                .filter(|(name, _)| name.starts_with(prefix))
                .map(|(name, label)| {
                    let file = match &current {
                        Some(path) if path == &label.file => String::new(),
                        _ => label
                            .file
                            .file_name()
                            .map(|file| file.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    };
                    completion::label_item(name, &label.path, &file)
                })
                .collect();
            let mut seen: HashSet<String> =
                items.iter().map(|item| item.label.clone()).collect();
            for item in self.same_file_label_items(uri, doc).iter() {
                if item.label.starts_with(prefix) && seen.insert(item.label.clone()) {
                    items.push(item.clone());
                }
            }
            items.sort_by(|a, b| a.label.cmp(&b.label));
            return items;
        }
        // no unit checked yet: the live buffer is all we can offer
        self.same_file_label_items(uri, doc)
            .iter()
            .filter(|item| item.label.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// Re-check a translation unit off the main loop. Coalesced like the index
    /// rebuild: typing only triggers one run once the edits settle.
    fn schedule_tu(&self, root_uri: Url, root_path: PathBuf) {
        debug!(root = %root_uri, "tu scheduled");
        let generation = {
            let mut pending = self.tu_pending.write().unwrap();
            let slot = pending.entry(root_uri.clone()).or_insert(0);
            *slot += 1;
            *slot
        };
        let documents = Arc::clone(&self.documents);
        let file_diags = Arc::clone(&self.file_diags);
        let tu_file_diags = Arc::clone(&self.tu_file_diags);
        let pending = Arc::clone(&self.tu_pending);
        let workspace = Arc::clone(&self.workspace);
        let doc_context = Arc::clone(&self.doc_context);
        let last_active = Arc::clone(&self.last_active);
        let include_dirs = self.include_dirs();
        let client = self.client.clone();
        let task_root = root_uri.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if pending.read().unwrap().get(&task_root) != Some(&generation) {
                return;
            }
            let overlays_docs = Arc::clone(&documents);
            let blocking_workspace = Arc::clone(&workspace);
            let publish_root = root_path.clone();
            let blocking_root = task_root.clone();
            let root_path_task = root_path.clone();
            let started = std::time::Instant::now();
            let tu_result = tokio::task::spawn_blocking(move || {
                // clangd's VFS equivalent: every open buffer overrides the
                // on-disk file inside the preprocessor run
                let overlays: BTreeMap<PathBuf, String> = {
                    let docs = overlays_docs.read().unwrap();
                    docs.iter()
                        .filter_map(|(uri, doc)| {
                            let path = uri.to_file_path().ok()?;
                            let path = std::fs::canonicalize(&path).unwrap_or(path);
                            Some((path, doc.text.clone()))
                        })
                        .collect()
                };
                let result = crate::tu::preprocess(&crate::tu::TuInput {
                    root: &root_path_task,
                    include_dirs: &include_dirs,
                    overlays: &overlays,
                });
                let previous = blocking_workspace.take_snapshot_uri(&blocking_root);
                let mut doc = match previous {
                    Some((doc, _files)) => doc,
                    None => Document::open(&result.text, 0),
                };
                doc.update_text(&result.text);
                let output = tu_output(&result, &doc);
                blocking_workspace.put_snapshot_uri(
                    blocking_root.clone(),
                    doc,
                    result.files.clone(),
                );
                output
            })
            .await;
            debug!(
                root = %task_root,
                elapsed_us = started.elapsed().as_micros() as u64,
                files = tu_result.as_ref().map(|out| out.per_file.len()).unwrap_or(0),
                "translation unit checked"
            );
            let output = match tu_result {
                Ok(output) => output,
                Err(error) => {
                    // a panic or cancellation inside the analysis must reach
                    // the user, never disappear
                    warn!(root = %task_root, %error, "translation unit check failed");
                    {
                        client
                            .publish_diagnostics(
                                task_root.clone(),
                                vec![Diagnostic {
                                    range: Range::default(),
                                    severity: Some(DiagnosticSeverity::ERROR),
                                    source: Some("dtsls".to_string()),
                                    message: format!(
                                        "internal: translation unit analysis failed ({error});                                          diagnostics for this file are incomplete"
                                    ),
                                    ..Default::default()
                                }],
                                None,
                            )
                            .await;
                    }
                    return;
                }
            };
            workspace.store_unit_uri(task_root.clone(), Arc::new(output.data));
            let protected: HashSet<PathBuf> =
                doc_context.read().unwrap().values().cloned().collect();
            workspace.prune(MAX_CONTEXTS, &protected);
            let per_file = output.per_file;
            let root_canonical = std::fs::canonicalize(&publish_root)
                .unwrap_or_else(|_| publish_root.clone());
            let mut open_docs: Vec<(Url, i32)> = {
                let docs = documents.read().unwrap();
                docs.iter().map(|(uri, doc)| (uri.clone(), doc.version)).collect()
            };
            let active = last_active.read().unwrap().clone();
            if let Some(active) = active {
                open_docs.sort_by_key(|(uri, _)| uri != &active);
            }
            for (uri, version) in open_docs {
                let in_context = {
                    let contexts = doc_context.read().unwrap();
                    contexts
                        .get(&uri)
                        .map(|context| {
                            std::fs::canonicalize(context).unwrap_or_else(|_| context.clone())
                                == root_canonical
                        })
                        .unwrap_or(false)
                };
                if !in_context {
                    continue;
                }
                let structural = uri
                    .to_file_path()
                    .ok()
                    .map(|path| {
                        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                        per_file
                            .get(&canonical)
                            .or_else(|| per_file.get(&canonical))
                            .cloned()
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                let mut merged = file_diags
                    .read()
                    .unwrap()
                    .get(&uri)
                    .cloned()
                    .unwrap_or_default();
                merge_extra(&mut merged, structural.clone());
                tu_file_diags
                    .write()
                    .unwrap()
                    .insert(uri.clone(), structural);
                client.publish_diagnostics(uri, merged, Some(version)).await;
            }
        });
    }
}

/// Append diagnostics the per-file pass could not see (cross-file duplicates,
/// preprocessor errors), skipping ones already reported at the same position.
fn merge_extra(base: &mut Vec<Diagnostic>, extra: Vec<Diagnostic>) {
    for diag in extra {
        let known = base.iter().any(|existing| {
            existing.range.start == diag.range.start && existing.message == diag.message
        });
        if !known {
            base.push(diag);
        }
    }
}

/// `#include` paths become clickable links, so "go to file" lands in the
/// included source with the board still the active context.
fn collect_include_links(
    doc: &Document,
    node: tree_sitter::Node,
    file_dir: Option<&std::path::Path>,
    include_dirs: &[PathBuf],
    out: &mut Vec<DocumentLink>,
) {
    if node.kind() == "preproc_include"
        && let Some(path_node) = node.child_by_field_name("path")
    {
        let raw = util::node_text(doc, path_node);
        let (target, quoted) = match path_node.kind() {
            "preproc_string_literal" => (raw.trim_matches('"'), true),
            "system_lib_string" => (raw.trim_matches(|c| c == '<' || c == '>'), false),
            _ => return,
        };
        if let Some(resolved) = workspace::resolve_include(file_dir, target, quoted, include_dirs)
            && let Ok(url) = Url::from_file_path(&resolved)
        {
            out.push(DocumentLink {
                range: Range {
                    start: util::byte_to_position(&doc.rope, path_node.start_byte()),
                    end: util::byte_to_position(&doc.rope, path_node.end_byte()),
                },
                target: Some(url),
                tooltip: Some(resolved.display().to_string()),
                data: None,
            });
        }
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            collect_include_links(doc, child, file_dir, include_dirs, out);
        }
    }
}

struct TuOutput {
    per_file: HashMap<PathBuf, Vec<Diagnostic>>,
    data: TuData,
}

/// Publish an internal failure to every open document. A parser crash must be
/// visible, never swallowed.
async fn report_internal(
    client: &Client,
    documents: &Arc<RwLock<HashMap<Url, Document>>>,
    message: String,
) {
    let uris: Vec<Url> = documents.read().unwrap().keys().cloned().collect();
    for uri in uris {
        client
            .publish_diagnostics(
                uri,
                vec![Diagnostic {
                    range: Range::default(),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("dtsls".to_string()),
                    message: message.clone(),
                    ..Default::default()
                }],
                None,
            )
            .await;
    }
}

fn tu_output(unit: &crate::tu::TuResult, doc: &Document) -> TuOutput {
    let data = tu_data(unit, doc);
    let mut per_file = group_diagnostics(unit, doc);
    let report = crate::phandle_cells::check_document(doc);
    // phandle and interrupt checks resolve labels from the unit's own text
    for problem in report.diagnostics {
        push_problem(unit, doc, &mut per_file, problem);
    }

    // `&label { }` blocks that extend nodes outside this unit: one diagnostic
    // per file, listing what could not be resolved
    let mut unresolved: HashMap<PathBuf, (Vec<String>, u32, u32)> = HashMap::new();
    for (start, end, label) in report.unresolved_fragments {
        let line = util::byte_to_position(&doc.rope, start as usize).line;
        let Some((_, path)) = unit.locate(line) else {
            continue;
        };
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let entry = unresolved.entry(path).or_insert_with(|| (Vec::new(), start, end));
        entry.0.push(label);
    }
    for (_path, (labels, start, end)) in unresolved {
        let mut names = labels;
        names.sort();
        names.dedup();
        let shown = names.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        let more = names.len().saturating_sub(5);
        let problem = crate::phandle_cells::Problem {
            start,
            end,
            error: true,
            message: format!(
                "{} override block(s) extend nodes that are not defined in this file's \
                 translation unit ({shown}{}); the board file that includes this one must \
                 provide them, so their contents are not checked",
                names.len(),
                if more > 0 { format!(", and {more} more") } else { String::new() }
            ),
        };
        push_problem(unit, doc, &mut per_file, problem);
    }

    TuOutput { per_file, data }
}

/// Map a byte-span problem back to the file it came from and publish it there.
fn push_problem(
    unit: &crate::tu::TuResult,
    doc: &Document,
    per_file: &mut HashMap<PathBuf, Vec<Diagnostic>>,
    problem: crate::phandle_cells::Problem,
) {
    let (start, end) = (problem.start as usize, problem.end as usize);
    let line = util::byte_to_position(&doc.rope, start).line;
    let Some((source, path)) = unit.locate(line) else {
        // the source map is missing this line: a preprocessor bug, never
        // silently drop the diagnostic
        let fallback = unit
            .files
            .first()
            .map(|file| std::fs::canonicalize(file).unwrap_or_else(|_| file.clone()))
            .unwrap_or_default();
        per_file.entry(fallback).or_default().push(Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("dtsls".to_string()),
            message: format!(
                "internal: cannot map a diagnostic at line {} back to a source file: {}",
                line + 1,
                problem.message
            ),
            ..Default::default()
        });
        return;
    };
    let end_position = util::byte_to_position(&doc.rope, end);
    let end_position = if unit
        .locate(end_position.line)
        .map(|(source, _)| source.file)
        == Some(source.file)
    {
        end_position
    } else {
        Position {
            line: source.line,
            character: util::byte_to_position(&doc.rope, start).character + 1,
        }
    };
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    per_file.entry(path).or_default().push(Diagnostic {
        range: Range {
            start: Position {
                line: source.line,
                character: util::byte_to_position(&doc.rope, start).character,
            },
            end: end_position,
        },
        severity: Some(if problem.error {
            DiagnosticSeverity::ERROR
        } else {
            DiagnosticSeverity::WARNING
        }),
        source: Some("dtsls".to_string()),
        message: problem.message,
        ..Default::default()
    });
}

fn tu_data(unit: &crate::tu::TuResult, doc: &Document) -> workspace::TuData {
    let mut data = workspace::TuData {
        labels: HashMap::new(),
        refs: Vec::new(),
        defines: HashMap::new(),
        cells: Vec::new(),
        provider_cells: HashMap::new(),
    };
    for define in &unit.defines {
        let file = define
            .file
            .and_then(|index| unit.files.get(index as usize))
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        data.defines
            .entry(define.name.clone())
            .or_insert((define.value.clone(), file));
    }
    let mut cursor = doc.tree.root_node().walk();
    collect_unit_names(unit, doc, &mut cursor, &mut data);
    data
}

fn collect_unit_names(
    unit: &crate::tu::TuResult,
    doc: &Document,
    cursor: &mut tree_sitter::TreeCursor<'_>,
    data: &mut TuData,
) {
    loop {
        let node = cursor.node();
        match node.kind() {
            "label" => {
                let text = util::node_text(doc, node);
                let name = text.trim_end_matches(':');
                if !name.is_empty()
                    && let Some((file, range)) = unit_range(unit, doc, node, name.len())
                {
                    let path = crate::analysis::node_path(doc, node);
                    data.labels.entry(name.to_string()).or_insert(workspace::TuLabel {
                        path,
                        file,
                        range,
                    });
                }
            }
            "label_ref" => {
                let text = util::node_text(doc, node);
                let name = text.trim_start_matches('&');
                if !name.is_empty()
                    && let Some((file, range)) = unit_range(unit, doc, node, text.len())
                {
                    data.refs.push(workspace::TuRef {
                        label: name.to_string(),
                        file,
                        range,
                    });
                }
            }
            "property" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = util::node_text(doc, name_node);
                    if name.starts_with('#') && name.ends_with("-cells") {
                        let path = crate::analysis::node_path(doc, node);
                        if let Some(value) = crate::phandle_cells::property_u32(doc, node) {
                            data.cells.push(workspace::CellsProp {
                                path: path.clone(),
                                name: name.to_string(),
                                value,
                            });
                            data.provider_cells
                                .entry(path)
                                .or_default()
                                .insert(name.to_string(), value);
                        }
                    }
                }
                // `&label` references live inside property values, so keep
                // descending (a property cannot contain another property)
            }
            _ => {}
        }
        if cursor.goto_first_child() {
            collect_unit_names(unit, doc, cursor, data);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn unit_range(
    unit: &crate::tu::TuResult,
    doc: &Document,
    node: tree_sitter::Node,
    length: usize,
) -> Option<(PathBuf, Range)> {
    let position = util::byte_to_position(&doc.rope, node.start_byte());
    let (source, path) = unit.locate(position.line)?;
    Some((
        path.to_path_buf(),
        Range {
            start: Position {
                line: source.line,
                character: position.character,
            },
            end: Position {
                line: source.line,
                character: position.character + length as u32,
            },
        },
    ))
}

/// Diagnostics produced for a preprocessed unit, grouped by the file they
/// belong to (positions mapped back through the preprocessor line markers).
fn group_diagnostics(
    unit: &crate::tu::TuResult,
    doc: &Document,
) -> HashMap<PathBuf, Vec<Diagnostic>> {
    let mut out: HashMap<PathBuf, Vec<Diagnostic>> = HashMap::new();
    for diag in diagnostics::structural(&doc) {
        let Some((source, path)) = unit.locate(diag.range.start.line) else {
            continue;
        };
        let end = unit
            .locate(diag.range.end.line)
            .map(|(source, _)| source)
            .unwrap_or(source);
        let start = Position {
            line: source.line,
            character: diag.range.start.character,
        };
        let end = if end.file == source.file && end.line >= source.line {
            Position {
                line: end.line,
                character: diag.range.end.character,
            }
        } else {
            Position {
                line: start.line,
                character: start.character + 1,
            }
        };
        out.entry(path.to_path_buf())
            .or_default()
            .push(Diagnostic {
                range: Range { start, end },
                severity: diag.severity,
                source: diag.source,
                message: diag.message,
                ..Default::default()
            });
    }
    for error in &unit.errors {
        // an error without a file must still be shown, not dropped
        let path = error
            .file
            .and_then(|file| unit.files.get(file as usize).cloned())
            .or_else(|| unit.files.first().cloned());
        let Some(path) = path else {
            continue;
        };
        out.entry(path).or_default().push(Diagnostic {
            range: Range {
                start: Position {
                    line: error.line,
                    character: error.column,
                },
                end: Position {
                    line: error.line,
                    character: error.column + 1,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("dtsls".to_string()),
            message: format!("preprocessor: {}", error.message),
            ..Default::default()
        });
    }
    out
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        info!(root = ?params.root_uri, client = ?params.client_info, "initialize");
        let mut roots = Vec::new();
        if let Some(root) = params.root_uri.as_ref().and_then(|u| u.to_file_path().ok()) {
            roots.push(root);
        }
        if let Some(folders) = params.workspace_folders.as_ref() {
            for folder in folders {
                if let Ok(path) = folder.uri.to_file_path() {
                    roots.push(path);
                }
            }
        }
        let had_client_root = !roots.is_empty();
        // `<dt-bindings/...>` resolution: auto-detect a kernel tree by walking
        // up for the kernel layout, plus client-supplied includePaths.
        let kernel_root = workspace::detect_kernel_root(&roots);
        roots.extend(workspace::detect_include_dirs(&roots));
        let mut bindings_dir = kernel_root
            .as_ref()
            .map(|root| root.join("Documentation/devicetree/bindings"))
            .filter(|dir| dir.is_dir());
        if let Some(options) = params.initialization_options {
            if let Some(paths) = options.get("includePaths").and_then(|v| v.as_array()) {
                for path in paths {
                    if let Some(p) = path.as_str() {
                        roots.push(PathBuf::from(p));
                    }
                }
            }
            if let Some(path) = options.get("bindingsPath").and_then(|v| v.as_str()) {
                let dir = PathBuf::from(path);
                if dir.is_dir() {
                    bindings_dir = Some(dir);
                }
            }
        }

        // Whole-feature status: unavailable = ERROR, degraded = WARNING, both
        // reported at the top of every file. A feature never fails silently.
        let mut issues = Vec::new();
        let mut scan_dir: Option<PathBuf> = None;
        if !had_client_root {
            issues.push(FeatureIssue::degraded(
                "workspace index",
                "no workspace root — cross-file go-to-definition/references are \
                 limited to open buffers; open the editor inside the project",
            ));
        }
        match &bindings_dir {
            Some(dir) => {
                if bindings::dtschema_package_dir().is_none() {
                    issues.push(FeatureIssue::degraded(
                        "dtschema",
                        "python package not installed — `$ref` resolution and \
                         unknown-property checks are limited; install it with: \
                         pip install dtschema",
                    ));
                }
                info!(
                    kernel_root = ?kernel_root,
                    bindings_dir = %dir.display(),
                    dtschema = ?bindings::dtschema_package_dir(),
                    "binding configuration"
                );
                scan_dir = Some(dir.clone());
            }
            None => {
                issues.push(FeatureIssue::unavailable(
                    "devicetree bindings",
                    "no Documentation/devicetree/bindings found — binding \
                     validation, property completion and go-to-binding are \
                     disabled; open the kernel tree root or set \
                     initializationOptions.bindingsPath",
                ));
                warn!(
                    kernel_root = ?kernel_root,
                    roots = ?roots,
                    "no devicetree bindings directory found"
                );
            }
        }
        if let Some(root) = &kernel_root
            && !root.join("include/dt-bindings").is_dir()
        {
            issues.push(FeatureIssue::degraded(
                "dt-bindings headers",
                "include/dt-bindings is missing — `<dt-bindings/...>` includes \
                 cannot be resolved",
            ));
        }
        *self.status.write().unwrap() = issues.clone();
        *self.root_dirs.write().unwrap() = roots;
        if let Some(dir) = scan_dir {
            // Scan bindings, then refresh every open document: during the scan
            // binding checks are disabled, so the first diagnostics would
            // otherwise keep stale (or missing) binding results until an edit.
            let bindings = Arc::clone(&self.bindings);
            let documents = Arc::clone(&self.documents);
            let include_dirs = self.include_dirs();
            let client = self.client.clone();
            let diag_cache = Arc::clone(&self.diag_cache);
            let file_diags = Arc::clone(&self.file_diags);
            let tu_file_diags = Arc::clone(&self.tu_file_diags);
            let last_active = Arc::clone(&self.last_active);
            let workspace = Arc::clone(&self.workspace);
            let scan_roots = self.root_dirs.read().unwrap().clone();
            tokio::spawn(async move {
                let scan_started = std::time::Instant::now();
                let scan_bindings = Arc::clone(&bindings);
                if let Err(error) = tokio::task::spawn_blocking(move || {
                    bindings::scan(&scan_bindings, &dir);
                })
                .await
                {
                    // a panicking scan leaves every binding check unreliable:
                    // say so instead of silently serving stale results
                    error!(%error, "binding scan failed");
                    report_internal(&client, &documents, format!(
                        "internal: the binding scan failed ({error}); binding validation \
                         is disabled for this session"
                    ))
                    .await;
                }
                let scan_us = scan_started.elapsed().as_micros() as u64;
                let entries: Vec<(Url, i32)> = {
                    let docs = documents.read().unwrap();
                    docs.iter().map(|(uri, doc)| (uri.clone(), doc.version)).collect()
                };
                let active = last_active.read().unwrap().clone();
                let publish_started = std::time::Instant::now();
                let mut docs = 0usize;
                let mut diagnostics = 0usize;
                let mut active_us = 0u64;

                // the active document first: it is what the editor shows
                let mut rest = entries;
                if let Some(active) = active.clone()
                    && let Some(position) = rest.iter().position(|(uri, _)| *uri == active)
                {
                    let (uri, version) = rest.remove(position);
                    let active_started = std::time::Instant::now();
                    let diags = {
                        let docs = documents.read().unwrap();
                        match docs.get(&uri) {
                            Some(doc) => diagnostics::all_cached(
                                doc,
                                uri.to_file_path().ok().as_deref(),
                                &include_dirs,
                                &bindings,
                                &issues,
                                &diag_cache,
                            ),
                            None => Vec::new(),
                        }
                    };
                    active_us = active_started.elapsed().as_micros() as u64;
                    file_diags.write().unwrap().insert(uri.clone(), diags.clone());
                    let mut merged = diags;
                    let structural = tu_file_diags
                        .read()
                        .unwrap()
                        .get(&uri)
                        .cloned()
                        .unwrap_or_default();
                    merge_extra(&mut merged, structural);
                    diagnostics += merged.len();
                    docs += 1;
                    client
                        .publish_diagnostics(uri, merged, Some(version))
                        .await;
                }

                let compute_started = std::time::Instant::now();
                let computed: Vec<(Url, i32, Vec<Diagnostic>)> = {
                    let docs_guard = documents.read().unwrap();
                    let entries: Vec<(&Url, i32, &Document)> = rest
                        .iter()
                        .filter_map(|(uri, version)| {
                            docs_guard.get(uri).map(|doc| (uri, *version, doc))
                        })
                        .collect();
                    let workers = std::thread::available_parallelism()
                        .map(|count| count.get())
                        .unwrap_or(4)
                        .min(entries.len().max(1));
                    let chunk = entries.len().div_ceil(workers).max(1);
                    std::thread::scope(|scope| {
                        let handles: Vec<_> = entries
                            .chunks(chunk)
                            .map(|slice| {
                                scope.spawn(|| {
                                    slice
                                        .iter()
                                        .map(|(uri, version, doc)| {
                                            let diags = diagnostics::all_cached(
                                                doc,
                                                uri.to_file_path().ok().as_deref(),
                                                &include_dirs,
                                                &bindings,
                                                &issues,
                                                &diag_cache,
                                            );
                                            ((*uri).clone(), *version, diags)
                                        })
                                        .collect::<Vec<_>>()
                                })
                            })
                            .collect();
                        handles
                            .into_iter()
                            .flat_map(|handle| handle.join().expect("diagnostics worker"))
                            .collect()
                    })
                };
                let compute_us = compute_started.elapsed().as_micros() as u64;
                for (uri, version, diags) in computed {
                    file_diags
                        .write()
                        .unwrap()
                        .insert(uri.clone(), diags.clone());
                    let mut merged = diags;
                    let structural = tu_file_diags
                        .read()
                        .unwrap()
                        .get(&uri)
                        .cloned()
                        .unwrap_or_default();
                    merge_extra(&mut merged, structural);
                    diagnostics += merged.len();
                    docs += 1;
                    client
                        .publish_diagnostics(uri, merged, Some(version))
                        .await;
                }
                debug!(
                    scan_us,
                    active_us,
                    compute_us,
                    refresh_us = publish_started.elapsed().as_micros() as u64,
                    docs,
                    diagnostics,
                    "bindings ready; diagnostics refreshed"
                );
                // workspace scan after the refresh: running both at once just
                // makes the first post-scan diagnostics slower
                if let Err(error) = tokio::task::spawn_blocking(move || {
                    workspace.scan(&scan_roots, &scan_roots);
                })
                .await
                {
                    error!(%error, "workspace scan failed");
                    report_internal(&client, &documents, format!(
                        "internal: the workspace scan failed ({error}); label and ref \
                         navigation is incomplete"
                    ))
                    .await;
                }
            });
        } else {
            // no binding scan: go straight to the workspace scan
            let workspace = Arc::clone(&self.workspace);
            let scan_roots = self.root_dirs.read().unwrap().clone();
            tokio::task::spawn_blocking(move || workspace.scan(&scan_roots, &scan_roots));
        }
        let capabilities = ServerCapabilities {
            position_encoding: Some(PositionEncodingKind::UTF16),
            text_document_sync: Some(TextDocumentSyncCapability::Kind(
                TextDocumentSyncKind::INCREMENTAL,
            )),
            completion_provider: Some(CompletionOptions {
                trigger_characters: Some(vec![
                    "&".to_string(),
                    "<".to_string(),
                    "\"".to_string(),
                ]),
                resolve_provider: Some(false),
                ..Default::default()
            }),
            document_link_provider: Some(DocumentLinkOptions {
                resolve_provider: Some(false),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            }),
            definition_provider: Some(OneOf::Left(true)),
            declaration_provider: Some(DeclarationCapability::Simple(true)),
            references_provider: Some(OneOf::Left(true)),
            inlay_hint_provider: Some(OneOf::Left(true)),
            document_symbol_provider: Some(OneOf::Left(true)),
            semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
                SemanticTokensOptions {
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                    legend: SemanticTokensLegend {
                        token_types: semantic_tokens::LEGEND_TOKEN_TYPES.to_vec(),
                        token_modifiers: semantic_tokens::LEGEND_TOKEN_MODIFIERS.to_vec(),
                    },
                    range: Some(false),
                    full: Some(SemanticTokensFullOptions::Bool(true)),
                },
            )),
            ..Default::default()
        };
        Ok(InitializeResult {
            capabilities,
            server_info: Some(ServerInfo {
                name: "dtsls".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "dtsls initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        // tower-lsp only ends its serve loop on stdin EOF, which some clients
        // never send after the shutdown/exit handshake. Guarantee termination.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            std::process::exit(0);
        });
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let td = params.text_document;
        info!(uri = %td.uri, version = td.version, bytes = td.text.len(), "did_open");
        *self.last_active.write().unwrap() = Some(td.uri.clone());
        let path = td.uri.to_file_path().ok();
        let is_board = path
            .as_ref()
            .is_some_and(|p| p.extension().and_then(|e| e.to_str()) == Some("dts"));
        if is_board {
            *self.active_board.write().unwrap() = path.clone();
        }
        if let Some(context) = path.clone().map(|path| {
            if is_board {
                path
            } else {
                match self.active_board.read().unwrap().clone() {
                    Some(board) if self.workspace.closure_contains(&board, &path) => board,
                    _ => path,
                }
            }
        }) {
            self.doc_context
                .write()
                .unwrap()
                .insert(td.uri.clone(), context);
        }
        // a board opened after its includes adopts those standalone documents
        if is_board && let Some(board) = path.clone() {
            let open: Vec<Url> = self.documents.read().unwrap().keys().cloned().collect();
            let mut contexts = self.doc_context.write().unwrap();
            for other in open {
                let Ok(other_path) = other.to_file_path() else {
                    continue;
                };
                if other_path == board {
                    continue;
                }
                let standalone = contexts.get(&other) == Some(&other_path);
                if standalone && self.workspace.closure_contains(&board, &other_path) {
                    contexts.insert(other, board.clone());
                }
            }
        }
        let tu_roots = self.analysis_roots(&td.uri);
        debug!(tu_roots = tu_roots.len(), "tu roots for did_open");
        let (diags, version) = {
            let mut docs = self.documents.write().unwrap();
            docs.insert(td.uri.clone(), Document::open(&td.text, td.version));
            match docs.get(&td.uri) {
                Some(doc) => (
                    diagnostics::all_cached(
                        doc,
                        td.uri.to_file_path().ok().as_deref(),
                        &self.include_dirs(),
                        &self.bindings,
                        &self.status.read().unwrap(),
                        &self.diag_cache,
                    ),
                    doc.version,
                ),
                None => return,
            }
        };
        self.file_diags
            .write()
            .unwrap()
            .insert(td.uri.clone(), diags.clone());
        self.client
            .publish_diagnostics(td.uri.clone(), diags, Some(version))
            .await;
        for (root_uri, root_path) in tu_roots {
            self.schedule_tu(root_uri, root_path);
        }
        let protected = self.protected_contexts();
        self.workspace.prune(MAX_CONTEXTS, &protected);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let started = std::time::Instant::now();
        let uri = params.text_document.uri;
        *self.last_active.write().unwrap() = Some(uri.clone());
        let tu_roots = self.analysis_roots(&uri);
        let (diags, version) = {
            let mut docs = self.documents.write().unwrap();
            match docs.get_mut(&uri) {
                Some(doc) => {
                    doc.apply_changes(&params.content_changes, params.text_document.version);
                    let parse_done = started.elapsed();
                    let diags = diagnostics::all_cached(
                        doc,
                        uri.to_file_path().ok().as_deref(),
                        &self.include_dirs(),
                        &self.bindings,
                        &self.status.read().unwrap(),
                        &self.diag_cache,
                    );
                    debug!(
                        parse_us = parse_done.as_micros() as u64,
                        diagnostics_us = started.elapsed().as_micros() as u64,
                        "did_change handled"
                    );
                    (diags, doc.version)
                }
                None => {
                    warn!(uri = %uri, "did_change for unknown document");
                    return;
                }
            }
        };
        self.file_diags
            .write()
            .unwrap()
            .insert(uri.clone(), diags.clone());
        self.client
            .publish_diagnostics(uri.clone(), diags, Some(version))
            .await;
        for (root_uri, root_path) in tu_roots {
            self.schedule_tu(root_uri, root_path);
        }
        debug!(total_us = started.elapsed().as_micros() as u64, "did_change done");
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        self.documents.write().unwrap().remove(&uri);
        self.doc_context.write().unwrap().remove(&uri);
        if let Ok(path) = uri.to_file_path()
            && self.active_board.read().unwrap().as_ref() == Some(&path)
        {
            *self.active_board.write().unwrap() = None;
        }
        self.file_diags.write().unwrap().remove(&uri);
        self.same_file_labels.write().unwrap().remove(&uri);
        self.prune_contexts();
        // The index entry is intentionally kept (stale until the next scan):
        // goto definition into a closed file must keep working.
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let started = std::time::Instant::now();
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let items = {
            let docs = self.documents.read().unwrap();
            let Some(doc) = docs.get(&uri) else {
                return Ok(None);
            };
            let Some(context) =
                completion::completion_context(doc, util::position_to_byte(&doc.rope, position))
            else {
                return Ok(None);
            };
            match context {
                completion::Context::Labels(prefix) => {
                    self.label_items(&uri, doc, &prefix)
                },
                completion::Context::Defines(prefix) => {
                    if let Some(data) = self.unit_for(&uri) {
                        completion::items_from_defines(&data.defines, &prefix)
                    } else {
                        completion::same_file_define_items(doc, &prefix)
                    }
                }
                completion::Context::Properties(prefix) => {
                    let byte = util::position_to_byte(&doc.rope, position);
                    property_completion(doc, byte, &prefix, &self.bindings)
                }
                completion::Context::Compatible { prefix, vendor } => {
                    let byte = util::position_to_byte(&doc.rope, position);
                    // replace everything typed inside the quotes so far; if the
                    // cursor sits mid-string, replace up to the closing quote
                    let line_start =
                        doc.rope.line_to_byte(doc.rope.byte_to_line(byte));
                    let head = doc.slice(line_start..byte);
                    let quote_start = head
                        .rfind('"')
                        .map(|offset| line_start + offset + 1)
                        .unwrap_or(byte);
                    let line_end = doc
                        .rope
                        .line_to_byte(doc.rope.byte_to_line(byte) + 1)
                        .min(doc.rope.len_bytes());
                    let tail = doc.slice(byte..line_end);
                    let end = tail
                        .find('"')
                        .map(|offset| byte + offset)
                        .unwrap_or(byte);
                    let range = Range {
                        start: util::byte_to_position(&doc.rope, quote_start),
                        end: util::byte_to_position(&doc.rope, end),
                    };
                    match vendor {
                        None => {
                            let mut items = completion::vendor_completion_items(
                                self.bindings.compatible_vendors(),
                                &prefix,
                                &range,
                            );
                            items.extend(completion::vendor_less_items(
                                self.bindings.vendor_less_compatibles(),
                                &prefix,
                                &range,
                            ));
                            items.sort_by(|a, b| a.label.cmp(&b.label));
                            items
                        }
                        Some(vendor) => completion::device_completion_items(
                            self.bindings.compatible_devices(&vendor),
                            &prefix,
                            &range,
                        ),
                    }
                }
            }
        };
        debug!(count = items.len(), elapsed_us = started.elapsed().as_micros() as u64, "completion");
        Ok(Some(CompletionResponse::List(completion::completion_list(
            items,
        ))))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let started = std::time::Instant::now();
        let response = self.definition(params);
        debug!(
            found = response.is_some(),
            elapsed_us = started.elapsed().as_micros() as u64,
            "definition"
        );
        Ok(response)
    }

    async fn goto_declaration(
        &self,
        params: GotoDeclarationParams,
    ) -> Result<Option<GotoDeclarationResponse>> {
        let started = std::time::Instant::now();
        let response = self
            .location(
                &params.text_document_position_params.text_document.uri,
                params.text_document_position_params.position,
            )
            .map(GotoDeclarationResponse::Scalar);
        debug!(
            found = response.is_some(),
            elapsed_us = started.elapsed().as_micros() as u64,
            "declaration"
        );
        Ok(response)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let label = {
            let docs = self.documents.read().unwrap();
            let Some(doc) = docs.get(&uri) else {
                return Ok(None);
            };
            completion::label_at_cursor(doc, util::position_to_byte(&doc.rope, position))
        };
        let Some(label) = label else {
            return Ok(None);
        };
        if let Some(data) = self.unit_for(&uri) {
            let mut locations = Vec::new();
            if params.context.include_declaration
                && let Some(def) = data.labels.get(&label)
                && let Ok(def_uri) = Url::from_file_path(&def.file)
            {
                locations.push(Location {
                    uri: def_uri,
                    range: def.range,
                });
            }
            for reference in data.refs.iter().filter(|reference| reference.label == label) {
                if let Ok(ref_uri) = Url::from_file_path(&reference.file) {
                    locations.push(Location {
                        uri: ref_uri,
                        range: reference.range,
                    });
                }
                if locations.len() >= 500 {
                    break;
                }
            }
            if !locations.is_empty() {
                return Ok(Some(locations));
            }
        }
        // no unit yet: everything visible is this buffer
        let mut ranges = Vec::new();
        {
            let docs = self.documents.read().unwrap();
            if let Some(doc) = docs.get(&uri) {
                if params.context.include_declaration
                    && let Some(range) = completion::label_definition(doc, &label)
                {
                    ranges.push(range);
                }
                ranges.extend(completion::label_references(doc, &label));
            }
        }
        Ok(Some(
            ranges
                .into_iter()
                .map(|range| Location {
                    uri: uri.clone(),
                    range,
                })
                .collect(),
        ))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let docs = self.documents.read().unwrap();
        let Some(doc) = docs.get(&uri) else {
            return Ok(None);
        };
        let unit = self.unit_for(&uri);
        let hints = hints::inlay_hints(doc, &uri, unit.as_deref(), &params.range);
        Ok((!hints.is_empty()).then_some(hints))
    }

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let docs = self.documents.read().unwrap();
        let Some(doc) = docs.get(&params.text_document.uri) else {
            return Ok(None);
        };
        let file_dir = params
            .text_document
            .uri
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(|dir| dir.to_path_buf()));
        let include_dirs = self.include_dirs();
        let mut links = Vec::new();
        collect_include_links(
            doc,
            doc.tree.root_node(),
            file_dir.as_deref(),
            &include_dirs,
            &mut links,
        );
        Ok(Some(links))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let docs = self.documents.read().unwrap();
        let Some(doc) = docs.get(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(Some(DocumentSymbolResponse::Nested(symbols::document_symbols(
            doc,
        ))))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let docs = self.documents.read().unwrap();
        let Some(doc) = docs.get(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(Some(SemanticTokensResult::Tokens(
            semantic_tokens::semantic_tokens(doc),
        )))
    }
}

impl Backend {
    fn definition(&self, params: GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        self.location(&uri, position)
            .map(GotoDefinitionResponse::Scalar)
    }

    fn location(&self, uri: &Url, position: Position) -> Option<Location> {
        let docs = self.documents.read().unwrap();
        let doc = docs.get(uri)?;
        let byte = util::position_to_byte(&doc.rope, position);
        if let Some(label) = completion::label_at_cursor(doc, byte) {
            if let Some(data) = self.unit_for(&uri)
                && let Some(def) = data.labels.get(&label)
                && let Ok(target) = Url::from_file_path(&def.file)
            {
                return Some(Location {
                    uri: target,
                    range: def.range,
                });
            }
            let range = completion::label_definition(doc, &label)?;
            return Some(Location {
                uri: uri.clone(),
                range,
            });
        }
        // not on a label: jump to the binding of the enclosing node. A
        // half-written node may not have a usable tree, so fall back to the
        // text reader (same one property completion uses).
        let compatibles = completion::enclosing_node(doc, byte)
            .and_then(|subnode| node_compatibles(doc, subnode))
            .filter(|list| !list.is_empty())
            .or_else(|| {
                completion::text_node(doc, byte)
                    .map(|node| node.compatibles)
                    .filter(|list| !list.is_empty())
            })?;
        let binding = self.bindings.lookup(&compatibles)?;
        let line = self.binding_line(&binding.uri);
        Some(Location {
            uri: binding.uri.clone(),
            range: Range {
                start: Position {
                    line,
                    character: 0,
                },
                end: Position {
                    line,
                    character: 0,
                },
            },
        })
    }

    /// Line of the `compatible:` property in a binding file (the jump target
    /// inside the YAML; cached per binding file).
    fn binding_line(&self, uri: &Url) -> u32 {
        if let Some(line) = self.binding_lines.read().unwrap().get(uri) {
            return *line;
        }
        let line = uri
            .to_file_path()
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| {
                text.lines().position(|line| {
                    let trimmed = line.trim_start();
                    trimmed.starts_with("compatible:") && !trimmed.starts_with("compatible: true")
                })
            })
            .unwrap_or(0) as u32;
        self.binding_lines
            .write()
            .unwrap()
            .insert(uri.clone(), line);
        line
    }
}


fn node_compatibles(doc: &Document, node: tree_sitter::Node) -> Option<Vec<String>> {
    diagnostics::compatible_strings(doc, node)
}

/// What the node around `byte` looks like for completion: its binding, the
/// property names it already has, and whether it can still take a
/// `compatible`. The syntax tree is preferred; a half-written node makes
/// tree-sitter collapse the region into a single ERROR node, so the raw text
/// is used as a fallback.
struct NodeView {
    binding: Option<Arc<crate::bindings::Binding>>,
    existing: HashSet<String>,
    can_add_compatible: bool,
}

fn node_view(doc: &Document, byte: usize, bindings: &BindingIndex) -> NodeView {
    let mut existing = HashSet::new();
    let mut compatibles = None;
    let mut endpoint = None;
    // A node starting on the cursor's own line is not the one being typed
    // inside: error recovery glues the partial line onto the *following*
    // node, so its compatible would win. Only nodes whose header is above the
    // cursor are trusted; the text reader handles the rest.
    let line_start = doc
        .rope
        .line_to_byte(doc.rope.byte_to_line(byte.min(doc.rope.len_bytes())));
    if let Some(node) = completion::enclosing_node(doc, byte)
        && node.start_byte() < line_start
    {
        compatibles = node_compatibles(doc, node);
        endpoint = Some(completion::is_endpoint_or_port(doc, node));
        if let Some(body) = util::node_def_of(node) {
            for i in 0..body.child_count() {
                let Some(child) = body.child(i as u32) else { continue };
                if child.kind() == "property"
                    && let Some(name) = child.child_by_field_name("name")
                {
                    existing.insert(util::node_text(doc, name).to_string());
                } else if child.kind() == "subnode"
                    && let Some(name) = child.child_by_field_name("name")
                {
                    // child nodes (`ports`, `mdio`, ...) are declared under
                    // `properties:` too, so they count as already present
                    let name = util::node_text(doc, name);
                    existing.insert(name.split('@').next().unwrap_or(name).to_string());
                }
            }
        }
    }
    if compatibles.is_none() {
        // incomplete node: read compatible/properties from the text
        if let Some(text_node) = completion::text_node(doc, byte) {
            endpoint.get_or_insert_with(|| text_node.is_endpoint_or_port());
            existing.extend(text_node.properties);
            if !text_node.compatibles.is_empty() {
                compatibles = Some(text_node.compatibles);
            }
        }
    }
    let endpoint = endpoint.unwrap_or_else(|| completion::in_endpoint_or_port(doc, byte));
    let binding = compatibles.as_ref().and_then(|list| bindings.lookup(list));
    let can_add_compatible =
        !endpoint && compatibles.is_none() && !existing.contains("compatible");
    NodeView {
        binding,
        existing,
        can_add_compatible,
    }
}

/// Binding-based property completion for the node enclosing `byte`, plus the
/// `compatible` snippet when the node can still take one.
fn property_completion(
    doc: &Document,
    byte: usize,
    prefix: &str,
    bindings: &BindingIndex,
) -> Vec<CompletionItem> {
    let view = node_view(doc, byte, bindings);
    let mut items = view
        .binding
        .as_ref()
        .map(|binding| completion::items_from_binding(binding, &view.existing, prefix))
        .unwrap_or_default();
    if view.can_add_compatible
        && let Some(item) = completion::compatible_property_item(prefix)
    {
        items.push(item);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::Binding;

    fn widget_bindings() -> BindingIndex {
        let bindings = BindingIndex::default();
        bindings.insert_for_test(Binding {
            uri: Url::parse("file:///mmc.yaml").unwrap(),
            singles: vec!["acme,mmc".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "reg", "clocks", "clock-names"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            required: vec!["compatible".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: Vec::new(),
        });
        bindings
    }

    #[test]
    fn property_items_work_in_a_half_written_node() {
        // compatible accepted, but no `;` and no closing braces yet
        let text = "/dts-v1/;\n\n/ {\n    mmc@8804000 {\n        compatible = \"acme,mmc\"\n        clo\n";
        let doc = Document::open(text, 0);
        let bindings = widget_bindings();
        let byte = text.rfind("clo").unwrap() + 3;
        let items = property_completion(&doc, byte, "clo", &bindings);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["clock-names", "clocks"], "items: {labels:?}");
        // the node already has a compatible, so no snippet is offered
        assert!(!labels.contains(&"compatible"));
    }

    #[test]
    fn property_completion_offers_compatible_for_a_fresh_node() {
        let text = "/dts-v1/;\n\n/ {\n    node@1000 {\n        comp\n";
        let doc = Document::open(text, 0);
        let byte = text.rfind("comp").unwrap() + 4;
        let items = property_completion(&doc, byte, "comp", &BindingIndex::default());
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["compatible"]);
    }

    #[test]
    fn property_items_resolve_through_the_enclosing_node() {
        let text = "/dts-v1/;\n\n/ {\n    mmc@8804000 {\n        compatible = \"acme,mmc\";\n        reg = <0x1>;\n        clo\n    };\n};\n";
        let doc = Document::open(text, 0);
        let bindings = BindingIndex::default();
        bindings.insert_for_test(Binding {
            uri: Url::parse("file:///mmc.yaml").unwrap(),
            singles: vec!["acme,mmc".to_string()],
            compounds: Vec::new(),
            properties: ["compatible", "reg", "clocks", "clock-names"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            required: vec!["compatible".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: Vec::new(),
        });
        // cursor right after "clo" on its line
        let byte = text.find("clo").unwrap() + 3;
        let items = property_completion(&doc, byte, "clo", &bindings);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["clock-names", "clocks"], "items: {labels:?}");
    }

    fn nested_bindings() -> BindingIndex {
        let bindings = BindingIndex::default();
        let make = |single: &str, props: &[&str]| Binding {
            uri: Url::parse(&format!("file:///{single}.yaml")).unwrap(),
            singles: vec![single.to_string()],
            compounds: Vec::new(),
            properties: props.iter().map(|p| p.to_string()).collect(),
            required: vec!["compatible".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: Vec::new(),
        };
        bindings.insert_for_test(make("acme,parent", &["compatible", "parent-prop", "parent-extra"]));
        bindings.insert_for_test(make("acme,child", &["compatible", "child-prop"]));
        bindings
    }

    #[test]
    fn completion_above_a_node_uses_the_parent() {
        // cursor on the blank line between the parent's last property and the
        // child node header: the child's compatible must not leak in
        let text = "/dts-v1/;\n\n/ {\n    parent@1000 {\n        compatible = \"acme,parent\";\n        parent-prop = <1>;\n        p\n        child@2000 {\n            compatible = \"acme,child\";\n        };\n    };\n};\n";
        let doc = Document::open(text, 0);
        let byte = text.find("        p\n").unwrap() + 9;
        let items = property_completion(&doc, byte, "p", &nested_bindings());
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        // parent-prop is already written in the node, so only the other one
        assert_eq!(labels, vec!["parent-extra"], "items: {labels:?}");
    }

    #[test]
    fn completion_above_a_node_offers_compatible_for_the_parent() {
        let text = "/dts-v1/;\n\n/ {\n    parent@1000 {\n        reg = <1>;\n        c\n        child@2000 {\n            compatible = \"acme,child\";\n        };\n    };\n};\n";
        let doc = Document::open(text, 0);
        let byte = text.find("        c\n").unwrap() + 9;
        let items = property_completion(&doc, byte, "c", &nested_bindings());
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["compatible"], "items: {labels:?}");
    }

    #[test]
    fn completion_inside_a_node_still_uses_that_node() {
        let text = "/dts-v1/;\n\n/ {\n    parent@1000 {\n        compatible = \"acme,parent\";\n        child@2000 {\n            compatible = \"acme,child\";\n            c\n        };\n    };\n};\n";
        let doc = Document::open(text, 0);
        let byte = text.find("            c\n").unwrap() + 13;
        let items = property_completion(&doc, byte, "c", &nested_bindings());
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["child-prop"], "items: {labels:?}");
    }
}

#[cfg(test)]
mod tu_bench {
    use super::*;

    #[test]
    #[ignore]
    fn bench_tu_phases() {
        let root = std::path::Path::new("/linux-7.2.2/arch/arm64/boot/dts/qcom/sm8650-qrd.dts");
        let include_dirs = [
            PathBuf::from("/linux-7.2.2/include"),
            PathBuf::from("/linux-7.2.2/scripts/dtc/include-prefixes"),
        ];
        let overlays = BTreeMap::new();
        let t = std::time::Instant::now();
        let unit = crate::tu::preprocess(&crate::tu::TuInput {
            root,
            include_dirs: &include_dirs,
            overlays: &overlays,
        });
        println!("preprocess: {:?} ({} bytes)", t.elapsed(), unit.text.len());
        for round in 0..3 {
            let t = std::time::Instant::now();
            let _ = Document::open(&unit.text, 0);
            println!("parse #{round}: {:?}", t.elapsed());
        }
        let doc = Document::open(&unit.text, 0);
        let t = std::time::Instant::now();
        let diags = diagnostics::structural(&doc);
        println!("structural: {:?} ({} diags)", t.elapsed(), diags.len());
        for diag in diags.iter().take(6) {
            println!(
                "  diag: {}:{} {}",
                diag.range.start.line, diag.range.start.character, diag.message
            );
        }
        let t = std::time::Instant::now();
        let data = tu_data(&unit, &doc);
        println!(
            "names: {:?} ({} labels, {} refs, {} cells, {} defines)",
            t.elapsed(),
            data.labels.len(),
            data.refs.len(),
            data.cells.len(),
            data.defines.len()
        );
        let t = std::time::Instant::now();
        let grouped = group_diagnostics(&unit, &doc);
        println!("group: {:?} ({} files)", t.elapsed(), grouped.len());

        // simulate one localized edit in the preprocessed text
        let mut edited = unit.text.clone();
        let pivot = edited.len() / 2;
        let pivot = (pivot..edited.len())
            .find(|index| edited.is_char_boundary(*index))
            .unwrap_or(edited.len());
        edited.insert(pivot, ' ');
        let t = std::time::Instant::now();
        let doc = {
            let mut doc = doc;
            doc.update_text(&edited);
            doc
        };
        println!("incremental update+parse: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let diags = diagnostics::structural(&doc);
        println!("structural after edit: {:?} ({} diags)", t.elapsed(), diags.len());
        let t = std::time::Instant::now();
        let _ = tu_data(&unit, &doc);
        println!("names after edit: {:?}", t.elapsed());
    }
}

#[cfg(test)]
mod refresh_bench {
    use super::*;

    #[test]
    #[ignore]
    fn bench_refresh_parallel() {
        let files = [
            "/linux-7.2.2/arch/arm64/boot/dts/qcom/sm8650.dtsi",
            "/linux-7.2.2/arch/arm64/boot/dts/qcom/hamoa.dtsi",
            "/linux-7.2.2/arch/arm64/boot/dts/qcom/agatti.dtsi",
            "/linux-7.2.2/arch/arm64/boot/dts/qcom/sm8650-qrd.dts",
        ];
        let docs: Vec<(Url, Document)> = files
            .iter()
            .map(|path| {
                let text = std::fs::read_to_string(path).unwrap();
                (Url::from_file_path(path).unwrap(), Document::open(&text, 0))
            })
            .collect();
        let bindings = Arc::new(BindingIndex::default());
        bindings::scan(&bindings, std::path::Path::new("/linux-7.2.2/Documentation/devicetree/bindings"));
        let issues = Vec::new();

        let run = |parallel: bool| {
            let cache = Arc::new(diagnostics::DiagCache::default());
            let started = std::time::Instant::now();
            let compute = |(uri, doc): &(Url, Document)| {
                diagnostics::all_cached(
                    doc,
                    uri.to_file_path().ok().as_deref(),
                    &[],
                    &bindings,
                    &issues,
                    &cache,
                )
            };
            if parallel {
                std::thread::scope(|scope| {
                    let handles: Vec<_> = docs
                        .iter()
                        .map(|entry| scope.spawn(|| compute(entry)))
                        .collect();
                    for handle in handles {
                        let _ = handle.join().unwrap();
                    }
                });
            } else {
                for entry in &docs {
                    let _ = compute(entry);
                }
            }
            started.elapsed()
        };
        // warm the page cache / binding caches
        let _ = run(false);
        let serial = run(false);
        let parallel = run(true);
        println!("refresh serial: {serial:?}, parallel: {parallel:?}");
    }
}

