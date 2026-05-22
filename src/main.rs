//! autoit-lsp — Language Server for AutoIt v3
//!
//! v0.2 wrapped `Au3Check.exe` and added edit-time diagnostics
//! (temp-file staging + 400ms debounce). v0.2.1 layers on
//! configurability and polish: configurable debounce, Au3Check
//! warning-level / debug flag settings, immediate lint on the first
//! edit after open (no debounce wait), multi-character squiggle
//! ranges via a token heuristic, and content-hash caching to skip
//! redundant checks. Speaks LSP over stdio.

mod au3check;
mod builtins;
mod complete;
mod hover;
mod index;
mod macros;
mod staging;
mod symbols;
mod tree;

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::RwLock as AsyncRwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    CompletionOptions, CompletionParams, CompletionResponse, *,
};
use tower_lsp::{Client, LanguageServer, LspService, Server};

use au3check::Au3CheckConfig;

/// Default debounce when `debounceMs` setting isn't provided.
const DEFAULT_DEBOUNCE_MS: u64 = 400;

/// Bounds on `debounceMs` setting. Lower than 50 produces near-no-op
/// debouncing (each keystroke spawns a check); higher than 5000 makes
/// the LSP feel broken.
const MIN_DEBOUNCE_MS: u64 = 50;
const MAX_DEBOUNCE_MS: u64 = 5000;

/// LSP `initializationOptions` / `workspace/didChangeConfiguration`
/// payload. Adding fields is additive — serde's default
/// deserialization ignores unknown keys, so older clients that don't
/// send a new field just get the default.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct InitializationOptions {
    /// Absolute path to `Au3Check.exe`. Used by portable / non-installer
    /// AutoIt setups that aren't in the registry and aren't at the
    /// default install path. If unset (or pointing to a non-existent
    /// file), the server falls back to its registry-and-default
    /// discovery chain.
    au3check_path: Option<String>,
    /// Milliseconds to wait after the last keystroke before re-linting.
    /// Defaults to 400. Clamped to [50, 5000].
    debounce_ms: Option<u64>,
}

/// Resolved server-wide settings. Populated from
/// `InitializationOptions` at `initialize` and updated on
/// `workspace/didChangeConfiguration`.
#[derive(Debug, Default, Clone)]
struct Settings {
    /// Validated path to Au3Check.exe. `None` means use discovery.
    au3check_path: Option<PathBuf>,
    /// Validated debounce in ms. `None` means use `DEFAULT_DEBOUNCE_MS`.
    debounce_ms: Option<u64>,
}

/// In-memory state for one open document.
///
/// `Tree` (from tree-sitter) doesn't impl `Debug`, so we can't derive
/// `Debug` here — manual impl below shows just the metadata bits, no
/// tree dump.
#[derive(Default)]
struct DocState {
    /// Current buffer text. Updated by `did_change` (FULL sync).
    text: String,
    /// Monotonic counter, bumped on every edit and on `did_save`.
    /// Debounced check tasks compare this against the version they
    /// were spawned with and bail out if a newer edit superseded
    /// them.
    version: u64,
    /// A3 — true until the first `did_change` post-open / post-save.
    /// When true, the change handler skips the debounce timer and
    /// runs the check immediately for snappier first-keystroke
    /// feedback.
    first_edit_pending: bool,
    /// A5 — hash of the buffer text most recently passed to
    /// Au3Check. If a subsequent check's hash matches, we skip the
    /// subprocess spawn entirely (no-op edit, save-on-idle, etc.).
    last_checked_hash: Option<u64>,
    /// Sprint 1 — tree-sitter parse tree for this document. Refreshed
    /// on every `did_open` / `did_change` / `did_save` via a full reparse.
    /// `None` if no parse has run yet, or (extremely rare) if `parser.parse`
    /// returned None. Higher-level features (document symbols, hover,
    /// later go-to-def / find-refs / completion) read this lazily on
    /// demand — they don't trigger reparses themselves.
    tree: Option<tree_sitter::Tree>,
    /// Sprint 2 — per-document symbol index. Rebuilt alongside the parse
    /// tree on every `did_open` / `did_change`. Feeds go-to-definition
    /// and find-references. `None` only when `tree` is also None.
    index: Option<index::FileIndex>,
}

