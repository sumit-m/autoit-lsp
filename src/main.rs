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
mod codeaction;
mod color;
mod complete;
mod doccomment;
mod folding;
mod format_diff;
mod highlight;
mod hints;
mod hover;
mod includes;
mod index;
mod macros;
mod project_index;
mod signature;
mod staging;
mod symbols;
mod tree;

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::RwLock as AsyncRwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability, CompletionOptions,
    CompletionParams, CompletionResponse, SignatureHelpOptions, SignatureHelpParams, *,
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

/// Debounce window for `workspace/didChangeWatchedFiles` bursts (e.g. a git
/// checkout). The project index updates immediately on each notification; only
/// the expensive open-doc rebuild + re-lint waits this long for things to settle.
const WATCH_DEBOUNCE_MS: u64 = 300;

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
    /// Extra raw arguments for Au3Check, typed as a single command-line
    /// string (e.g. `"-w 1 -d"`). Tokenized (quote-aware) and appended
    /// verbatim to the argv. Not validated — the user reads `Au3Check.exe
    /// -h` and is responsible for the contents.
    au3check_extra_args: Option<String>,
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
    /// Tokenized extra Au3Check args appended verbatim to the argv.
    /// Empty = none. Not validated.
    au3check_extra_args: Vec<String>,
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
    /// Sprint 4 — cross-file workspace index built by resolving the full
    /// `#include` tree on `did_open` and `did_save`. Feeds cross-file
    /// go-to-definition, find-references, and completion from included files.
    /// `None` until the first open/save completes.
    workspace_index: Option<includes::WorkspaceIndex>,
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
            .field("has_workspace_index", &self.workspace_index.is_some())
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
    /// Path to AutoIt's standard `Include\` directory for
    /// `#include <Library.au3>` resolution. `None` on non-Windows or
    /// when AutoIt isn't installed.
    autoit_include_dir: Option<PathBuf>,
    /// `AutoIt3.exe` — interpreter used to run AutoIt3Wrapper /Tidy.
    autoit3_exe: Option<PathBuf>,
    /// `AutoIt3Wrapper.au3` — SciTE4AutoIt3 script providing /Tidy.
    autoit3wrapper: Option<PathBuf>,
    /// `Tidy.exe` — the actual formatter binary AutoIt3Wrapper calls.
    tidy_exe: Option<PathBuf>,
    /// All server-wide settings. `std::sync::RwLock` is fine because
    /// we never hold the lock across an await.
    settings: RwLock<Settings>,
    /// Open documents, keyed by URI. Tokio RwLock because we *do*
    /// hold reads across await (publishing diagnostics while a check
    /// is in flight).
    docs: AsyncRwLock<HashMap<Url, DocState>>,
    /// v0.6.0 — workspace root folder from `initialize`'s `rootUri` /
    /// `workspaceFolders`. `None` when Zed opened a bare single file (no
    /// folder), which disables the project-wide index. `std::sync::RwLock`
    /// because it's set once and read without holding across an await.
    workspace_root: RwLock<Option<PathBuf>>,
    /// v0.6.0 — project-wide symbol index spanning every `.au3` under
    /// `workspace_root`. Built in the background on `initialized` and kept
    /// fresh by `workspace/didChangeWatchedFiles`. Empty when there's no
    /// workspace root. Backs call hierarchy and cross-file find-references
    /// (the upward-caller direction the `#include` graph can't reach).
    project_index: AsyncRwLock<project_index::ProjectIndex>,
    /// v0.6.0 — whether the client advertised dynamic registration for
    /// `workspace/didChangeWatchedFiles`. File watching in LSP is *only*
    /// available via dynamic registration, so we register watchers in
    /// `initialized` only when this is true. Set from the client capabilities
    /// in `initialize`.
    supports_dynamic_watchers: RwLock<bool>,
    /// v0.6.0 — monotonic counter for debouncing watched-file bursts (e.g. a
    /// git checkout touching many files). Each notification bumps it; the
    /// expensive open-doc refresh runs only if no newer notification arrived
    /// during the debounce window. The project index itself is updated
    /// eagerly (cheap) — only the re-lint/refresh is debounced.
    watch_generation: AtomicU64,
}

#[derive(Debug, Clone)]
struct Backend {
    inner: Arc<Inner>,
}

