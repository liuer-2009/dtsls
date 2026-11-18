mod analysis;
mod backend;
mod bindings;
mod completion;
mod diagnostics;
mod document;
mod hints;
mod index;
mod semantic_tokens;
mod status;
mod symbols;
mod util;

use tower_lsp::{LspService, Server};

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let path = std::env::var("DTSLS_LOG").unwrap_or_else(|_| "/tmp/dtsls.log".to_string());
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        eprintln!("dtsls: cannot open log file {path}");
        return;
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let (service, socket) = LspService::new(backend::Backend::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
