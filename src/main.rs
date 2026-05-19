//! autoit-lsp — Language Server for AutoIt v3
//!
//! v0.1 wraps `Au3Check.exe` (AutoIt's official linter) and surfaces its
//! output as LSP diagnostics. Speaks LSP over stdio.

mod au3check;

use std::path::PathBuf;
use std::sync::RwLock;

use serde::Deserialize;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// LSP `initializationOptions` payload. The client (e.g. Zed) forwards
/// `lsp.autoit-lsp.initialization_options` from settings.json verbatim.
///
/// Today there's a single field. Adding more later is additive — serde's
/// default deserialization ignores unknown keys.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct InitializationOptions {
    /// Absolute path to `Au3Check.exe`. Used by portable / non-installer
    /// AutoIt setups that aren't in the registry and aren't at the default
    /// install path. If unset (or pointing to a non-existent file), the
    /// server falls back to its registry-and-default discovery chain.
    au3check_path: Option<String>,
}

#[derive(Debug)]
struct Backend {
    client: Client,
    /// Path to Au3Check.exe resolved at startup via the registry/default
    /// chain. `None` means none of those probes hit a real file.
    au3check: Option<PathBuf>,
    /// Override from `initializationOptions.au3checkPath`, populated on
    /// `initialize`. Takes priority over `au3check` when set. RwLock so
    /// the LSP trait's `&self` handlers can write it (interior mut).
    setting_override: RwLock<Option<PathBuf>>,
}

impl Backend {
    /// Effective Au3Check path: setting override if present, otherwise
    /// the path discovered at startup.
    fn resolved_au3check(&self) -> Option<PathBuf> {
        self.setting_override
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .or_else(|| self.au3check.clone())
    }

    /// Parse a settings payload (from `initializationOptions` at startup
    /// or from `workspace/didChangeConfiguration` later) and update the
    /// override path accordingly. Tolerant of missing/wrong-shape input.
    fn apply_settings(&self, value: serde_json::Value, source: &'static str) {
        match serde_json::from_value::<InitializationOptions>(value) {
            Ok(opts) => match opts.au3check_path {
                Some(raw) => {
                    let candidate = PathBuf::from(&raw);
                    if candidate.is_file() {
                        tracing::info!(
                            path = %candidate.display(),
                            source,
                            "au3checkPath override accepted"
                        );
                        *self.setting_override.write().expect("lock not poisoned") =
                            Some(candidate);
                    } else {
                        tracing::warn!(
                            path = %raw,
                            source,
                            "au3checkPath setting points to a non-existent file — ignoring"
                        );
                        *self.setting_override.write().expect("lock not poisoned") = None;
                    }
                }
                None => {
                    // Setting was sent but au3checkPath is absent — clear
                    // any previous override so we fall back to discovery.
                    *self.setting_override.write().expect("lock not poisoned") = None;
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, source, "failed to parse settings");
            }
        }
    }

    /// Run Au3Check against the URI's file path and publish diagnostics
    /// back to the client. No-op if Au3Check isn't available or the URI
    /// can't be resolved to a local path.
    async fn check_and_publish(&self, uri: Url) {
        let Some(au3check) = self.resolved_au3check() else {
            return;
        };
        let Ok(path) = uri.to_file_path() else {
            tracing::debug!(uri = %uri, "ignoring non-file URI");
            return;
        };
        match au3check::run_au3check(&au3check, &path).await {
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
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Unconditional entry log — confirms the handler fires and shows
        // what (if anything) the client sent in initializationOptions. If
        // this never appears, dispatch itself is broken, not our parse.
        tracing::info!(
            initialization_options = ?params.initialization_options,
            "initialize received"
        );

        if let Some(value) = params.initialization_options {
            self.apply_settings(value, "initializationOptions");
        }

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
        match self.resolved_au3check() {
            Some(path) => tracing::info!(
                au3check = %path.display(),
                "autoit-lsp initialized"
            ),
            None => tracing::warn!(
                "Au3Check.exe not found in registry, default path, or \
                 initializationOptions.au3checkPath — diagnostics disabled"
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

    /// LSP clients (notably Zed) often deliver settings through this
    /// notification rather than through `initializationOptions`. We parse
    /// the same `InitializationOptions` shape from either path so users
    /// only need to write one settings.json entry.
    ///
    /// Zed nests its settings under the language-server id, so the
    /// payload arrives as `{ "autoit-lsp": { "au3checkPath": "..." } }`.
    /// We peel that layer if present, otherwise treat the value as the
    /// settings object directly (some clients do).
    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        tracing::info!(settings = ?params.settings, "didChangeConfiguration received");
        let value = match params.settings.get("autoit-lsp") {
            Some(nested) => nested.clone(),
            None => params.settings,
        };
        self.apply_settings(value, "didChangeConfiguration");
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

    let (service, socket) = LspService::new(|client| Backend {
        client,
        au3check,
        setting_override: RwLock::new(None),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_au3check_path_setting() {
        let json = serde_json::json!({ "au3checkPath": "D:/Tools/AutoIt3/Au3Check.exe" });
        let opts: InitializationOptions = serde_json::from_value(json).unwrap();
        assert_eq!(
            opts.au3check_path.as_deref(),
            Some("D:/Tools/AutoIt3/Au3Check.exe")
        );
    }

    #[test]
    fn missing_setting_yields_none() {
        let opts: InitializationOptions =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(opts.au3check_path.is_none());
    }

    #[test]
    fn unknown_fields_dont_break_parse() {
        // Tolerant of extra keys — future versions may add settings and
        // older servers should still parse the payload without erroring.
        let json = serde_json::json!({
            "au3checkPath": "C:/x.exe",
            "futureUnknownSetting": 42
        });
        let opts: InitializationOptions = serde_json::from_value(json).unwrap();
        assert_eq!(opts.au3check_path.as_deref(), Some("C:/x.exe"));
    }
}
