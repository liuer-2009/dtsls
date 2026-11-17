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
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: RwLock::new(HashMap::new()),
            root_dirs: RwLock::new(Vec::new()),
            index: Arc::new(Index::default()),
            bindings: Arc::new(BindingIndex::default()),
        }
    }

    /// Include dirs for diagnostics (file-relative resolution is separate).
    fn include_dirs(&self) -> Vec<PathBuf> {
        self.root_dirs.read().unwrap().clone()
    }


    /// Rebuild one document's contribution table off the main loop.
    fn schedule_file_index(&self, uri: Url, text: String, version: i32) {
        let index = Arc::clone(&self.index);
        let roots = self.root_dirs.read().unwrap().clone();
        tokio::task::spawn_blocking(move || {
            index::build_and_insert(&index, &uri, &text, version, &roots, 0);
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
        // `<dt-bindings/...>` resolution: auto-detect a kernel tree by walking
        // up for `include/dt-bindings`, plus client-supplied includePaths.
        let kernel_root = index::detect_kernel_root(&roots);
        roots.extend(index::detect_include_dirs(&roots));
        let mut bindings_dir = kernel_root
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
        if let Some(dir) = bindings_dir {
            let bindings = Arc::clone(&self.bindings);
            tokio::task::spawn_blocking(move || bindings::scan(&bindings, &dir));
        }
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
                trigger_characters: Some(vec!["&".to_string(), "<".to_string()]),
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
        let uri = params.text_document.uri;
        let (diags, version, text) = {
            let mut docs = self.documents.write().unwrap();
            match docs.get_mut(&uri) {
                Some(doc) => {
                    doc.apply_changes(&params.content_changes, params.text_document.version);
                    (
                        diagnostics::all(
                            doc,
                            uri.to_file_path().ok().as_deref(),
                            &self.include_dirs(),
                            &self.bindings,
                        ),
                        doc.version,
                        doc.text.clone(),
                    )
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
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let docs = self.documents.read().unwrap();
        let Some(doc) = docs.get(&uri) else {
            return Ok(None);
        };
        let byte = util::position_to_byte(&doc.rope, position);
        if let Some(label) = completion::label_at_cursor(doc, byte) {
            return Ok(index::resolve_label(&self.index, &uri, &label).map(
                |(target_uri, def)| {
                    GotoDefinitionResponse::Scalar(Location {
                        uri: target_uri,
                        range: def.range,
                    })
                },
            ));
        }
        // not on a label: jump to the binding of the enclosing node
        let Some(node) = doc.tree.root_node().descendant_for_byte_range(byte, byte) else {
            return Ok(None);
        };
        let Some(subnode) = enclosing_node(node) else {
            return Ok(None);
        };
        let Some(compatibles) = node_compatibles(doc, subnode) else {
            return Ok(None);
        };
        let binding = self.bindings.lookup(&compatibles);
        Ok(binding.map(|binding| {
            GotoDefinitionResponse::Scalar(Location {
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
            })
        }))
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
