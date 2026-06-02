//! `textDocument/semanticTokens/full` — LSP semantic highlighting.
//!
//! v0.6.0. Classifies identifiers/variables/macros using the symbol indexes,
//! distinguishing things tree-sitter can't see from syntax alone:
//!
//! * **builtin vs user-defined functions** (`MsgBox` vs `MyHelper`)
//! * **parameters vs local vs global variables** (all `$…` to tree-sitter)
//! * **constants** (index-recognized) and **macros** (`@CRLF`)
//! * **definitions vs calls** (the `declaration` modifier on a `Func` name)
//!
//! ## Resolution scope (locked design)
//! * **Variables — `FileIndex` only (current file).** They're the
//!   highest-frequency token, so a workspace lookup per occurrence on every
//!   full request is the expensive path; current-file scope also removes
//!   cross-file invalidation for variables. *Given up:* a variable declared
//!   `Global` in an `#include`d file isn't resolved here, so it falls back to a
//!   generic variable token rather than the "global" styling.
//! * **Functions — builtin catalog → `FileIndex` → `WorkspaceIndex`.** A UDF
//!   may be defined in an included file; UDF-library functions in the catalog
//!   are treated as `defaultLibrary` (the catalog wins, so e.g. `_ArrayAdd`
//!   styles consistently even when the user has `Array.au3` open).
//!
//! ## Request variant
//! `full` only, computed on demand (no `range` — Zed doesn't implement it; no
//! `delta` — it only shrinks the payload, not the recompute). No server-side
//! debounce: this is a *pull* request, so the client controls cadence and
//! delaying the response would just lag highlighting.

use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend,
};
use tree_sitter::{Node, Tree};

use crate::builtins;
use crate::includes::WorkspaceIndex;
use crate::index::{cursor_scope, DefKind, FileIndex};
use crate::tree::byte_to_position;

// ─── Legend ─────────────────────────────────────────────────────────────────
// Token type indices (positions in the legend's `token_types`).
const TYPE_FUNCTION: u32 = 0;
const TYPE_PARAMETER: u32 = 1;
const TYPE_VARIABLE: u32 = 2;
const TYPE_MACRO: u32 = 3;

// Modifier bit flags (positions in the legend's `token_modifiers`).
const MOD_DECLARATION: u32 = 1 << 0;
const MOD_DEFAULT_LIBRARY: u32 = 1 << 1;
const MOD_READONLY: u32 = 1 << 2;
const MOD_STATIC: u32 = 1 << 3;

/// The legend advertised in `initialize`. Order MUST match the index/bit
/// constants above — the wire protocol references types/modifiers by position.
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::FUNCTION,  // 0
            SemanticTokenType::PARAMETER, // 1
            SemanticTokenType::VARIABLE,  // 2
            SemanticTokenType::MACRO,     // 3
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,     // bit 0
            SemanticTokenModifier::DEFAULT_LIBRARY, // bit 1
            SemanticTokenModifier::READONLY,        // bit 2
            SemanticTokenModifier::STATIC,          // bit 3
        ],
    }
}

// ─── Token collection ───────────────────────────────────────────────────────

/// A token before delta-encoding: absolute line + UTF-16 start column.
struct Raw {
    line: u32,
    start: u32,
    length: u32,
    token_type: u32,
    modifiers: u32,
}

/// Build the full semantic-token set for a document.
pub fn semantic_tokens(
    tree: &Tree,
    source: &str,
    file_index: &FileIndex,
    workspace: Option<&WorkspaceIndex>,
) -> Vec<SemanticToken> {
    let mut raws = Vec::new();
    collect(tree.root_node(), source, file_index, workspace, &mut raws);

    // The protocol requires tokens in ascending (line, start) order.
    raws.sort_by(|a, b| a.line.cmp(&b.line).then(a.start.cmp(&b.start)));

    // Delta-encode relative to the previous token.
    let mut out = Vec::with_capacity(raws.len());
    let (mut prev_line, mut prev_start) = (0u32, 0u32);
    for r in raws {
        let delta_line = r.line - prev_line;
        let delta_start = if delta_line == 0 {
            r.start - prev_start
        } else {
            r.start
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length: r.length,
            token_type: r.token_type,
            token_modifiers_bitset: r.modifiers,
        });
        prev_line = r.line;
        prev_start = r.start;
    }
    out
}

