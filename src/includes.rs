//! Cross-file #include resolution and workspace-level symbol index.
//!
//! Resolves the full include tree of the currently-edited document,
//! reads included files from disk, and aggregates their globally-visible
//! symbol definitions into a `WorkspaceIndex`.
//!
//! ## Include forms
//!   - `#include "relative/path.au3"` → resolved relative to the including file's dir
//!   - `#include <Library.au3>`       → resolved from AutoIt's `Include\` directory
//!
//! ## Caps (prevent pathological trees / cycles)
//!   - Visited HashSet<PathBuf>: primary cycle guard
//!   - MAX_INCLUDE_FILES (200): total unique files processed per document
//!   - MAX_INCLUDE_DEPTH (8): nesting depth safety net

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat, Position};

use crate::index::{build_index, DefKind, FileIndex, SymbolDef, SymbolRef};
use crate::tree;

const MAX_INCLUDE_FILES: usize = 200;
const MAX_INCLUDE_DEPTH: usize = 8;

// ─── Include directive extraction ────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncludeForm {
    Quoted,       // #include "path.au3"
    AngleBracket, // #include <Library.au3>
}

#[derive(Debug, Clone)]
pub struct IncludeDirective {
    pub path: String,
    pub form: IncludeForm,
}

/// Walk `tree` and return all `#include` directives found in `source`.
pub fn extract_includes(tree: &tree_sitter::Tree, source: &str) -> Vec<IncludeDirective> {
    let mut out = Vec::new();
    collect_includes(tree.root_node(), source, &mut out);
    out
}

