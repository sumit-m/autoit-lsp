//! autoit-lsp — Language Server for AutoIt v3
//!
//! v0.1 wraps `Au3Check.exe` (AutoIt's official linter) and surfaces its
//! output as LSP diagnostics. Speaks LSP over stdio.

mod au3check;

use std::path::PathBuf;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug)]
struct Backend {
    client: Client,
    /// Path to Au3Check.exe resolved at startup. `None` means AutoIt
    /// isn't installed in a known location — handlers log a warning
    /// at startup and silently skip diagnostic publishes thereafter.
    au3check: Option<PathBuf>,
}

impl Backend {
    /// Run Au3Check against the URI's file path and publish diagnostics
    /// back to the client. No-op if Au3Check isn't available or the URI
    /// can't be resolved to a local path.
    async fn check_and_publish(&self, uri: Url) {
        let Some(au3check) = self.au3check.as_deref() else {
            return;
        };
        let Ok(path) = uri.to_file_path() else {
            tracing::debug!(uri = %uri, "ignoring non-file URI");
            return;
        };
        match au3check::run_au3check(au3check, &path).await {
            Ok(output) => {
                let diags = au3check::parse_diagnostics(&output, &path);
                tracing::debug!(uri = %uri, count = diags.len(), "publishing diagnostics");
                self.client.publish_diagnostics(uri, diags, None).await;
            }
            Err(e) => {
                tracing::warn!(uri = %uri, error = %e, "Au3Check invocation failed");
            }
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "autoit-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                // openClose + save drive our diagnostic refresh. `change` is
                // NONE for v0.1 because Au3Check needs a file on disk and
                // re-linting unsaved buffers requires temp-file staging
                // (v0.2 work).
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::NONE),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        match self.au3check.as_deref() {
            Some(path) => tracing::info!(
                au3check = %path.display(),
                "autoit-lsp initialized"
            ),
            None => tracing::warn!(
                "Au3Check.exe not found in registry or default path — diagnostics disabled"
            ),
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.check_and_publish(params.text_document.uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.check_and_publish(params.text_document.uri).await;
    }
}

#[tokio::main]
async fn main() {
    // LSP servers speak JSON-RPC on stdout. Logging must go to stderr so
    // it doesn't corrupt the protocol stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let au3check = au3check::discover_au3check();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend { client, au3check });
    Server::new(stdin, stdout, socket).serve(service).await;
}
