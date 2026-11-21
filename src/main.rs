mod backend;
mod bindings;
mod completion;
mod diagnostics;
mod document;
mod hints;
mod model;
mod phandle_cells;
mod preproc;
mod semantic_tokens;
mod status;
mod symbols;
mod unit;
mod workspace;

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

/// What the command line asks for.
enum CliAction {
    Serve,
    Version,
    Help,
}

fn cli_action(args: &[String]) -> CliAction {
    if args.iter().any(|a| a == "--version" || a == "-V") {
        return CliAction::Version;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return CliAction::Help;
    }
    for arg in args {
        if arg.starts_with('-') {
            eprintln!("dtsls: ignoring unknown flag {arg}");
        }
    }
    CliAction::Serve
}

#[tokio::main]
async fn main() {
    match cli_action(&std::env::args().skip(1).collect::<Vec<_>>()) {
        CliAction::Version => {
            println!("dtsls {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        CliAction::Help => {
            println!(
                "dtsls {} — Device Tree Source LSP server\n\n\
                 Usage: dtsls [--version] [--help]\n\n\
                 Runs the LSP server over stdio; point your editor at this binary.\n\
                 Logs go to $DTSLS_LOG (default /tmp/dtsls.log), verbosity via RUST_LOG.",
                env!("CARGO_PKG_VERSION")
            );
            return;
        }
        CliAction::Serve => {}
    }

    init_tracing();

    let (service, socket) = LspService::new(backend::Backend::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cli_flags() {
        assert!(matches!(cli_action(&args(&[])), CliAction::Serve));
        assert!(matches!(
            cli_action(&args(&["--version"])),
            CliAction::Version
        ));
        assert!(matches!(cli_action(&args(&["-V"])), CliAction::Version));
        assert!(matches!(cli_action(&args(&["--help"])), CliAction::Help));
        assert!(matches!(cli_action(&args(&["-h"])), CliAction::Help));
        assert!(matches!(cli_action(&args(&["--bogus"])), CliAction::Serve));
    }
}