fn collect_includes(node: tree_sitter::Node, source: &str, out: &mut Vec<IncludeDirective>) {
    if node.kind() == "include_directive" {
        if let Some(path_node) = node.child_by_field_name("path") {
            match path_node.kind() {
                "string" => {
                    if let Ok(raw) = path_node.utf8_text(source.as_bytes()) {
                        // Strip surrounding single or double quotes.
                        let inner = raw.trim_matches(|c| c == '"' || c == '\'');
                        if !inner.is_empty() {
                            out.push(IncludeDirective {
                                path: inner.to_string(),
                                form: IncludeForm::Quoted,
                            });
                        }
                    }
                }
                "include_path" => {
                    // Find the include_path_content child.
                    let mut cursor = path_node.walk();
                    for child in path_node.children(&mut cursor) {
                        if child.kind() == "include_path_content" {
                            if let Ok(raw) = child.utf8_text(source.as_bytes()) {
                                if !raw.is_empty() {
                                    out.push(IncludeDirective {
                                        path: raw.to_string(),
                                        form: IncludeForm::AngleBracket,
                                    });
                                }
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        return; // Don't recurse into include directives.
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_includes(child, source, out);
    }
}

// ─── Path resolution ──────────────────────────────────────────────────────────

/// Resolve an include directive to an absolute, canonicalized `PathBuf`.
///
/// Returns `None` if the path doesn't exist on disk.
pub fn resolve_include(
    directive: &IncludeDirective,
    base_dir: &Path,
    autoit_include_dir: Option<&Path>,
) -> Option<PathBuf> {
    let candidate = match directive.form {
        IncludeForm::Quoted => {
            // AutoIt allows both `/` and `\` as separators.
            let normalized = directive.path.replace('/', std::path::MAIN_SEPARATOR_STR);
            base_dir.join(&normalized)
        }
        IncludeForm::AngleBracket => {
            let include_dir = autoit_include_dir?;
            include_dir.join(&directive.path)
        }
    };
    // Canonicalize gives us a stable key for the visited set and resolves `..`.
    candidate.canonicalize().ok()
}

// ─── Workspace index ──────────────────────────────────────────────────────────

/// Aggregated globally-visible symbol definitions from all files reachable
/// via the include tree of the currently-edited document.
///
/// Only file-global defs (`scope_func = None`) are stored here.
/// Locally-scoped symbols (Local vars, parameters) are never visible
/// outside their declaring file.
#[derive(Debug, Default)]
pub struct WorkspaceIndex {
    /// Lowercase name → `(origin_path, SymbolDef)` pairs.
    pub global_defs: HashMap<String, Vec<(PathBuf, SymbolDef)>>,
    /// Lowercase name → all reference sites across included files.
    /// Used by cross-file find-references to locate every usage of a
    /// globally-visible symbol in the include tree.
    pub global_refs: HashMap<String, Vec<(PathBuf, SymbolRef)>>,
    /// Number of included files processed.
    pub file_count: usize,
    /// `true` if the file-count or depth cap fired during resolution.
    pub truncated: bool,
}

impl WorkspaceIndex {
    /// Look up a globally-visible symbol by name (case-insensitive).
    /// Returns the first matching `(origin_path, def)`.
    pub fn resolve_global(&self, name: &str) -> Option<&(PathBuf, SymbolDef)> {
        let key = name.to_lowercase();
        self.global_defs.get(&key)?.first()
    }

    /// All globally-visible function definitions across included files.
    pub fn all_functions(&self) -> impl Iterator<Item = &(PathBuf, SymbolDef)> {
        self.global_defs
            .values()
            .flatten()
            .filter(|(_, d)| d.kind == DefKind::Function)
    }

    /// All globally-visible variable/constant/enum definitions across included files.
    pub fn all_variables(&self) -> impl Iterator<Item = &(PathBuf, SymbolDef)> {
        self.global_defs.values().flatten().filter(|(_, d)| {
            matches!(
                d.kind,
                DefKind::Variable | DefKind::Constant | DefKind::EnumMember
            )
        })
    }

    /// All reference sites for `name` across included files (case-insensitive).
    /// Returns an empty slice when the name has no recorded references.
    pub fn refs_for(&self, name: &str) -> &[(PathBuf, SymbolRef)] {
        let key = name.to_lowercase();
        self.global_refs
            .get(&key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

/// Build a `WorkspaceIndex` by resolving `entry_file`'s full include tree.
///
/// The entry file itself is NOT included — its symbols are already in the
/// per-file `FileIndex` stored in `DocState`. Only transitively included
/// files contribute to this index.
///
/// This is an iterative (BFS-style) async function to avoid recursive-async
/// overhead. Reads included files from disk with `tokio::fs::read_to_string`.
pub async fn build_workspace_index(
    entry_file: &Path,
    entry_tree: &tree_sitter::Tree,
    entry_source: &str,
    autoit_include_dir: Option<&Path>,
) -> WorkspaceIndex {
    let mut index = WorkspaceIndex::default();
    let mut visited: HashSet<PathBuf> = HashSet::new();

    // Mark the entry file as visited so it's never re-processed.
    if let Ok(canonical) = entry_file.canonicalize() {
        visited.insert(canonical);
    }

    let entry_base = match entry_file.parent() {
        Some(d) => d.to_path_buf(),
        None => return index,
    };

    // Work queue: (file_to_process, depth).
    let mut queue: Vec<(PathBuf, usize)> = Vec::new();

    // Seed the queue with the entry file's direct includes.
    for directive in extract_includes(entry_tree, entry_source) {
        if let Some(resolved) = resolve_include(&directive, &entry_base, autoit_include_dir) {
            if !visited.contains(&resolved) {
                queue.push((resolved, 1));
            }
        }
    }

    while let Some((path, depth)) = queue.pop() {
        // Visited / cycle guard.
        if visited.contains(&path) {
            continue;
        }
        visited.insert(path.clone());

        // File-count cap.
        if index.file_count >= MAX_INCLUDE_FILES {
            index.truncated = true;
            tracing::warn!(
                entry = %entry_file.display(),
                "workspace index truncated: file count cap ({MAX_INCLUDE_FILES}) reached"
            );
            break;
        }

        // Depth cap.
        if depth > MAX_INCLUDE_DEPTH {
            index.truncated = true;
            tracing::debug!(
                path = %path.display(),
                depth,
                "skipping: include depth cap ({MAX_INCLUDE_DEPTH}) reached"
            );
            continue;
        }

        // Read and parse.
        let source = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %e,
                    "skipping unreadable include file"
                );
                continue;
            }
        };

        let Some(file_tree) = tree::parse(&source) else {
            tracing::debug!(path = %path.display(), "skipping unparseable include file");
            continue;
        };

        // Build per-file index and harvest global defs.
        let file_index = build_index(&file_tree, &source);
        harvest_file_data(&path, &file_index, &mut index);
        index.file_count += 1;

        // Enqueue this file's includes.
        if let Some(base) = path.parent() {
            for directive in extract_includes(&file_tree, &source) {
                if let Some(resolved) =
                    resolve_include(&directive, base, autoit_include_dir)
                {
                    if !visited.contains(&resolved) {
                        queue.push((resolved, depth + 1));
                    }
                }
            }
        }
    }

    if index.file_count > 0 {
        tracing::debug!(
            entry = %entry_file.display(),
            file_count = index.file_count,
            truncated = index.truncated,
            "workspace index built"
        );
    }

    index
}

/// Copy all file-global defs and all reference sites from `file_index` into
/// the workspace index.
///
/// **Defs**: only file-global symbols (`scope_func = None`) are harvested —
/// locally-scoped defs are invisible outside their declaring file.
///
/// **Refs**: every reference site is harvested regardless of scope.  The
/// cross-file find-references handler uses these to locate all usages of a
/// globally-visible symbol across the full include tree.
fn harvest_file_data(origin: &Path, file_index: &FileIndex, workspace: &mut WorkspaceIndex) {
    for (key, defs) in &file_index.defs {
        for def in defs {
            if def.scope_func.is_none() {
                workspace
                    .global_defs
                    .entry(key.clone())
                    .or_default()
                    .push((origin.to_path_buf(), def.clone()));
            }
        }
    }
    for (key, refs) in &file_index.refs {
        for r in refs {
            workspace
                .global_refs
                .entry(key.clone())
                .or_default()
                .push((origin.to_path_buf(), r.clone()));
        }
    }
}

// ─── Include-path completion ──────────────────────────────────────────────────

/// Context detected when the cursor is inside a `#include` directive.
#[derive(Debug)]
pub enum IncludeContext {
    /// `#include "partial/path"` — complete relative to `base_dir`.
    Quoted { partial: String, base_dir: PathBuf },
    /// `#include <partial>` — complete from AutoIt's Include\ directory.
    AngleBracket { partial: String },
}

/// Determine whether the cursor is inside a `#include` directive and, if so,
/// return the completion context.
///
/// Uses a line-based scan rather than the parse tree so it works even when
/// tree-sitter's error recovery has mangled the node around the partial token
/// (e.g. `#include <` without a closing `>`).
pub fn detect_include_context(
    source: &str,
    position: Position,
    entry_file: &Path,
) -> Option<IncludeContext> {
    let line = source.lines().nth(position.line as usize)?;

    // Convert UTF-16 column to byte offset — same logic used in partial_token_at.
    let mut byte_col = 0usize;
    let mut utf16_count = 0usize;
    for ch in line.chars() {
        if utf16_count >= position.character as usize {
            break;
        }
        utf16_count += ch.len_utf16();
        byte_col += ch.len_utf8();
    }
    let before_cursor = &line[..byte_col];

    // Skip leading whitespace, then look for `#include`.
    let trimmed = before_cursor.trim_start();
    let lower = trimmed.to_lowercase();

    // Skip #include-once — it has no path argument.
    if lower.starts_with("#include-once") || lower.starts_with("#includeonce") {
        return None;
    }
    if !lower.starts_with("#include") {
        return None;
    }

    // Text after "#include" (8 bytes).
    let rest = trimmed.get(8..)?.trim_start();

    if let Some(after_quote) = rest.strip_prefix('"') {
        // Quoted form: everything between the opening `"` and the cursor.
        let base_dir = entry_file.parent()?.to_path_buf();
        Some(IncludeContext::Quoted {
            partial: after_quote.to_string(),
            base_dir,
        })
    } else if let Some(after_lt) = rest.strip_prefix('<') {
        // Angle-bracket form: everything between `<` and the cursor.
        Some(IncludeContext::AngleBracket {
            partial: after_lt.to_string(),
        })
    } else {
        None
    }
}

/// Build `CompletionItem`s for an include-path context.
pub fn include_path_completions(
    context: &IncludeContext,
    autoit_include_dir: Option<&Path>,
) -> Vec<CompletionItem> {
    match context {
        IncludeContext::Quoted { partial, base_dir } => file_completions(partial, base_dir),
        IncludeContext::AngleBracket { partial } => match autoit_include_dir {
            Some(dir) => file_completions(partial, dir),
            None => vec![],
        },
    }
}

/// List `.au3` files and subdirectories under `base_dir` that match `partial`.
///
/// `partial` may contain a leading directory component (e.g. `"utils/str"`),
/// in which case we list inside `base_dir/utils/` and filter by `"str"`.
fn file_completions(partial: &str, base_dir: &Path) -> Vec<CompletionItem> {
    // Split into (dir_prefix, name_prefix).
    // "utils/str" → dir_prefix="utils/", name_prefix="str"
    let sep_idx = partial.rfind(['/', '\\']);
    let (dir_prefix, name_prefix) = match sep_idx {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => ("", partial),
    };

    let search_dir = if dir_prefix.is_empty() {
        base_dir.to_path_buf()
    } else {
        base_dir.join(dir_prefix.replace('/', std::path::MAIN_SEPARATOR_STR))
    };

    let lower_prefix = name_prefix.to_lowercase();

    let Ok(entries) = std::fs::read_dir(&search_dir) else {
        return vec![];
    };

    let mut items: Vec<CompletionItem> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.to_lowercase().starts_with(&lower_prefix) {
                return None;
            }
            let ft = e.file_type().ok()?;
            if ft.is_dir() {
                Some(CompletionItem {
                    label: format!("{name}/"),
                    kind: Some(CompletionItemKind::FOLDER),
                    insert_text: Some(format!("{name}/")),
                    insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                    ..Default::default()
                })
            } else if ft.is_file() && name.to_lowercase().ends_with(".au3") {
                Some(CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::FILE),
                    insert_text: Some(name),
                    insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                    ..Default::default()
                })
            } else {
                None
            }
        })
        .collect();

    // Directories first, then files, both alphabetical.
    items.sort_by(|a, b| {
        let a_dir = a.kind == Some(CompletionItemKind::FOLDER);
        let b_dir = b.kind == Some(CompletionItemKind::FOLDER);
        b_dir.cmp(&a_dir).then_with(|| a.label.cmp(&b.label))
    });

    items
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::parse;

    fn parse_includes(source: &str) -> Vec<IncludeDirective> {
        let tree = parse(source).expect("parse");
        extract_includes(&tree, source)
    }

    #[test]
    fn extract_quoted_include() {
        let d = parse_includes("#include \"utils.au3\"\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "utils.au3");
        assert_eq!(d[0].form, IncludeForm::Quoted);
    }

    #[test]
    fn extract_angle_bracket_include() {
        let d = parse_includes("#include <Array.au3>\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "Array.au3");
        assert_eq!(d[0].form, IncludeForm::AngleBracket);
    }

    #[test]
    fn extract_multiple_includes() {
        let source = "#include \"a.au3\"\n#include <Array.au3>\n#include \"b/c.au3\"\n";
        let d = parse_includes(source);
        assert_eq!(d.len(), 3);
    }

    #[test]
    fn extract_no_includes_from_plain_source() {
        let source = "Global $x = 1\nFunc F()\nEndFunc\n";
        assert!(parse_includes(source).is_empty());
    }

    #[test]
    fn resolve_quoted_include_relative_to_base() {
        let base = std::env::temp_dir();
        // Create a temp file to resolve against.
        let target = base.join("test_resolve_include.au3");
        std::fs::write(&target, "").unwrap();
        let dir = IncludeDirective {
            path: "test_resolve_include.au3".to_string(),
            form: IncludeForm::Quoted,
        };
        let resolved = resolve_include(&dir, &base, None);
        assert!(resolved.is_some(), "should resolve relative quoted path");
        let _ = std::fs::remove_file(&target);
    }

    #[test]
    fn resolve_angle_bracket_returns_none_without_include_dir() {
        let directive = IncludeDirective {
            path: "Array.au3".to_string(),
            form: IncludeForm::AngleBracket,
        };
        let resolved = resolve_include(&directive, Path::new("."), None);
        assert!(resolved.is_none());
    }

    #[test]
    fn detect_include_context_quoted() {
        let source = "#include \"utils/str\"\n";
        // Cursor after "str" — position (0, 19) in UTF-16 units.
        let pos = tower_lsp::lsp_types::Position::new(0, 19);
        let entry = Path::new("C:/project/main.au3");
        let ctx = detect_include_context(source, pos, entry);
        assert!(matches!(ctx, Some(IncludeContext::Quoted { partial, .. }) if partial == "utils/str"));
    }

    #[test]
    fn detect_include_context_angle_bracket() {
        let source = "#include <Array\n";
        let pos = tower_lsp::lsp_types::Position::new(0, 15);
        let entry = Path::new("C:/project/main.au3");
        let ctx = detect_include_context(source, pos, entry);
        assert!(matches!(ctx, Some(IncludeContext::AngleBracket { partial }) if partial == "Array"));
    }

    #[test]
    fn detect_include_context_returns_none_outside_include() {
        let source = "Global $x = 1\n";
        let pos = tower_lsp::lsp_types::Position::new(0, 5);
        let entry = Path::new("C:/project/main.au3");
        assert!(detect_include_context(source, pos, entry).is_none());
    }

    #[test]
    fn detect_include_context_skips_include_once() {
        let source = "#include-once\n";
        let pos = tower_lsp::lsp_types::Position::new(0, 13);
        let entry = Path::new("C:/project/main.au3");
        assert!(detect_include_context(source, pos, entry).is_none());
    }

    #[test]
    fn harvest_file_data_only_takes_file_global_defs() {
        use crate::index::build_index;
        let source = "Global $g = 1\nFunc F()\n    Local $local = 2\nEndFunc\n";
        let tree = parse(source).expect("parse");
        let file_index = build_index(&tree, source);
        let mut ws = WorkspaceIndex::default();
        harvest_file_data(Path::new("test.au3"), &file_index, &mut ws);
        // Global $g and Func F should be harvested; Local $local should NOT.
        assert!(ws.global_defs.contains_key("$g"), "$g should be in workspace");
        assert!(ws.global_defs.contains_key("f"), "F should be in workspace");
        assert!(!ws.global_defs.contains_key("$local"), "$local must not be in workspace");
    }

    #[test]
    fn harvest_file_data_collects_refs() {
        use crate::index::build_index;
        let source = "Global $g = 1\nFunc F()\n    Return $g\nEndFunc\n";
        let tree = parse(source).expect("parse");
        let file_index = build_index(&tree, source);
        let mut ws = WorkspaceIndex::default();
        harvest_file_data(Path::new("test.au3"), &file_index, &mut ws);
        // $g is used inside F — should appear in global_refs.
        assert!(
            !ws.refs_for("$g").is_empty(),
            "$g should have at least one ref in the workspace"
        );
        // F is called at... well, not in this source, but the refs map should exist.
        // At minimum global_refs for $g should have an entry.
        assert!(
            ws.refs_for("$g").iter().all(|(_, r)| r.usage_range.start.line > 0),
            "the ref to $g should be inside function F (line > 0)"
        );
    }
}
