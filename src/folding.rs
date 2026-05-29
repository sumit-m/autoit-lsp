//! `textDocument/foldingRange` — LSP-driven code folding.
//!
//! v0.6.0. Walks the parse tree and emits a [`FoldingRange`] for every
//! multi-line foldable construct. This replaces reliance on Zed's
//! indent-detection heuristic and, crucially, fixes `#region` folding
//! ([zed-industries/zed#22703](https://github.com/zed-industries/zed/issues/22703)):
//! a `region_block` is a multi-node structure whose body sits at the *same*
//! indent as the directives, so neither of Zed's built-in fold mechanisms
//! (multi-line leaf tokens, indent ranges) catch it.
//!
//! Fold extent: each fold runs from the construct's header line through the
//! line **before** its closing keyword, so the explicit AutoIt closer
//! (`EndFunc`, `Wend`, `Next`, `EndIf`, `#EndRegion`, `#ce`, …) stays visible
//! when collapsed — matching the indent-fold behaviour users see today.
//!
//! Requires the user to opt in per language with
//! `languages.<Name>.document_folding_ranges = "on"` (default is `"off"`,
//! which keeps Zed's tree-sitter/indent folding). Verified against Zed 1.4.4;
//! documented in the README.

use tower_lsp::lsp_types::{FoldingRange, FoldingRangeKind};
use tree_sitter::{Node, Tree};

/// Build folding ranges for the whole document. Folds nest naturally — a
/// function containing an `If` yields one range for each.
pub fn folding_ranges(tree: &Tree, source: &str) -> Vec<FoldingRange> {
    let mut ranges = Vec::new();
    collect(tree.root_node(), &mut ranges);
    let _ = source; // reserved for future doc-comment / #include-block folds
    ranges
}

fn collect(node: Node, ranges: &mut Vec<FoldingRange>) {
    if let Some(range) = fold_for(&node) {
        ranges.push(range);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, ranges);
    }
}

/// Map a node to a [`FoldingRange`] if it is a foldable, multi-line construct.
fn fold_for(node: &Node) -> Option<FoldingRange> {
    let kind = match node.kind() {
        "function_declaration"
        | "if_statement"
        | "while_statement"
        | "do_statement"
        | "for_to_statement"
        | "for_in_statement"
        | "switch_statement"
        | "select_statement"
        | "with_statement" => None,
        "region_block" => Some(FoldingRangeKind::Region),
        "block_comment" => Some(FoldingRangeKind::Comment),
        _ => return None,
    };

    let start_line = node.start_position().row as u32;
    let close_line = node.end_position().row as u32;

    // Keep the closing keyword/delimiter visible: fold through the line above
    // it. `saturating_sub` guards the degenerate single-line case.
    let end_line = close_line.saturating_sub(1);

    // Nothing to fold unless the body spans at least one line.
    if end_line <= start_line {
        return None;
    }

    Some(FoldingRange {
        start_line,
        end_line,
        kind,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::parse;

    fn folds(source: &str) -> Vec<FoldingRange> {
        let tree = parse(source).expect("parse");
        folding_ranges(&tree, source)
    }

    fn has_fold(ranges: &[FoldingRange], start: u32, end: u32) -> bool {
        ranges
            .iter()
            .any(|r| r.start_line == start && r.end_line == end)
    }

    #[test]
    fn function_folds_body_keeping_endfunc_visible() {
        // 0: Func Foo()
        // 1:     ConsoleWrite("a")
        // 2:     ConsoleWrite("b")
        // 3: EndFunc
        let src = "Func Foo()\n    ConsoleWrite(\"a\")\n    ConsoleWrite(\"b\")\nEndFunc\n";
        let f = folds(src);
        assert!(has_fold(&f, 0, 2), "expected func fold lines 0..2, got {f:?}");
    }

    #[test]
    fn empty_body_function_does_not_fold() {
        // Func on line 0, EndFunc on line 1 — no body, nothing to fold.
        let src = "Func Foo()\nEndFunc\n";
        assert!(folds(src).is_empty());
    }

    #[test]
    fn region_block_folds_with_region_kind() {
        let src = "#Region UI\nLocal $a = 1\nLocal $b = 2\n#EndRegion UI\n";
        let f = folds(src);
        let region = f
            .iter()
            .find(|r| r.kind == Some(FoldingRangeKind::Region))
            .expect("region fold present");
        assert_eq!(region.start_line, 0);
        assert_eq!(region.end_line, 2);
    }

    #[test]
    fn block_comment_folds_with_comment_kind() {
        // 0: #cs
        // 1: line one
        // 2: line two
        // 3: #ce
        let src = "#cs\nline one\nline two\n#ce\n";
        let f = folds(src);
        let comment = f
            .iter()
            .find(|r| r.kind == Some(FoldingRangeKind::Comment))
            .expect("comment fold present");
        assert_eq!(comment.start_line, 0);
        assert_eq!(comment.end_line, 2);
    }

    #[test]
    fn nested_constructs_each_fold() {
        // Func containing an If — expect two folds, the If nested in the Func.
        let src = "Func Foo()\n    If $x Then\n        ConsoleWrite(\"y\")\n    EndIf\nEndFunc\n";
        let f = folds(src);
        assert!(has_fold(&f, 0, 3), "func fold 0..3 missing: {f:?}");
        assert!(has_fold(&f, 1, 2), "if fold 1..2 missing: {f:?}");
    }

    #[test]
    fn loops_and_switch_fold() {
        let src = "While $i < 10\n    $i += 1\nWend\n";
        assert!(has_fold(&folds(src), 0, 1), "while fold expected");

        let src = "For $i = 1 To 5\n    ConsoleWrite($i)\nNext\n";
        assert!(has_fold(&folds(src), 0, 1), "for fold expected");

        let src = "Switch $x\n    Case 1\n        ConsoleWrite(\"a\")\nEndSwitch\n";
        assert!(has_fold(&folds(src), 0, 2), "switch fold expected");
    }

    #[test]
    fn single_line_code_yields_no_folds() {
        assert!(folds("Local $x = 1\nConsoleWrite($x)\n").is_empty());
    }
}
