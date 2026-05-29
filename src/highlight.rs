//! `textDocument/documentHighlight` — cursor-driven same-symbol highlighting
//! within the current file.
//!
//! v0.6.0. **Current-file only** by design: unlike find-references it does not
//! consult the workspace / `#include` graph — passive in-buffer highlighting of
//! the symbol under the cursor. Reuses [`FileIndex::find_refs`] and
//! [`crate::index::cursor_scope`], the same scope logic as go-to-definition and
//! find-references, so the highlighted set matches what those features consider
//! "the same symbol".
//!
//! Read vs Write: a `variable` that is the `left` operand of an
//! `assignment_statement` (`$x = …`, `$x += …`) is reported as `Write`; the
//! declaration site is `Write` (or `Text` for a function definition); every
//! other occurrence is `Read`.

use tower_lsp::lsp_types::{DocumentHighlight, DocumentHighlightKind, Position};
use tree_sitter::Tree;

use crate::index::{DefKind, FileIndex};
use crate::tree::node_at_position;

/// Compute document highlights for the symbol under `position`.
///
/// Returns `None` when the cursor is not on a variable or function identifier,
/// or when the symbol has no occurrences. Otherwise returns one
/// [`DocumentHighlight`] per occurrence in the current file, scope-filtered the
/// same way find-references is:
///   - local / param → occurrences inside the declaring function only
///   - global / function → all occurrences in the file
///
/// A symbol defined in an `#include`d file (not the current file) won't resolve
/// in `index`; we then treat it as file-global and still highlight its
/// current-file occurrences.
pub fn document_highlights(
    tree: &Tree,
    source: &str,
    index: &FileIndex,
    position: Position,
) -> Option<Vec<DocumentHighlight>> {
    let node = node_at_position(tree, source, position)?;

    // Same symbol-node resolution as goto_definition / references: accept the
    // leaf when it's a variable/identifier, otherwise step up once.
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

    let name = sym_node.utf8_text(source.as_bytes()).ok()?;
    let cursor_scope = crate::index::cursor_scope(sym_node, source);

    // Resolve the def in the current file to learn its scope. A cross-file
    // symbol won't resolve here → `def_scope` stays `None` (file-global) and we
    // still highlight every current-file occurrence.
    let def = index.resolve_def(name, cursor_scope.as_deref());
    let def_scope = def.and_then(|d| d.scope_func.clone());

    let mut highlights: Vec<DocumentHighlight> = Vec::new();

    // The declaration site itself — defs are not stored as refs, so add it
    // explicitly. A variable/const/param/enum declaration binds the name (Write);
    // a function definition is neither a read nor a write (Text).
    if let Some(def) = def {
        let kind = match def.kind {
            DefKind::Function => DocumentHighlightKind::TEXT,
            _ => DocumentHighlightKind::WRITE,
        };
        highlights.push(DocumentHighlight {
            range: def.name_range,
            kind: Some(kind),
        });
    }

    // All usage sites, scope-filtered.
    for r in index.find_refs(name, def_scope.as_deref()) {
        let kind = if is_write_target(tree, source, r.usage_range.start) {
            DocumentHighlightKind::WRITE
        } else {
            DocumentHighlightKind::READ
        };
        highlights.push(DocumentHighlight {
            range: r.usage_range,
            kind: Some(kind),
        });
    }

    if highlights.is_empty() {
        return None;
    }
    Some(highlights)
}

