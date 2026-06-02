//! Inlay hints — always-visible parameter-name ghost text on call sites.
//!
//! For every `call_expression` in the requested viewport range whose function
//! is known to the builtin catalog or UDF index, we emit one [`InlayHint`]
//! per argument showing the corresponding parameter name:
//!
//! ```text
//! MsgBox( flag: 0,  title: "Hi",  text: "Hello" )
//!        ↑ghost    ↑ghost         ↑ghost
//! ```
//!
//! The hints use [`InlayHintKind::PARAMETER`] so editors can style them
//! distinctly (typically dimmed/italic).  `padding_right = true` adds a
//! thin space between the label and the value it annotates.

use tower_lsp::lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Range};
use tree_sitter::{Node, Tree};

use crate::builtins;
use crate::includes::WorkspaceIndex;
use crate::index::{DefKind, FileIndex};
use crate::signature::parse_udf_params;
use crate::tree::byte_to_position;

// ─── Public entry point ───────────────────────────────────────────────────────

/// Return parameter-name inlay hints for every call expression in `source`
/// whose byte range overlaps `range`.
///
/// Only processes calls where the function name is resolvable to a parameter
/// list (builtin catalog, current-file UDF, or workspace UDF).
pub fn inlay_hints_for(
    tree: &Tree,
    source: &str,
    range: Range,
    file_index: Option<&FileIndex>,
    workspace: Option<&WorkspaceIndex>,
) -> Vec<InlayHint> {
    let mut hints = Vec::new();
    collect_hints(
        tree.root_node(),
        source,
        &range,
        file_index,
        workspace,
        &mut hints,
    );
    hints
}

// ─── Tree walker ──────────────────────────────────────────────────────────────

/// Recursively walk `node`, collecting hints from every `call_expression`
/// that overlaps `range`.
fn collect_hints(
    node: Node,
    source: &str,
    range: &Range,
    file_index: Option<&FileIndex>,
    workspace: Option<&WorkspaceIndex>,
    out: &mut Vec<InlayHint>,
) {
    if node.kind() == "call_expression" {
        hints_for_call(node, source, file_index, workspace, out);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Prune subtrees that end before the range starts or start after it
        // ends (row-number comparison — cheap and good enough for a viewport).
        let child_end_row = child.end_position().row as u32;
        let child_start_row = child.start_position().row as u32;
        if child_end_row < range.start.line || child_start_row > range.end.line {
            continue;
        }
        collect_hints(child, source, range, file_index, workspace, out);
    }
}

/// Emit one [`InlayHint`] per argument in `call_node` whose parameter name
/// is known.  Silently skips when the function name can't be resolved or
/// when there are no parameters.
fn hints_for_call(
    call_node: Node,
    source: &str,
    file_index: Option<&FileIndex>,
    workspace: Option<&WorkspaceIndex>,
    out: &mut Vec<InlayHint>,
) {
    // Only handle plain identifier callees (not member expressions, etc.).
    let func_node = match call_node.child_by_field_name("function") {
        Some(n) if n.kind() == "identifier" => n,
        _ => return,
    };
    let func_name = match func_node.utf8_text(source.as_bytes()) {
        Ok(s) => s,
        Err(_) => return,
    };

    let arg_list = match call_node.child_by_field_name("arguments") {
        Some(n) => n,
        None => return,
    };

    // Collect argument expression nodes — skip the `(`, `)`, and `,` tokens.
    let args: Vec<Node> = {
        let mut cur = arg_list.walk();
        arg_list
            .children(&mut cur)
            .filter(|n| !matches!(n.kind(), "(" | ")" | ","))
            .collect()
    };

    if args.is_empty() {
        return;
    }

    let param_names = lookup_param_names(func_name, file_index, workspace);
    if param_names.is_empty() {
        return;
    }

    // Emit one hint per argument, stopping at whichever list is shorter.
    for (arg_node, param_name) in args.iter().zip(param_names.iter()) {
        let pos = byte_to_position(source, arg_node.start_byte());
        out.push(InlayHint {
            position: pos,
            label: InlayHintLabel::String(format!("{param_name}:")),
            kind: Some(InlayHintKind::PARAMETER),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: Some(true), // thin space between hint and value
            data: None,
        });
    }
}

// ─── Parameter name lookup ────────────────────────────────────────────────────

