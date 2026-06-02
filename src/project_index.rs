//! Project-wide symbol index.
//!
//! v0.6.0. Scans every `.au3` file under the Zed workspace root and harvests
//! its file-global definitions and *all* reference sites, keyed by origin file.
//!
//! ## Why this exists alongside [`WorkspaceIndex`](crate::includes::WorkspaceIndex)
//!
//! `WorkspaceIndex` follows the `#include` graph **downward** from one entry
//! document — it sees a file's dependencies, never its *dependents*. So from a
//! leaf/library file it can't find the callers in the files that `#include` it.
//! `ProjectIndex` spans the **entire project**, so it catches those upward
//! callers. Call hierarchy and the cross-file find-references upgrade both
//! consume it; the `#include` graph remains the semantic authority for *which*
//! same-named definition a usage refers to, and this layer supplies the
//! completeness the graph can't.
//!
//! Requires a folder open in Zed (a workspace root). Bare single-file sessions
//! have no root, so this index stays empty and features degrade to
//! current-file / include-graph scope.
//!
//! Kept fresh by `workspace/didChangeWatchedFiles` (see `main.rs`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::index::{build_index, SymbolDef, SymbolRef};
use crate::tree;

/// Upper bound on `.au3` files indexed project-wide. A safety ceiling for
/// pathological trees, not a target — typical AutoIt projects are far smaller.
pub const MAX_PROJECT_FILES: usize = 2000;

/// Directory names skipped during the scan (VCS / build / dependency noise).
const SKIP_DIRS: &[&str] = &[".git", ".svn", ".hg", "node_modules", "target", ".zed"];

// ─── Per-file harvested data ────────────────────────────────────────────────

/// The globally-visible definitions and all reference sites harvested from a
/// single project file.
#[derive(Debug, Default, Clone)]
struct ProjectFileData {
    /// Lowercase name → file-global defs (`scope_func == None`).
    defs: HashMap<String, Vec<SymbolDef>>,
    /// Lowercase name → every reference site in the file (any scope).
    refs: HashMap<String, Vec<SymbolRef>>,
}

// ─── The index ──────────────────────────────────────────────────────────────

/// Project-wide map of `normalized path → harvested data`.
///
/// Keys are normalized via [`norm`] so a file has one stable key regardless of
/// canonicalization quirks (Windows case-insensitivity, the `\\?\` verbatim
/// prefix) and remains removable after it's deleted from disk.
#[derive(Debug, Default)]
pub struct ProjectIndex {
    files: HashMap<PathBuf, ProjectFileData>,
    /// `true` if the file-count cap fired during the initial scan.
    pub truncated: bool,
}

impl ProjectIndex {
    /// Parse `source`, harvest `path`'s globally-visible defs and all refs, and
    /// store them (replacing any previous data for that file). A no-op if the
    /// source doesn't parse.
    pub fn upsert(&mut self, path: &Path, source: &str) {
        let Some(parsed) = tree::parse(source) else {
            return;
        };
        let fi = build_index(&parsed, source);

        let mut data = ProjectFileData::default();
        for (key, defs) in &fi.defs {
            for def in defs {
                if def.scope_func.is_none() {
                    data.defs.entry(key.clone()).or_default().push(def.clone());
                }
            }
        }
        data.refs = fi.refs;

        self.files.insert(norm(path), data);
    }

    /// Drop a file's contribution (e.g. it was deleted or renamed away).
    /// Returns `true` if the file was present.
    pub fn remove(&mut self, path: &Path) -> bool {
        self.files.remove(&norm(path)).is_some()
    }

    /// All project files that define `name` at file-global scope
    /// (case-insensitive), as `(origin_path, def)` pairs.
    pub fn defs_for(&self, name: &str) -> Vec<(&Path, &SymbolDef)> {
        let key = name.to_lowercase();
        let mut out = Vec::new();
        for (path, data) in &self.files {
            if let Some(defs) = data.defs.get(&key) {
                for def in defs {
                    out.push((path.as_path(), def));
                }
            }
        }
        out
    }

