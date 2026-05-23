//! Signature-help responses for AutoIt function calls.
//!
//! When the user types `(` after a function name or `,` inside an argument
//! list, Zed sends `textDocument/signatureHelp`.  We respond with the full
//! function signature and highlight the currently-active parameter.
//!
//! ## Data sources (priority order)
//!
//! 1. AutoIt built-in / UDF library catalog (~3,542 entries with full
//!    parameter names and descriptions).
//! 2. User-defined functions in the current file (parsed from
//!    `SymbolDef::signature_line`).
//! 3. User-defined functions in included files (workspace index).
//!
//! ## Active parameter detection
//!
//! Walk the text of the `argument_list` node from its opening `(` to the
//! cursor byte, tracking nesting depth.  The number of commas encountered
//! at depth 1 (the outermost argument separator level) equals the 0-based
//! index of the active parameter.
//!
//! ```text
//! MsgBox( 0 ,  "title" ,  "text" )
//!        ^0^   ^  1  ^    ^ 2 ^
//! ```

use tower_lsp::lsp_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, Position,
    SignatureHelp, SignatureInformation,
};
use tree_sitter::{Node, Tree};

use crate::builtins;
use crate::includes::WorkspaceIndex;
use crate::index::{DefKind, FileIndex};
use crate::tree::{node_at_position, position_to_byte};

// ─── Public entry point ───────────────────────────────────────────────────────

/// Compute signature help for the call expression enclosing `position`.
///
/// Returns `None` when:
/// - the cursor is not inside a function argument list,
/// - the function name is not an identifier (e.g. a computed expression),
/// - the function is not found in any known source.
pub fn signature_help_for(
    tree: &Tree,
    source: &str,
    position: Position,
    file_index: Option<&FileIndex>,
    workspace: Option<&WorkspaceIndex>,
) -> Option<SignatureHelp> {
    let cursor_byte = position_to_byte(source, position)?;
    let cursor_node = node_at_position(tree, source, position)?;

    // Walk up to the nearest call_expression ancestor.
    let call_node = find_call_ancestor(cursor_node)?;

    // Get the argument_list child via the grammar field name.
    let arg_list = call_node.child_by_field_name("arguments")?;

    // Only provide help when the cursor is inside the argument list
    // (after the opening `(` and before or at the closing `)`).
    if cursor_byte <= arg_list.start_byte() || cursor_byte > arg_list.end_byte() {
        return None;
    }

    // Require the callee to be a plain identifier (not a computed expression).
    let func_node = call_node.child_by_field_name("function")?;
    if func_node.kind() != "identifier" {
        return None;
    }
    let func_name = func_node.utf8_text(source.as_bytes()).ok()?;

    // Count top-level commas before the cursor → active parameter index.
    let active_param = count_active_param(source, &arg_list, cursor_byte);

    // ── 1. Built-in / UDF catalog ────────────────────────────────────────────
    if let Some(doc) = builtins::lookup(func_name) {
        return Some(builtin_signature_help(doc, active_param));
    }

    // ── 2. User-defined function in the current file ─────────────────────────
    if let Some(def) = file_index.and_then(|idx| idx.resolve_def(func_name, None)) {
        if def.kind == DefKind::Function {
            if let Some(sig) = &def.signature_line {
                return Some(udf_signature_help(sig, active_param, def.doc_comment.as_deref()));
            }
        }
    }

    // ── 3. User-defined function in an included file ─────────────────────────
    if let Some(ws) = workspace {
        if let Some(entry) = ws.resolve_global(func_name) {
            if entry.1.kind == DefKind::Function {
                if let Some(sig) = &entry.1.signature_line {
                    return Some(udf_signature_help(sig, active_param, entry.1.doc_comment.as_deref()));
                }
            }
        }
    }

    None
}

// ─── Tree helpers ─────────────────────────────────────────────────────────────

