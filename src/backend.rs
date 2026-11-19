use std::collections::{HashMap, HashSet};
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
        self.schedule_file_index(td.uri.clone(), td.text, version);
        self.client
            .publish_diagnostics(td.uri, diags, Some(version))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let started = std::time::Instant::now();
        let uri = params.text_document.uri;
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
        self.schedule_file_index(uri.clone(), text, version);
        self.client
            .publish_diagnostics(uri, diags, Some(version))
            .await;
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
                completion::Context::Labels(prefix) => match index.with_file(&uri, |fi| {
                    fi.map(|f| completion::items_from_index(&f.completion_labels, &prefix))
                }) {
                    Some(items) => items,
                    None => completion::same_file_items(doc)
                        .into_iter()
                        .filter(|item| item.label.starts_with(&prefix))
                        .collect(),
                },
                completion::Context::Defines(prefix) => index
                    .with_file(&uri, |fi| {
                        fi.map(|f| completion::items_from_defines(&f.defines, &prefix))
                    })
                    .unwrap_or_default(),
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
        let hints = hints::inlay_hints(doc, &uri, &self.index, &params.range);
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