/// True when the `variable` node at `pos` is the `left` target of an
/// `assignment_statement` (`$x = …`, `$x += …`). Compound-assignment targets
/// count as writes. Index/member targets (`$arr[0] = …`) are **not** treated as
/// a write of the base variable — there the base is read to locate the element,
/// so `left` is the `index_expression`, not the bare `variable`, and this
/// correctly returns `false`.
fn is_write_target(tree: &Tree, source: &str, pos: Position) -> bool {
    let Some(node) = node_at_position(tree, source, pos) else {
        return false;
    };
    // `variable` is a single regex token, so node_at_position lands on it
    // directly; step up once as a guard against token-boundary placement.
    let var = if node.kind() == "variable" {
        node
    } else if node.parent().map(|p| p.kind()) == Some("variable") {
        node.parent().unwrap()
    } else {
        node
    };
    let Some(parent) = var.parent() else {
        return false;
    };
    if parent.kind() != "assignment_statement" {
        return false;
    }
    parent
        .child_by_field_name("left")
        .map(|left| left.id() == var.id())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_index;
    use crate::tree::parse;

    fn run(source: &str, pos: Position) -> Vec<DocumentHighlight> {
        let tree = parse(source).expect("parse");
        let index = build_index(&tree, source);
        document_highlights(&tree, source, &index, pos).unwrap_or_default()
    }

    #[test]
    fn global_variable_highlights_all_occurrences() {
        // line0: Global $x = 1   (decl → def)
        // line1: $x = 2          (assignment LHS → ref/write)
        // line2: ConsoleWrite($x)(arg → ref/read)
        let src = "Global $x = 1\n$x = 2\nConsoleWrite($x)\n";
        let hls = run(src, Position::new(1, 0));
        assert!(
            hls.len() >= 3,
            "expected ≥3 highlights (def + 2 refs), got {}",
            hls.len()
        );
    }

    #[test]
    fn assignment_lhs_is_write_rhs_is_read() {
        // def $x (Write) + LHS $x (Write) + RHS $x (Read)
        let src = "Global $x = 1\n$x = $x + 1\n";
        let hls = run(src, Position::new(1, 0));
        let writes = hls
            .iter()
            .filter(|h| h.kind == Some(DocumentHighlightKind::WRITE))
            .count();
        let reads = hls
            .iter()
            .filter(|h| h.kind == Some(DocumentHighlightKind::READ))
            .count();
        assert!(writes >= 2, "expected ≥2 writes (def + LHS), got {writes}");
        assert!(reads >= 1, "expected ≥1 read (RHS), got {reads}");
    }

    #[test]
    fn local_scope_isolates_same_named_variable() {
        // F: lines 0-3, G: lines 4-7. Same local name $t in both.
        let src = "Func F()\n    Local $t = 1\n    Return $t\nEndFunc\n\
                   Func G()\n    Local $t = 9\n    Return $t\nEndFunc\n";
        // Cursor on $t in F's Return (line2, col 11 = 4 spaces + "Return ").
        let hls = run(src, Position::new(2, 11));
        assert!(!hls.is_empty(), "expected highlights for F's $t");
        assert!(
            hls.iter().all(|h| h.range.start.line <= 3),
            "highlights must stay inside F (lines 0-3), not leak into G"
        );
    }

    #[test]
    fn function_call_highlights_def_and_calls() {
        let src = "Func Foo()\nEndFunc\nFoo()\nFoo()\n";
        // Cursor on the first Foo() call (line2).
        let hls = run(src, Position::new(2, 0));
        assert!(
            hls.len() >= 3,
            "expected ≥3 highlights (def + 2 calls), got {}",
            hls.len()
        );
        assert!(
            hls.iter()
                .any(|h| h.kind == Some(DocumentHighlightKind::TEXT)),
            "function definition site should be Text kind"
        );
        // Call sites are reads, never writes.
        assert!(
            hls.iter()
                .all(|h| h.kind != Some(DocumentHighlightKind::WRITE)),
            "function references must not be marked Write"
        );
    }

    #[test]
    fn cursor_on_keyword_returns_none() {
        let src = "Global $x = 1\n";
        let tree = parse(src).expect("parse");
        let index = build_index(&tree, src);
        // Column 0 is the `Global` keyword, not a symbol.
        assert!(document_highlights(&tree, src, &index, Position::new(0, 0)).is_none());
    }

    #[test]
    fn cursor_on_unknown_symbol_returns_none() {
        // A bare variable never declared and used only once still highlights
        // itself; but a position on whitespace yields nothing.
        let src = "Global $x = 1\n";
        let tree = parse(src).expect("parse");
        let index = build_index(&tree, src);
        // Trailing position past content on an empty line.
        assert!(document_highlights(&tree, src, &index, Position::new(1, 0)).is_none());
    }
}
