use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{debug, info, warn};

use crate::bindings::{self, BindingIndex};
use crate::completion;
use crate::document::Document;
use crate::hints;
use crate::index::{self, Index};
use crate::util;
use crate::status::FeatureIssue;
use crate::{diagnostics, semantic_tokens, symbols};

pub struct Backend {
    client: Client,
    documents: Arc<RwLock<HashMap<Url, Document>>>,
    root_dirs: RwLock<Vec<PathBuf>>,
    /// Workspace-wide contribution tables (labels, refs, includes), built in
    /// background; requests read finished entries only.
    index: Arc<Index>,
    /// compatible -> devicetree binding, built in background at startup.
    bindings: Arc<BindingIndex>,
    /// Whole-feature conditions (unavailable = error, degraded = warning),
    /// reported at the top of every file.
    status: RwLock<Vec<FeatureIssue>>,
    /// Per-subtree diagnostic results, so a keystroke only rechecks the
    /// subtree it touched.
    diag_cache: diagnostics::DiagCache,
    /// Latest scheduled index rebuild per document; older ones are dropped,
    /// so typing does not queue one full re-index per keystroke.
    pending_index: Arc<RwLock<HashMap<Url, u64>>>,
    /// Last per-file diagnostics, so translation-unit results can be merged
    /// with them instead of replacing them.
    file_diags: Arc<RwLock<HashMap<Url, Vec<Diagnostic>>>>,
    /// Coalesced translation-unit re-runs, per root.
    tu_pending: Arc<RwLock<HashMap<Url, u64>>>,
    /// Names visible in each root's preprocessed unit (labels, references,
    /// macros, `#*-cells`); completion/goto/hints prefer this over the index.
    tu_data: Arc<RwLock<HashMap<Url, Arc<TuData>>>>,
    /// Preprocessed text of each root, parsed incrementally across edits
    /// (clangd's preamble equivalent: only the changed region reparses).
    tu_snapshots: Arc<std::sync::Mutex<HashMap<Url, (Document, Vec<PathBuf>)>>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(RwLock::new(HashMap::new())),
            root_dirs: RwLock::new(Vec::new()),
            index: Arc::new(Index::default()),
            bindings: Arc::new(BindingIndex::default()),
            status: RwLock::new(Vec::new()),
            diag_cache: diagnostics::DiagCache::default(),
            pending_index: Arc::new(RwLock::new(HashMap::new())),
            file_diags: Arc::new(RwLock::new(HashMap::new())),
            tu_pending: Arc::new(RwLock::new(HashMap::new())),
            tu_data: Arc::new(RwLock::new(HashMap::new())),
            tu_snapshots: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Include dirs for diagnostics (file-relative resolution is separate).
    fn include_dirs(&self) -> Vec<PathBuf> {
        self.root_dirs.read().unwrap().clone()
    }

