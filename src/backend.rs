// SPDX-License-Identifier: AGPL-3.0-only
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
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
use crate::unit::TuData;
use crate::workspace::{self, Workspace};

use crate::model;
use crate::status::FeatureIssue;
use crate::unit;
use crate::{diagnostics, semantic_tokens, symbols};

pub struct Inner {
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
    /// Per-buffer diagnostics (syntax, includes, bindings, notices).
    buf_diags: Arc<RwLock<HashMap<Url, Vec<Diagnostic>>>>,
    /// Unit-level diagnostics mapped back per file (duplicates, phandle,
    /// preprocessor errors). Disjoint from `buf_diags` by design.
    unit_diags: Arc<RwLock<HashMap<Url, Vec<Diagnostic>>>>,
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

/// The tower-lsp service handle: a shared handle onto the server state.
pub struct Backend(Arc<Inner>);

impl Backend {
    pub fn new(client: Client) -> Self {
        Self(Arc::new(Inner::new(client)))
    }
}

impl Inner {
    fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(RwLock::new(HashMap::new())),
            root_dirs: RwLock::new(Vec::new()),
            workspace: Arc::new(Workspace::default()),
            bindings: Arc::new(BindingIndex::default()),
            status: RwLock::new(Vec::new()),
            diag_cache: Arc::new(diagnostics::DiagCache::default()),
            buf_diags: Arc::new(RwLock::new(HashMap::new())),
            unit_diags: Arc::new(RwLock::new(HashMap::new())),
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

    /// Context of a document: the board whose unit contains it, or the file
    /// itself when it was not opened inside a board. A board opened later
    /// adopts standalone documents that are in its include closure.
    fn context_path(&self, uri: &Url) -> Option<PathBuf> {
        let path = uri.to_file_path().ok()?;
        if let Some(context) = self.doc_context.read().unwrap().get(uri) {
            return Some(context.clone());
        }
        let is_board = path.extension().and_then(|ext| ext.to_str()) == Some("dts");
        Some(context_for(
            &path,
            is_board,
            self.active_board.read().unwrap().as_ref(),
            &self.workspace,
        ))
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
        self.workspace.prune(&protected);
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
        self.workspace.unit(&self.context_path(uri)?)
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
}

/// Re-check a translation unit off the main loop. Coalesced: typing only
/// triggers one run once the edits settle.
fn schedule_tu(shared: &Arc<Inner>, root_uri: Url, root_path: PathBuf) {
    debug!(root = %root_uri, "tu scheduled");
    let this = Arc::clone(shared);
    let generation = {
        let mut pending = this.tu_pending.write().unwrap();
        let slot = pending.entry(root_uri.clone()).or_insert(0);
        *slot += 1;
        *slot
    };
    let include_dirs = shared.include_dirs();
    let task_root = root_uri.clone();
    let publish_root = root_path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if this.tu_pending.read().unwrap().get(&task_root) != Some(&generation) {
            return;
        }
        let started = std::time::Instant::now();
        let root_path_task = root_path.clone();
        let blocking = Arc::clone(&this);
        let tu_result = tokio::task::spawn_blocking(move || {
            // clangd's VFS equivalent: every open buffer overrides the
            // on-disk file inside the preprocessor run
            let overlays: BTreeMap<PathBuf, String> = {
                let docs = blocking.documents.read().unwrap();
                docs.iter()
                    .filter_map(|(uri, doc)| {
                        let path = uri.to_file_path().ok()?;
                        let path = std::fs::canonicalize(&path).unwrap_or(path);
                        Some((path, doc.text.clone()))
                    })
                    .collect()
            };
            let result = unit::preprocess(&unit::TuInput {
                root: &root_path_task,
                include_dirs: &include_dirs,
                overlays: &overlays,
            });
            let previous = blocking.workspace.take_snapshot(&root_path);
            let mut doc = match previous {
                Some((doc, _files)) => doc,
                None => Document::open(&result.text, 0),
            };
            doc.update_text(&result.text);
            let output = unit::tu_output(&result, &doc);
            blocking.workspace.put_snapshot(root_path.clone(), doc, result.files.clone());
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
                this.client
                    .publish_diagnostics(
                        task_root,
                        vec![Diagnostic {
                            range: Range::default(),
                            severity: Some(DiagnosticSeverity::ERROR),
                            source: Some("dtsls".to_string()),
                            message: format!(
                                "internal: translation unit analysis failed ({error}); \
                                 diagnostics for this file are incomplete"
                            ),
                            ..Default::default()
                        }],
                        None,
                    )
                    .await;
                return;
            }
        };
        this.workspace.store_unit(publish_root.clone(), Arc::new(output.data));
        let protected: HashSet<PathBuf> = this
            .doc_context
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect();
        this.workspace.prune(&protected);
        let per_file = output.per_file;
        let root_canonical = std::fs::canonicalize(&publish_root)
            .unwrap_or_else(|_| publish_root.clone());
        let mut open_docs: Vec<(Url, i32)> = {
            let docs = this.documents.read().unwrap();
            docs.iter().map(|(uri, doc)| (uri.clone(), doc.version)).collect()
        };
        if let Some(active) = this.last_active.read().unwrap().clone() {
            open_docs.sort_by_key(|(uri, _)| uri != &active);
        }
        for (uri, version) in open_docs {
            let in_context = this
                .doc_context
                .read()
                .unwrap()
                .get(&uri)
                .map(|context| {
                    std::fs::canonicalize(context).unwrap_or_else(|_| context.clone())
                        == root_canonical
                })
                .unwrap_or(false);
            if !in_context {
                continue;
            }
            let structural = uri
                .to_file_path()
                .ok()
                .map(|path| {
                    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                    per_file.get(&canonical).cloned().unwrap_or_default()
                })
                .unwrap_or_default();
            // The buffer tier (did_open/did_change) always publishes the
            // buffer diagnostics immediately, so the unit tier only has to
            // republish a file when its unit diagnostics actually changed —
            // and a file that never had unit diagnostics needs nothing when
            // it still has none.
            let changed = match this.unit_diags.read().unwrap().get(&uri) {
                Some(old) => {
                    old.len() != structural.len()
                        || old.iter().zip(&structural).any(|(old, new)| {
                            old.range != new.range
                                || old.severity != new.severity
                                || old.message != new.message
                        })
                }
                None => !structural.is_empty(),
            };
            if !changed {
                continue;
            }
            let mut merged = this
                .buf_diags
                .read()
                .unwrap()
                .get(&uri)
                .cloned()
                .unwrap_or_default();
            merged.extend(structural.iter().cloned());
            this.unit_diags.write().unwrap().insert(uri.clone(), structural);
            this.client.publish_diagnostics(uri, merged, Some(version)).await;
        }
    });
}