    /// All reference sites for `name` across the project (case-insensitive),
    /// as `(origin_path, ref)` pairs.
    pub fn refs_for(&self, name: &str) -> Vec<(&Path, &SymbolRef)> {
        let key = name.to_lowercase();
        let mut out = Vec::new();
        for (path, data) in &self.files {
            if let Some(refs) = data.refs.get(&key) {
                for r in refs {
                    out.push((path.as_path(), r));
                }
            }
        }
        out
    }

    /// Number of files currently indexed.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

// ─── Directory scan ───────────────────────────────────────────────────────────

/// Recursively scan `root` for `.au3` files and build a [`ProjectIndex`].
///
/// Synchronous (filesystem-bound) — call from a blocking context
/// (`tokio::task::spawn_blocking`) so it doesn't stall the async runtime.
/// Caps at [`MAX_PROJECT_FILES`]; skips VCS/build/dependency directories.
pub fn scan(root: &Path, max: usize) -> ProjectIndex {
    let mut index = ProjectIndex::default();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // unreadable dir — skip
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if !is_skip_dir(&path) {
                    stack.push(path);
                }
            } else if file_type.is_file() && is_au3(&path) {
                if index.file_count() >= max {
                    index.truncated = true;
                    tracing::warn!(
                        root = %root.display(),
                        "project index truncated: file cap ({max}) reached"
                    );
                    return index;
                }
                if let Ok(source) = std::fs::read_to_string(&path) {
                    index.upsert(&path, &source);
                }
            }
        }
    }

    tracing::info!(
        root = %root.display(),
        files = index.file_count(),
        truncated = index.truncated,
        "project index built"
    );
    index
}

/// `true` if `path` has a `.au3` extension (case-insensitive). Compiled `.a3x`
/// files are binary, not source, so they're never indexed.
pub fn is_au3(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("au3"))
}

/// `true` if a directory's final component is one of [`SKIP_DIRS`].
fn is_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| SKIP_DIRS.iter().any(|s| n.eq_ignore_ascii_case(s)))
}

