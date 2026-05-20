//! autoit-lsp — Language Server for AutoIt v3
//!
//! v0.2 wraps `Au3Check.exe` (AutoIt's official linter) and surfaces
//! its output as LSP diagnostics. Diagnostics refresh on open, on
//! save, and ~400ms after the user stops typing (temp-file staging
//! lets us lint the in-memory buffer without writing the user's file
//! to disk). Speaks LSP over stdio.

mod au3check;
mod staging;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::RwLock as AsyncRwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// How long the server waits after the last `didChange` before
/// running Au3Check. Each new keystroke supersedes the in-flight
/// timer (via the per-document version counter), so mid-word typing
/// produces zero check runs.
const DEBOUNCE_MS: u64 = 400;

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

/// In-memory state for one open document. We need the latest text so
/// we can stage it to a temp file for Au3Check, and we need a version
/// counter so debounced check tasks can detect that a newer edit has
/// superseded them.
#[derive(Debug, Default)]
struct DocState {
    text: String,
    version: u64,
}

/// All Backend state lives behind an `Arc` so we can hand cheap clones
/// to spawned debounce tasks. `tower-lsp` gives handlers `&self`, but
/// the debounce timer fires from a `tokio::spawn` that outlives the
/// handler call — that future has to own its references.
#[derive(Debug)]
struct Inner {
    client: Client,
    /// Path to Au3Check.exe resolved at startup via the registry/default
    /// chain. `None` means none of those probes hit a real file.
    au3check: Option<PathBuf>,
    /// Override from the `au3checkPath` setting, populated on
    /// `initialize` and on `didChangeConfiguration`. Takes priority
    /// over `au3check` when set. `std::sync::RwLock` is fine here
    /// because we never hold the lock across an await.
    setting_override: RwLock<Option<PathBuf>>,
    /// Open documents, keyed by URI. Tokio RwLock because we *do*
    /// hold reads across await (publishing diagnostics while a check
    /// is in flight).
    docs: AsyncRwLock<HashMap<Url, DocState>>,
}

#[derive(Debug, Clone)]
struct Backend {
    inner: Arc<Inner>,
}

