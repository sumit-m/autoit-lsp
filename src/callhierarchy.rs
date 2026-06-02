//! Call hierarchy tree helpers — `textDocument/prepareCallHierarchy`,
//! `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls`.
//!
//! v0.6.0. This module holds the pure parse-tree operations; the index/IO
//! orchestration (resolving definitions across the current file, the
//! `#include` graph, and the project-wide index) lives in `main.rs`.
//!
//! ## Model (locked design)
//! Dual-index, mirroring cross-file find-references:
//! * the `#include` graph is the *semantic authority* for which same-named
//!   definition a call refers to;
//! * the project-wide index supplies *completeness* — incoming callers in files
//!   that include the target, which the downward graph can't reach.
//!
//! Incoming calls are reconstructed from the symbol indexes (a reference
//! carries its enclosing function via `scope_func`). Outgoing calls walk the
//! target function's body in its source — [`calls_in_function`].

use tower_lsp::lsp_types::Range;
use tree_sitter::{Node, Tree};

use crate::tree::{node_at_position, node_range};

/// The function-name identifier at `position`, as `(name, name_range)`.
///
/// Returns the identifier whether the cursor is on a function *definition*
/// name or a *call* site. `None` when the cursor isn't on a bare identifier
/// (e.g. it's on a `$variable`, a `@macro`, a string, or whitespace) — those
/// are never user-defined functions, so call hierarchy doesn't apply.
pub fn function_ident_at(tree: &Tree, source: &str, position: tower_lsp::lsp_types::Position) -> Option<(String, Range)> {
    let node = node_at_position(tree, source, position)?;
    let ident = if node.kind() == "identifier" {
        node
    } else {
        let parent = node.parent()?;
        if parent.kind() == "identifier" {
            parent
        } else {
            return None;
        }
    };
    let name = ident.utf8_text(source.as_bytes()).ok()?.to_string();
    Some((name, node_range(&ident, source)))
}

/// Every call site inside the function named `func_name`, as
/// `(callee_name, call_name_range)`.
///
/// The callee name keeps its original casing; resolution is case-insensitive.
/// Returns an empty vec if no function with that name is found in `tree`.
/// (AutoIt has no nested functions, so a function body never contains another
/// `function_declaration`.)
pub fn calls_in_function(tree: &Tree, source: &str, func_name: &str) -> Vec<(String, Range)> {
    let mut out = Vec::new();
    if let Some(func_node) = find_function_decl(tree.root_node(), source, func_name) {
        collect_calls(func_node, source, &mut out);
    }
    out
}

/// Locate the `function_declaration` node named `name` (case-insensitive).
fn find_function_decl<'a>(node: Node<'a>, source: &str, name: &str) -> Option<Node<'a>> {
    if node.kind() == "function_declaration"
        && let Some(name_node) = node.child_by_field_name("name")
        && name_node
            .utf8_text(source.as_bytes())
            .is_ok_and(|s| s.eq_ignore_ascii_case(name))
    {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_function_decl(child, source, name) {
            return Some(found);
        }
    }
    None
}

/// Collect every `call_expression` callee identifier under `node`.
fn collect_calls(node: Node, source: &str, out: &mut Vec<(String, Range)>) {
    if node.kind() == "call_expression"
        && let Some(fn_node) = node.child_by_field_name("function")
        && fn_node.kind() == "identifier"
        && let Ok(text) = fn_node.utf8_text(source.as_bytes())
    {
        out.push((text.to_string(), node_range(&fn_node, source)));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_calls(child, source, out);
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::parse;
    use tower_lsp::lsp_types::Position;

    #[test]
    fn ident_at_definition_name() {
        // Func MyFunc()  — cursor on "MyFunc" (col 5).
        let src = "Func MyFunc()\nEndFunc\n";
        let tree = parse(src).unwrap();
        let (name, _) = function_ident_at(&tree, src, Position::new(0, 7)).expect("ident");
        assert_eq!(name, "MyFunc");
    }

    #[test]
    fn ident_at_call_site() {
        // Helper() on line 1 — cursor on the call.
        let src = "Func Caller()\n    Helper()\nEndFunc\n";
        let tree = parse(src).unwrap();
        let (name, _) = function_ident_at(&tree, src, Position::new(1, 6)).expect("ident");
        assert_eq!(name, "Helper");
    }

    #[test]
    fn ident_at_variable_returns_none() {
        // Cursor on $x (a variable, not a function identifier).
        let src = "Local $x = 1\n";
        let tree = parse(src).unwrap();
        assert!(function_ident_at(&tree, src, Position::new(0, 7)).is_none());
    }

    #[test]
    fn calls_in_function_lists_callees() {
        let src = concat!(
            "Func Outer()\n",
            "    Alpha()\n",
            "    Beta(1, 2)\n",
            "EndFunc\n",
        );
        let tree = parse(src).unwrap();
        let calls = calls_in_function(&tree, src, "Outer");
        let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"Alpha"), "got {names:?}");
        assert!(names.contains(&"Beta"), "got {names:?}");
    }

    #[test]
    fn calls_in_function_is_case_insensitive_on_name() {
        let src = "Func MyFunc()\n    Inner()\nEndFunc\n";
        let tree = parse(src).unwrap();
        assert_eq!(calls_in_function(&tree, src, "myfunc").len(), 1);
    }

    #[test]
    fn calls_in_function_excludes_calls_outside_it() {
        let src = concat!(
            "Func A()\n",
            "    InsideA()\n",
            "EndFunc\n",
            "Func B()\n",
            "    InsideB()\n",
            "EndFunc\n",
        );
        let tree = parse(src).unwrap();
        let calls = calls_in_function(&tree, src, "A");
        let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["InsideA"], "A should only contain its own calls");
    }

    #[test]
    fn calls_in_function_unknown_name_empty() {
        let src = "Func A()\n    X()\nEndFunc\n";
        let tree = parse(src).unwrap();
        assert!(calls_in_function(&tree, src, "DoesNotExist").is_empty());
    }

    #[test]
    fn calls_include_nested_in_arguments() {
        // A call nested inside another call's arguments is still a callee.
        let src = "Func F()\n    Outer(Inner())\nEndFunc\n";
        let tree = parse(src).unwrap();
        let names: Vec<String> = calls_in_function(&tree, src, "F")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert!(names.contains(&"Outer".to_string()));
        assert!(names.contains(&"Inner".to_string()));
    }
}