/// Normalize a path into a stable map key: strips the Windows `\\?\` verbatim
/// prefix and lowercases on Windows (its filesystem is case-insensitive). The
/// result is comparison-stable whether or not the file still exists, which is
/// what lets [`ProjectIndex::remove`] work for already-deleted files and what
/// lets find-references compare project defs against canonicalized
/// `#include`-resolved paths (run those through [`norm`] too).
pub fn norm(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    #[cfg(windows)]
    let normalized = s.replace('/', "\\").to_lowercase();
    #[cfg(not(windows))]
    let normalized = s.to_string();
    PathBuf::from(normalized)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_harvests_global_defs_only() {
        let mut idx = ProjectIndex::default();
        let src = "Global $g = 1\nFunc Helper()\n    Local $loc = 2\nEndFunc\n";
        idx.upsert(Path::new("lib.au3"), src);

        // Global $g and Func Helper are file-global → indexed.
        assert_eq!(idx.defs_for("$g").len(), 1);
        assert_eq!(idx.defs_for("Helper").len(), 1);
        // Local $loc is function-scoped → NOT a project-global def.
        assert!(idx.defs_for("$loc").is_empty());
    }

    #[test]
    fn upsert_collects_all_refs() {
        let mut idx = ProjectIndex::default();
        let src = "Func Caller()\n    Helper()\nEndFunc\n";
        idx.upsert(Path::new("a.au3"), src);
        // Helper is referenced inside Caller.
        assert_eq!(idx.refs_for("Helper").len(), 1);
    }

    #[test]
    fn defs_span_multiple_files() {
        let mut idx = ProjectIndex::default();
        idx.upsert(Path::new("a.au3"), "Func Shared()\nEndFunc\n");
        idx.upsert(Path::new("b.au3"), "Func Shared()\nEndFunc\n");
        // Same name defined in two unrelated files → both surface.
        assert_eq!(idx.defs_for("Shared").len(), 2);
    }

    #[test]
    fn refs_span_multiple_files() {
        let mut idx = ProjectIndex::default();
        idx.upsert(Path::new("a.au3"), "Func A()\n    Target()\nEndFunc\n");
        idx.upsert(Path::new("b.au3"), "Func B()\n    Target()\nEndFunc\n");
        // Target is called once in each file.
        assert_eq!(idx.refs_for("Target").len(), 2);
    }

    #[test]
    fn upsert_replaces_previous_file_data() {
        let mut idx = ProjectIndex::default();
        idx.upsert(Path::new("a.au3"), "Func Old()\nEndFunc\n");
        assert_eq!(idx.defs_for("Old").len(), 1);
        // Re-upsert the same path with different content.
        idx.upsert(Path::new("a.au3"), "Func New()\nEndFunc\n");
        assert!(idx.defs_for("Old").is_empty(), "old def should be gone");
        assert_eq!(idx.defs_for("New").len(), 1);
    }

    #[test]
    fn remove_drops_contribution() {
        let mut idx = ProjectIndex::default();
        idx.upsert(Path::new("a.au3"), "Func Gone()\nEndFunc\n");
        assert_eq!(idx.defs_for("Gone").len(), 1);
        assert!(idx.remove(Path::new("a.au3")));
        assert!(idx.defs_for("Gone").is_empty());
        // Removing again returns false.
        assert!(!idx.remove(Path::new("a.au3")));
    }

    #[test]
    fn is_au3_matches_case_insensitively() {
        assert!(is_au3(Path::new("Foo.au3")));
        assert!(is_au3(Path::new("Foo.AU3")));
        assert!(!is_au3(Path::new("Foo.a3x")));
        assert!(!is_au3(Path::new("Foo.txt")));
        assert!(!is_au3(Path::new("Foo")));
    }

    #[test]
    fn norm_strips_verbatim_prefix() {
        // On Windows the canonicalized form carries a \\?\ prefix; norm strips
        // it so a canonicalized path and a plain one compare equal.
        let plain = norm(Path::new(r"C:\proj\a.au3"));
        let verbatim = norm(Path::new(r"\\?\C:\proj\a.au3"));
        #[cfg(windows)]
        assert_eq!(plain, verbatim);
        // On non-Windows the prefix never appears; just assert norm is stable.
        #[cfg(not(windows))]
        let _ = (plain, verbatim);
    }

    #[cfg(windows)]
    #[test]
    fn norm_is_case_insensitive_on_windows() {
        assert_eq!(
            norm(Path::new(r"C:\Proj\File.au3")),
            norm(Path::new(r"c:\proj\file.au3"))
        );
    }

    #[test]
    fn scan_indexes_au3_files_under_root() {
        // Build a temp dir tree: root/a.au3, root/sub/b.au3, root/.git/c.au3
        let root = std::env::temp_dir().join(format!("autoit_pi_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("a.au3"), "Func Aaa()\nEndFunc\n").unwrap();
        std::fs::write(root.join("sub").join("b.au3"), "Func Bbb()\nEndFunc\n").unwrap();
        std::fs::write(root.join("notes.txt"), "ignore me").unwrap();
        std::fs::write(root.join(".git").join("c.au3"), "Func Ccc()\nEndFunc\n").unwrap();

        let idx = scan(&root, MAX_PROJECT_FILES);

        assert_eq!(idx.defs_for("Aaa").len(), 1, "top-level .au3 indexed");
        assert_eq!(idx.defs_for("Bbb").len(), 1, "nested .au3 indexed");
        assert!(idx.defs_for("Ccc").is_empty(), ".git contents skipped");
        assert_eq!(idx.file_count(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_respects_file_cap() {
        let root = std::env::temp_dir().join(format!("autoit_pi_cap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..5 {
            std::fs::write(root.join(format!("f{i}.au3")), "Func F()\nEndFunc\n").unwrap();
        }
        let idx = scan(&root, 3);
        assert!(idx.truncated, "cap should have fired");
        assert!(idx.file_count() <= 3, "no more than the cap indexed");
        let _ = std::fs::remove_dir_all(&root);
    }
}
