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
    documents: RwLock<HashMap<Url, Document>>,
    root_dirs: RwLock<Vec<PathBuf>>,
    /// Workspace-wide contribution tables (labels, refs, includes), built in
    /// background; requests read finished entries only.
    index: Arc<Index>,
    /// compatible -> devicetree binding, built in background at startup.
    bindings: Arc<BindingIndex>,
    /// Whole-feature conditions (unavailable = error, degraded = warning),
    /// reported at the top of every file.
    status: RwLock<Vec<FeatureIssue>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: RwLock::new(HashMap::new()),
            root_dirs: RwLock::new(Vec::new()),
            index: Arc::new(Index::default()),
            bindings: Arc::new(BindingIndex::default()),
            status: RwLock::new(Vec::new()),
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
        let index = Arc::clone(&self.index);
        let roots = self.root_dirs.read().unwrap().clone();
        // capture live text of open dependents before leaving the request
        let dependents: Vec<(Url, Option<(String, i32)>)> = {
            let docs = self.documents.read().unwrap();
            self.index
                .dependents_transitive(&uri)
                .into_iter()
                .map(|dependent| {
                    let live = docs
                        .get(&dependent)
                        .map(|doc| (doc.text.clone(), doc.version));
                    (dependent, live)
                })
                .collect()
        };
        tokio::task::spawn_blocking(move || {
            index::build_and_insert(&index, &uri, &text, version, &roots, 0);
            // Closed dependents keep their own (unchanged) label/include data;
            // only their *merged* completion view would be stale, and that is
            // only consumed while they are open. Rebuilding every closed board
            // file that includes this dtsi on every keystroke is not worth it.
            for (dependent, live) in dependents {
                if let Some((text, version)) = live {
                    index::build_and_insert(&index, &dependent, &text, version, &roots, 0);
                }
            }
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
                    diagnostics::all(
                        doc,
                        td.uri.to_file_path().ok().as_deref(),
                        &self.include_dirs(),
                        &self.bindings,
                        &self.status.read().unwrap(),
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
                    let diags = diagnostics::all(
                        doc,
                        uri.to_file_path().ok().as_deref(),
                        &self.include_dirs(),
                        &self.bindings,
                        &self.status.read().unwrap(),
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
                    let mut items = property_items(doc, byte, &prefix, &self.bindings);
                    // any node may get a `compatible` unless it already has
                    // one (or is endpoint/port plumbing)
                    if node_can_take_compatible(doc, byte)
                        && let Some(item) = completion::compatible_property_item(&prefix)
                    {
                        items.push(item);
                    }
                    items
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

/// Binding-based property completion for the node enclosing `byte`.
fn property_items(
    doc: &Document,
    byte: usize,
    prefix: &str,
    bindings: &BindingIndex,
) -> Vec<CompletionItem> {
    let Some(node) = doc.tree.root_node().descendant_for_byte_range(byte, byte) else {
        return Vec::new();
    };
    let Some(subnode) = enclosing_node(node) else {
        return Vec::new();
    };
    let Some(compatibles) = node_compatibles(doc, subnode) else {
        return Vec::new();
    };
    let Some(binding) = bindings.lookup(&compatibles) else {
        return Vec::new();
    };
    let mut existing = HashSet::new();
    if let Some(body) = util::node_def_of(subnode) {
        for i in 0..body.child_count() {
            let Some(child) = body.child(i as u32) else { continue };
            if child.kind() == "property"
                && let Some(name) = child.child_by_field_name("name")
            {
                existing.insert(util::node_text(doc, name).to_string());
            }
        }
    }
    completion::items_from_binding(&binding, &existing, prefix)
}

/// `compatible` may be added to any node that does not already have one,
/// except endpoint/port plumbing.
fn node_can_take_compatible(doc: &Document, byte: usize) -> bool {
    if completion::in_endpoint_or_port(doc, byte) {
        return false;
    }
    let Some(node) = completion::enclosing_node(doc, byte) else {
        return false;
    };
    diagnostics::compatible_strings(doc, node).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::Binding;

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
            strict: true,
            refs_complete: true,
            patterns: Vec::new(),
        });
        // cursor right after "clo" on its line
        let byte = text.find("clo").unwrap() + 3;
        let items = property_items(&doc, byte, "clo", &bindings);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["clock-names", "clocks"], "items: {labels:?}");
    }
}