/// Look up the ordered parameter names for a function.
///
/// Priority: builtin catalog → current-file UDF → workspace (included files).
fn lookup_param_names(
    func_name: &str,
    file_index: Option<&FileIndex>,
    workspace: Option<&WorkspaceIndex>,
) -> Vec<String> {
    // 1. Built-in / UDF library catalog.
    if let Some(doc) = builtins::lookup(func_name) {
        return doc.parameters.iter().map(|p| p.name.clone()).collect();
    }

    // 2. User-defined function in the current file.
    if let Some(def) = file_index.and_then(|idx| idx.resolve_def(func_name, None))
        && def.kind == DefKind::Function
        && let Some(sig) = &def.signature_line
    {
        return parse_udf_params(sig);
    }

    // 3. User-defined function in an included file.
    if let Some(ws) = workspace
        && let Some(entry) = ws.resolve_global(func_name)
        && entry.1.kind == DefKind::Function
        && let Some(sig) = &entry.1.signature_line
    {
        return parse_udf_params(sig);
    }

    vec![]
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_index;
    use crate::tree::parse;
    use tower_lsp::lsp_types::Position;

    fn all_lines() -> Range {
        Range {
            start: Position::new(0, 0),
            end: Position::new(9999, 0),
        }
    }

    fn labels(hints: &[InlayHint]) -> Vec<String> {
        hints
            .iter()
            .map(|h| match &h.label {
                InlayHintLabel::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect()
    }

    // ── Builtin catalog hints ─────────────────────────────────────────────────

    #[test]
    fn builtin_call_emits_param_hints() {
        let source = "MsgBox(0, \"title\", \"text\")\n";
        let tree = parse(source).unwrap();
        let hints = inlay_hints_for(&tree, source, all_lines(), None, None);
        // MsgBox has ≥ 3 documented parameters.
        assert!(hints.len() >= 3, "expected at least 3 hints, got {}", hints.len());
    }

    #[test]
    fn builtin_hints_are_parameter_kind() {
        let source = "ConsoleWrite(\"hi\")\n";
        let tree = parse(source).unwrap();
        let hints = inlay_hints_for(&tree, source, all_lines(), None, None);
        assert!(!hints.is_empty());
        for h in &hints {
            assert_eq!(h.kind, Some(InlayHintKind::PARAMETER));
        }
    }

    #[test]
    fn builtin_hint_has_padding_right() {
        let source = "ConsoleWrite(\"hi\")\n";
        let tree = parse(source).unwrap();
        let hints = inlay_hints_for(&tree, source, all_lines(), None, None);
        assert!(!hints.is_empty());
        assert_eq!(hints[0].padding_right, Some(true));
    }

    #[test]
    fn hint_label_ends_with_colon() {
        let source = "MsgBox(0, \"t\", \"m\")\n";
        let tree = parse(source).unwrap();
        let hints = inlay_hints_for(&tree, source, all_lines(), None, None);
        for h in &hints {
            let label = match &h.label {
                InlayHintLabel::String(s) => s.as_str(),
                _ => panic!("expected string label"),
            };
            assert!(label.ends_with(':'), "label '{label}' should end with ':'");
        }
    }

    #[test]
    fn hints_are_in_source_order() {
        let source = "MsgBox(0, \"title\", \"text\")\n";
        let tree = parse(source).unwrap();
        let hints = inlay_hints_for(&tree, source, all_lines(), None, None);
        for i in 1..hints.len() {
            let prev = hints[i - 1].position;
            let curr = hints[i].position;
            assert!(
                curr.line > prev.line
                    || (curr.line == prev.line && curr.character >= prev.character),
                "hints out of order at index {i}"
            );
        }
    }

    // ── UDF hints ─────────────────────────────────────────────────────────────

    #[test]
    fn udf_call_emits_param_hints() {
        let source = concat!(
            "Func MyAdd($x, $y)\n",
            "    Return $x + $y\n",
            "EndFunc\n",
            "\n",
            "MyAdd(1, 2)\n",
        );
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        let hints = inlay_hints_for(&tree, source, all_lines(), Some(&file_idx), None);
        let ls = labels(&hints);
        assert!(ls.iter().any(|l| l.contains("$x")), "expected $x hint");
        assert!(ls.iter().any(|l| l.contains("$y")), "expected $y hint");
    }

    #[test]
    fn udf_byref_param_hint_uses_variable_name() {
        let source = concat!(
            "Func _Fill(ByRef $aArr, $vVal)\n",
            "    Return 0\n",
            "EndFunc\n",
            "\n",
            "_Fill($x, 1)\n",
        );
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        let hints = inlay_hints_for(&tree, source, all_lines(), Some(&file_idx), None);
        let ls = labels(&hints);
        assert!(ls.iter().any(|l| l.contains("$aArr")));
        assert!(ls.iter().any(|l| l.contains("$vVal")));
    }

    // ── Edge cases ────────────────────────────────────────────────────────────

    #[test]
    fn no_args_no_hints() {
        let source = "Func Bare()\nEndFunc\n\nBare()\n";
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        let hints = inlay_hints_for(&tree, source, all_lines(), Some(&file_idx), None);
        assert!(hints.is_empty());
    }

    #[test]
    fn unknown_function_no_hints() {
        let source = "UnknownFunctionXyz(1, 2, 3)\n";
        let tree = parse(source).unwrap();
        let hints = inlay_hints_for(&tree, source, all_lines(), None, None);
        assert!(hints.is_empty());
    }

    #[test]
    fn more_args_than_params_truncates_at_param_count() {
        // MsgBox has a fixed parameter count; passing extra args shouldn't panic.
        let source = "ConsoleWrite(\"a\", \"extra1\", \"extra2\")\n";
        let tree = parse(source).unwrap();
        // ConsoleWrite has 1 parameter — hints should stop after 1.
        let hints = inlay_hints_for(&tree, source, all_lines(), None, None);
        assert_eq!(hints.len(), 1, "should stop at the number of known params");
    }

    #[test]
    fn nested_calls_each_get_hints() {
        // Both the outer and inner call should have their parameters annotated.
        let source = "MsgBox(0, StringFormat(\"%s\", \"hi\"), \"t\")\n";
        let tree = parse(source).unwrap();
        let hints = inlay_hints_for(&tree, source, all_lines(), None, None);
        // MsgBox hints + StringFormat hints (if StringFormat is in catalog).
        // At minimum MsgBox's 3 params should be there.
        assert!(hints.len() >= 3);
    }

    #[test]
    fn range_filtering_limits_to_visible_lines() {
        let source = "ConsoleWrite(\"a\")\nConsoleWrite(\"b\")\n";
        let tree = parse(source).unwrap();
        let range = Range {
            start: Position::new(0, 0),
            end: Position::new(0, 999),
        };
        let hints = inlay_hints_for(&tree, source, range, None, None);
        for h in &hints {
            assert_eq!(h.position.line, 0, "only line-0 hints expected");
        }
    }
}
