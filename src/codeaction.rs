//! Code actions for AutoIt source files.
//!
//! Two quick-fix categories, both triggered when an identifier at the cursor
//! resolves (case-insensitively) to a builtin catalog entry:
//!
//! ## Add missing `#include`
//!
//! When the catalog entry has an `include` field (e.g. `#include <Array.au3>`)
//! and the file does not already contain that directive, we offer a quick-fix
//! that inserts the directive after the last existing `#include` line (or at
//! the very top of the file when none exist).
//!
//! ```text
//! _ArrayAdd($arr, 5)   ← Au3Check: "undefined function"
//! ↑ quick-fix: Add #include <Array.au3>
//! ```
//!
//! ## Fix function casing
//!
//! When the identifier at the cursor matches a catalog entry but uses different
//! casing than the canonical name (e.g. `msgbox` vs `MsgBox`), we offer a
//! text-edit that replaces that occurrence with the correct spelling.
//!
//! ```text
//! msgbox(0, "hi", "there")   ← squiggle on `msgbox`
//! ↑ quick-fix: Fix casing: `msgbox` → `MsgBox`
//! ```
//!
//! Both actions can apply simultaneously (e.g. `_arrayadd` — wrong case AND
//! missing include) and are returned as separate `CodeAction` items.
//!
//! ## Candidate discovery — two paths
//!
//! 1. **`context.diagnostics`** — the client sends the diagnostics overlapping
//!    the request range.  Each diagnostic's range in the source gives the
//!    identifier and the precise span for a fix-casing edit.
//!
//! 2. **Tree fallback** — when `context.diagnostics` is empty (some LSP clients
//!    omit it), we find the innermost `identifier` node at `request_range.start`
//!    via the parse tree.  This makes the feature work without relying on client
//!    co-operation.

use std::collections::{HashMap, HashSet};

use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, Diagnostic, Position, Range, TextEdit, Url, WorkspaceEdit,
};
use tree_sitter::Tree;

use crate::builtins;
use crate::tree::{node_at_position, node_range, position_to_byte};

// ─── Public entry point ───────────────────────────────────────────────────────

/// Build all applicable quick-fix code actions.
///
/// `request_range` — the range from the `textDocument/codeAction` request
/// (typically the cursor position or current selection).
/// `context_diagnostics` — diagnostics the client reported at that range.
/// `source` — current buffer text.
/// `tree` — parse tree; used as fallback when `context_diagnostics` is empty.
pub fn code_actions_for(
    uri: &Url,
    request_range: Range,
    context_diagnostics: &[Diagnostic],
    source: &str,
    tree: Option<&Tree>,
) -> Vec<CodeAction> {
    // Collect (name_in_source, edit_range, diagnostic) candidates.
    // edit_range is the span we'd replace for a fix-casing action.
    let mut candidates: Vec<(String, Range, Option<Diagnostic>)> = Vec::new();

    // Path 1: client-supplied diagnostics.
    for diag in context_diagnostics {
        if let Some(name) = identifier_at_range(source, &diag.range) {
            if !name.is_empty() {
                candidates.push((name.to_string(), diag.range, Some(diag.clone())));
            }
        }
    }

    // Path 2: tree-based fallback when the client sent no diagnostics.
    if candidates.is_empty() {
        if let Some(t) = tree {
            if let Some(node) = node_at_position(t, source, request_range.start) {
                // Walk up to the nearest identifier or variable node.
                let mut cur = node;
                loop {
                    if matches!(cur.kind(), "identifier" | "variable") {
                        break;
                    }
                    match cur.parent() {
                        Some(p) => cur = p,
                        None => break,
                    }
                }
                if matches!(cur.kind(), "identifier" | "variable") {
                    if let Ok(name) = cur.utf8_text(source.as_bytes()) {
                        if !name.is_empty() {
                            let range = node_range(&cur, source);
                            candidates.push((name.to_string(), range, None));
                        }
                    }
                }
            }
        }
    }

    let mut actions: Vec<CodeAction> = Vec::new();
    // Deduplicate "add #include" offers so the same include isn't proposed
    // more than once when the same function appears in multiple diagnostics.
    let mut offered_includes: HashSet<String> = HashSet::new();

    for (name, edit_range, diag_opt) in &candidates {
        // Case-insensitive catalog lookup.
        let doc = match builtins::lookup(name) {
            Some(d) => d,
            None => continue,
        };

        let diag_list: Option<Vec<Diagnostic>> =
            diag_opt.as_ref().map(|d| vec![d.clone()]);

        // ── Action 1: add missing #include ───────────────────────────────────
        if let Some(include_str) = &doc.include {
            if !already_has_include(source, include_str)
                && offered_includes.insert(include_str.clone())
            {
                let insert_line = include_insertion_line(source);
                let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
                changes.insert(
                    uri.clone(),
                    vec![TextEdit {
                        range: Range {
                            start: Position::new(insert_line, 0),
                            end: Position::new(insert_line, 0),
                        },
                        new_text: format!("{include_str}\n"),
                    }],
                );
                actions.push(CodeAction {
                    title: format!("Add {include_str}"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: diag_list.clone(),
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        ..Default::default()
                    }),
                    // Mark as preferred so editors can apply it with one keystroke.
                    is_preferred: Some(true),
                    ..Default::default()
                });
            }
        }

        // ── Action 2: fix function casing ────────────────────────────────────
        if doc.name != name.as_str() {
            let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
            changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: *edit_range,
                    new_text: doc.name.clone(),
                }],
            );
            actions.push(CodeAction {
                title: format!("Fix casing: `{name}` → `{}`", doc.name),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: diag_list,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                is_preferred: Some(false),
                ..Default::default()
            });
        }
    }

    actions
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Extract the identifier text from `source` at the span defined by `range`.
fn identifier_at_range<'s>(source: &'s str, range: &Range) -> Option<&'s str> {
    let start = position_to_byte(source, range.start)?;
    let end = position_to_byte(source, range.end)?;
    if start >= end || end > source.len() {
        return None;
    }
    let text = source[start..end].trim();
    if text.is_empty() {
        return None;
    }
    let first = text.chars().next()?;
    if first == '$' || first == '@' || first.is_alphabetic() || first == '_' {
        Some(text)
    } else {
        None
    }
}

