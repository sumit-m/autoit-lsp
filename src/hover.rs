//! Hover responses for AutoIt functions — both built-in and user-defined.
//!
//! **Built-in / UDF library functions** (static catalog, ~3,542 entries):
//!
//! ```text
//! ```autoit
//! #include <Array.au3>
//! _ArrayAdd ( ByRef $aArray, $vValue [, ...] )
//! ```
//!
//! Adds a specified value at the end of an existing 1D or 2D array
//!
//! **Parameters:**
//! - `$aArray` — Array to modify
//!
//! **Returns:** Success: the index of last added item.
//! ```
//!
//! **User-defined functions** (from the per-file index or workspace index):
//!
//! ```text
//! ```autoit
//! Func MyHelper($a, $b = 0)
//! ```
//!
//! *(User-defined function)*
//! ```
//!
//! Lookup priority:
//! 1. AutoIt built-in / UDF library (static catalog)
//! 2. User-defined function in the current file (`file_index`)
//! 3. User-defined function in an included file (`workspace`)

use std::fmt::Write as _;
use std::path::Path;

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};
use tree_sitter::Tree;

use crate::builtins::{self, FunctionDoc};
use crate::includes::WorkspaceIndex;
use crate::index::{DefKind, FileIndex, SymbolDef};
use crate::tree::{node_at_position, node_range};

/// Compute the hover response, if any, for `position` in `source` (parsed
/// into `tree`).
///
/// Lookup priority:
/// 1. AutoIt built-in / UDF library (static catalog, ~3,542 entries)
/// 2. User-defined function in the current file (`file_index`)
/// 3. User-defined function in an included file (`workspace`)
///
/// Returns `None` when the cursor isn't on an identifier or the identifier
/// isn't found in any of the three sources.
pub fn hover_for(
    tree: &Tree,
    source: &str,
    position: Position,
    file_index: Option<&FileIndex>,
    workspace: Option<&WorkspaceIndex>,
) -> Option<Hover> {
    let node = node_at_position(tree, source, position)?;
    // Only identifier nodes carry function names. Other leaf kinds
    // (keywords, operators, strings, variables) are not function names.
    if node.kind() != "identifier" {
        return None;
    }
    let name = node.utf8_text(source.as_bytes()).ok()?;
    let node_rng = node_range(&node, source);

    // 1. Built-in / UDF library catalog.
    if let Some(doc) = builtins::lookup(name) {
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format_markdown(doc),
            }),
            range: Some(node_rng),
        });
    }

    // 2. User-defined function in the current file.
    if let Some(idx) = file_index {
        if let Some(def) = idx.resolve_def(name, None) {
            if def.kind == DefKind::Function {
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format_udf_hover(def, None),
                    }),
                    range: Some(node_rng),
                });
            }
        }
    }

    // 3. User-defined function in an included file.
    if let Some(ws) = workspace {
        if let Some(entry) = ws.resolve_global(name) {
            if entry.1.kind == DefKind::Function {
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format_udf_hover(&entry.1, Some(&entry.0)),
                    }),
                    range: Some(node_rng),
                });
            }
        }
    }

    None
}

/// Render a user-defined function as a hover popup.
///
/// Shows the `Func Name(params...)` declaration line in a code fence, plus
/// a provenance note — "*(User-defined function)*" for in-file functions or
/// "*(Defined in `filename.au3`)*" for functions from included files.
fn format_udf_hover(def: &SymbolDef, origin: Option<&Path>) -> String {
    let mut out = String::with_capacity(256);

    out.push_str("```autoit\n");
    match &def.signature_line {
        Some(sig) => {
            out.push_str(sig);
            out.push('\n');
        }
        None => {
            // Defensive fallback — should not occur for Function defs built
            // by collect_function_decl, but guards against future refactors.
            let _ = write!(out, "Func {}(...)\n", def.display_name);
        }
    }
    out.push_str("```\n");

    // Doc comment (if present) rendered between the signature fence and the
    // provenance footnote.
    if let Some(doc) = &def.doc_comment {
        out.push('\n');
        out.push_str(doc);
        if !doc.ends_with('\n') {
            out.push('\n');
        }
    }

    match origin {
        None => out.push_str("\n*(User-defined function)*\n"),
        Some(path) => {
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            let _ = write!(out, "\n*(Defined in `{filename}`)*\n");
        }
    }

    out
}