/// Walk up from `node` to find the innermost enclosing `call_expression`.
/// Returns `None` when there is no such ancestor (cursor is outside all calls).
fn find_call_ancestor(mut node: Node) -> Option<Node> {
    loop {
        if node.kind() == "call_expression" {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// Count top-level argument-separator commas in the argument list text
/// from the opening `(` up to (but not including) `cursor_byte`.
///
/// "Top-level" means commas at nesting depth 1 (inside the outermost `(`
/// but not inside any nested `(…)` or `[…]`).  The count equals the
/// 0-based index of the parameter the cursor is positioned inside.
fn count_active_param(source: &str, arg_list: &Node, cursor_byte: usize) -> usize {
    let start = arg_list.start_byte();
    // Clamp to the argument list's end so an out-of-range cursor still works.
    let end = cursor_byte.min(arg_list.end_byte());

    let slice = match source.get(start..end) {
        Some(s) => s,
        None => return 0,
    };

    let mut depth: i32 = 0;
    let mut commas: usize = 0;

    for ch in slice.chars() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                depth -= 1;
                if depth < 0 {
                    break; // past the end of the argument list
                }
            }
            ',' if depth == 1 => commas += 1,
            _ => {}
        }
    }

    commas
}

// ─── Signature builders ───────────────────────────────────────────────────────

/// Build `SignatureHelp` from a builtin catalog entry.
///
/// Uses the scraped `signature` string as the label and the catalog's
/// `parameters` list for per-parameter highlight labels and documentation.
fn builtin_signature_help(doc: &builtins::FunctionDoc, active_param: usize) -> SignatureHelp {
    let label = doc.signature.clone().unwrap_or_else(|| doc.name.clone());

    let params: Vec<ParameterInformation> = doc
        .parameters
        .iter()
        .map(|p| ParameterInformation {
            label: ParameterLabel::Simple(p.name.clone()),
            documentation: if p.description.is_empty() {
                None
            } else {
                Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::PlainText,
                    value: p.description.clone(),
                }))
            },
        })
        .collect();

    // Clamp to the last defined parameter so we never send an out-of-bounds
    // index even when the user passes more arguments than the function declares.
    let clamped_param = if params.is_empty() {
        0
    } else {
        (active_param).min(params.len() - 1) as u32
    };

    SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: doc.summary.as_ref().map(|s| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: s.clone(),
                })
            }),
            parameters: Some(params),
            active_parameter: None,
        }],
        active_signature: Some(0),
        active_parameter: Some(clamped_param),
    }
}

/// Build `SignatureHelp` from a user-defined function's signature line.
///
/// The signature line looks like `Func MyHelper($a, $b = 0)`.
/// We strip the leading `Func ` keyword for the display label and parse
/// the parameter names for per-parameter highlighting.
/// `doc_comment` is the rendered Markdown from the function's preceding
/// `;`-comment block (extracted by `doccomment::extract_doc_comment`).
fn udf_signature_help(
    signature_line: &str,
    active_param: usize,
    doc_comment: Option<&str>,
) -> SignatureHelp {
    let label = strip_func_keyword(signature_line);
    let param_names = parse_udf_params(signature_line);

    let params: Vec<ParameterInformation> = param_names
        .iter()
        .map(|name| ParameterInformation {
            label: ParameterLabel::Simple(name.clone()),
            documentation: None,
        })
        .collect();

    let clamped_param = if params.is_empty() {
        0
    } else {
        (active_param).min(params.len() - 1) as u32
    };

    SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: doc_comment.map(|s| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: s.to_string(),
                })
            }),
            parameters: Some(params),
            active_parameter: None,
        }],
        active_signature: Some(0),
        active_parameter: Some(clamped_param),
    }
}

/// Strip the leading `Func ` keyword (case-insensitive) from a signature line.
///
/// `"Func MyAdd($a, $b)"` → `"MyAdd($a, $b)"`.
/// Lines that don't start with `Func ` are returned as-is.
fn strip_func_keyword(signature_line: &str) -> String {
    let t = signature_line.trim();
    // "Func " is 5 chars; AutoIt is case-insensitive.
    if t.len() >= 5 && t[..5].eq_ignore_ascii_case("Func ") {
        t[5..].to_string()
    } else {
        t.to_string()
    }
}