/// Return `true` when `source` already contains a line that exactly matches
/// `include_str` (case-insensitive, trimmed). Exact-line matching avoids
/// false positives like `#include <ArrayConstants.au3>` satisfying a check
/// for `#include <Array.au3>`.
fn already_has_include(source: &str, include_str: &str) -> bool {
    let needle = include_str.to_lowercase();
    source
        .lines()
        .any(|line| line.trim().to_lowercase() == needle)
}

/// Return the 0-based line number where a new `#include` should be inserted:
/// after the last existing `#include` / `#include-once` line, or at line 0.
fn include_insertion_line(source: &str) -> u32 {
    let mut last: Option<u32> = None;
    for (i, line) in source.lines().enumerate() {
        if line.trim_start().to_lowercase().starts_with("#include") {
            last = Some(i as u32);
        }
    }
    match last {
        Some(n) => n + 1,
        None => 0,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::parse;
    use tower_lsp::lsp_types::{DiagnosticSeverity, Url};

    fn test_uri() -> Url {
        Url::parse("file:///test/sample.au3").unwrap()
    }

    fn zero_range() -> Range {
        Range {
            start: Position::new(0, 0),
            end: Position::new(0, 0),
        }
    }

    /// Build a minimal `Diagnostic` with the given range and message.
    fn diag(start: (u32, u32), end: (u32, u32), msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position::new(start.0, start.1),
                end: Position::new(end.0, end.1),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            message: msg.to_string(),
            ..Default::default()
        }
    }

    // ── Add #include actions (via context diagnostics) ────────────────────────

    #[test]
    fn add_include_offered_for_udf_library_function() {
        let source = "_ArrayAdd($arr, 5)\n";
        let d = diag((0, 0), (0, 9), "_ArrayAdd(): undefined function.");
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[d], source, None);
        let titles: Vec<&str> = actions.iter().map(|a| a.title.as_str()).collect();
        assert!(
            titles.iter().any(|t| t.contains("Array.au3")),
            "expected an add-include action; got {titles:?}"
        );
    }

    #[test]
    fn add_include_not_offered_for_core_builtins() {
        let source = "MsgBox(0, \"t\", \"m\")\n";
        let d = diag((0, 0), (0, 6), "MsgBox(): something.");
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[d], source, None);
        assert!(
            !actions.iter().any(|a| a.title.contains("include")),
            "core builtin should not trigger add-include"
        );
    }

    #[test]
    fn add_include_not_offered_when_already_present() {
        let source = "#include <Array.au3>\n_ArrayAdd($arr, 5)\n";
        let d = diag((1, 0), (1, 9), "_ArrayAdd(): undefined function.");
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[d], source, None);
        assert!(
            !actions.iter().any(|a| a.title.contains("Array.au3")),
            "should not offer include when already present"
        );
    }

    #[test]
    fn add_include_deduplicated_across_multiple_diagnostics() {
        let source = "_ArrayAdd($a, 1)\n_ArrayAdd($b, 2)\n";
        let d1 = diag((0, 0), (0, 9), "_ArrayAdd(): undefined function.");
        let d2 = diag((1, 0), (1, 9), "_ArrayAdd(): undefined function.");
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[d1, d2], source, None);
        let include_actions: Vec<_> = actions
            .iter()
            .filter(|a| a.title.contains("Array.au3"))
            .collect();
        assert_eq!(include_actions.len(), 1, "should offer include exactly once");
    }

    #[test]
    fn add_include_inserts_after_last_existing_include() {
        let source = "#include-once\n#include <String.au3>\n_ArrayAdd($arr, 5)\n";
        let d = diag((2, 0), (2, 9), "_ArrayAdd(): undefined function.");
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[d], source, None);
        let action = actions
            .iter()
            .find(|a| a.title.contains("Array.au3"))
            .expect("add-include action expected");
        let edits = action
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(edits[0].range.start.line, 2);
    }

    #[test]
    fn add_include_inserts_at_top_when_no_includes_exist() {
        let source = "_ArrayAdd($arr, 5)\n";
        let d = diag((0, 0), (0, 9), "_ArrayAdd(): undefined function.");
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[d], source, None);
        let action = actions
            .iter()
            .find(|a| a.title.contains("Array.au3"))
            .expect("add-include action expected");
        let edits = action
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(edits[0].range.start.line, 0);
    }

    #[test]
    fn add_include_new_text_ends_with_newline() {
        let source = "_ArrayAdd($arr, 5)\n";
        let d = diag((0, 0), (0, 9), "_ArrayAdd(): undefined function.");
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[d], source, None);
        let action = actions
            .iter()
            .find(|a| a.title.contains("Array.au3"))
            .unwrap();
        let edits = action
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert!(edits[0].new_text.ends_with('\n'));
    }

    // ── Fix casing actions (via context diagnostics) ──────────────────────────

    #[test]
    fn fix_casing_offered_for_wrong_case() {
        let source = "msgbox(0, \"t\", \"m\")\n";
        let d = diag((0, 0), (0, 6), "msgbox(): undefined function.");
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[d], source, None);
        let fix = actions.iter().find(|a| a.title.contains("Fix casing"));
        assert!(fix.is_some(), "expected a fix-casing action");
        assert!(fix.unwrap().title.contains("MsgBox"));
    }

    #[test]
    fn fix_casing_not_offered_when_name_is_correct() {
        let source = "MsgBox(0, \"t\", \"m\")\n";
        let d = diag((0, 0), (0, 6), "MsgBox(): something.");
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[d], source, None);
        assert!(!actions.iter().any(|a| a.title.contains("Fix casing")));
    }

    #[test]
    fn fix_casing_edit_replaces_diagnostic_range() {
        let source = "msgbox(0, \"t\", \"m\")\n";
        let d = diag((0, 0), (0, 6), "msgbox(): undefined function.");
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[d], source, None);
        let fix = actions
            .iter()
            .find(|a| a.title.contains("Fix casing"))
            .unwrap();
        let edits = fix
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(edits[0].new_text, "MsgBox");
        assert_eq!(edits[0].range.start, Position::new(0, 0));
    }

    // ── Tree-based fallback (no context diagnostics) ──────────────────────────

    #[test]
    fn tree_fallback_offers_add_include() {
        let source = "_ArrayAdd($arr, 5)\n";
        let tree = parse(source).unwrap();
        // Cursor at (0,0) — on "_ArrayAdd", no diagnostics provided.
        let cursor = Range {
            start: Position::new(0, 0),
            end: Position::new(0, 0),
        };
        let actions =
            code_actions_for(&test_uri(), cursor, &[], source, Some(&tree));
        assert!(
            actions.iter().any(|a| a.title.contains("Array.au3")),
            "tree fallback should offer add-include"
        );
    }

    #[test]
    fn tree_fallback_offers_fix_casing() {
        let source = "msgbox(0, \"t\", \"m\")\n";
        let tree = parse(source).unwrap();
        let cursor = Range {
            start: Position::new(0, 0),
            end: Position::new(0, 0),
        };
        let actions =
            code_actions_for(&test_uri(), cursor, &[], source, Some(&tree));
        assert!(
            actions.iter().any(|a| a.title.contains("Fix casing")),
            "tree fallback should offer fix-casing"
        );
    }

    #[test]
    fn tree_fallback_no_actions_for_unknown_function() {
        let source = "MyCustomFunc(1, 2)\n";
        let tree = parse(source).unwrap();
        let cursor = Range {
            start: Position::new(0, 0),
            end: Position::new(0, 0),
        };
        let actions =
            code_actions_for(&test_uri(), cursor, &[], source, Some(&tree));
        assert!(actions.is_empty());
    }

    // ── No-action cases ───────────────────────────────────────────────────────

    #[test]
    fn no_actions_for_unknown_function_via_diag() {
        let source = "MyCustomFunc(1, 2)\n";
        let d = diag((0, 0), (0, 12), "MyCustomFunc(): undefined function.");
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[d], source, None);
        assert!(actions.is_empty());
    }

    #[test]
    fn no_actions_when_no_diagnostics_and_no_tree() {
        let source = "MsgBox(0, \"t\", \"m\")\n";
        let actions =
            code_actions_for(&test_uri(), zero_range(), &[], source, None);
        assert!(actions.is_empty());
    }
}