impl Backend {
    fn new(
        client: Client,
        au3check: Option<PathBuf>,
        autoit_include_dir: Option<PathBuf>,
        autoit3_exe: Option<PathBuf>,
        autoit3wrapper: Option<PathBuf>,
        tidy_exe: Option<PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                au3check,
                autoit_include_dir,
                autoit3_exe,
                autoit3wrapper,
                tidy_exe,
                settings: RwLock::new(Settings::default()),
                docs: AsyncRwLock::new(HashMap::new()),
                workspace_root: RwLock::new(None),
                project_index: AsyncRwLock::new(project_index::ProjectIndex::default()),
                supports_dynamic_watchers: RwLock::new(false),
                watch_generation: AtomicU64::new(0),
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

    /// Effective extra Au3Check args (already tokenized). Empty = none.
    fn resolved_extra_args(&self) -> Vec<String> {
        self.inner
            .settings
            .read()
            .expect("lock not poisoned")
            .au3check_extra_args
            .clone()
    }

    /// v0.6.0 — (re)build the project-wide index in the background.
    ///
    /// Reads the workspace root, then spawns a blocking task to walk the tree
    /// and parse every `.au3` (CPU + filesystem bound — kept off the async
    /// runtime). The freshly-built index replaces the stored one when done.
    /// No-op when there's no workspace root (bare single-file session).
    async fn spawn_project_scan(&self) {
        let root = self
            .inner
            .workspace_root
            .read()
            .expect("lock not poisoned")
            .clone();
        let Some(root) = root else {
            return;
        };

        let backend = self.clone();
        tokio::spawn(async move {
            let scanned = tokio::task::spawn_blocking(move || {
                project_index::scan(&root, project_index::MAX_PROJECT_FILES)
            })
            .await;
            match scanned {
                Ok(index) => {
                    let count = index.file_count();
                    *backend.inner.project_index.write().await = index;
                    tracing::info!(files = count, "project index ready");
                }
                Err(e) => tracing::warn!(error = %e, "project index scan task panicked"),
            }
        });
    }

    /// v0.6.0 — register `**/*.au3` + `**/*.a3x` file watchers via dynamic
    /// capability registration, so the client notifies us of on-disk changes.
    ///
    /// The glob is workspace-relative; the client (Zed) backs it with native
    /// OS watchers. No-op without a workspace root or when the client doesn't
    /// support dynamic registration. We watch `.a3x` for completeness, but the
    /// handler only re-indexes `.au3` (compiled `.a3x` is binary).
    /// Angle-bracket `#include <…>` library files live in AutoIt's read-only
    /// install dir, outside the workspace glob — intentionally not watched.
    async fn register_file_watchers(&self) {
        let has_root = self
            .inner
            .workspace_root
            .read()
            .expect("lock not poisoned")
            .is_some();
        let supported = *self
            .inner
            .supports_dynamic_watchers
            .read()
            .expect("lock not poisoned");
        if !has_root || !supported {
            tracing::info!(has_root, supported, "skipping file-watcher registration");
            return;
        }

        let make_watcher = |glob: &str| FileSystemWatcher {
            glob_pattern: GlobPattern::String(glob.to_string()),
            kind: None, // None = Create | Change | Delete
        };
        let options = DidChangeWatchedFilesRegistrationOptions {
            watchers: vec![make_watcher("**/*.au3"), make_watcher("**/*.a3x")],
        };
        let register_options = match serde_json::to_value(options) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "could not serialize watcher options");
                return;
            }
        };
        let registration = Registration {
            id: "autoit-watched-files".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: Some(register_options),
        };

        match self
            .inner
            .client
            .register_capability(vec![registration])
            .await
        {
            Ok(()) => tracing::info!("registered **/*.au3 + **/*.a3x file watchers"),
            Err(e) => tracing::warn!(error = %e, "failed to register file watchers"),
        }
    }

    /// v0.6.0 — apply a batch of watched-file changes to the project index,
    /// then debounce-refresh open documents.
    ///
    /// Only `.au3` changes matter to the symbol index. Reads happen before the
    /// lock (async I/O) so the write lock is held only for the cheap apply.
    async fn handle_watched_changes(&self, changes: Vec<FileEvent>) {
        // Read sources for create/change up front (no lock held across I/O).
        enum Apply {
            Upsert(PathBuf, String),
            Remove(PathBuf),
        }
        let mut applies: Vec<Apply> = Vec::new();
        for change in changes {
            let Ok(path) = change.uri.to_file_path() else {
                continue;
            };
            if !project_index::is_au3(&path) {
                continue; // .a3x is binary; nothing to index
            }
            match change.typ {
                FileChangeType::CREATED | FileChangeType::CHANGED => {
                    match tokio::fs::read_to_string(&path).await {
                        Ok(src) => applies.push(Apply::Upsert(path, src)),
                        // Unreadable (e.g. deleted between event and read) —
                        // drop any stale entry so we don't keep ghost data.
                        Err(_) => applies.push(Apply::Remove(path)),
                    }
                }
                FileChangeType::DELETED => applies.push(Apply::Remove(path)),
                _ => {}
            }
        }

        if applies.is_empty() {
            return; // nothing relevant (e.g. only .a3x changes)
        }

        {
            let mut pi = self.inner.project_index.write().await;
            for apply in &applies {
                match apply {
                    Apply::Upsert(path, src) => pi.upsert(path, src),
                    Apply::Remove(path) => {
                        pi.remove(path);
                    }
                }
            }
        }

        // Debounce the expensive open-doc refresh: coalesce bursts (e.g. a git
        // checkout) so we rebuild include graphs + re-lint only once things
        // settle. The project index above is already up to date regardless.
        let generation = self.inner.watch_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let backend = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(WATCH_DEBOUNCE_MS)).await;
            if backend.inner.watch_generation.load(Ordering::SeqCst) == generation {
                backend.refresh_open_docs_after_watch().await;
            }
        });
    }

    /// v0.6.0 — rebuild every open document's `#include` workspace index from
    /// disk (the changed file may be in its include graph), re-lint, and ask
    /// the client to re-pull inlay hints (cross-file parameter names may have
    /// changed). Runs after the watched-file debounce settles.
    async fn refresh_open_docs_after_watch(&self) {
        // Snapshot open docs (uri + text + cloned tree) without holding the
        // lock across the async index rebuilds below.
        let snapshot: Vec<(Url, String, tree_sitter::Tree)> = {
            let docs = self.inner.docs.read().await;
            docs.iter()
                .filter_map(|(uri, state)| {
                    state
                        .tree
                        .as_ref()
                        .map(|t| (uri.clone(), state.text.clone(), t.clone()))
                })
                .collect()
        };
        if snapshot.is_empty() {
            return;
        }

        let include_dir = self.inner.autoit_include_dir.clone();
        for (uri, text, tree) in snapshot {
            let Ok(path) = uri.to_file_path() else {
                continue;
            };
            let ws =
                includes::build_workspace_index(&path, &tree, &text, include_dir.as_deref()).await;
            {
                let mut docs = self.inner.docs.write().await;
                if let Some(state) = docs.get_mut(&uri) {
                    state.workspace_index = Some(ws);
                }
            }
            self.check_and_publish(uri, text).await;
        }

        // Re-pulled inlay hints pick up cross-file param-name changes. (No
        // documentColor refresh exists, and colors are current-file only, so
        // an external include change can't affect them.)
        let _ = self.inner.client.inlay_hint_refresh().await;
    }

    /// Parse a settings payload (from `initializationOptions` at startup
    /// or from `workspace/didChangeConfiguration` later) and update the
    /// server-wide settings. Tolerant of missing/wrong-shape input:
    /// parse errors are logged and leave settings untouched.
    /// Returns `true` if an Au3Check-affecting setting (path or extra args)
    /// changed, so the caller can re-lint open documents. `debounceMs` changes
    /// don't affect lint *output*, so they don't count.
    fn apply_settings(&self, value: serde_json::Value, source: &'static str) -> bool {
        let opts: InitializationOptions = match serde_json::from_value(value) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(error = %e, source, "failed to parse settings — keeping current values");
                return false;
            }
        };

        let mut settings = self.inner.settings.write().expect("lock not poisoned");

        // Snapshot the Au3Check-affecting settings to detect a real change.
        let prev_au3check_path = settings.au3check_path.clone();
        let prev_extra_args = settings.au3check_extra_args.clone();

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

        // Extra Au3Check args: tokenized (quote-aware) and stored verbatim,
        // with no validation. An absent / empty / whitespace-only setting
        // clears it (falls back to the default argv).
        settings.au3check_extra_args = opts
            .au3check_extra_args
            .as_deref()
            .map(split_args)
            .unwrap_or_default();
        if settings.au3check_extra_args.is_empty() {
            tracing::debug!(source, "au3checkExtraArgs empty — using default Au3Check argv");
        } else {
            tracing::info!(
                args = ?settings.au3check_extra_args,
                source,
                "au3checkExtraArgs accepted (appended verbatim, unvalidated)"
            );
        }

        settings.au3check_path != prev_au3check_path
            || settings.au3check_extra_args != prev_extra_args
    }

    /// Re-lint every open document with the current settings. Called when an
    /// Au3Check-affecting setting changes so the new flags take effect
    /// immediately, without the user having to edit each file. Clears each
    /// doc's content-hash cache first so `check_and_publish` doesn't skip the
    /// re-check as a no-op (the text is unchanged — only the settings are).
    async fn relint_all_open_docs(&self) {
        let to_check: Vec<(Url, String)> = {
            let mut docs = self.inner.docs.write().await;
            docs.iter_mut()
                .map(|(uri, state)| {
                    state.last_checked_hash = None;
                    (uri.clone(), state.text.clone())
                })
                .collect()
        };
        if to_check.is_empty() {
            return;
        }
        tracing::debug!(
            count = to_check.len(),
            "re-linting open docs after Au3Check settings change"
        );
        for (uri, text) in to_check {
            self.check_and_publish(uri, text).await;
        }
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
        let extra_args = self.resolved_extra_args();
        let config = Au3CheckConfig {
            target: &temp_path,
            include_dirs: &include_dirs,
            cwd: Some(&original_dir),
            extra_args: &extra_args,
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

/// Tokenize a command-line-style argument string into individual argv
/// elements. Whitespace-separated, but text inside `"double quotes"` is kept
/// together (so `-I "C:\Program Files\x"` yields two tokens, with the quotes
/// stripped). No escape handling beyond quotes — adequate for Au3Check flags;
/// the user owns the contents. Used for the `au3checkExtraArgs` setting.
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true; // a (possibly empty) quoted token exists
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
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

        // v0.6.0 — capture the workspace root for the project-wide index.
        // Prefer the first workspaceFolder; fall back to the (deprecated)
        // rootUri. `None` (bare single-file session) leaves the project index
        // empty — cross-file features degrade to include-graph scope.
        let root = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
            .map(|f| f.uri.clone())
            .or(params.root_uri)
            .and_then(|uri| uri.to_file_path().ok());
        if let Some(ref path) = root {
            tracing::info!(root = %path.display(), "workspace root detected");
        } else {
            tracing::info!("no workspace root (single-file session) — project index disabled");
        }
        *self.inner.workspace_root.write().expect("lock not poisoned") = root;

        // v0.6.0 — file watching in LSP is available only via dynamic
        // registration, gated on the client advertising it. Record support so
        // `initialized` knows whether to register watchers. (This is the
        // "verify Zed support" checkpoint — the log line confirms it at runtime.)
        let supports_watchers = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref())
            .and_then(|d| d.dynamic_registration)
            .unwrap_or(false);
        tracing::info!(
            supports_watchers,
            "client didChangeWatchedFiles dynamic-registration support"
        );
        *self
            .inner
            .supports_dynamic_watchers
            .write()
            .expect("lock not poisoned") = supports_watchers;

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
                // v0.6.0 — document highlight: passive same-symbol highlighting
                // within the current file (no cross-file resolution). Reuses the
                // FileIndex find-refs + cursor-scope logic; marks assignment
                // targets as Write, other occurrences as Read.
                document_highlight_provider: Some(OneOf::Left(true)),
                // v0.6.0 — folding ranges. Tree-walk emits folds for functions,
                // #region blocks, control-flow bodies, and block comments —
                // notably fixing #region folding (zed#22703). Users must set
                // languages.AutoIt.document_folding_ranges = "on" (default
                // "off"); verified on Zed 1.4.4.
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                // Sprint 3 — completion. Trigger characters `$` and `@`
                // fire the popup immediately when those sigils are typed;
                // regular alpha input triggers via Zed's word-completion path.
                // Sprint 4 — `<` added for include-path completion in
                // `#include <Library.au3>` directives.
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["$".into(), "@".into(), "<".into()]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                // v0.5.0 — signature help. Fires on `(` (new call) and `,`
                // (moving to the next argument). Retrigger on `,` keeps the
                // popup updated as the user steps through arguments.
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".into(), ",".into()]),
                    retrigger_characters: Some(vec![",".into()]),
                    work_done_progress_options: Default::default(),
                }),
                // v0.5.0 — inlay hints: always-visible parameter-name ghost
                // text on existing call sites (e.g. `flag: 0, title: "Hi"`).
                // Zed pulls hints for the visible viewport on every scroll /
                // edit; we walk the tree and return one hint per argument.
                inlay_hint_provider: Some(OneOf::Left(true)),
                // v0.5.0 — code actions: quick-fixes surfaced on diagnostic
                // squiggles. Two kinds: add missing #include for UDF library
                // functions, and fix function-name casing to the canonical form.
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                // v0.5.0 — code formatting via AutoIt3Wrapper /Tidy.
                // Only advertised when all three required binaries are present:
                // AutoIt3.exe (driver), AutoIt3Wrapper.au3 (entry point), and
                // Tidy.exe (the actual formatter AutoIt3Wrapper calls internally).
                // Missing any one means formatting will fail, so we don't
                // advertise the capability at all in that case.
                document_formatting_provider: (self.inner.autoit3_exe.is_some()
                    && self.inner.autoit3wrapper.is_some()
                    && self.inner.tidy_exe.is_some())
                .then_some(OneOf::Left(true)),
                // v0.6.0 — document color. Inline swatches on literal 0x… color
                // arguments to known color functions, decoded per-function as
                // RGB or BGR (see color.rs). Zed renders swatches today; the
                // click-to-edit picker (colorPresentation) is dormant until Zed
                // ships it (zed#52208) but answered correctly meanwhile.
                color_provider: Some(ColorProviderCapability::Simple(true)),
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

        // v0.6.0 — build the project-wide index in the background. The scan is
        // filesystem-bound (recursive walk + parse), so it runs on a blocking
        // thread and the result is stored once ready; no handler blocks on it.
        self.spawn_project_scan().await;

        // v0.6.0 — register file watchers so the project index and open-doc
        // include graphs stay fresh when `.au3` files change on disk outside
        // Zed. Only when the client supports dynamic registration.
        self.register_file_watchers().await;
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
        // Parse and index synchronously (microsecond-scale, no lock held).
        let tree = tree::parse(&text);
        let file_index = tree.as_ref().map(|t| index::build_index(t, &text));

        // Store the document with its parsed tree *immediately*, before the
        // async #include resolution below. tower-lsp does not order a
        // notification (didOpen) against the requests that follow it — they
        // run concurrently — so an early textDocument/documentColor (or
        // folding / documentSymbol) pull can race in while we're awaiting the
        // workspace-index disk reads. If the document isn't in the map yet,
        // those handlers return empty, and Zed caches the empty result until
        // the next buffer change — which is exactly why color swatches didn't
        // appear until the user's first edit. Inserting the tree now closes
        // that window. `workspace_index` (needed only by cross-file features)
        // is built next and patched in.
        {
            let mut docs = self.inner.docs.write().await;
            docs.insert(
                uri.clone(),
                DocState {
                    text: text.clone(),
                    version: 0,
                    first_edit_pending: true,
                    last_checked_hash: None,
                    tree: tree.clone(),
                    index: file_index,
                    workspace_index: None,
                },
            );
        }

        // Sprint 4 — build the workspace index by resolving the #include tree.
        // Done outside the docs lock because it involves async disk reads.
        let workspace_index = if let (Some(t), Ok(path)) = (tree.as_ref(), uri.to_file_path()) {
            let include_dir = self.inner.autoit_include_dir.as_deref();
            Some(includes::build_workspace_index(&path, t, &text, include_dir).await)
        } else {
            None
        };

        // Patch the freshly-built workspace index into the stored document —
        // but only if no edit or save has superseded the open (version still
        // 0). did_change / did_save bump the version and own the index from
        // that point, so skipping here avoids clobbering newer data with this
        // open-time build. In that (vanishingly rare) lost-race case the
        // cross-file index repopulates on the next save.
        if let Some(ws) = workspace_index {
            let mut docs = self.inner.docs.write().await;
            if let Some(state) = docs.get_mut(&uri)
                && state.version == 0
            {
                state.workspace_index = Some(ws);
            }
        }

        self.check_and_publish(uri, text).await;
        // Zed (and most LSP clients) don't fetch inlay hints on didOpen —
        // hints only populate after the first edit by default. Sending a
        // refresh notification here forces the client to request hints for
        // the visible viewport immediately, so users see param-name ghost
        // text as soon as the file opens rather than after their first
        // keystroke. Errors (client doesn't support refresh, etc.) are
        // harmless — silently drop them.
        let _ = self.inner.client.inlay_hint_refresh().await;
    }

    /// v0.6.0 — `workspace/didChangeWatchedFiles`. Fired by the client when a
    /// watched `.au3` is created/changed/deleted on disk (including outside
    /// Zed — git checkout, external editor, generated files). Updates the
    /// project-wide index immediately, then debounce-refreshes open documents'
    /// `#include` graphs and diagnostics.
    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        tracing::debug!(count = params.changes.len(), "didChangeWatchedFiles");
        self.handle_watched_changes(params.changes).await;
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
            // workspace_index is NOT rebuilt on every keystroke — only on
            // did_open and did_save (eager resolution strategy).
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
        let Some(text) = text else { return; };

        // Sprint 4 — rebuild workspace index on save (outside the lock because
        // it does async disk reads). Re-parse the text cheaply (microseconds)
        // to avoid holding the docs lock across the await.
        let workspace_index = if let Ok(path) = uri.to_file_path() {
            if let Some(tree) = tree::parse(&text) {
                let include_dir = self.inner.autoit_include_dir.as_deref();
                Some(includes::build_workspace_index(&path, &tree, &text, include_dir).await)
            } else {
                None
            }
        } else {
            None
        };

        // Store the new workspace index.
        {
            let mut docs = self.inner.docs.write().await;
            if let Some(state) = docs.get_mut(&uri) {
                state.workspace_index = workspace_index;
            }
        }

        self.check_and_publish(uri, text).await;
        // Refresh inlay hints — UDF param names depend on the workspace
        // index, which we just rebuilt on save. Without this, hints for
        // newly-added UDF library calls wouldn't appear until the user
        // types another character.
        let _ = self.inner.client.inlay_hint_refresh().await;
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
        Ok(hover::hover_for(
            tree,
            &state.text,
            position,
            state.index.as_ref(),
            state.workspace_index.as_ref(),
        ))
    }

    /// v0.5.0 — signature help. Shows the active function's parameter list
    /// in a popup as the user types inside a call expression, with the
    /// currently-active argument highlighted.
    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let docs = self.inner.docs.read().await;
        let Some(state) = docs.get(&uri) else {
            return Ok(None);
        };
        let Some(tree) = state.tree.as_ref() else {
            return Ok(None);
        };
        Ok(signature::signature_help_for(
            tree,
            &state.text,
            position,
            state.index.as_ref(),
            state.workspace_index.as_ref(),
        ))
    }

    /// v0.5.0 — inlay hints. Returns parameter-name labels for every call
    /// expression in the viewport range whose function is known (builtin
    /// catalog, current-file UDF, or workspace UDF).
    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let range = params.range;
        let docs = self.inner.docs.read().await;
        let Some(state) = docs.get(&uri) else {
            return Ok(None);
        };
        let Some(tree) = state.tree.as_ref() else {
            return Ok(None);
        };
        let hints = hints::inlay_hints_for(
            tree,
            &state.text,
            range,
            state.index.as_ref(),
            state.workspace_index.as_ref(),
        );
        Ok(Some(hints))
    }

    /// v0.5.0 — code actions. Returns quick-fix actions for the diagnostics
    /// present on the current line/range:
    ///   • "Add #include <Lib.au3>" — when the identifier is a UDF library
    ///     function whose include directive is missing from the file.
    ///   • "Fix casing: `name` → `Name`" — when the identifier matches a
    ///     catalog entry case-insensitively but uses the wrong spelling.
    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri.clone();
        let docs = self.inner.docs.read().await;
        let Some(state) = docs.get(&uri) else {
            return Ok(None);
        };
        let actions = codeaction::code_actions_for(
            &uri,
            params.range,
            &params.context.diagnostics,
            &state.text,
            state.tree.as_ref(),
        );
        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(
                actions
                    .into_iter()
                    .map(CodeActionOrCommand::CodeAction)
                    .collect(),
            ))
        }
    }

    /// v0.5.0 — code formatting via AutoIt3Wrapper /Tidy.
    ///
    /// Writes the buffer to a temp file, runs:
    ///   `AutoIt3.exe AutoIt3Wrapper.au3 /Tidy /in <tempfile>`
    /// reads back the modified file, and returns a single whole-document
    /// `TextEdit`.  No-op when:
    ///   - `AutoIt3.exe` or `AutoIt3Wrapper.au3` are not found
    ///   - Tidy produces no changes
    ///   - Running on a non-Windows platform
    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let Some(autoit3) = self.inner.autoit3_exe.clone() else {
            return Ok(None);
        };
        let Some(wrapper) = self.inner.autoit3wrapper.clone() else {
            tracing::warn!(
                "AutoIt3Wrapper.au3 not found — formatting disabled. \
                 Install SciTE4AutoIt3 to enable Tidy formatting."
            );
            return Ok(None);
        };

        let uri = params.text_document.uri;
        let text = {
            let docs = self.inner.docs.read().await;
            let Some(state) = docs.get(&uri) else {
                return Ok(None);
            };
            state.text.clone()
        };

        // Stage the buffer to a temp .au3 file.
        let temp_path = match staging::stage_buffer(&uri, &text).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "formatting: failed to stage buffer");
                return Ok(None);
            }
        };

        // Run: AutoIt3.exe AutoIt3Wrapper.au3 /Tidy /in <tempfile>
        let source_dir = staging::original_dir(&uri);
        let mut cmd = tokio::process::Command::new(&autoit3);
        cmd.arg(&wrapper).arg("/Tidy").arg("/in").arg(&temp_path);
        if let Some(dir) = source_dir {
            cmd.current_dir(dir);
        }

        match cmd.output().await {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                tracing::warn!(
                    status = ?out.status,
                    stderr = %String::from_utf8_lossy(&out.stderr),
                    "AutoIt3Wrapper /Tidy exited with non-zero status"
                );
                return Ok(None);
            }
            Err(e) => {
                tracing::warn!(error = %e, "AutoIt3Wrapper /Tidy failed to spawn");
                return Ok(None);
            }
        }

        // Read back the formatted file (Tidy modifies in-place).
        let formatted = match tokio::fs::read_to_string(&temp_path).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "formatting: failed to read Tidy output");
                return Ok(None);
            }
        };

        // Clean up the .bak backup Tidy creates alongside the edited file.
        let bak = format!("{}.bak", temp_path.display());
        let _ = tokio::fs::remove_file(&bak).await;

        // Normalize line endings to LF before diffing, for two reasons:
        //   1. Tidy emits CRLF on Windows. If Zed's buffer is LF, a raw diff
        //      would flag every line as changed (\n vs \r\n) and degenerate to
        //      a whole-document edit. Normalizing both sides yields a real,
        //      minimal line diff.
        //   2. Returning CRLF in formatting edits triggers a Zed cursor-jump
        //      bug on Windows (zed#39547): Zed issues an extra didChange to
        //      convert the text back to LF, miscalculates positions, and jumps
        //      the cursor to EOF. Returning LF avoids it. Line-boundary
        //      positions are identical for LF and CRLF, so the edit ranges
        //      stay valid against Zed's buffer either way.
        //
        // Then emit minimal per-hunk edits covering only the changed lines —
        // unchanged lines (including the cursor's) are left untouched. Empty
        // vec (no Tidy changes) maps to None.
        let text_lf = text.replace("\r\n", "\n");
        let formatted_lf = formatted.replace("\r\n", "\n");
        let edits = format_diff::diff_edits(&text_lf, &formatted_lf);
        Ok((!edits.is_empty()).then_some(edits))
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

            // Try per-file index first.
            if let Some(def) = file_index.resolve_def(name, scope.as_deref()) {
                return Some(GotoDefinitionResponse::Scalar(Location {
                    uri: uri.clone(),
                    range: def.name_range,
                }));
            }

            // Sprint 4 — fall back to workspace index (cross-file).
            let workspace = state.workspace_index.as_ref()?;
            let entry = workspace.resolve_global(name)?;
            let origin_path = &entry.0;
            let def = &entry.1;
            let target_uri = Url::from_file_path(origin_path).ok()?;
            Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri,
                range: def.name_range,
            }))
        })();

        Ok(result)
    }

    /// Find-references. Returns all usage sites of the symbol under the cursor,
    /// scope-filtered to match the definition's visibility:
    ///   - Locals/params → refs only inside their declaring function (current file).
    ///   - Globals/functions → refs in the current file **plus** refs across all
    ///     transitively included files (Sprint 4 workspace index).
    ///
    /// Falls back gracefully when the symbol is defined in an included file
    /// (not the current file): the workspace index supplies the definition scope
    /// so current-file and cross-file refs are still collected correctly.
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_decl = params.context.include_declaration;

        let docs = self.inner.docs.read().await;
        // v0.6.0 — the project-wide index supplies upward callers (files that
        // include the current one) that the downward `#include` graph can't see.
        let project_index = self.inner.project_index.read().await;
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

            // Resolve the definition to determine its scope and declaration
            // location. Try the current file first; fall back to the workspace
            // index for symbols defined in included files.
            let (def_scope, decl_location) =
                if let Some(def) = file_index.resolve_def(name, cursor_scope.as_deref()) {
                    let loc = Location {
                        uri: uri.clone(),
                        range: def.name_range,
                    };
                    (def.scope_func.clone(), Some(loc))
                } else if let Some(ws) = state.workspace_index.as_ref() {
                    if let Some(entry) = ws.resolve_global(name) {
                        let target_uri = Url::from_file_path(&entry.0).ok()?;
                        let loc = Location {
                            uri: target_uri,
                            range: entry.1.name_range,
                        };
                        (None, Some(loc)) // workspace defs are always file-global
                    } else {
                        return None;
                    }
                } else {
                    return None;
                };

            // Current-file refs, filtered by the resolved scope.
            let file_refs = file_index.find_refs(name, def_scope.as_deref());
            let mut locations: Vec<Location> = file_refs
                .iter()
                .map(|r| Location {
                    uri: uri.clone(),
                    range: r.usage_range,
                })
                .collect();

            // Cross-file refs — only meaningful for global symbols (no scope).
            // Two layers (the locked dual-index model):
            if def_scope.is_none() {
                // (a) Downward `#include` graph — the semantic authority. Reaches
                //     files the current document includes (even outside the
                //     workspace root). Excludes the entry file itself, so no
                //     overlap with the current-file refs above.
                if let Some(ws) = state.workspace_index.as_ref() {
                    for (path, r) in ws.refs_for(name) {
                        if let Ok(ref_uri) = Url::from_file_path(path) {
                            locations.push(Location {
                                uri: ref_uri,
                                range: r.usage_range,
                            });
                        }
                    }
                }

                // (b) Project-wide index — completeness for *upward* callers
                //     (files that include the current one), which the downward
                //     graph can't reach. #include-precedence guard: only safe
                //     when the name is unambiguous project-wide (≤1 definition).
                //     On a collision (≥2 same-named defs in unrelated files) we
                //     can't tell which definition a given project ref points to
                //     without resolving each ref-file's own include graph, so we
                //     conservatively skip this layer — no false positives, at the
                //     cost of completeness in that rare case.
                if project_index.defs_for(name).len() <= 1 {
                    // The current file's refs come authoritatively from
                    // `file_index` (the live buffer); the project index only
                    // has its possibly-stale on-disk copy, so skip it here.
                    let current = uri.to_file_path().ok().map(|p| project_index::norm(&p));
                    for (path, r) in project_index.refs_for(name) {
                        if current.as_ref() == Some(&project_index::norm(path)) {
                            continue;
                        }
                        if let Ok(ref_uri) = Url::from_file_path(path) {
                            locations.push(Location {
                                uri: ref_uri,
                                range: r.usage_range,
                            });
                        }
                    }
                }
            }

            if include_decl {
                if let Some(loc) = decl_location {
                    locations.insert(0, loc);
                }
            }

            // Dedupe — the current-file / `#include` / project layers overlap
            // (the project index also covers the current file and any included
            // files under the root). Keep first occurrence to preserve the
            // declaration's leading position when `include_decl` is set.
            let mut seen = std::collections::HashSet::new();
            locations.retain(|l| {
                seen.insert((
                    l.uri.as_str().to_string(),
                    l.range.start.line,
                    l.range.start.character,
                    l.range.end.line,
                    l.range.end.character,
                ))
            });

            Some(locations)
        })();

        Ok(result)
    }

    /// v0.6.0 — document highlight. Cursor-driven same-symbol highlighting
    /// within the **current file only** (no `#include`/workspace resolution —
    /// that's find-references' job). Returns one highlight per occurrence,
    /// scope-filtered like find-references, with assignment targets marked
    /// `Write` and other occurrences `Read`.
    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let docs = self.inner.docs.read().await;
        let result = (|| -> Option<Vec<DocumentHighlight>> {
            let state = docs.get(&uri)?;
            let tree = state.tree.as_ref()?;
            let file_index = state.index.as_ref()?;
            highlight::document_highlights(tree, &state.text, file_index, position)
        })();

        Ok(result)
    }

    /// v0.6.0 — folding ranges. Walks the parse tree and returns a fold for
    /// every multi-line construct (functions, `#region` blocks, control-flow
    /// bodies, block comments). Fixes `#region` folding which Zed's built-in
    /// heuristics can't catch. Current-file only; no index needed.
    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri;

        let docs = self.inner.docs.read().await;
        let result = (|| -> Option<Vec<FoldingRange>> {
            let state = docs.get(&uri)?;
            let tree = state.tree.as_ref()?;
            Some(folding::folding_ranges(tree, &state.text))
        })();

        Ok(result)
    }

    /// v0.6.0 — document color. Returns an inline color swatch for every
    /// literal `0x…` argument to a known color function, decoded per-function
    /// as RGB (native GUI setters) or BGR (`_GUICtrl*` COLORREF wrappers).
    /// Current-file only; no index needed.
    async fn document_color(
        &self,
        params: DocumentColorParams,
    ) -> Result<Vec<ColorInformation>> {
        let uri = params.text_document.uri;

        let docs = self.inner.docs.read().await;
        let result = (|| -> Option<Vec<ColorInformation>> {
            let state = docs.get(&uri)?;
            let tree = state.tree.as_ref()?;
            Some(color::document_colors(tree, &state.text))
        })();

        Ok(result.unwrap_or_default())
    }

    /// v0.6.0 — color presentation. Formats a picked color back into the
    /// literal at the requested range, matching the enclosing function's
    /// encoding. Dormant in Zed today (no color picker yet — zed#52208) but
    /// kept correct so it works the moment Zed ships one.
    async fn color_presentation(
        &self,
        params: ColorPresentationParams,
    ) -> Result<Vec<ColorPresentation>> {
        let uri = params.text_document.uri;

        let docs = self.inner.docs.read().await;
        let result = (|| -> Option<Vec<ColorPresentation>> {
            let state = docs.get(&uri)?;
            let tree = state.tree.as_ref()?;
            Some(color::color_presentations(
                tree,
                &state.text,
                params.color,
                params.range,
            ))
        })();

        Ok(result.unwrap_or_default())
    }

    /// Sprint 3 — completion. Determines context from the partial token at
    /// the cursor:
    ///   `$…`  → scope-filtered variables, constants, parameters.
    ///   `@…`  → AutoIt built-in macros.
    ///   letter → user-defined functions + 3,542 AutoIt built-in functions.
    ///
    /// Sprint 4 — `#include` path completion: `#include "…"` and
    /// `#include <…>` directives get file/directory completions from disk.
    ///
    /// Returns an empty list when the cursor is inside a string or comment —
    /// detected by checking the tree-sitter node kind at the cursor position.
    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        // Extract include_dir before the docs lock — needed inside the closure.
        let autoit_include_dir = self.inner.autoit_include_dir.clone();

        let docs = self.inner.docs.read().await;
        let result = (|| -> Option<CompletionResponse> {
            let state = docs.get(&uri)?;
            let tree = state.tree.as_ref()?;
            let file_index = state.index.as_ref()?;

            // Sprint 4 — check for include-path completion context first.
            // Uses a line-based scan so it works even with partial/malformed
            // `#include` lines that tree-sitter has error-recovered.
            if let Ok(path) = uri.to_file_path() {
                if let Some(ctx) =
                    includes::detect_include_context(&state.text, position, &path)
                {
                    let items =
                        includes::include_path_completions(&ctx, autoit_include_dir.as_deref());
                    return Some(CompletionResponse::Array(items));
                }
            }

            // Determine the partial token at the cursor. We walk back from
            // the cursor column to find the start of the current word.
            let prefix = partial_token_at(&state.text, position);

            // Check if the cursor is inside a string or comment via the
            // parse tree — suppress completions in those contexts.
            let in_noise = tree::node_at_position(tree, &state.text, position)
                .map(|n| matches!(n.kind(), "string" | "line_comment" | "block_comment"))
                .unwrap_or(false);

            // Determine cursor scope for variable filtering.
            // Use scope_at_line (line-range based, immune to tree-sitter
            // error-recovery hoisting) rather than cursor_scope (tree-walk).
            // During mid-edit states a bare `$` is invalid syntax, so
            // tree-sitter may place the error node outside the enclosing
            // function_declaration — cursor_scope would then return None and
            // only globals would appear.  scope_at_line checks whether the
            // cursor line falls within a function's stored full_range, which
            // survives any parse-error recovery intact.
            let cursor_scope = index::scope_at_line(file_index, position.line);

            let workspace = state.workspace_index.as_ref();

            let items = complete::completions_at(
                &prefix,
                file_index,
                cursor_scope.as_deref(),
                in_noise,
                workspace,
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
        if self.apply_settings(value, "didChangeConfiguration") {
            // An Au3Check-affecting setting changed — re-lint open docs now so
            // the new flags take effect without requiring an edit per file.
            self.relint_all_open_docs().await;
        }
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
    let autoit_include_dir = au3check::discover_autoit_include_dir();
    let autoit3_exe = au3check::discover_autoit3_exe();
    let autoit3wrapper = au3check::discover_autoit3wrapper();
    let tidy_exe = au3check::discover_tidy_exe();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| {
        Backend::new(client, au3check, autoit_include_dir, autoit3_exe, autoit3wrapper, tidy_exe)
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

    // -- au3checkExtraArgs tokenization --

    #[test]
    fn split_args_basic_flags() {
        assert_eq!(split_args("-w 1 -d"), vec!["-w", "1", "-d"]);
    }

    #[test]
    fn split_args_collapses_extra_whitespace() {
        assert_eq!(split_args("  -w   1  "), vec!["-w", "1"]);
        assert_eq!(split_args("-w\t1"), vec!["-w", "1"]);
    }

    #[test]
    fn split_args_empty_and_blank_yield_nothing() {
        assert!(split_args("").is_empty());
        assert!(split_args("   \t  ").is_empty());
    }

    #[test]
    fn split_args_respects_double_quotes() {
        // A quoted path with spaces stays one token, quotes stripped.
        assert_eq!(
            split_args(r#"-I "C:\Program Files\x" -d"#),
            vec!["-I", r"C:\Program Files\x", "-d"]
        );
    }

    #[test]
    fn split_args_quote_adjacent_to_text() {
        // Quotes can open/close mid-token.
        assert_eq!(split_args(r#"-I"C:\a b"x"#), vec![r"-IC:\a bx"]);
    }

    #[test]
    fn parse_init_options_with_extra_args() {
        let v = serde_json::json!({ "au3checkExtraArgs": "-w 1 -d" });
        let opts: InitializationOptions = serde_json::from_value(v).unwrap();
        assert_eq!(opts.au3check_extra_args.as_deref(), Some("-w 1 -d"));
    }
}
