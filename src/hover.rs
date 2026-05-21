//! Hover responses for AutoIt builtin and UDF library functions.
//!
//! Looks up the identifier under the cursor in the static [`builtins`]
//! catalog and formats a Markdown response:
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
//! - `$vValue` — Value(s) to add
//!
//! **Returns:** Success: the index of last added item. Failure: -1.
//! ```
//!
//! Sprint 1 deliberately handles only `identifier` nodes (function names).
//! Variables, macros (`@CRLF`), and member access fall through to no hover
//! — they need a symbol index or macro table which is Sprint 2+ work.

use std::fmt::Write as _;

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};
use tree_sitter::Tree;

use crate::builtins::{self, FunctionDoc};
use crate::tree::{node_at_position, node_range};

/// Compute the hover response, if any, for `position` in `source` (parsed
/// into `tree`). Returns `None` when the cursor isn't on a recognized
/// identifier or when that identifier isn't in the builtin catalog.
pub fn hover_for(tree: &Tree, source: &str, position: Position) -> Option<Hover> {
    let node = node_at_position(tree, source, position)?;
    // Only identifier nodes carry a builtin reference. Other leaf kinds
    // (keywords, operators, string content, variables) aren't builtin
    // function names by construction.
    if node.kind() != "identifier" {
        return None;
    }
    let name = node.utf8_text(source.as_bytes()).ok()?;
    let doc = builtins::lookup(name)?;

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format_markdown(doc),
        }),
        // Highlighting the identifier range tells the client to underline
        // exactly the matched token (rather than guessing the word boundary).
        range: Some(node_range(&node, source)),
    })
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
    use crate::tree::parse;

    #[test]
    fn hover_on_known_builtin_returns_markdown() {
        let source = "MsgBox(0, \"title\", \"text\")\n";
        let tree = parse(source).unwrap();
        // 'M' of "MsgBox" is at col 0; cursor at col 2 lands inside.
        let hover = hover_for(&tree, source, Position::new(0, 2))
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
        let hover = hover_for(&tree, source, Position::new(0, 2));
        assert!(hover.is_some(), "case-insensitive lookup should still match");
    }

    #[test]
    fn hover_on_unknown_identifier_returns_none() {
        let source = "MyCustomFunction()\n";
        let tree = parse(source).unwrap();
        assert!(hover_for(&tree, source, Position::new(0, 2)).is_none());
    }

    #[test]
    fn hover_on_keyword_or_variable_returns_none() {
        // `Func` is a keyword (not an identifier), and `$x` is a variable
        // — neither has builtin docs.
        let source = "Func F($x)\nEndFunc\n";
        let tree = parse(source).unwrap();
        // Position 0 = 'F' of "Func" — a keyword.
        assert!(hover_for(&tree, source, Position::new(0, 0)).is_none());
        // Position 7 = '$' of "$x" — a variable.
        assert!(hover_for(&tree, source, Position::new(0, 7)).is_none());
    }

    #[test]
    fn hover_markdown_has_expected_sections() {
        let source = "ConsoleWrite(\"hi\")\n";
        let tree = parse(source).unwrap();
        let hover = hover_for(&tree, source, Position::new(0, 2))
            .expect("ConsoleWrite should resolve");
        let HoverContents::Markup(MarkupContent { value, .. }) = hover.contents else {
            panic!();
        };
        // We expect signature in code fence and at least one of Parameters /
        // Returns sections — ConsoleWrite has both.
        assert!(value.contains("```autoit"));
        assert!(value.contains("**Parameters:**") || value.contains("**Returns:**"));
    }
}