impl std::fmt::Debug for DocState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocState")
            .field("text_len", &self.text.len())
            .field("version", &self.version)
            .field("first_edit_pending", &self.first_edit_pending)
            .field("last_checked_hash", &self.last_checked_hash)
            .field("has_tree", &self.tree.is_some())
            .field("has_index", &self.index.is_some())
            .finish()
    }
}

/// All Backend state lives behind an `Arc` so we can hand cheap clones
/// to spawned debounce tasks. `tower-lsp` gives handlers `&self`, but
/// the debounce timer fires from a `tokio::spawn` that outlives the
/// handler call — that future has to own its references.
#[derive(Debug)]
struct Inner {
    client: Client,
    /// Path to Au3Check.exe resolved at startup via the registry /
    /// default chain. `None` means none of those probes hit a real
    /// file.
    au3check: Option<PathBuf>,
    /// All server-wide settings. `std::sync::RwLock` is fine because
    /// we never hold the lock across an await.
    settings: RwLock<Settings>,
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
                settings: RwLock::new(Settings::default()),
                docs: AsyncRwLock::new(HashMap::new()),
            }),
        }
    }

    /// Effective Au3Check path: setting override if present, otherwise
    /// the path discovered at startup.
    fn resolved_au3check(&self) -> Option<PathBuf> {
        let settings = self.inner.settings.read().expect("lock not poisoned");
        settings
            .au3check_path
            .clone()
            .or_else(|| self.inner.au3check.clone())
    }

    /// Effective debounce in milliseconds.
    fn resolved_debounce_ms(&self) -> u64 {
        self.inner
            .settings
            .read()
            .expect("lock not poisoned")
            .debounce_ms
            .unwrap_or(DEFAULT_DEBOUNCE_MS)
    }

    /// Parse a settings payload (from `initializationOptions` at startup
    /// or from `workspace/didChangeConfiguration` later) and update the
    /// server-wide settings. Tolerant of missing/wrong-shape input:
    /// parse errors are logged and leave settings untouched.
    fn apply_settings(&self, value: serde_json::Value, source: &'static str) {
        let opts: InitializationOptions = match serde_json::from_value(value) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(error = %e, source, "failed to parse settings — keeping current values");
                return;
            }
        };

        let mut settings = self.inner.settings.write().expect("lock not poisoned");

        // Au3Check path with file-exists validation. Stale settings
        // (non-existent file) are cleared rather than retained, so
        // installing AutoIt later "just works" without reconfiguring.
        settings.au3check_path = match opts.au3check_path {
            Some(raw) => {
                let candidate = PathBuf::from(&raw);
                if candidate.is_file() {
                    tracing::info!(
                        path = %candidate.display(),
                        source,
                        "au3checkPath override accepted"
                    );
                    Some(candidate)
                } else {
                    tracing::warn!(
                        path = %raw,
                        source,
                        "au3checkPath setting points to a non-existent file — ignoring"
                    );
                    None
                }
            }
            None => None,
        };

        // Debounce: clamp to a sane range. Values outside [50, 5000]
        // are accepted but warned, and the clamped value is what
        // takes effect.
        settings.debounce_ms = opts.debounce_ms.map(|ms| {
            let clamped = ms.clamp(MIN_DEBOUNCE_MS, MAX_DEBOUNCE_MS);
            if clamped != ms {
                tracing::warn!(
                    requested = ms,
                    clamped,
                    source,
                    "debounceMs outside [{MIN_DEBOUNCE_MS}, {MAX_DEBOUNCE_MS}] — clamped"
                );
            } else {
                tracing::info!(debounce_ms = ms, source, "debounceMs accepted");
            }
            clamped
        });

    }

    /// Stage the given buffer to a temp file, run Au3Check, and publish
    /// diagnostics under the original URI. No-op if Au3Check isn't
    /// available, the URI doesn't resolve to a local path, or the
    /// content hash matches a previous successful check.
    async fn check_and_publish(&self, uri: Url, text: String) {
        let Some(au3check) = self.resolved_au3check() else {
            return;
        };
        let Some(original_dir) = staging::original_dir(&uri) else {
            tracing::debug!(uri = %uri, "ignoring non-file URI");
            return;
        };

        // A5 — hash cache check before any expensive work.
        let new_hash = hash_text(&text);
        {
            let docs = self.inner.docs.read().await;
            if let Some(state) = docs.get(&uri) {
                if state.last_checked_hash == Some(new_hash) {
                    tracing::debug!(
                        uri = %uri,
                        "skipping check — content hash matches last lint"
                    );
                    return;
                }
            }
        }

        let temp_path = match staging::stage_buffer(&uri, &text).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(uri = %uri, error = %e, "failed to stage buffer");
                return;
            }
        };

        // Build the Au3Check config. Vec<&Path> instead of an array
        // literal so lifetimes are obviously sound across the await.
        let include_dirs: Vec<&Path> = vec![original_dir.as_path()];
        let config = Au3CheckConfig {
            target: &temp_path,
            include_dirs: &include_dirs,
            cwd: Some(&original_dir),
        };

        match au3check::run_au3check(&au3check, config).await {
            Ok(output) => {
                // A4 — pass source text so parse_diagnostics can size
                // each diagnostic range to its offending token. We
                // also use this to filter out includes (target =
                // temp_path; #include'd files have other paths).
                let diags = au3check::parse_diagnostics(&output, &temp_path, &text);
                tracing::debug!(uri = %uri, count = diags.len(), "publishing diagnostics");
                self.inner
                    .client
                    .publish_diagnostics(uri.clone(), diags, None)
                    .await;

                // A5 — only update the hash after a successful check.
                // If Au3Check errored, leave the hash untouched so we
                // retry next time the user edits.
                let mut docs = self.inner.docs.write().await;
                if let Some(state) = docs.get_mut(&uri) {
                    state.last_checked_hash = Some(new_hash);
                }
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

/// Extract the partial token that the user is currently typing, ending at
/// `position`. Walks backwards from the cursor column collecting characters
/// that are valid in an AutoIt identifier or sigil:
///   - `$` prefix for variables
///   - `@` prefix for macros
///   - Letters, digits, underscore for function/identifier names
///
/// Returns an empty string when the cursor is between tokens (whitespace,
/// operator, punctuation).
fn partial_token_at(source: &str, position: tower_lsp::lsp_types::Position) -> String {
    let line_idx = position.line as usize;
    let col_utf16 = position.character as usize;

    // Find the line as a &str.
    let line = source.split('\n').nth(line_idx).unwrap_or("");

    // Convert UTF-16 column to a byte offset within the line.
    let mut byte_col = 0usize;
    let mut utf16_count = 0usize;
    for ch in line.chars() {
        if utf16_count >= col_utf16 {
            break;
        }
        utf16_count += ch.len_utf16();
        byte_col += ch.len_utf8();
    }

    // Walk back from byte_col collecting identifier characters.
    let before = &line[..byte_col];
    let token: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$' || *c == '@')
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    // Only keep the token if it starts with a recognised sigil or letter.
    // This prevents returning a bare `_` or digit sequence as a prefix.
    if token.starts_with('$')
        || token.starts_with('@')
        || token.starts_with(|c: char| c.is_alphabetic() || c == '_')
    {
        token
    } else {
        String::new()
    }
}

/// DefaultHasher of the buffer text. We don't need cryptographic
/// strength — only "did this exact text get linted already." Collisions
/// (~1 in 2^64) would skip a lint where they shouldn't, which is
/// recoverable via the next edit.
fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
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
                // Sprint 1 Day 2 — Zed's outline panel queries documentSymbol.
                // We respond with a hierarchical (nested) list rather than the
                // legacy flat SymbolInformation form.
                document_symbol_provider: Some(OneOf::Left(true)),
                // Sprint 1 Day 3 — hover surfaces docs for built-in + UDF
                // library functions sourced from autoitscript.com via the
                // scrape-builtins.ps1 script.
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                // Sprint 2 — go-to-definition and find-references backed
                // by the per-document FileIndex built in index.rs.
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                // Sprint 3 — completion. Trigger characters `$` and `@`
                // fire the popup immediately when those sigils are typed;
                // regular alpha input triggers via Zed's word-completion path.
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["$".into(), "@".into()]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
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
        // Parse and index before taking the docs write lock so we don't
        // hold it across the (cheap, microsecond-scale) work.
        let tree = tree::parse(&text);
        let file_index = tree.as_ref().map(|t| index::build_index(t, &text));
        {
            let mut docs = self.inner.docs.write().await;
            docs.insert(
                uri.clone(),
                DocState {
                    text: text.clone(),
                    version: 0,
                    first_edit_pending: true,
                    last_checked_hash: None,
                    tree,
                    index: file_index,
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
        let Some(new_text) = params
            .content_changes
            .into_iter()
            .rev()
            .find_map(|c| if c.range.is_none() { Some(c.text) } else { None })
        else {
            tracing::debug!(uri = %uri, "ignoring didChange with no full-text replacement");
            return;
        };

        // Parse and index before taking the docs write lock (both operate
        // on their own copy of the text, no lock needed).
        let new_tree = tree::parse(&new_text);
        let new_index = new_tree.as_ref().map(|t| index::build_index(t, &new_text));

        // A3 — capture the first-edit flag and clear it in one
        // critical section, so we know whether to skip the debounce
        // for this notification.
        let (text_for_check, version, skip_debounce) = {
            let mut docs = self.inner.docs.write().await;
            let state = docs.entry(uri.clone()).or_default();
            state.text = new_text;
            state.version = state.version.wrapping_add(1);
            state.tree = new_tree;
            state.index = new_index;
            let skip = state.first_edit_pending;
            state.first_edit_pending = false;
            (state.text.clone(), state.version, skip)
        };

        if skip_debounce {
            // Lint immediately — gives the user instant feedback on
            // the very first keystroke after opening/saving, instead
            // of making them wait `debounceMs` for the most attentive
            // moment of editing.
            tracing::debug!(uri = %uri, "first-edit-after-open: skipping debounce");
            self.check_and_publish(uri, text_for_check).await;
            return;
        }

        let debounce_ms = self.resolved_debounce_ms();
        let backend = self.clone();
        let uri_for_task = uri.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce_ms)).await;
            backend.check_after_debounce(uri_for_task, version).await;
        });
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        // Pull the latest in-memory buffer rather than re-reading
        // from disk. By LSP protocol, didChange always precedes
        // didSave, so the docs map is current.
        let text = {
            let mut docs = self.inner.docs.write().await;
            if let Some(state) = docs.get_mut(&uri) {
                // Bump version so any in-flight debounced check sees
                // it as superseded and bails out — we're about to
                // publish a fresher result from this immediate
                // save-triggered check.
                state.version = state.version.wrapping_add(1);
                // A3 — re-arm first-edit-pending so a save→resume-
                // typing cycle gets the same instant-feedback
                // behaviour as a fresh open.
                state.first_edit_pending = true;
                Some(state.text.clone())
            } else {
                None
            }
        };
        if let Some(text) = text {
            self.check_and_publish(uri, text).await;
        }
    }

    /// Sprint 1 Day 2 — outline-panel response. Looks up the cached parse
    /// tree for the document (populated by did_open/did_change), walks it
    /// to produce a hierarchical DocumentSymbol list, and returns. If we
    /// haven't seen the document yet (race with did_open?) we return an
    /// empty list rather than erroring — the client retries shortly.
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let docs = self.inner.docs.read().await;
        let Some(state) = docs.get(&uri) else {
            tracing::debug!(uri = %uri, "documentSymbol on unknown doc — returning empty");
            return Ok(Some(DocumentSymbolResponse::Nested(vec![])));
        };
        let Some(tree) = state.tree.as_ref() else {
            return Ok(Some(DocumentSymbolResponse::Nested(vec![])));
        };
        let syms = symbols::document_symbols(tree, &state.text);
        tracing::debug!(uri = %uri, count = syms.len(), "documentSymbol responding");
        Ok(Some(DocumentSymbolResponse::Nested(syms)))
    }

    /// Sprint 1 Day 3 — hover for built-in / UDF library functions. Uses
    /// the cached parse tree to find the identifier under the cursor, then
    /// looks it up in the static `builtins` catalog.
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let docs = self.inner.docs.read().await;
        let Some(state) = docs.get(&uri) else {
            return Ok(None);
        };
        let Some(tree) = state.tree.as_ref() else {
            return Ok(None);
        };
        Ok(hover::hover_for(tree, &state.text, position))
    }

    /// Sprint 2 — go-to-definition. Resolves the symbol under the cursor
    /// using the per-document FileIndex with scope-aware lookup:
    /// parameters and local variables shadow global declarations of the
    /// same name when the cursor is inside their function.
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let docs = self.inner.docs.read().await;
        let result = (|| -> Option<GotoDefinitionResponse> {
            let state = docs.get(&uri)?;
            let tree = state.tree.as_ref()?;
            let file_index = state.index.as_ref()?;

            let node = tree::node_at_position(tree, &state.text, position)?;

            // Walk up from the deepest leaf to find a variable or identifier.
            let sym_node = if matches!(node.kind(), "variable" | "identifier") {
                node
            } else {
                let parent = node.parent()?;
                if matches!(parent.kind(), "variable" | "identifier") {
                    parent
                } else {
                    return None;
                }
            };

            let name = sym_node.utf8_text(state.text.as_bytes()).ok()?;
            let scope = index::cursor_scope(sym_node, &state.text);

            let def = file_index.resolve_def(name, scope.as_deref())?;

            Some(GotoDefinitionResponse::Scalar(Location {
                uri: uri.clone(),
                range: def.name_range,
            }))
        })();

        Ok(result)
    }

    /// Sprint 2 — find-references. Returns all usage sites of the symbol
    /// under the cursor, scope-filtered to match the definition's visibility:
    /// globals/functions return file-wide refs; locals/params return only
    /// refs inside their declaring function.
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_decl = params.context.include_declaration;

        let docs = self.inner.docs.read().await;
        let result = (|| -> Option<Vec<Location>> {
            let state = docs.get(&uri)?;
            let tree = state.tree.as_ref()?;
            let file_index = state.index.as_ref()?;

            let node = tree::node_at_position(tree, &state.text, position)?;

            let sym_node = if matches!(node.kind(), "variable" | "identifier") {
                node
            } else {
                let parent = node.parent()?;
                if matches!(parent.kind(), "variable" | "identifier") {
                    parent
                } else {
                    return None;
                }
            };

            let name = sym_node.utf8_text(state.text.as_bytes()).ok()?;
            let cursor_scope = index::cursor_scope(sym_node, &state.text);

            // Resolve the definition first so we know its scope, which
            // determines the ref-filtering strategy.
            let def = file_index.resolve_def(name, cursor_scope.as_deref())?;
            let def_scope = def.scope_func.as_deref();

            let refs = file_index.find_refs(name, def_scope);

            let mut locations: Vec<Location> = refs
                .iter()
                .map(|r| Location {
                    uri: uri.clone(),
                    range: r.usage_range,
                })
                .collect();

            if include_decl {
                locations.insert(
                    0,
                    Location {
                        uri: uri.clone(),
                        range: def.name_range,
                    },
                );
            }

            Some(locations)
        })();

        Ok(result)
    }

    /// Sprint 3 — completion. Determines context from the partial token at
    /// the cursor:
    ///   `$…`  → scope-filtered variables, constants, parameters.
    ///   `@…`  → AutoIt built-in macros.
    ///   letter → user-defined functions + 3,542 AutoIt built-in functions.
    ///
    /// Returns an empty list when the cursor is inside a string or comment —
    /// detected by checking the tree-sitter node kind at the cursor position.
    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let docs = self.inner.docs.read().await;
        let result = (|| -> Option<CompletionResponse> {
            let state = docs.get(&uri)?;
            let tree = state.tree.as_ref()?;
            let file_index = state.index.as_ref()?;

            // Determine the partial token at the cursor. We walk back from
            // the cursor column to find the start of the current word.
            let prefix = partial_token_at(&state.text, position);

            // Check if the cursor is inside a string or comment via the
            // parse tree — suppress completions in those contexts.
            let in_noise = tree::node_at_position(tree, &state.text, position)
                .map(|n| matches!(n.kind(), "string" | "line_comment" | "block_comment"))
                .unwrap_or(false);

            // Determine cursor scope for variable filtering.
            let cursor_scope = tree::node_at_position(tree, &state.text, position)
                .and_then(|n| index::cursor_scope(n, &state.text));

            let items = complete::completions_at(
                &prefix,
                file_index,
                cursor_scope.as_deref(),
                in_noise,
            );

            Some(CompletionResponse::Array(items))
        })();

        Ok(result)
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
    /// payload arrives as `{ "autoit-lsp": { ... } }`. We peel that
    /// layer if present, otherwise treat the value as the settings
    /// object directly (some clients do).
    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        tracing::info!(settings = ?params.settings, "didChangeConfiguration received");
        let value = match params.settings.get("autoit-lsp") {
            Some(nested) => nested.clone(),
            None => params.settings,
        };
        self.apply_settings(value, "didChangeConfiguration");
    }
}

