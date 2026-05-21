//! Document symbols — feeds Zed's outline panel.
//!
//! Walks a tree-sitter parse tree and emits LSP [`DocumentSymbol`]s for:
//! - **Functions** with their parameters as children, signature in `detail`.
//! - **Top-level declarations** (`Global`, `Local`-at-file-scope, `Dim`, `Const`)
//!   emitted as `Variable` (or `Constant` when `Const` is one of the modifiers).
//! - **Enum members** from top-level `Enum` declarations.
//! - **Regions** (`#Region`/`#EndRegion` blocks) — emitted as `Module` with
//!   nested symbols inside. Orphan `#Region` / `#EndRegion` (no matching pair)
//!   are skipped because the grammar only exposes `region_block` for the
//!   paired form.
//!
//! Why an indexed walker rather than tree-sitter Queries: queries are great
//! for capturing flat sets of nodes (highlights, brackets) but awkward when
//! we need to walk into structures (regions can nest other symbols). A plain
//! Rust walker handles arbitrary tree shapes with one match-on-kind dispatch
//! and is easy to extend for Sprint 2+ (symbol index, find-references).
//!
//! Nodes inside function bodies (Local/Static declarations, nested constructs)
//! are intentionally NOT surfaced at this point — Sprint 1 keeps the outline
//! to the file's top-level shape. The symbol index in Sprint 2 will index
//! these for go-to-def/find-refs without changing the outline.

use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};
use tree_sitter::{Node, Tree};

use crate::tree::node_range;

/// Build the document-symbol response for an entire parse tree.
pub fn document_symbols(tree: &Tree, source: &str) -> Vec<DocumentSymbol> {
    collect_top_level(tree.root_node(), source)
}

/// Walk direct children of `node`, picking up every node that maps to a
/// top-level outline entry. Used both at the source-file root and inside
/// region_block bodies (regions can contain functions, declarations, and
/// nested regions, all of which should appear under the region).
fn collect_top_level(node: Node, source: &str) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(s) = function_symbol(child, source) {
                    out.push(s);
                }
            }
            "declaration_statement" => {
                out.extend(declaration_symbols(child, source));
            }
            "enum_declaration" => {
                out.extend(enum_symbols(child, source));
            }
            "region_block" => {
                if let Some(s) = region_symbol(child, source) {
                    out.push(s);
                }
            }
            _ => {}
        }
    }
    out
}

/// `Func Name(args...) ... EndFunc` → Function symbol with parameter
/// children. `detail` shows the parenthesized parameter list so the outline
/// reads like `Hello (x, y)` even with just the name in the main label.
fn function_symbol(node: Node, source: &str) -> Option<DocumentSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(source.as_bytes()).ok()?.to_string();

    let mut param_children = Vec::new();
    let mut param_names = Vec::new();

    if let Some(plist) = node.child_by_field_name("parameters") {
        let mut cursor = plist.walk();
        for p in plist.children(&mut cursor) {
            if p.kind() != "parameter" {
                continue;
            }
            let Some(pname_node) = p.child_by_field_name("name") else {
                continue;
            };
            let pname = pname_node
                .utf8_text(source.as_bytes())
                .unwrap_or("")
                .to_string();
            param_names.push(pname.clone());
            param_children.push(make_symbol(
                pname,
                None,
                SymbolKind::VARIABLE,
                node_range(&p, source),
                node_range(&pname_node, source),
                None,
            ));
        }
    }

    let detail = Some(format!("({})", param_names.join(", ")));

    Some(make_symbol(
        name,
        detail,
        SymbolKind::FUNCTION,
        node_range(&node, source),
        node_range(&name_node, source),
        (!param_children.is_empty()).then_some(param_children),
    ))
}

/// `Global $a, $b = 5, $c` → one symbol per declarator. If `Const` is
/// among the modifiers, kind is `Constant`; otherwise `Variable`.
fn declaration_symbols(node: Node, source: &str) -> Vec<DocumentSymbol> {
    let is_const = has_keyword_child(node, "keyword_const");
    let kind = if is_const {
        SymbolKind::CONSTANT
    } else {
        SymbolKind::VARIABLE
    };

    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let name = name_node
            .utf8_text(source.as_bytes())
            .unwrap_or("")
            .to_string();
        out.push(make_symbol(
            name,
            None,
            kind,
            node_range(&child, source),
            node_range(&name_node, source),
            None,
        ));
    }
    out
}

/// `Global Enum $RANK_BRONZE, $RANK_SILVER` → one EnumMember per declarator.
/// AutoIt has no `enum_type` declaration as a parent — enums are sugar for
/// a sequential set of constants — so we surface the members directly at
/// the same level as other top-level symbols.
fn enum_symbols(node: Node, source: &str) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let name = name_node
            .utf8_text(source.as_bytes())
            .unwrap_or("")
            .to_string();
        out.push(make_symbol(
            name,
            None,
            SymbolKind::ENUM_MEMBER,
            node_range(&child, source),
            node_range(&name_node, source),
            None,
        ));
    }
    out
}

/// `#Region NAME ... #EndRegion NAME` → Module symbol containing every
/// top-level child found inside the region. Recurses via `collect_top_level`
/// so a region inside a region inside a region all collapse correctly.
fn region_symbol(node: Node, source: &str) -> Option<DocumentSymbol> {
    // `name` field is `directive_args`; raw bytes include any whitespace
    // between `#Region` and the end of line. Trim for display.
    let name = node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(region)".into());

    let selection_node = node.child_by_field_name("name").unwrap_or(node);
    let children = collect_top_level(node, source);

    Some(make_symbol(
        name,
        None,
        SymbolKind::MODULE,
        node_range(&node, source),
        node_range(&selection_node, source),
        (!children.is_empty()).then_some(children),
    ))
}