fn collect(
    node: Node,
    source: &str,
    file_index: &FileIndex,
    workspace: Option<&WorkspaceIndex>,
    out: &mut Vec<Raw>,
) {
    match node.kind() {
        "variable" => {
            if let Some(raw) = classify_variable(node, source, file_index) {
                out.push(raw);
            }
        }
        "macro" => out.push(raw(node, source, TYPE_MACRO, 0)),
        "identifier" => {
            if let Some(r) = classify_identifier(node, source, file_index, workspace) {
                out.push(r);
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, file_index, workspace, out);
    }
}

/// Classify a `$variable` occurrence from the current-file index (parameter /
/// local / global / constant). `None` if unresolved (e.g. a global declared in
/// an included file — given up by design) so tree-sitter's highlight stands.
fn classify_variable(node: Node, source: &str, file_index: &FileIndex) -> Option<Raw> {
    let name = node.utf8_text(source.as_bytes()).ok()?;
    let scope = cursor_scope(node, source);
    let def = file_index.resolve_def(name, scope.as_deref())?;
    let (ty, mods) = match def.kind {
        DefKind::Parameter => (TYPE_PARAMETER, 0),
        DefKind::Constant | DefKind::EnumMember => (TYPE_VARIABLE, MOD_READONLY),
        DefKind::Variable if def.scope_func.is_some() => (TYPE_VARIABLE, 0), // local
        DefKind::Variable => (TYPE_VARIABLE, MOD_STATIC),                    // file-global
        DefKind::Function => return None, // a $var never names a function
    };
    Some(raw(node, source, ty, mods))
}

/// Classify a bare `identifier` — only function definitions and call sites.
/// Other identifier positions (member access, etc.) yield `None`.
fn classify_identifier(
    node: Node,
    source: &str,
    file_index: &FileIndex,
    workspace: Option<&WorkspaceIndex>,
) -> Option<Raw> {
    let name = node.utf8_text(source.as_bytes()).ok()?;
    let parent = node.parent()?;

    // Function definition name → declaration.
    if parent.kind() == "function_declaration"
        && parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
    {
        return Some(raw(node, source, TYPE_FUNCTION, MOD_DECLARATION));
    }

    // Call site (the `function` child of a call_expression).
    let is_call = parent.kind() == "call_expression"
        && parent.child_by_field_name("function").map(|n| n.id()) == Some(node.id());
    if !is_call {
        return None;
    }

    // Builtin catalog wins (UDF-library funcs are "default library").
    if builtins::lookup(name).is_some() {
        return Some(raw(node, source, TYPE_FUNCTION, MOD_DEFAULT_LIBRARY));
    }
    // User-defined function (current file or included files).
    let is_udf = file_index
        .resolve_def(name, None)
        .is_some_and(|d| d.kind == DefKind::Function)
        || workspace.is_some_and(|w| {
            w.resolve_global(name)
                .is_some_and(|(_, d)| d.kind == DefKind::Function)
        });
    if is_udf {
        return Some(raw(node, source, TYPE_FUNCTION, 0));
    }
    None // unknown — leave to tree-sitter's syntax highlight
}

/// Build a [`Raw`] token from a leaf node. AutoIt identifiers/variables/macros
/// are ASCII and single-line, but we still convert via UTF-16 (line may contain
/// earlier non-ASCII) and count UTF-16 units for `length`, per LSP.
fn raw(node: Node, source: &str, token_type: u32, modifiers: u32) -> Raw {
    let pos = byte_to_position(source, node.start_byte());
    let text = node.utf8_text(source.as_bytes()).unwrap_or("");
    let length: u32 = text.chars().map(|c| c.len_utf16() as u32).sum();
    Raw {
        line: pos.line,
        start: pos.character,
        length,
        token_type,
        modifiers,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_index;
    use crate::tree::parse;

    /// Decode the delta-encoded stream back to absolute
    /// `(line, start, len, type, mods)` tuples for assertions.
    fn decode(tokens: &[SemanticToken]) -> Vec<(u32, u32, u32, u32, u32)> {
        let mut out = Vec::new();
        let (mut line, mut start) = (0u32, 0u32);
        for t in tokens {
            if t.delta_line == 0 {
                start += t.delta_start;
            } else {
                line += t.delta_line;
                start = t.delta_start;
            }
            out.push((line, start, t.length, t.token_type, t.token_modifiers_bitset));
        }
        out
    }

    fn tokens(src: &str) -> Vec<(u32, u32, u32, u32, u32)> {
        let tree = parse(src).unwrap();
        let fi = build_index(&tree, src);
        decode(&semantic_tokens(&tree, src, &fi, None))
    }

    /// Find the token covering `(line, start)`, returning `(type, mods)`.
    fn at(toks: &[(u32, u32, u32, u32, u32)], line: u32, start: u32) -> Option<(u32, u32)> {
        toks.iter()
            .find(|(l, s, _, _, _)| *l == line && *s == start)
            .map(|(_, _, _, ty, m)| (*ty, *m))
    }

    #[test]
    fn builtin_call_is_function_default_library() {
        // ConsoleWrite at line 0, col 0.
        let t = tokens("ConsoleWrite(\"hi\")\n");
        assert_eq!(at(&t, 0, 0), Some((TYPE_FUNCTION, MOD_DEFAULT_LIBRARY)));
    }

    #[test]
    fn udf_definition_is_declaration() {
        // Func MyHelper() — name at col 5.
        let t = tokens("Func MyHelper()\nEndFunc\n");
        assert_eq!(at(&t, 0, 5), Some((TYPE_FUNCTION, MOD_DECLARATION)));
    }

    #[test]
    fn udf_call_is_plain_function() {
        let src = "Func MyHelper()\nEndFunc\nMyHelper()\n";
        let t = tokens(src);
        // The call on line 2 col 0 → function, no modifiers.
        assert_eq!(at(&t, 2, 0), Some((TYPE_FUNCTION, 0)));
    }

    #[test]
    fn parameter_is_parameter_token() {
        // Func F($p) ... $p used in body.
        let src = "Func F($p)\n    ConsoleWrite($p)\nEndFunc\n";
        let t = tokens(src);
        // $p inside ConsoleWrite on line 1. Find a PARAMETER token on line 1.
        assert!(
            t.iter().any(|(l, _, _, ty, _)| *l == 1 && *ty == TYPE_PARAMETER),
            "expected a parameter token on line 1: {t:?}"
        );
    }

    #[test]
    fn local_vs_global_variable_modifiers_differ() {
        let src = concat!(
            "Global $g = 1\n",
            "Func F()\n",
            "    Local $loc = 2\n",
            "    ConsoleWrite($g & $loc)\n",
            "EndFunc\n",
        );
        let t = tokens(src);
        // $g usage on line 3 → VARIABLE + STATIC (global).
        let g = t
            .iter()
            .find(|(l, _, _, ty, m)| *l == 3 && *ty == TYPE_VARIABLE && *m == MOD_STATIC);
        assert!(g.is_some(), "global $g should be VARIABLE+STATIC: {t:?}");
        // $loc usage on line 3 → VARIABLE, no modifier (local).
        let loc = t
            .iter()
            .find(|(l, _, _, ty, m)| *l == 3 && *ty == TYPE_VARIABLE && *m == 0);
        assert!(loc.is_some(), "local $loc should be plain VARIABLE: {t:?}");
    }

    #[test]
    fn constant_is_readonly() {
        let src = "Const $PI = 3\nConsoleWrite($PI)\n";
        let t = tokens(src);
        // $PI usage on line 1 → VARIABLE + READONLY.
        assert!(
            t.iter()
                .any(|(l, _, _, ty, m)| *l == 1 && *ty == TYPE_VARIABLE && *m == MOD_READONLY),
            "const $PI should be readonly: {t:?}"
        );
    }

    #[test]
    fn macro_is_macro_token() {
        let t = tokens("ConsoleWrite(@CRLF)\n");
        // @CRLF starts at col 13.
        assert!(
            t.iter().any(|(_, _, _, ty, _)| *ty == TYPE_MACRO),
            "expected a macro token: {t:?}"
        );
    }

    #[test]
    fn unknown_function_emits_no_token() {
        // Not a builtin, not a UDF → no semantic token (tree-sitter handles it).
        let t = tokens("TotallyUnknownFn()\n");
        assert!(
            at(&t, 0, 0).is_none(),
            "unknown function should not get a semantic token: {t:?}"
        );
    }

    #[test]
    fn tokens_are_delta_sorted_nonnegative() {
        // Two builtins on the same line — second delta_start must be relative.
        let src = "ConsoleWrite(MsgBox(0, \"a\", \"b\"))\n";
        let tree = parse(src).unwrap();
        let fi = build_index(&tree, src);
        let raw = semantic_tokens(&tree, src, &fi, None);
        // First token's delta_line/start are absolute-from-zero; subsequent
        // same-line tokens have delta_line 0 and positive delta_start.
        assert!(!raw.is_empty());
        for w in raw.windows(2) {
            if w[1].delta_line == 0 {
                // same line as previous → start strictly increases
                assert!(w[1].delta_start > 0, "same-line delta_start must be > 0");
            }
        }
    }
}