    /// Rebuild one document's contribution table off the main loop, then the
    /// tables of every file that includes it (directly or transitively) —
    /// their merged completion data caches this file's contributions.
    fn schedule_file_index(&self, uri: Url, text: String, version: i32) {
        // Coalesce: bump a generation and let a delayed task do the work only
        // if it is still the newest request for this document. Continuous
        // typing then triggers one rebuild, not one per keystroke.
        let generation = {
            let mut pending = self.pending_index.write().unwrap();
            let slot = pending.entry(uri.clone()).or_insert(0);
            *slot += 1;
            *slot
        };
        let index = Arc::clone(&self.index);
        let roots = self.root_dirs.read().unwrap().clone();
        let pending = Arc::clone(&self.pending_index);
        let documents = Arc::clone(&self.documents);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if pending.read().unwrap().get(&uri) != Some(&generation) {
                return;
            }
            tokio::task::spawn_blocking(move || {
                index::build_and_insert(&index, &uri, &text, version, &roots, 0);
                // Closed dependents keep their own (unchanged) label/include
                // data; only their *merged* completion view would be stale,
                // and that is only consumed while they are open.
                let dependents: Vec<(Url, String, i32)> = {
                    let docs = documents.read().unwrap();
                    index
                        .dependents_transitive(&uri)
                        .into_iter()
                        .filter_map(|dependent| {
                            docs.get(&dependent)
                                .map(|doc| (dependent.clone(), doc.text.clone(), doc.version))
                        })
                        .collect()
                };
                for (dependent, text, version) in dependents {
                    index::build_and_insert(&index, &dependent, &text, version, &roots, 0);
                }
            })
            .await
            .ok();
        });
    }

    /// Roots whose translation unit contains `uri`: a `.dts` is its own root,
    /// anything else is looked up through the reverse include edges (falling
    /// back to the file itself so a lone `.dtsi` still gets its closure
    /// checked).
    fn tu_roots_for(&self, uri: &Url) -> Vec<(Url, PathBuf)> {
        let Ok(path) = uri.to_file_path() else {
            return Vec::new();
        };
        if path.extension().and_then(|ext| ext.to_str()) == Some("dts") {
            return vec![(uri.clone(), path)];
        }
        let open = self.documents.read().unwrap();
        let mut roots: Vec<(Url, PathBuf)> = self
            .index
            .dependents_transitive(uri)
            .into_iter()
            .filter_map(|dependent| {
                let path = dependent.to_file_path().ok()?;
                (path.extension().and_then(|ext| ext.to_str()) == Some("dts"))
                    .then_some((dependent, path))
            })
            .collect();
        // prefer boards that are currently open, then a stable order
        roots.sort_by_key(|(root, path)| (!open.contains_key(root), path.clone()));
        drop(open);
        roots.truncate(4);
        if roots.is_empty() {
            roots.push((uri.clone(), path));
        }
        roots
    }

    /// The unit that covers `uri`, when one has been checked already.
    fn unit_for(&self, uri: &Url) -> Option<Arc<TuData>> {
        let (root, _) = self.tu_roots_for(uri).into_iter().next()?;
        self.tu_data.read().unwrap().get(&root).cloned()
    }

    /// `&label` completion. A checked unit is authoritative (it is what the
    /// preprocessor actually produced): labels expanded from macros are
    /// present and labels in disabled `#ifdef` branches are not. The live
    /// buffer is merged on top so a label just typed in this file is offered
    /// before the next run.
    fn label_items(&self, uri: &Url, doc: &Document, prefix: &str) -> Vec<CompletionItem> {
        if let Some(data) = self.unit_for(uri) {
            let labels = &data.labels;
            let current = uri.to_file_path().ok();
            let mut map: completion::LabelMap = HashMap::new();
            for (name, label) in labels {
                let file = match &current {
                    Some(path) if path == &label.file => String::new(),
                    _ => label
                        .file
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                };
                map.insert(name.clone(), (label.path.clone(), file));
            }
            let mut items = completion::items_from_index(&map, prefix);
            let mut seen: HashSet<String> =
                items.iter().map(|item| item.label.clone()).collect();
            for item in completion::same_file_items(doc) {
                if item.label.starts_with(prefix) && seen.insert(item.label.clone()) {
                    items.push(item);
                }
            }
            items.sort_by(|a, b| a.label.cmp(&b.label));
            return items;
        }
        // no unit yet (or no cpp): fall back to the index's merged views
        match self.index.with_file(uri, |file| {
            file.map(|file| completion::items_from_index(&file.completion_labels, prefix))
        }) {
            Some(items) => items,
            None => completion::same_file_items(doc)
                .into_iter()
                .filter(|item| item.label.starts_with(prefix))
                .collect(),
        }
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
        let pending = Arc::clone(&self.tu_pending);
        let tu_data = Arc::clone(&self.tu_data);
        let snapshots = Arc::clone(&self.tu_snapshots);
        let include_dirs = self.include_dirs();
        let client = self.client.clone();
        let task_root = root_uri.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if pending.read().unwrap().get(&task_root) != Some(&generation) {
                return;
            }
            let overlays_docs = Arc::clone(&documents);
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
                let previous = snapshots.lock().unwrap().remove(&blocking_root);
                let mut doc = match previous {
                    Some((doc, _files)) => doc,
                    None => Document::open(&result.text, 0),
                };
                doc.update_text(&result.text);
                let output = tu_output(&result, &doc);
                snapshots
                    .lock()
                    .unwrap()
                    .insert(blocking_root.clone(), (doc, result.files.clone()));
                output
            })
            .await
            .ok();
            debug!(
                root = %task_root,
                elapsed_us = started.elapsed().as_micros() as u64,
                files = tu_result.as_ref().map(|out| out.per_file.len()).unwrap_or(0),
                "translation unit checked"
            );
            let Some(output) = tu_result else {
                warn!(root = %task_root, "translation unit check could not run");
                return;
            };
            tu_data
                .write()
                .unwrap()
                .insert(task_root.clone(), Arc::new(output.data));
            for (path, structural) in output.per_file {
                let Ok(uri) = Url::from_file_path(&path) else {
                    continue;
                };
                let (open, version) = {
                    let docs = documents.read().unwrap();
                    match docs.get(&uri) {
                        Some(doc) => (true, Some(doc.version)),
                        None => (false, None),
                    }
                };
                if !open {
                    continue;
                }
                let mut merged = file_diags
                    .read()
                    .unwrap()
                    .get(&uri)
                    .cloned()
                    .unwrap_or_default();
                // the per-file pass already reports what it can see; only add
                // what the merged unit found (cross-file duplicates, ...)
                for diag in structural {
                    let known = merged.iter().any(|existing| {
                        existing.range.start == diag.range.start
                            && existing.message == diag.message
                    });
                    if !known {
                        merged.push(diag);
                    }
                }
                client.publish_diagnostics(uri, merged, version).await;
            }
        });
    }
}

