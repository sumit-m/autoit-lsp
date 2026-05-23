//! Completion item assembly for `textDocument/completion`.
//!
//! ## Context detection
//!
//! Three mutually-exclusive trigger contexts, determined by the partial
//! token at the cursor:
//!
//! | Prefix | Returns |
//! |--------|---------|
//! | `$`    | Visible variables, constants, and parameters (scope-filtered) |
//! | `@`    | AutoIt built-in macros |
//! | letter / `_` | User-defined functions + AutoIt built-in functions |
//!
//! Cursor is inside a string or comment? → empty list (checked via the
//! parse tree node kind before this function is called).
//!
//! ## Scope rules
//!
//! Uses the `FileIndex` from Sprint 2.  `visible_defs` walks the index
//! and returns only symbols reachable from the cursor position:
//! - File-global functions and variables are always visible.
//! - Parameters and `Local`/`Static` variables are visible only when the
//!   cursor is inside the function that declares them.
//!
//! ## Item cap
//!
//! Returns at most `MAX_ITEMS` entries to avoid flooding Zed's popup.
//! Prefix filtering is applied first so the cap rarely bites.

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, Documentation, InsertTextFormat, MarkupContent,
    MarkupKind,
};

use crate::builtins;
use crate::includes::WorkspaceIndex;
use crate::index::{DefKind, FileIndex};
use crate::macros;

/// Maximum number of items returned in a single completion response.
const MAX_ITEMS: usize = 200;

// ─── Public entry point ───────────────────────────────────────────────────────

/// Compute completion items for the partial token `prefix` at the cursor.
///
/// - `prefix` — the text already typed (may be empty); used for filtering.
/// - `file_index` — the per-document symbol index from Sprint 2.
/// - `cursor_scope` — lowercase name of the containing function, or `None`
///   for file-level code. Determines variable scope.
/// - `in_string_or_comment` — if true, return an empty list immediately.
/// - `workspace` — optional workspace index from Sprint 4 cross-file resolution.
pub fn completions_at(
    prefix: &str,
    file_index: &FileIndex,
    cursor_scope: Option<&str>,
    in_string_or_comment: bool,
    workspace: Option<&WorkspaceIndex>,
) -> Vec<CompletionItem> {
    if in_string_or_comment {
        return vec![];
    }

    let lower = prefix.to_lowercase();

    if lower.starts_with('$') {
        variable_completions(&lower, file_index, cursor_scope, workspace)
    } else if lower.starts_with('@') {
        macro_completions(&lower)
    } else {
        function_completions(&lower, file_index, workspace)
    }
}

// ─── Variable completions ($…) ────────────────────────────────────────────────

fn variable_completions(
    prefix: &str,
    file_index: &FileIndex,
    cursor_scope: Option<&str>,
    workspace: Option<&WorkspaceIndex>,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = file_index
        .defs
        .iter()
        .filter_map(|(key, defs)| {
            // Only variable-like kinds.
            let def = defs.first()?;
            match def.kind {
                DefKind::Variable | DefKind::Constant | DefKind::Parameter | DefKind::EnumMember => {}
                DefKind::Function => return None,
            }

            // Scope filter: local/param symbols are only visible inside
            // their declaring function.
            if let Some(sym_scope) = &def.scope_func {
                match cursor_scope {
                    Some(cur) if cur == sym_scope.as_str() => {} // same function ✓
                    _ => return None,                             // different / file scope ✗
                }
            }

            // Prefix filter (case-insensitive, both sides are already lowercase).
            if !key.starts_with(prefix) {
                return None;
            }

            let kind = match def.kind {
                DefKind::Constant | DefKind::EnumMember => CompletionItemKind::CONSTANT,
                _ => CompletionItemKind::VARIABLE,
            };

            // `$` is a trigger character, so Zed positions the insertion point
            // *right after the `$`* and keeps it in place.  If `insertText`
            // includes the `$`, the result is `$$name`.  Strip the sigil so
            // the editor inserts only `name` after the already-typed `$`.
            // The `label` retains `$name` for correct display in the popup.
            let insert = def.display_name.trim_start_matches('$').to_string();

            Some(CompletionItem {
                label: def.display_name.clone(),
                kind: Some(kind),
                insert_text: Some(insert),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                ..Default::default()
            })
        })
        .collect();

    // Cross-file variable/constant completions are intentionally excluded.
    // `$` completions are scoped to the current file only — showing constants
    // from every included library file produces too much noise for a sigil that
    // users expect to reflect their own symbols. Functions from included files
    // still appear on the letter-prefix path (see `function_completions`).
    let _ = workspace; // workspace arg kept for future opt-in or per-setting use

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items.truncate(MAX_ITEMS);
    items
}