/// Render a FunctionDoc as the body of a hover popup. Markdown so Zed can
/// syntax-highlight the signature block and bold the section headers.
fn format_markdown(doc: &FunctionDoc) -> String {
    let mut out = String::with_capacity(512);

    // Code-fenced header: optional #include line followed by the signature.
    // The `autoit` info-string tells the renderer to apply our grammar.
    out.push_str("```autoit\n");
    if let Some(inc) = &doc.include {
        let _ = writeln!(out, "{inc}");
    }
    if let Some(sig) = &doc.signature {
        let _ = writeln!(out, "{sig}");
    }
    out.push_str("```\n");

    if let Some(summary) = &doc.summary {
        if !summary.is_empty() {
            out.push('\n');
            out.push_str(summary);
            out.push('\n');
        }
    }

    if !doc.parameters.is_empty() {
        out.push_str("\n**Parameters:**\n");
        for p in &doc.parameters {
            // Convert embedded `\n` to CommonMark hard break (two trailing
            // spaces + newline) plus two spaces of continuation indent so
            // the wrapped lines stay inside the bullet's content column.
            // We can't use raw `<br>` here — Zed's hover renderer escapes
            // HTML to literal text. Multi-line param descriptions (like
            // _ArrayAdd's $iForce listing all the flag constants) need this
            // so each constant appears on its own line.
            let desc = bullet_continuation(&p.description);
            let _ = writeln!(out, "- `{}` — {}", p.name, desc);
        }
    }

    if let Some(rv) = &doc.return_value {
        if !rv.is_empty() {
            // **Returns:** on its own line so the body can be a markdown
            // bullet list (which the scraper emits for structured
            // success/failure tables) without the first bullet
            // collapsing onto the same line as the label.
            //
            // Internal `\n`s in rv are already meaningful as bullet
            // separators between rows — we don't transform them. (Rare
            // per-row multi-line content stays as a soft break, which is
            // acceptable for the supplementary nature of return-value
            // detail rows.)
            out.push_str("\n**Returns:**\n");
            out.push_str(rv);
            out.push('\n');
        }
    }

    // Footer with the documentation link — handy for "I need the full page".
    let _ = writeln!(out, "\n[Documentation]({})", doc.url);

    out
}