/// Extract parameter names from a `Func Name($a, ByRef $b, $c = 0)` line.
/// Exported so `hints.rs` can reuse the same parsing logic.
///
/// Handles:
/// - Plain `$param`
/// - `ByRef $param` / `Const $param` (keyword before sigil)
/// - `$param = default` (default value; name ends at `=`)
/// - Empty parameter list → `[]`
pub(crate) fn parse_udf_params(signature_line: &str) -> Vec<String> {
    let open = match signature_line.find('(') {
        Some(i) => i,
        None => return vec![],
    };
    let close = match signature_line.rfind(')') {
        Some(i) => i,
        None => return vec![],
    };
    if open >= close {
        return vec![];
    }

    let params_str = &signature_line[open + 1..close];

    params_str
        .split(',')
        .filter_map(|part| {
            let t = part.trim();
            if t.is_empty() {
                return None;
            }
            // Find the `$` sigil; take the identifier up to whitespace or `=`.
            if let Some(dollar_pos) = t.find('$') {
                let after_dollar = &t[dollar_pos..];
                let name_len = after_dollar
                    .find(|c: char| c.is_whitespace() || c == '=')
                    .unwrap_or(after_dollar.len());
                Some(after_dollar[..name_len].to_string())
            } else {
                // No `$` — return trimmed part as-is (e.g. a bare keyword).
                Some(t.to_string())
            }
        })
        .collect()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_index;
    use crate::tree::parse;

    // ── parse_udf_params ──────────────────────────────────────────────────────

    #[test]
    fn parse_udf_params_simple() {
        let params = parse_udf_params("Func Add($a, $b)");
        assert_eq!(params, vec!["$a", "$b"]);
    }

    #[test]
    fn parse_udf_params_with_defaults() {
        let params = parse_udf_params("Func Foo($x, $y = 0, $z = \"default\")");
        assert_eq!(params, vec!["$x", "$y", "$z"]);
    }

    #[test]
    fn parse_udf_params_with_byref() {
        let params = parse_udf_params("Func _ArrayAdd(ByRef $aArray, $vValue)");
        assert_eq!(params, vec!["$aArray", "$vValue"]);
    }

    #[test]
    fn parse_udf_params_no_params() {
        let params = parse_udf_params("Func Bare()");
        assert!(params.is_empty());
    }

    // ── strip_func_keyword ────────────────────────────────────────────────────

    #[test]
    fn strip_func_keyword_removes_prefix() {
        assert_eq!(strip_func_keyword("Func MyAdd($x, $y)"), "MyAdd($x, $y)");
    }

    #[test]
    fn strip_func_keyword_case_insensitive() {
        assert_eq!(strip_func_keyword("func foo()"), "foo()");
        assert_eq!(strip_func_keyword("FUNC Bar($a)"), "Bar($a)");
    }

    #[test]
    fn strip_func_keyword_no_prefix_unchanged() {
        assert_eq!(strip_func_keyword("NoPrefix()"), "NoPrefix()");
    }

    // ── active parameter detection ────────────────────────────────────────────

    #[test]
    fn active_param_first_arg() {
        // Cursor on `0` (first arg): `MsgBox(|0, "title", "text")`
        let source = "MsgBox(0, \"title\", \"text\")\n";
        let tree = parse(source).unwrap();
        let pos = Position::new(0, 7); // byte 7 = '0'
        let result = signature_help_for(&tree, source, pos, None, None);
        let help = result.expect("should return signature help");
        assert_eq!(help.active_parameter, Some(0));
    }

    #[test]
    fn active_param_second_arg() {
        // Cursor on `"title"` (second arg): `MsgBox(0, |"title", "text")`
        let source = "MsgBox(0, \"title\", \"text\")\n";
        let tree = parse(source).unwrap();
        let pos = Position::new(0, 11); // inside "title"
        let result = signature_help_for(&tree, source, pos, None, None);
        let help = result.expect("should return signature help");
        assert_eq!(help.active_parameter, Some(1));
    }

    #[test]
    fn active_param_third_arg() {
        // Cursor on `"text"` (third arg): `MsgBox(0, "title", |"text")`
        let source = "MsgBox(0, \"title\", \"text\")\n";
        let tree = parse(source).unwrap();
        let pos = Position::new(0, 20); // inside "text"
        let result = signature_help_for(&tree, source, pos, None, None);
        let help = result.expect("should return signature help");
        assert_eq!(help.active_parameter, Some(2));
    }

    #[test]
    fn signature_label_contains_function_name() {
        let source = "ConsoleWrite(\"hi\")\n";
        let tree = parse(source).unwrap();
        let pos = Position::new(0, 14); // inside "hi"
        let result = signature_help_for(&tree, source, pos, None, None);
        let help = result.expect("ConsoleWrite should resolve");
        assert!(!help.signatures.is_empty());
        assert!(help.signatures[0].label.contains("ConsoleWrite"));
    }

    #[test]
    fn outside_call_returns_none() {
        // Cursor is on the function name identifier, not inside the argument list.
        let source = "MsgBox(0, \"hi\", \"there\")\n";
        let tree = parse(source).unwrap();
        let pos = Position::new(0, 2); // 'g' of "MsgBox"
        assert!(signature_help_for(&tree, source, pos, None, None).is_none());
    }

    #[test]
    fn nested_call_resolves_inner() {
        // Cursor is inside the inner call `String(x)` — should resolve String,
        // not the outer MsgBox.
        let source = "MsgBox(0, String(42), \"t\")\n";
        let tree = parse(source).unwrap();
        let pos = Position::new(0, 17); // inside `42`
        let result = signature_help_for(&tree, source, pos, None, None);
        // `String` may or may not be in the catalog (it isn't a standard AutoIt
        // builtin), but the important thing is we don't return MsgBox's help.
        // We accept either None (not found) or a result that isn't MsgBox.
        if let Some(help) = result {
            let label = &help.signatures[0].label;
            assert!(
                !label.to_lowercase().starts_with("msgbox"),
                "nested call should not resolve to outer function"
            );
        }
    }

    // ── UDF signature help ────────────────────────────────────────────────────

    #[test]
    fn udf_signature_help_first_param() {
        let source = concat!(
            "Func MyAdd($x, $y)\n",
            "    Return $x + $y\n",
            "EndFunc\n",
            "\n",
            "MyAdd(1, 2)\n",
        );
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        // Cursor at (4, 7) — inside `1` (first arg)
        let pos = Position::new(4, 7);
        let result = signature_help_for(&tree, source, pos, Some(&file_idx), None);
        let help = result.expect("UDF signature help should fire");
        assert_eq!(help.active_parameter, Some(0));
        let sig = &help.signatures[0];
        assert!(sig.label.contains("MyAdd"), "label should contain function name");
        let param_count = sig.parameters.as_ref().map(|p| p.len()).unwrap_or(0);
        assert_eq!(param_count, 2, "should have 2 parameters");
    }

    #[test]
    fn udf_signature_help_second_param() {
        let source = concat!(
            "Func MyAdd($x, $y)\n",
            "    Return $x + $y\n",
            "EndFunc\n",
            "\n",
            "MyAdd(1, 2)\n",
        );
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        // Cursor at (4, 10) — inside `2` (second arg):
        // "MyAdd(1, 2)" → M=0,y=1,A=2,d=3,d=4,(=5,1=6,','=7,' '=8,2=9
        let pos = Position::new(4, 9);
        let result = signature_help_for(&tree, source, pos, Some(&file_idx), None);
        let help = result.expect("UDF signature help should fire");
        assert_eq!(help.active_parameter, Some(1));
    }

    #[test]
    fn udf_with_byref_shows_param_name() {
        let source = concat!(
            "Func _MyFunc(ByRef $aArr, $vVal)\n",
            "    Return 0\n",
            "EndFunc\n",
            "\n",
            "_MyFunc($x, 1)\n",
        );
        let tree = parse(source).unwrap();
        let file_idx = build_index(&tree, source);
        // Cursor on `$x` (first arg)
        let pos = Position::new(4, 9);
        let result = signature_help_for(&tree, source, pos, Some(&file_idx), None);
        let help = result.expect("should show UDF signature");
        let params = help.signatures[0].parameters.as_ref().unwrap();
        assert_eq!(params.len(), 2);
        // First param should be $aArr (ByRef stripped)
        if let ParameterLabel::Simple(name) = &params[0].label {
            assert_eq!(name, "$aArr");
        } else {
            panic!("expected Simple label");
        }
    }

    #[test]
    fn builtin_has_parameters_populated() {
        let source = "MsgBox(0, \"t\", \"m\")\n";
        let tree = parse(source).unwrap();
        let pos = Position::new(0, 8); // inside `0`
        let help = signature_help_for(&tree, source, pos, None, None)
            .expect("MsgBox should resolve");
        let params = help.signatures[0]
            .parameters
            .as_ref()
            .expect("parameters should be present");
        assert!(!params.is_empty(), "MsgBox should have at least one parameter");
    }
}
