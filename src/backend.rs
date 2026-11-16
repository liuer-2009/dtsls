use std::collections::HashMap;
use std::sync::RwLock;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{info, warn};

use crate::document::Document;
use crate::{diagnostics, semantic_tokens, symbols};

pub struct Backend {
    client: Client,
    documents: RwLock<HashMap<Url, Document>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: RwLock::new(HashMap::new()),
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        info!(root = ?params.root_uri, client = ?params.client_info, "initialize");
        let capabilities = ServerCapabilities {
            position_encoding: Some(PositionEncodingKind::UTF16),
            text_document_sync: Some(TextDocumentSyncCapability::Kind(
                TextDocumentSyncKind::INCREMENTAL,
            )),
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
                Some(doc) => (diagnostics::compute(doc), doc.version),
                None => return,
            }
        };
        self.client
            .publish_diagnostics(td.uri, diags, Some(version))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let (diags, version) = {
            let mut docs = self.documents.write().unwrap();
            match docs.get_mut(&uri) {
                Some(doc) => {
                    doc.apply_changes(&params.content_changes, params.text_document.version);
                    (diagnostics::compute(doc), doc.version)
                }
                None => {
                    warn!(uri = %uri, "did_change for unknown document");
                    return;
                }
            }
        };
        self.client
            .publish_diagnostics(uri, diags, Some(version))
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents
            .write()
            .unwrap()
            .remove(&params.text_document.uri);
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
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