/// Write the path of `autoit-run.exe` to the Windows registry so that
/// Zed's `tasks.json` can discover it without requiring the user to add
/// anything to their PATH.
///
/// Looks for `autoit-run.exe` next to this executable (i.e. in the Zed
/// extension cache directory after the zip is extracted). If found, writes
/// its absolute path to `HKCU\SOFTWARE\zed-autoit\RunnerPath`.
///
/// Best-effort — all errors are silently ignored. If registration fails
/// (no `autoit-run.exe` sibling, permissions issue, non-UTF8 path), the
/// `tasks.json` PATH lookup still works for users who have `autoit-run`
/// on their PATH.
#[cfg(windows)]
fn register_autoit_run() {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let runner = match std::env::current_exe() {
        Ok(exe) => match exe.parent() {
            Some(dir) => dir.join("autoit-run.exe"),
            None => return,
        },
        Err(_) => return,
    };

    if !runner.is_file() {
        return;
    }

    let runner_str = match runner.to_str() {
        Some(s) => s.to_owned(),
        None => return,
    };

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok((key, _)) = hkcu.create_subkey(r"SOFTWARE\zed-autoit") {
        if key.set_value("RunnerPath", &runner_str).is_ok() {
            tracing::info!(path = %runner.display(), "registered autoit-run.exe in HKCU");
        }
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

    // Register autoit-run.exe in HKCU so tasks.json can discover it even
    // when it isn't on the user's PATH. No-op on non-Windows and silently
    // ignored on any failure (missing sibling binary, permission error, etc.).
    #[cfg(windows)]
    register_autoit_run();

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
    fn parses_all_settings() {
        let json = serde_json::json!({
            "au3checkPath": "C:/x.exe",
            "debounceMs": 250
        });
        let opts: InitializationOptions = serde_json::from_value(json).unwrap();
        assert_eq!(opts.au3check_path.as_deref(), Some("C:/x.exe"));
        assert_eq!(opts.debounce_ms, Some(250));
    }

    #[test]
    fn missing_settings_yield_defaults() {
        let opts: InitializationOptions =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(opts.au3check_path.is_none());
        assert!(opts.debounce_ms.is_none());
    }

    #[test]
    fn unknown_fields_dont_break_parse() {
        // Tolerant of extra keys — future versions may add settings and
        // older servers should still parse the payload without erroring.
        // Also catches removed v0.2.1-dev names (au3checkParams etc.) —
        // they're just unknown-and-ignored now.
        let json = serde_json::json!({
            "au3checkPath": "C:/x.exe",
            "futureUnknownSetting": 42,
            "au3checkParams": { "warningLevels": "1 2 3" }
        });
        let opts: InitializationOptions = serde_json::from_value(json).unwrap();
        assert_eq!(opts.au3check_path.as_deref(), Some("C:/x.exe"));
    }

    // The clamp / validation logic lives in `apply_settings`, which
    // needs a Backend (and thus a Client). Verifying the clamp at the
    // pure-arithmetic layer instead — same logic, no LSP plumbing.

    #[test]
    fn debounce_clamp_low() {
        assert_eq!(10_u64.clamp(MIN_DEBOUNCE_MS, MAX_DEBOUNCE_MS), 50);
    }

    #[test]
    fn debounce_clamp_high() {
        assert_eq!(99999_u64.clamp(MIN_DEBOUNCE_MS, MAX_DEBOUNCE_MS), 5000);
    }

    #[test]
    fn debounce_clamp_in_range() {
        assert_eq!(250_u64.clamp(MIN_DEBOUNCE_MS, MAX_DEBOUNCE_MS), 250);
    }

    #[test]
    fn hash_text_is_stable_and_different_for_different_input() {
        assert_eq!(hash_text("abc"), hash_text("abc"));
        assert_ne!(hash_text("abc"), hash_text("abcd"));
        assert_ne!(hash_text("abc"), hash_text(""));
    }
}