/// A label defined in a preprocessed unit.
pub(crate) struct TuLabel {
    /// node path, for the completion detail
    pub(crate) path: String,
    /// file the label is defined in
    pub(crate) file: PathBuf,
    /// range of the label in that file
    pub(crate) range: Range,
}

/// An `&label` use in a preprocessed unit.
struct TuRef {
    label: String,
    file: PathBuf,
    range: Range,
}

/// Everything the unit tells us besides diagnostics: the names that exist
/// after preprocessing (so macros and conditionals are already resolved).
pub(crate) struct TuData {
    pub(crate) labels: HashMap<String, TuLabel>,
    refs: Vec<TuRef>,
    pub(crate) defines: index::DefineMap,
    pub(crate) cells: Vec<index::CellsProp>,
}

struct TuOutput {
    per_file: HashMap<PathBuf, Vec<Diagnostic>>,
    data: TuData,
}

fn tu_output(unit: &crate::tu::TuResult, doc: &Document) -> TuOutput {
    TuOutput {
        per_file: group_diagnostics(unit, doc),
        data: tu_data(unit, doc),
    }
}

fn tu_data(unit: &crate::tu::TuResult, doc: &Document) -> TuData {
    let mut data = TuData {
        labels: HashMap::new(),
        refs: Vec::new(),
        defines: HashMap::new(),
        cells: Vec::new(),
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
                    let owner = node.parent().map(|parent| {
                        if parent.kind() == "property" {
                            util::owning_node(parent).unwrap_or(parent)
                        } else {
                            parent
                        }
                    });
                    let path = owner
                        .map(|owner| crate::analysis::node_path(doc, owner))
                        .unwrap_or_default();
                    data.labels.entry(name.to_string()).or_insert(TuLabel {
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
                    data.refs.push(TuRef {
                        label: name.to_string(),
                        file,
                        range,
                    });
                }
            }
            "property" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = util::node_text(doc, name_node);
                    if name.starts_with('#')
                        && name.ends_with("-cells")
                        && let Some(owner) = util::owning_node(node)
                    {
                        let path = crate::analysis::node_path(doc, owner);
                        if let Some(value) = property_u32(doc, node) {
                            data.cells.push(index::CellsProp {
                                path,
                                name: name.to_string(),
                                value,
                            });
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

fn property_u32(doc: &Document, property: tree_sitter::Node) -> Option<u32> {
    for i in 0..property.child_count() {
        let child = property.child(i as u32)?;
        if child.kind() == "prop_data" {
            let text = util::node_text(doc, child);
            return text
                .split(|c: char| !c.is_ascii_hexdigit() && c != 'x' && c != 'X')
                .find(|piece| !piece.is_empty())
                .and_then(|piece| {
                    let piece = piece.trim_start_matches(['x', 'X']);
                    u32::from_str_radix(piece, 16).ok().or_else(|| piece.parse().ok())
                });
        }
    }
    None
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
        let Some(file) = error.file else { continue };
        let path = unit.files[file as usize].clone();
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
        let kernel_root = index::detect_kernel_root(&roots);
        roots.extend(index::detect_include_dirs(&roots));
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
                let bindings = Arc::clone(&self.bindings);
                let dir = dir.clone();
                tokio::task::spawn_blocking(move || bindings::scan(&bindings, &dir));
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
        *self.status.write().unwrap() = issues;
        *self.root_dirs.write().unwrap() = roots;
        // Background workspace scan: index every dts file under the roots so
        // goto definition / references work for files never opened.
        let index = Arc::clone(&self.index);
        let scan_roots = self.root_dirs.read().unwrap().clone();
        let open: HashSet<Url> = self.documents.read().unwrap().keys().cloned().collect();
        tokio::task::spawn_blocking(move || index::scan_workspace(&index, &scan_roots, &open));
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
            definition_provider: Some(OneOf::Left(true)),
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
        let tu_roots = self.tu_roots_for(&td.uri);
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
        self.schedule_file_index(td.uri.clone(), td.text, version);
        self.client
            .publish_diagnostics(td.uri.clone(), diags, Some(version))
            .await;
        for (root_uri, root_path) in tu_roots {
            self.schedule_tu(root_uri, root_path);
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let started = std::time::Instant::now();
        let uri = params.text_document.uri;
        let tu_roots = self.tu_roots_for(&uri);
        let (diags, version, text) = {
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
                    (diags, doc.version, doc.text.clone())
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
        self.schedule_file_index(uri.clone(), text, version);
        self.client
            .publish_diagnostics(uri.clone(), diags, Some(version))
            .await;
        for (root_uri, root_path) in tu_roots {
            self.schedule_tu(root_uri, root_path);
        }
        debug!(total_us = started.elapsed().as_micros() as u64, "did_change done");
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents
            .write()
            .unwrap()
            .remove(&params.text_document.uri);
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
            // Read-only, in-memory, no awaits: steady state reads the
            // prebuilt index (no tree walk) and stays well under 1 ms.
            let index = &self.index;
            match context {
                completion::Context::Labels(prefix) => {
                    self.label_items(&uri, doc, &prefix)
                },
                completion::Context::Defines(prefix) => {
                    if let Some(data) = self.unit_for(&uri) {
                        completion::items_from_defines(&data.defines, &prefix)
                    } else {
                        index
                            .with_file(&uri, |file| {
                                file.map(|file| {
                                    completion::items_from_defines(&file.defines, &prefix)
                                })
                            })
                            .unwrap_or_default()
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
        let mut locations = Vec::new();
        if params.context.include_declaration {
            for (def_uri, def) in self.index.all_label_defs(&label) {
                locations.push(Location {
                    uri: def_uri,
                    range: def.range,
                });
                if locations.len() >= 500 {
                    return Ok(Some(locations));
                }
            }
        }
        for (ref_uri, range) in self.index.label_references(&label, 500) {
            locations.push(Location {
                uri: ref_uri,
                range,
            });
        }
        Ok(Some(locations))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let docs = self.documents.read().unwrap();
        let Some(doc) = docs.get(&uri) else {
            return Ok(None);
        };
        let unit = self.unit_for(&uri);
        let hints = hints::inlay_hints(
            doc,
            &uri,
            &self.index,
            unit.as_deref(),
            &params.range,
        );
        Ok((!hints.is_empty()).then_some(hints))
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
        let docs = self.documents.read().unwrap();
        let doc = docs.get(&uri)?;
        let byte = util::position_to_byte(&doc.rope, position);
        if let Some(label) = completion::label_at_cursor(doc, byte) {
            if let Some(data) = self.unit_for(&uri)
                && let Some(def) = data.labels.get(&label)
                && let Ok(target) = Url::from_file_path(&def.file)
            {
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: target,
                    range: def.range,
                }));
            }
            let (target_uri, def) = index::resolve_label(&self.index, &uri, &label)?;
            return Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri,
                range: def.range,
            }));
        }
        // not on a label: jump to the binding of the enclosing node
        let node = doc.tree.root_node().descendant_for_byte_range(byte, byte)?;
        let subnode = enclosing_node(node)?;
        let compatibles = node_compatibles(doc, subnode)?;
        let binding = self.bindings.lookup(&compatibles)?;
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: binding.uri.clone(),
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
        }))
    }
}

fn enclosing_node(mut node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    loop {
        if node.kind() == "subnode" {
            return Some(node);
        }
        node = node.parent()?;
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
