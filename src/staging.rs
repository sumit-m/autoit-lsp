//! Temp-file staging for edit-time diagnostics.
//!
//! Au3Check.exe reads from disk only — there's no stdin mode and no
//! library API. To lint an unsaved buffer we write its current text to
//! a temp file and point Au3Check at that. The original file's
//! directory is passed via `-I` so quoted `#include "x.au3"` still
//! resolves (Au3Check looks in the script's own directory first, which
//! for us is `%TEMP%\autoit-lsp\<hash>\` — empty — so it falls through
//! to the `-I` paths).
//!
//! Layout:
//!
//! ```text
//! %TEMP%\autoit-lsp\
//!   <hash-of-uri>\
//!     <original-basename>.au3
//! ```
//!
//! Per-document subdirs keep concurrent linting of different files
//! from colliding. The original basename is preserved so diagnostic
//! output (which echoes the path Au3Check was given) reads naturally
//! during debugging.
//!
//! Known limitation: transitive `#include`d files are always read
//! from disk, never from a sibling buffer. If the user has two open
//! files and edits both, the dependency direction sees the stale
//! on-disk version. Fixing this would need a virtual filesystem we
//! can't get from a closed-source Au3Check binary.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use tower_lsp::lsp_types::Url;

const TEMP_SUBDIR: &str = "autoit-lsp";

/// Root staging directory: `%TEMP%\autoit-lsp\`. Created lazily by
/// [`stage_buffer`]; not created here.
pub fn temp_root() -> PathBuf {
    std::env::temp_dir().join(TEMP_SUBDIR)
}

/// Per-document subdirectory under [`temp_root`]. Hash is stable
/// within one process run (which is all we need — temp files don't
/// persist across LSP restarts).
pub fn doc_dir_for(uri: &Url) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    uri.as_str().hash(&mut hasher);
    temp_root().join(format!("{:016x}", hasher.finish()))
}

/// The directory containing the document's original file on disk.
/// Passed to Au3Check via `-I` (so quoted includes resolve) and as
/// the process cwd (belt-and-braces — Au3Check's resolution is
/// script-dir-relative, but cwd-relative fallbacks exist in some
/// other tooling and the doubled-up safety has no downside).
///
/// Returns `None` for non-file URIs (e.g. `untitled:`) or paths
/// without a parent (e.g. `file:///C:/`).
pub fn original_dir(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// Write `text` to the staging path for `uri`. Creates the per-doc
/// subdirectory if needed. Returns the path that Au3Check should be
/// invoked against.
///
/// The temp file's basename matches the original file's basename so
/// any `#include` directives that other files use to reference this
/// one would resolve identically (not that they will, since the file
/// is in a hashed subdir — but the visible name in diagnostics stays
/// readable).
pub async fn stage_buffer(uri: &Url, text: &str) -> std::io::Result<PathBuf> {
    let dir = doc_dir_for(uri);
    tokio::fs::create_dir_all(&dir).await?;
    let basename = uri
        .to_file_path()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_os_string()))
        .and_then(|n| n.into_string().ok())
        .unwrap_or_else(|| "buffer.au3".to_string());
    let path = dir.join(basename);
    tokio::fs::write(&path, text).await?;
    Ok(path)
}

/// Best-effort removal of a single document's staging subdir.
/// Errors are swallowed — leaked temp files are sweep-able by the
/// OS and not worth surfacing to the user.
pub async fn cleanup_doc(uri: &Url) {
    let _ = tokio::fs::remove_dir_all(doc_dir_for(uri)).await;
}

/// Best-effort removal of the entire staging root. Called from the
/// LSP `shutdown` handler.
pub async fn cleanup_all() {
    let _ = tokio::fs::remove_dir_all(temp_root()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_root_is_under_system_temp() {
        let root = temp_root();
        assert!(root.starts_with(std::env::temp_dir()));
        assert!(root.ends_with(TEMP_SUBDIR));
    }

    #[test]
    fn doc_dir_is_stable_within_process() {
        let uri = Url::parse("file:///C:/proj/main.au3").unwrap();
        assert_eq!(doc_dir_for(&uri), doc_dir_for(&uri));
    }

    #[test]
    fn doc_dirs_differ_per_uri() {
        let a = Url::parse("file:///C:/proj/main.au3").unwrap();
        let b = Url::parse("file:///C:/proj/other.au3").unwrap();
        assert_ne!(doc_dir_for(&a), doc_dir_for(&b));
    }

    #[test]
    fn doc_dir_lives_under_temp_root() {
        let uri = Url::parse("file:///C:/proj/main.au3").unwrap();
        assert!(doc_dir_for(&uri).starts_with(temp_root()));
    }

    #[cfg(windows)]
    #[test]
    fn original_dir_extracts_parent_on_windows() {
        let uri = Url::parse("file:///C:/proj/main.au3").unwrap();
        let dir = original_dir(&uri).expect("file URI yields a path");
        assert!(dir.ends_with("proj"));
    }

    #[test]
    fn original_dir_returns_none_for_non_file_uri() {
        let uri = Url::parse("untitled:Untitled-1").unwrap();
        assert!(original_dir(&uri).is_none());
    }

    #[tokio::test]
    async fn stage_buffer_writes_text_and_returns_path() {
        let uri = Url::parse("file:///C:/proj/stage_test.au3").unwrap();
        let text = "ConsoleWrite(\"hi\" & @CRLF)\n";
        let path = stage_buffer(&uri, text).await.expect("stage succeeds");
        assert!(path.exists());
        let got = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(got, text);
        // Basename preserved.
        assert_eq!(path.file_name().unwrap(), "stage_test.au3");
        // Cleanup so the test is idempotent.
        cleanup_doc(&uri).await;
        assert!(!path.exists());
    }
}