/// True if any direct child of `node` has the given kind. Used for
/// modifier checks like `keyword_const`.
fn has_keyword_child(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| c.kind() == kind)
}

/// DocumentSymbol literal helper. Centralises the `#[allow(deprecated)]`
/// dance for the deprecated `deprecated` field (use `tags` instead, per
/// LSP 3.15+; we leave both at None).
#[allow(deprecated)]
fn make_symbol(
    name: String,
    detail: Option<String>,
    kind: SymbolKind,
    range: tower_lsp::lsp_types::Range,
    selection_range: tower_lsp::lsp_types::Range,
    children: Option<Vec<DocumentSymbol>>,
) -> DocumentSymbol {
    DocumentSymbol {
        name,
        detail,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::parse;

    fn symbols_for(source: &str) -> Vec<DocumentSymbol> {
        let tree = parse(source).expect("parse");
        document_symbols(&tree, source)
    }

    #[test]
    fn empty_source_yields_no_symbols() {
        assert!(symbols_for("").is_empty());
    }

    #[test]
    fn single_function_with_no_params() {
        let syms = symbols_for("Func Hello()\nEndFunc\n");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Hello");
        assert_eq!(syms[0].kind, SymbolKind::FUNCTION);
        assert_eq!(syms[0].detail.as_deref(), Some("()"));
        assert!(syms[0].children.is_none());
    }

    #[test]
    fn function_with_params_yields_param_children() {
        let syms = symbols_for("Func Add($a, $b)\nReturn $a + $b\nEndFunc\n");
        assert_eq!(syms.len(), 1);
        let f = &syms[0];
        assert_eq!(f.name, "Add");
        assert_eq!(f.detail.as_deref(), Some("($a, $b)"));
        let kids = f.children.as_ref().expect("params as children");
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].name, "$a");
        assert_eq!(kids[0].kind, SymbolKind::VARIABLE);
        assert_eq!(kids[1].name, "$b");
    }

    #[test]
    fn top_level_global_variable() {
        let syms = symbols_for("Global $foo = 5\n");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "$foo");
        assert_eq!(syms[0].kind, SymbolKind::VARIABLE);
    }

    #[test]
    fn top_level_const_emits_constant_kind() {
        let syms = symbols_for("Global Const $MAX = 100\n");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "$MAX");
        assert_eq!(syms[0].kind, SymbolKind::CONSTANT);
    }

    #[test]
    fn multi_var_declaration_emits_one_symbol_per_declarator() {
        let syms = symbols_for("Global $a = 1, $b = 2, $c = 3\n");
        assert_eq!(syms.len(), 3);
        assert_eq!(syms[0].name, "$a");
        assert_eq!(syms[1].name, "$b");
        assert_eq!(syms[2].name, "$c");
    }

    #[test]
    fn enum_emits_enum_member_per_declarator() {
        let syms = symbols_for("Global Enum $RANK_BRONZE, $RANK_SILVER, $RANK_GOLD\n");
        assert_eq!(syms.len(), 3);
        assert!(syms.iter().all(|s| s.kind == SymbolKind::ENUM_MEMBER));
        assert_eq!(syms[0].name, "$RANK_BRONZE");
        assert_eq!(syms[2].name, "$RANK_GOLD");
    }

    #[test]
    fn region_emits_module_with_nested_symbols() {
        let source = "#Region constants\nGlobal $a = 1\nGlobal Const $B = 2\n#EndRegion constants\n";
        let syms = symbols_for(source);
        assert_eq!(syms.len(), 1);
        let region = &syms[0];
        assert_eq!(region.kind, SymbolKind::MODULE);
        assert_eq!(region.name, "constants");
        let kids = region.children.as_ref().expect("region has children");
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].name, "$a");
        assert_eq!(kids[0].kind, SymbolKind::VARIABLE);
        assert_eq!(kids[1].name, "$B");
        assert_eq!(kids[1].kind, SymbolKind::CONSTANT);
    }

    #[test]
    fn region_without_name_still_emits() {
        let source = "#Region\nGlobal $a = 1\n#EndRegion\n";
        let syms = symbols_for(source);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "(region)");
    }

    #[test]
    fn mixed_file_outline() {
        let source = "\
#Region constants
Global Const $MAX = 100
#EndRegion constants

Global $oDict

Func BuildScoreboard()
    Local $i
    Return $i
EndFunc

Func Main()
    BuildScoreboard()
EndFunc
";
        let syms = symbols_for(source);
        // Expected top-level entries: region, $oDict, BuildScoreboard, Main.
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["constants", "$oDict", "BuildScoreboard", "Main"]);
        // Region has its constant child.
        let const_kids = syms[0].children.as_ref().expect("region kids");
        assert_eq!(const_kids[0].name, "$MAX");
        assert_eq!(const_kids[0].kind, SymbolKind::CONSTANT);
        // Functions have no params.
        assert_eq!(syms[2].detail.as_deref(), Some("()"));
    }

    #[test]
    fn function_inside_region_appears_as_region_child() {
        let source = "#Region helpers\nFunc DoIt()\nEndFunc\n#EndRegion\n";
        let syms = symbols_for(source);
        assert_eq!(syms.len(), 1);
        let region = &syms[0];
        assert_eq!(region.kind, SymbolKind::MODULE);
        let kids = region.children.as_ref().expect("kids");
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].name, "DoIt");
        assert_eq!(kids[0].kind, SymbolKind::FUNCTION);
    }
}