impl Backend {
    fn new(client: Client, au3check: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                au3check,
                setting_override: RwLock::new(None),
                docs: AsyncRwLock::new(HashMap::new()),
            }),
        }
    }

    /// Effective Au3Check path: setting override if present, otherwise
    /// the path discovered at startup.
    fn resolved_au3check(&self) -> Option<PathBuf> {
        self.inner
            .setting_override
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .or_else(|| self.inner.au3check.clone())
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
                        *self.inner.setting_override.write().expect("lock not poisoned") =
                            Some(candidate);
                    } else {
                        tracing::warn!(
                            path = %raw,
                            source,
                            "au3checkPath setting points to a non-existent file — ignoring"
                        );
                        *self.inner.setting_override.write().expect("lock not poisoned") = None;
                    }
                }
                None => {
                    // Setting was sent but au3checkPath is absent — clear
                    // any previous override so we fall back to discovery.
                    *self.inner.setting_override.write().expect("lock not poisoned") = None;
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, source, "failed to parse settings");
            }
        }
    }

    /// Stage the given buffer to a temp file, run Au3Check, and publish
    /// diagnostics under the original URI. No-op if Au3Check isn't
    /// available or the URI doesn't resolve to a local path.
    async fn check_and_publish(&self, uri: Url, text: String) {
        let Some(au3check) = self.resolved_au3check() else {
            return;
        };
        let Some(original_dir) = staging::original_dir(&uri) else {
            tracing::debug!(uri = %uri, "ignoring non-file URI");
            return;
        };

        let temp_path = match staging::stage_buffer(&uri, &text).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(uri = %uri, error = %e, "failed to stage buffer");
                return;
            }
        };

        let include_dirs = [original_dir.as_path()];
        match au3check::run_au3check(
            &au3check,
            &temp_path,
            &include_dirs,
            Some(&original_dir),
        )
        .await
        {
            Ok(output) => {
                // parse_diagnostics filters to the file we asked about
                // (temp_path), so #include'd-file diagnostics get
                // dropped as before. We publish under the original URI
                // so Zed associates the squigglies with the user's
                // buffer, not the temp file.
                let diags = au3check::parse_diagnostics(&output, &temp_path);
                tracing::debug!(uri = %uri, count = diags.len(), "publishing diagnostics");
                self.inner.client.publish_diagnostics(uri, diags, None).await;
            }
            Err(e) => {
                tracing::warn!(uri = %uri, error = %e, "Au3Check invocation failed");
            }
        }
    }

    /// After the debounce delay, run a check only if no newer edit
    /// has bumped the version. Late-fired timers from superseded
    /// edits become no-ops here.
    async fn check_after_debounce(&self, uri: Url, expected_version: u64) {
        let (text, current_version) = {
            let docs = self.inner.docs.read().await;
            match docs.get(&uri) {
                Some(state) => (state.text.clone(), state.version),
                None => return,
            }
        };
        if current_version != expected_version {
            return;
        }
        self.check_and_publish(uri, text).await;
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
                // FULL sync: each didChange carries the entire buffer
                // text. Au3Check needs to be invoked against full source
                // anyway (we stage to a temp file every check), so the
                // efficiency gain from INCREMENTAL doesn't apply. When
                // Sprint 1 adds tree-sitter incremental reparse, this
                // will likely switch to INCREMENTAL.
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
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
        // Best-effort sweep of the staging root. If the process is
        // killed before this runs, %TEMP% gets cleaned by Windows
        // eventually, so a leak isn't catastrophic.
        staging::cleanup_all().await;
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        {
            let mut docs = self.inner.docs.write().await;
            docs.insert(
                uri.clone(),
                DocState {
                    text: text.clone(),
                    version: 0,
                },
            );
        }
        self.check_and_publish(uri, text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        // FULL sync means a single change event carries the entire
        // new text. If the client sent INCREMENTAL changes anyway
        // (shouldn't happen — we advertised FULL — but be defensive),
        // we take the last full-text replacement and ignore the rest.
        let Some(text) = params
            .content_changes
            .into_iter()
            .rev()
            .find_map(|c| if c.range.is_none() { Some(c.text) } else { None })
        else {
            tracing::debug!(uri = %uri, "ignoring didChange with no full-text replacement");
            return;
        };

        let version = {
            let mut docs = self.inner.docs.write().await;
            let state = docs.entry(uri.clone()).or_default();
            state.text = text;
            state.version = state.version.wrapping_add(1);
            state.version
        };

        // Spawn the debounce timer. Clones an Arc handle into the
        // task so it can outlive this handler invocation.
        let backend = self.clone();
        let uri_for_task = uri.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;
            backend.check_after_debounce(uri_for_task, version).await;
        });
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        // Pull the latest in-memory buffer rather than re-reading
        // from disk. By LSP protocol, didChange always precedes
        // didSave, so the docs map is current.
        let text = {
            let docs = self.inner.docs.read().await;
            docs.get(&uri).map(|s| s.text.clone())
        };
        if let Some(text) = text {
            // Bump version so any in-flight debounced check sees it
            // as superseded and bails out — we're about to publish a
            // fresher result from this immediate save-triggered check.
            {
                let mut docs = self.inner.docs.write().await;
                if let Some(state) = docs.get_mut(&uri) {
                    state.version = state.version.wrapping_add(1);
                }
            }
            self.check_and_publish(uri, text).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        {
            let mut docs = self.inner.docs.write().await;
            docs.remove(&uri);
        }
        staging::cleanup_doc(&uri).await;
        // Clear any lingering diagnostics from the client's UI.
        // Without this, problems-panel entries for the closed file
        // would persist until the next session.
        self.inner
            .client
            .publish_diagnostics(uri, vec![], None)
            .await;
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

    let (service, socket) = LspService::new(|client| Backend::new(client, au3check));
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