// ─── Macro completions (@…) ───────────────────────────────────────────────────

fn macro_completions(prefix: &str) -> Vec<CompletionItem> {
    let items: Vec<CompletionItem> = macros::MACROS
        .iter()
        .filter(|m| m.name.to_lowercase().starts_with(prefix))
        .map(|m| CompletionItem {
            label: m.name.to_string(),
            kind: Some(CompletionItemKind::CONSTANT),
            detail: Some(m.description.to_string()),
            // `@` is a trigger character: Zed keeps the `@` in place and
            // inserts after it.  Strip the leading `@` from insertText so
            // the result is `@CRLF` not `@@CRLF`.
            insert_text: Some(m.name.trim_start_matches('@').to_string()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        })
        .collect();

    // Already sorted alphabetically in the static list; just cap.
    items.into_iter().take(MAX_ITEMS).collect()
}

// ─── Function completions (letter…) ──────────────────────────────────────────

fn function_completions(
    prefix: &str,
    file_index: &FileIndex,
    workspace: Option<&WorkspaceIndex>,
) -> Vec<CompletionItem> {
    // User-defined functions are always included — there are never thousands
    // of them and they're the most contextually relevant results.
    let mut user_items: Vec<CompletionItem> = file_index
        .defs
        .iter()
        .filter_map(|(key, defs)| {
            let def = defs.first()?;
            if def.kind != DefKind::Function {
                return None;
            }
            if !key.starts_with(prefix) {
                return None;
            }
            Some(CompletionItem {
                label: def.display_name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some("(user function)".into()),
                ..Default::default()
            })
        })
        .collect();
    user_items.sort_by(|a, b| a.label.cmp(&b.label));

    // Workspace functions from included files — middle tier between user funcs and builtins.
    if let Some(ws) = workspace {
        let ws_items: Vec<CompletionItem> = ws
            .all_functions()
            .filter(|entry| entry.1.display_name.to_lowercase().starts_with(prefix))
            .map(|entry| CompletionItem {
                label: entry.1.display_name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some("(included)".into()),
                ..Default::default()
            })
            .collect();
        user_items.extend(ws_items);
        user_items.sort_by(|a, b| a.label.cmp(&b.label));
        user_items.dedup_by(|a, b| a.label.eq_ignore_ascii_case(&b.label));
    }

    // Built-ins fill the remaining capacity after user functions are reserved.
    let builtin_cap = MAX_ITEMS.saturating_sub(user_items.len());
    let mut builtin_items: Vec<CompletionItem> = builtins::all_entries()
        .filter(|e| e.name.to_lowercase().starts_with(prefix))
        .map(|entry| {
            let detail = entry.signature.clone().or_else(|| Some(entry.name.clone()));
            let docs = entry.summary.as_ref().map(|s| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: s.clone(),
                })
            });
            CompletionItem {
                label: entry.name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail,
                documentation: docs,
                ..Default::default()
            }
        })
        .collect();
    builtin_items.sort_by(|a, b| a.label.cmp(&b.label));
    builtin_items.truncate(builtin_cap);

    // Return user functions (+ workspace) first, then builtins.
    user_items.extend(builtin_items);
    user_items
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_index;
    use crate::tree::parse;

    fn idx(source: &str) -> FileIndex {
        let tree = parse(source).expect("parse");
        build_index(&tree, source)
    }

    // ── Variable completions ──────────────────────────────────────────────────

    #[test]
    fn dollar_prefix_returns_variables_not_functions() {
        let index = idx("Global $foo = 1\nFunc Bar()\nEndFunc\n");
        let items = completions_at("$", &index, None, false, None);
        assert!(items.iter().any(|i| i.label == "$foo"), "should include $foo");
        assert!(
            items.iter().all(|i| i.label != "Bar"),
            "should not include function Bar"
        );
    }

    #[test]
    fn dollar_prefix_filters_by_partial_name() {
        let index = idx("Global $fooA = 1\nGlobal $fooB = 2\nGlobal $other = 3\n");
        let items = completions_at("$foo", &index, None, false, None);
        assert!(items.iter().any(|i| i.label == "$fooA"));
        assert!(items.iter().any(|i| i.label == "$fooB"));
        assert!(items.iter().all(|i| i.label != "$other"));
    }

    #[test]
    fn local_variable_only_visible_inside_function() {
        let source = "Func F()\n    Local $local = 1\nEndFunc\nGlobal $global = 2\n";
        let index = idx(source);
        // At file scope: only global visible.
        let file_items = completions_at("$", &index, None, false, None);
        assert!(file_items.iter().any(|i| i.label == "$global"));
        assert!(file_items.iter().all(|i| i.label != "$local"));
        // Inside F: both visible.
        let func_items = completions_at("$", &index, Some("f"), false, None);
        assert!(func_items.iter().any(|i| i.label == "$global"));
        assert!(func_items.iter().any(|i| i.label == "$local"));
    }

    #[test]
    fn parameter_visible_inside_function_only() {
        let source = "Func Add($a, $b)\nReturn $a\nEndFunc\n";
        let index = idx(source);
        let in_func = completions_at("$", &index, Some("add"), false, None);
        assert!(in_func.iter().any(|i| i.label == "$a"));
        let at_file = completions_at("$", &index, None, false, None);
        assert!(at_file.iter().all(|i| i.label != "$a"));
    }

    #[test]
    fn const_has_constant_completion_kind() {
        let index = idx("Global Const $MAX = 100\n");
        let items = completions_at("$", &index, None, false, None);
        let item = items.iter().find(|i| i.label == "$MAX").expect("$MAX");
        assert_eq!(item.kind, Some(CompletionItemKind::CONSTANT));
    }

    // ── Macro completions ─────────────────────────────────────────────────────

    #[test]
    fn at_prefix_returns_macros_not_variables() {
        let index = idx("Global $x = 1\n");
        let items = completions_at("@", &index, None, false, None);
        assert!(items.iter().any(|i| i.label == "@CRLF"));
        assert!(items.iter().all(|i| i.label != "$x"));
    }

    #[test]
    fn at_prefix_filters_by_partial_name() {
        let index = idx("");
        let items = completions_at("@sc", &index, None, false, None);
        assert!(items.iter().any(|i| i.label.to_lowercase().starts_with("@sc")));
        assert!(items.iter().all(|i| i.label.to_lowercase().starts_with("@sc")));
    }

    #[test]
    fn at_items_have_constant_kind() {
        let index = idx("");
        let items = completions_at("@CR", &index, None, false, None);
        assert!(items.iter().all(|i| i.kind == Some(CompletionItemKind::CONSTANT)));
    }

    // ── Function completions ──────────────────────────────────────────────────

    #[test]
    fn letter_prefix_returns_builtins() {
        let index = idx("");
        let items = completions_at("msg", &index, None, false, None);
        assert!(items.iter().any(|i| i.label.to_lowercase().starts_with("msg")));
    }

    #[test]
    fn letter_prefix_returns_user_functions() {
        let index = idx("Func MyHelper()\nEndFunc\n");
        let items = completions_at("my", &index, None, false, None);
        assert!(items.iter().any(|i| i.label == "MyHelper"));
    }

    #[test]
    fn user_function_does_not_appear_on_dollar_prefix() {
        let index = idx("Func Foo()\nEndFunc\n");
        let items = completions_at("$", &index, None, false, None);
        assert!(items.iter().all(|i| i.label != "Foo"));
    }

    #[test]
    fn empty_letter_prefix_includes_builtins_and_user_functions() {
        let index = idx("Func Zzzz()\nEndFunc\n");
        let items = completions_at("", &index, None, false, None);
        assert!(items.iter().any(|i| i.label == "Zzzz"));
        assert!(items.len() > 100, "should include many builtins");
    }

    // ── String / comment suppression ──────────────────────────────────────────

    #[test]
    fn in_string_returns_empty() {
        let index = idx("Global $x = 1\n");
        assert!(completions_at("$", &index, None, true, None).is_empty());
    }

    #[test]
    fn in_comment_returns_empty() {
        let index = idx("Global $x = 1\n");
        assert!(completions_at("msg", &index, None, true, None).is_empty());
    }

    // ── Item cap ──────────────────────────────────────────────────────────────

    #[test]
    fn result_never_exceeds_max_items() {
        let index = idx("");
        let items = completions_at("", &index, None, false, None);
        assert!(items.len() <= MAX_ITEMS);
    }
}