/// Turn a multi-line description into a markdown bullet's continuation form:
/// each embedded `\n` becomes `  \n  ` (CommonMark hard break + two-space
/// indent for the next line under the bullet's content column).
///
/// For single-line descriptions this is a no-op (no `\n` in source → no
/// allocation either — `String::replace` returns the same content but the
/// extra cost is trivial at this call site).
fn bullet_continuation(s: &str) -> String {
    s.replace('\n', "  \n  ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_index;
    use crate::tree::parse;

    #[test]
    fn hover_on_known_builtin_returns_markdown() {
        let source = "MsgBox(0, \"title\", \"text\")\n";
        let tree = parse(source).unwrap();
        // 'M' of "MsgBox" is at col 0; cursor at col 2 lands inside.
        let hover = hover_for(&tree, source, Position::new(0, 2), None, None)
            .expect("expected a hover response for MsgBox");
        let HoverContents::Markup(MarkupContent { value, .. }) = hover.contents else {
            panic!("expected Markup hover contents");
        };
        assert!(value.contains("MsgBox"), "markdown should mention MsgBox");
        assert!(value.contains("```autoit"), "should include code fence");
        // Documentation footer link present.
        assert!(value.contains("autoitscript.com/autoit3/docs"));
    }

    #[test]
    fn hover_lookup_is_case_insensitive() {
        // AutoIt source is case-insensitive on identifiers; the lookup
        // should follow suit so `msgbox(...)` shows MsgBox docs.
        let source = "msgbox(0, \"t\", \"x\")\n";
        let tree = parse(source).unwrap();
        let hover = hover_for(&tree, source, Position::new(0, 2), None, None);
        assert!(hover.is_some(), "case-insensitive lookup should still match");
    }

    #[test]
    fn hover_on_unknown_identifier_returns_none() {
        let source = "MyCustomFunction()\n";
        let tree = parse(source).unwrap();
        assert!(hover_for(&tree, source, Position::new(0, 2), None, None).is_none());
    }

    #[test]
    fn hover_on_keyword_or_variable_returns_none() {
        // `Func` is a keyword (not an identifier), and `$x` is a variable
        // — neither has builtin docs.
        let source = "Func F($x)\nEndFunc\n";
        let tree = parse(source).unwrap();
        // Position 0 = 'F' of "Func" — a keyword.
        assert!(hover_for(&tree, source, Position::new(0, 0), None, None).is_none());
        // Position 7 = '$' of "$x" — a variable.
        assert!(hover_for(&tree, source, Position::new(0, 7), None, None).is_none());
    }

    #[test]
    fn hover_markdown_has_expected_sections() {
        let source = "ConsoleWrite(\"hi\")\n";
        let tree = parse(source).unwrap();
        let hover = hover_for(&tree, source, Position::new(0, 2), None, None)
            .expect("ConsoleWrite should resolve");
        let HoverContents::Markup(MarkupContent { value, .. }) = hover.contents else {
            panic!();
        };
        // We expect signature in code fence and at least one of Parameters /
        // Returns sections — ConsoleWrite has both.
        assert!(value.contains("```autoit"));
        assert!(value.contains("**Parameters:**") || value.contains("**Returns:**"));
    }

    // ── User-defined function hover ───────────────────────────────────────────

    #[test]
    fn hover_on_user_defined_function_shows_signature() {
        let source =
            "Func MyHelper($a, $b = 0)\n    Return $a + $b\nEndFunc\n\nMyHelper(1, 2)\n";
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        // Cursor at col 2 of line 4 ("MyHelper(1, 2)") — the call site.
        let hover = hover_for(&tree, source, Position::new(4, 2), Some(&file_idx), None)
            .expect("UDF hover should be returned for a defined function");
        let HoverContents::Markup(MarkupContent { value, .. }) = hover.contents else {
            panic!("expected Markup");
        };
        assert!(value.contains("```autoit"), "should use a code fence");
        assert!(
            value.contains("MyHelper"),
            "signature should mention the function name"
        );
        assert!(
            value.contains("*(User-defined function)*"),
            "in-file UDF should have the provenance note"
        );
    }

    #[test]
    fn hover_on_udf_definition_itself_shows_signature() {
        // Hovering directly on the Func keyword's identifier (the function
        // name in the declaration) should also return the signature.
        let source = "Func Calculate($x)\n    Return $x * 2\nEndFunc\n";
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        // Line 0 col 5 = 'C' of "Calculate" in the Func declaration.
        let hover =
            hover_for(&tree, source, Position::new(0, 5), Some(&file_idx), None);
        assert!(hover.is_some(), "hover on the func name node should work");
    }

    #[test]
    fn hover_builtin_takes_priority_over_udf_with_same_name() {
        // If somehow a UDF shadows a builtin name, the builtin catalog wins.
        // (In practice, AutoIt allows re-defining builtins, but we show
        // the canonical docs regardless.)
        let source = "Func MsgBox($f, $t, $b)\nEndFunc\nMsgBox(0, \"\", \"\")\n";
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        // Cursor on the call site "MsgBox" at line 2.
        let hover = hover_for(&tree, source, Position::new(2, 2), Some(&file_idx), None)
            .expect("should return builtin docs even when name is shadowed");
        let HoverContents::Markup(MarkupContent { value, .. }) = hover.contents else {
            panic!();
        };
        // The builtin result will have a docs link; UDF result would not.
        assert!(
            value.contains("autoitscript.com"),
            "should show builtin docs, not UDF hover"
        );
    }

    // ── Doc-comment hover ─────────────────────────────────────────────────────

    #[test]
    fn hover_shows_plain_doc_comment() {
        let source = concat!(
            "; Calculates the sum of two numbers.\n",
            "Func Add($a, $b)\n",
            "    Return $a + $b\n",
            "EndFunc\n",
            "\n",
            "Add(1, 2)\n",
        );
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        let hover = hover_for(&tree, source, Position::new(5, 1), Some(&file_idx), None)
            .expect("should hover");
        let HoverContents::Markup(MarkupContent { value, .. }) = hover.contents else {
            panic!();
        };
        assert!(
            value.contains("Calculates the sum of two numbers."),
            "plain doc comment should appear in hover"
        );
    }

    #[test]
    fn hover_shows_autodoc_description_and_params() {
        let source = concat!(
            "; Description ...: Adds a value to an array\n",
            "; Parameters ....: $arr - The array\n",
            ";                  $val - The value\n",
            "Func MyAdd($arr, $val)\n",
            "EndFunc\n",
            "\n",
            "MyAdd(1, 2)\n",
        );
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        let hover = hover_for(&tree, source, Position::new(6, 1), Some(&file_idx), None)
            .expect("should hover");
        let HoverContents::Markup(MarkupContent { value, .. }) = hover.contents else {
            panic!();
        };
        assert!(value.contains("Adds a value to an array"));
        assert!(value.contains("**Parameters:**"));
        assert!(value.contains("`$arr`"));
    }

    #[test]
    fn hover_udf_without_doc_comment_still_works() {
        // Functions with no preceding comment should still show the signature.
        let source = "Func Bare()\nEndFunc\n\nBare()\n";
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        let hover = hover_for(&tree, source, Position::new(3, 1), Some(&file_idx), None)
            .expect("should hover");
        let HoverContents::Markup(MarkupContent { value, .. }) = hover.contents else {
            panic!();
        };
        assert!(value.contains("Bare"));
        assert!(value.contains("*(User-defined function)*"));
    }
}