/// The analysis context of a file: the board when the file is a board or is
/// inside the active board's include closure, else the file itself (a lone
/// `.dtsi` is its own root, like dts-lsp's ad-hoc context).
fn context_for(
    path: &Path,
    is_board: bool,
    active_board: Option<&PathBuf>,
    workspace: &Workspace,
) -> PathBuf {
    if is_board {
        return path.to_path_buf();
    }
    match active_board {
        Some(board) if workspace.closure_contains(board, path) => board.to_path_buf(),
        _ => path.to_path_buf(),
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
        let raw = model::node_text(doc, path_node);
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
                    start: model::byte_to_position(doc, path_node.start_byte()),
                    end: model::byte_to_position(doc, path_node.end_byte()),
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

    impl Inner {
    /// Publish the union of the buffer-tier and unit-tier diagnostics. The
    /// two tiers are disjoint by design, so plain concatenation is exact.
    async fn publish(&self, uri: &Url, version: Option<i32>) {
        let mut diags = self
            .buf_diags
            .read()
            .unwrap()
            .get(uri)
            .cloned()
            .unwrap_or_default();
        diags.extend(
            self.unit_diags
                .read()
                .unwrap()
                .get(uri)
                .cloned()
                .unwrap_or_default(),
        );
        self.client.publish_diagnostics(uri.clone(), diags, version).await;
    }
}

/// Everything the background scan needs: bindings dir (when found), include
/// dirs, the roots to scan and the feature notices computed at initialize.
struct ScanJob {
    bindings_dir: Option<PathBuf>,
    include_dirs: Vec<PathBuf>,
    scan_roots: Vec<PathBuf>,
    issues: Vec<FeatureIssue>,
}

/// Scan bindings, refresh every open document, then build the workspace
/// include graph — in that order, so the refresh is not competing for CPU.
fn spawn_scan(shared: &Arc<Inner>, job: ScanJob) {
    let this = Arc::clone(shared);
    tokio::spawn(async move {
        let ScanJob {
            bindings_dir,
            include_dirs,
            scan_roots,
            issues,
        } = job;
        if let Some(dir) = bindings_dir {
            if let Err(error) = tokio::task::spawn_blocking({
                let bindings = Arc::clone(&this.bindings);
                move || bindings::scan(&bindings, &dir)
            })
            .await
            {
                // a panicking scan leaves every binding check unreliable:
                // say so instead of silently serving stale results
                error!(%error, "binding scan failed");
                report_internal(
                    &this.client,
                    &this.documents,
                    format!(
                        "internal: the binding scan failed ({error}); binding validation \
                         is disabled for this session"
                    ),
                )
                .await;
            }
            // the active document first: it is what the editor shows
            let entries: Vec<(Url, i32)> = {
                let docs = this.documents.read().unwrap();
                docs.iter().map(|(uri, doc)| (uri.clone(), doc.version)).collect()
            };
            let active = this.last_active.read().unwrap().clone();
            let mut ordered = entries;
            if let Some(active) = active {
                ordered.sort_by_key(|(uri, _)| *uri != active);
            }
            let compute_started = std::time::Instant::now();
            let mut docs = 0usize;
            let mut diagnostics = 0usize;
            let publish_started = std::time::Instant::now();
            for (uri, version) in ordered {
                let diags = {
                    let docs = this.documents.read().unwrap();
                    match docs.get(&uri) {
                        Some(doc) => diagnostics::buffer(
                            doc,
                            uri.to_file_path().ok().as_deref(),
                            &include_dirs,
                            &this.bindings,
                            &issues,
                            &this.diag_cache,
                            this.workspace
                                .unit(&uri.to_file_path().unwrap_or_default())
                                .is_none(),
                        ),
                        None => Vec::new(),
                    }
                };
                this.buf_diags.write().unwrap().insert(uri.clone(), diags);
                let mut merged = this.buf_diags.read().unwrap().get(&uri).cloned().unwrap_or_default();
                merged.extend(this.unit_diags.read().unwrap().get(&uri).cloned().unwrap_or_default());
                diagnostics += merged.len();
                docs += 1;
                this.client.publish_diagnostics(uri, merged, Some(version)).await;
            }
            debug!(
                compute_us = compute_started.elapsed().as_micros() as u64,
                refresh_us = publish_started.elapsed().as_micros() as u64,
                docs,
                diagnostics,
                "bindings ready; diagnostics refreshed"
            );
        }
        if let Err(error) = tokio::task::spawn_blocking({
            let workspace = Arc::clone(&this.workspace);
            move || workspace.scan(&scan_roots, &scan_roots)
        })
        .await
        {
            error!(%error, "workspace scan failed");
            report_internal(
                &this.client,
                &this.documents,
                format!(
                    "internal: the workspace scan failed ({error}); label and ref \
                     navigation is incomplete"
                ),
            )
            .await;
        }
    });
}

impl Inner {
    fn initialize(&self, params: InitializeParams) -> (InitializeResult, Option<ScanJob>) {
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
        // capture include dirs AFTER root_dirs is populated: the scan refresh
        // republishes every open document with these, and an empty list would
        // flag every `<dt-bindings/...>` include as unresolved
        let job = Some(ScanJob {
            bindings_dir: scan_dir,
            include_dirs: self.include_dirs(),
            scan_roots: self.root_dirs.read().unwrap().clone(),
            issues: issues.clone(),
        });
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
        (InitializeResult {
            capabilities,
            server_info: Some(ServerInfo {
                name: "dtsls".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        }, job)
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

    async fn did_open(&self, params: DidOpenTextDocumentParams) -> Vec<(Url, PathBuf)> {
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
                    diagnostics::buffer(
                        doc,
                        td.uri.to_file_path().ok().as_deref(),
                        &self.include_dirs(),
                        &self.bindings,
                        &self.status.read().unwrap(),
                        &self.diag_cache,
                        self.unit_for(&td.uri).is_none(),
                    ),
                    doc.version,
                ),
                None => return Vec::new(),
            }
        };
        self.buf_diags
            .write()
            .unwrap()
            .insert(td.uri.clone(), diags);
        self.publish(&td.uri, Some(version)).await;
        let protected = self.protected_contexts();
        self.workspace.prune(&protected);
        tu_roots
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) -> Vec<(Url, PathBuf)> {
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
                    let diags = diagnostics::buffer(
                        doc,
                        uri.to_file_path().ok().as_deref(),
                        &self.include_dirs(),
                        &self.bindings,
                        &self.status.read().unwrap(),
                        &self.diag_cache,
                        self.unit_for(&uri).is_none(),
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
                    return Vec::new();
                }
            }
        };
        self.buf_diags.write().unwrap().insert(uri.clone(), diags);
        self.publish(&uri, Some(version)).await;
        debug!(total_us = started.elapsed().as_micros() as u64, "did_change done");
        tu_roots
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
        self.buf_diags.write().unwrap().remove(&uri);
        self.unit_diags.write().unwrap().remove(&uri);
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
                completion::completion_context(doc, model::position_to_byte(doc, position))
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
                    let byte = model::position_to_byte(doc, position);
                    completion::property_completion(doc, byte, &prefix, &self.bindings)
                }
                completion::Context::Compatible { prefix, vendor } => {
                    let byte = model::position_to_byte(doc, position);
                    // replace everything typed inside the quotes so far; if the
                    // cursor sits mid-string, replace up to the closing quote
                    let line_start = model::line_start_of(&doc.text, byte);
                    let head = doc.slice(line_start..byte);
                    let quote_start = head
                        .rfind('"')
                        .map(|offset| line_start + offset + 1)
                        .unwrap_or(byte);
                    let line_end = doc
                        .line_starts
                        .iter()
                        .find(|&&start| start > byte)
                        .copied()
                        .unwrap_or(doc.text.len());
                    let tail = doc.slice(byte..line_end);
                    let end = tail
                        .find('"')
                        .map(|offset| byte + offset)
                        .unwrap_or(byte);
                    let range = Range {
                        start: model::byte_to_position(doc, quote_start),
                        end: model::byte_to_position(doc, end),
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
            completion::label_at_cursor(doc, model::position_to_byte(doc, position))
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

impl Inner {
    fn definition(&self, params: GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        self.location(&uri, position)
            .map(GotoDefinitionResponse::Scalar)
    }

    fn location(&self, uri: &Url, position: Position) -> Option<Location> {
        let docs = self.documents.read().unwrap();
        let doc = docs.get(uri)?;
        let byte = model::position_to_byte(doc, position);
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
            .and_then(|subnode| diagnostics::compatible_strings(doc, subnode))
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


#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let (result, job) = self.0.initialize(params);
        if let Some(job) = job {
            spawn_scan(&Arc::clone(&self.0), job);
        }
        Ok(result)
    }

    async fn initialized(&self, params: InitializedParams) {
        self.0.initialized(params).await;
    }

    async fn shutdown(&self) -> Result<()> {
        self.0.shutdown().await
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let roots = self.0.did_open(params).await;
        for (root_uri, root_path) in roots {
            schedule_tu(&Arc::clone(&self.0), root_uri, root_path);
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let roots = self.0.did_change(params).await;
        for (root_uri, root_path) in roots {
            schedule_tu(&Arc::clone(&self.0), root_uri, root_path);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.0.did_close(params).await;
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.0.completion(params).await
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.0.goto_definition(params).await
    }

    async fn goto_declaration(
        &self,
        params: GotoDeclarationParams,
    ) -> Result<Option<GotoDeclarationResponse>> {
        self.0.goto_declaration(params).await
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        self.0.references(params).await
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        self.0.inlay_hint(params).await
    }

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        self.0.document_link(params).await
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.0.document_symbol(params).await
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        self.0.semantic_tokens_full(params).await
    }
}

