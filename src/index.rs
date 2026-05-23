//! Per-document symbol index for go-to-definition and find-references.
//!
//! Sprint 2 infrastructure. Walks the tree-sitter parse tree once and
//! builds two maps:
//!   - `defs`:  lowercase name → all declaration sites (function defs,
//!              variable/const/enum declarations, parameters).
//!   - `refs`:  lowercase name → all usage sites (call expressions,
//!              variable reads/writes, assignments).
//!
//! Case-insensitive throughout: AutoIt is case-insensitive, so "Foo",
//! "FOO", and "foo" all refer to the same symbol. Index keys are always
//! lowercase.
//!
//! Scope model (AutoIt rules):
//!   - Functions are file-global (`scope_func = None`).
//!   - `Global` / `Dim` declarations at file level are file-global.
//!   - Parameters are scoped to their function (`scope_func = Some("funcname")`).
//!   - `Local` / `Static` declarations inside a function body are scoped
//!     to that function.
//!   - AutoIt has no nested functions, so scope depth is at most one level.

use std::collections::HashMap;

use tower_lsp::lsp_types::Range;
use tree_sitter::{Node, Tree};

use crate::tree::node_range;

// ─── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    Function,
    Variable,
    Constant,
    Parameter,
    EnumMember,
}

/// One declaration site for a named symbol.
#[derive(Debug, Clone)]
pub struct SymbolDef {
    /// Original-casing name (e.g. `"$MyVar"`, `"BuildScoreboard"`).
    pub display_name: String,
    pub kind: DefKind,
    /// Range of the whole declaration node (entire `Func … EndFunc`,
    /// or the individual `$x = 5` declarator).
    pub full_range: Range,
    /// Range of just the name token — used as the jump destination.
    pub name_range: Range,
    /// `None` = file-global scope.
    /// `Some("funcname")` = local to that function (lowercase key).
    pub scope_func: Option<String>,
    /// For `Function` defs only: the trimmed `Func Name(params...)` declaration
    /// line from the source text. Populated by `collect_function_decl` so that
    /// hover can show the signature without needing the original source buffer.
    /// `None` for all non-function def kinds.
    pub signature_line: Option<String>,
}

/// One usage (reference) of a named symbol.
#[derive(Debug, Clone)]
pub struct SymbolRef {
    pub usage_range: Range,
    /// Same semantics as `SymbolDef::scope_func`.
    pub scope_func: Option<String>,
}

/// Per-document symbol index built by [`build_index`].
#[derive(Debug, Default)]
pub struct FileIndex {
    pub defs: HashMap<String, Vec<SymbolDef>>,
    pub refs: HashMap<String, Vec<SymbolRef>>,
}

impl FileIndex {
    /// Scope-aware definition lookup.
    ///
    /// Priority: param / local in the same function  >  file-global.
    /// Returns the most-local matching def visible from `cursor_scope`.
    pub fn resolve_def(&self, name: &str, cursor_scope: Option<&str>) -> Option<&SymbolDef> {
        let key = name.to_lowercase();
        let defs = self.defs.get(&key)?;

        // Prefer a def scoped to the same function (params + locals).
        if let Some(func) = cursor_scope {
            if let Some(d) = defs.iter().find(|d| d.scope_func.as_deref() == Some(func)) {
                return Some(d);
            }
        }

        // Fall back to file-global.
        defs.iter().find(|d| d.scope_func.is_none())
    }

    /// All references to `name` that are visible given the symbol's own scope.
    ///
    /// - Global symbol (`def_scope = None`): all refs in the file.
    /// - Function-local symbol: only refs inside that same function.
    pub fn find_refs<'a>(&'a self, name: &str, def_scope: Option<&str>) -> Vec<&'a SymbolRef> {
        let key = name.to_lowercase();
        let Some(refs) = self.refs.get(&key) else {
            return vec![];
        };
        match def_scope {
            None => refs.iter().collect(),
            Some(func) => refs
                .iter()
                .filter(|r| r.scope_func.as_deref() == Some(func))
                .collect(),
        }
    }
}

// ─── Public builder ───────────────────────────────────────────────────────────

/// Build a [`FileIndex`] from `tree` + `source` by walking the full parse tree.
///
/// Costs one full-tree traversal per call — cheap on typical AutoIt files
/// (microseconds). Called alongside the tree reparse on every `did_open` /
/// `did_change`.
pub fn build_index(tree: &Tree, source: &str) -> FileIndex {
    let mut index = FileIndex::default();
    collect(tree.root_node(), source, None, &mut index);
    index
}

/// Find the lowercase name of the function that encloses `node`, or `None`
/// if the node is at file scope. Used by the definition/references handlers
/// to determine the caller's scope before looking up the index.
///
/// **Limitation:** if tree-sitter's error recovery hoists the node out of
/// its containing `function_declaration` (common when the buffer has a
/// mid-edit parse error, e.g. a bare `$`), this returns `None`. For
/// completion use [`scope_at_line`] which is immune to that.
pub fn cursor_scope(mut node: Node, source: &str) -> Option<String> {
    while let Some(parent) = node.parent() {
        node = parent;
        if node.kind() == "function_declaration" {
            let name_node = node.child_by_field_name("name")?;
            return Some(
                name_node
                    .utf8_text(source.as_bytes())
                    .ok()?
                    .to_lowercase(),
            );
        }
    }
    None
}

/// Determine the containing function scope for a given line number by
/// comparing against the stored function declaration ranges in `index`.
///
/// Unlike [`cursor_scope`] this does **not** touch the live parse tree, so
/// it is unaffected by tree-sitter error recovery during mid-edit states
/// (e.g. a bare `$` or `@` that hasn't been completed yet). Used as the
/// primary scope source for completion.
pub fn scope_at_line(index: &FileIndex, line: u32) -> Option<String> {
    for defs in index.defs.values() {
        for def in defs {
            if def.kind == DefKind::Function
                && line >= def.full_range.start.line
                && line <= def.full_range.end.line
            {
                return Some(def.display_name.to_lowercase());
            }
        }
    }
    None
}

// ─── Internal walker ──────────────────────────────────────────────────────────

/// Recursively walk `node`, classifying every symbol encounter as either
/// a definition (→ `index.defs`) or a reference (→ `index.refs`).
///
/// `scope_func` is the lowercase name of the containing function, or `None`
/// for file-level code.
fn collect(node: Node, source: &str, scope_func: Option<&str>, index: &mut FileIndex) {
    match node.kind() {
        "function_declaration" => {
            // collect_function_decl handles name + params + body recursion.
            collect_function_decl(node, source, index);
            return;
        }
        "declaration_statement" => {
            collect_decl_stmt(node, source, scope_func, index);
            return;
        }
        "enum_declaration" => {
            collect_enum_decl(node, source, scope_func, index);
            return;
        }
        "variable" => {
            // Variable in usage position. Declaration walkers above consume
            // the `variable` nodes that ARE declarations and return without
            // reaching this arm.
            add_ref(node, source, scope_func, index);
            return;
        }
        "call_expression" => {
            // Record the called identifier as a function reference, then
            // recurse into the argument list.
            if let Some(fn_node) = node.child_by_field_name("function") {
                if fn_node.kind() == "identifier" {
                    let name = fn_node
                        .utf8_text(source.as_bytes())
                        .unwrap_or("")
                        .to_lowercase();
                    if !name.is_empty() {
                        index.refs.entry(name).or_default().push(SymbolRef {
                            usage_range: node_range(&fn_node, source),
                            scope_func: scope_func.map(String::from),
                        });
                    }
                }
            }
            // Recurse into everything except the `identifier` function-name
            // child (already handled above).
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() != "identifier" {
                    collect(child, source, scope_func, index);
                }
            }
            return;
        }
        _ => {}
    }

    // Default: recurse into all children.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, scope_func, index);
    }
}

/// Handle `Func Name(params...) body... EndFunc`.
///
/// - Records the function name as a file-global `Function` def.
/// - Records each parameter as a `Parameter` def scoped to this function.
/// - Recurses into the body with the function's scope.
fn collect_function_decl(node: Node, source: &str, index: &mut FileIndex) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = match name_node.utf8_text(source.as_bytes()) {
        Ok(s) => s.to_string(),
        Err(_) => return,
    };
    let key = name.to_lowercase();

    // Extract the `Func Name(...)` declaration line for use in hover popups.
    // Trimmed to remove leading indentation (nested Funcs are unusual but legal).
    let sig_line = source
        .lines()
        .nth(node.start_position().row)
        .map(|l| l.trim().to_string());

    // Function definitions are file-global (scope_func = None).
    index.defs.entry(key.clone()).or_default().push(SymbolDef {
        display_name: name.clone(),
        kind: DefKind::Function,
        full_range: node_range(&node, source),
        name_range: node_range(&name_node, source),
        scope_func: None,
        signature_line: sig_line,
    });

    // Parameters — scoped to this function.
    if let Some(plist) = node.child_by_field_name("parameters") {
        let mut cursor = plist.walk();
        for param in plist.children(&mut cursor) {
            if param.kind() != "parameter" {
                continue;
            }
            if let Some(pname_node) = param.child_by_field_name("name") {
                let pname = pname_node
                    .utf8_text(source.as_bytes())
                    .unwrap_or("")
                    .to_string();
                if !pname.is_empty() {
                    let pkey = pname.to_lowercase();
                    index.defs.entry(pkey).or_default().push(SymbolDef {
                        display_name: pname,
                        kind: DefKind::Parameter,
                        full_range: node_range(&param, source),
                        name_range: node_range(&pname_node, source),
                        scope_func: Some(key.clone()),
                        signature_line: None,
                    });
                }
            }
            // Collect refs from the parameter default value, if any.
            if let Some(default_node) = param.child_by_field_name("default") {
                collect(default_node, source, Some(&key), index);
            }
        }
    }

    // Walk the function body — everything except the name, parameter_list,
    // and Func/EndFunc keywords (those are already handled or irrelevant).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" | "parameter_list" | "keyword_func" | "keyword_endfunc" => continue,
            _ => collect(child, source, Some(&key), index),
        }
    }
}

/// Handle `[Global|Local|Dim|Static] [Const] $var [= expr], ...`.
///
/// Adds each declarator's name to `defs` with the current scope, then
/// recurses into the value expression to capture any variable references.
fn collect_decl_stmt(node: Node, source: &str, scope_func: Option<&str>, index: &mut FileIndex) {
    let is_const = has_keyword_child(node, "keyword_const");
    let kind = if is_const {
        DefKind::Constant
    } else {
        DefKind::Variable
    };

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
        if name.is_empty() {
            continue;
        }
        let key = name.to_lowercase();

        index.defs.entry(key).or_default().push(SymbolDef {
            display_name: name,
            kind,
            full_range: node_range(&child, source),
            name_range: node_range(&name_node, source),
            scope_func: scope_func.map(String::from),
            signature_line: None,
        });

        // Recurse into the value expression (RHS), skipping the name node
        // itself (identified by start byte — unique within this declarator).
        let name_start = name_node.start_byte();
        let mut inner = child.walk();
        for inner_child in child.children(&mut inner) {
            if inner_child.start_byte() != name_start {
                collect(inner_child, source, scope_func, index);
            }
        }
    }
}

/// Handle `[Global|Local] Enum [$x, $y, ...]` — adds each member as
/// `EnumMember` at the current scope.
fn collect_enum_decl(node: Node, source: &str, scope_func: Option<&str>, index: &mut FileIndex) {
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
        if name.is_empty() {
            continue;
        }
        let key = name.to_lowercase();
        index.defs.entry(key).or_default().push(SymbolDef {
            display_name: name,
            kind: DefKind::EnumMember,
            full_range: node_range(&child, source),
            name_range: node_range(&name_node, source),
            scope_func: scope_func.map(String::from),
            signature_line: None,
        });
    }
}

/// Push a reference entry for `node` (a `variable` in usage position).
fn add_ref(node: Node, source: &str, scope_func: Option<&str>, index: &mut FileIndex) {
    let name = node
        .utf8_text(source.as_bytes())
        .unwrap_or("")
        .to_lowercase();
    if name.is_empty() {
        return;
    }
    index.refs.entry(name).or_default().push(SymbolRef {
        usage_range: node_range(&node, source),
        scope_func: scope_func.map(String::from),
    });
}

/// True if any direct child of `node` has the given node kind.
fn has_keyword_child(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| c.kind() == kind)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{node_at_position, parse};
    use tower_lsp::lsp_types::Position;

    fn index_for(source: &str) -> FileIndex {
        let tree = parse(source).expect("parse");
        build_index(&tree, source)
    }

    // ── Definition indexing ───────────────────────────────────────────────────

    #[test]
    fn empty_source_yields_empty_index() {
        let idx = index_for("");
        assert!(idx.defs.is_empty());
        assert!(idx.refs.is_empty());
    }

    #[test]
    fn function_declaration_is_indexed_as_global() {
        let idx = index_for("Func Hello()\nEndFunc\n");
        let defs = idx.defs.get("hello").expect("hello in defs");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].kind, DefKind::Function);
        assert_eq!(defs[0].display_name, "Hello");
        assert!(defs[0].scope_func.is_none(), "function must be file-global");
    }

    #[test]
    fn parameter_is_indexed_with_function_scope() {
        let idx = index_for("Func Add($a, $b)\nReturn $a + $b\nEndFunc\n");
        let a_defs = idx.defs.get("$a").expect("$a in defs");
        assert_eq!(a_defs[0].kind, DefKind::Parameter);
        assert_eq!(a_defs[0].scope_func.as_deref(), Some("add"));
        let b_defs = idx.defs.get("$b").expect("$b in defs");
        assert_eq!(b_defs[0].scope_func.as_deref(), Some("add"));
    }

    #[test]
    fn global_variable_has_no_scope() {
        let idx = index_for("Global $foo = 5\n");
        let defs = idx.defs.get("$foo").expect("$foo in defs");
        assert_eq!(defs[0].kind, DefKind::Variable);
        assert!(defs[0].scope_func.is_none());
    }

    #[test]
    fn local_variable_has_function_scope() {
        let idx = index_for("Func F()\n    Local $x = 1\nEndFunc\n");
        let defs = idx.defs.get("$x").expect("$x in defs");
        assert_eq!(defs[0].kind, DefKind::Variable);
        assert_eq!(defs[0].scope_func.as_deref(), Some("f"));
    }

    #[test]
    fn global_const_is_constant_kind() {
        let idx = index_for("Global Const $MAX = 100\n");
        let defs = idx.defs.get("$max").expect("$max in defs");
        assert_eq!(defs[0].kind, DefKind::Constant);
        assert!(defs[0].scope_func.is_none());
    }

    #[test]
    fn enum_members_are_indexed() {
        let idx = index_for("Global Enum $A, $B, $C\n");
        assert_eq!(idx.defs["$a"][0].kind, DefKind::EnumMember);
        assert!(idx.defs.contains_key("$b"));
        assert!(idx.defs.contains_key("$c"));
    }

    #[test]
    fn multiple_functions_are_all_indexed() {
        let idx = index_for("Func Foo()\nEndFunc\nFunc Bar()\nEndFunc\n");
        assert!(idx.defs.contains_key("foo"));
        assert!(idx.defs.contains_key("bar"));
    }

    // ── Reference indexing ────────────────────────────────────────────────────

    #[test]
    fn function_call_is_indexed_as_ref() {
        let idx = index_for("Func F()\nEndFunc\n\nF()\n");
        let refs = idx.refs.get("f").expect("f in refs");
        assert_eq!(refs.len(), 1);
        assert!(refs[0].scope_func.is_none(), "call at file scope");
    }

    #[test]
    fn variable_usage_in_expression_is_indexed_as_ref() {
        let idx = index_for("Global $x = 1\n$x = $x + 1\n");
        // $x appears in the RHS ($x + 1) and as the LHS assignment target.
        let refs = idx.refs.get("$x").expect("$x in refs");
        assert!(refs.len() >= 1, "at least one usage of $x");
    }

    #[test]
    fn function_call_inside_function_is_scoped() {
        let idx = index_for("Func Caller()\n    Callee()\nEndFunc\nFunc Callee()\nEndFunc\n");
        let refs = idx.refs.get("callee").expect("callee in refs");
        assert_eq!(refs[0].scope_func.as_deref(), Some("caller"));
    }

    #[test]
    fn param_usage_inside_function_is_scoped_ref() {
        // $a is a parameter of Add; its usage in the body is a scoped ref.
        let idx = index_for("Func Add($a, $b)\nReturn $a + $b\nEndFunc\n");
        let refs = idx.refs.get("$a").expect("$a in refs");
        assert!(refs.iter().all(|r| r.scope_func.as_deref() == Some("add")));
    }

    // ── resolve_def ───────────────────────────────────────────────────────────

    #[test]
    fn resolve_def_prefers_local_over_global() {
        let source =
            "Global $x = 1\nFunc F()\n    Local $x = 2\n    Return $x\nEndFunc\n";
        let idx = index_for(source);
        // From inside F: should resolve to the local $x.
        let def = idx.resolve_def("$x", Some("f")).expect("resolved");
        assert_eq!(def.scope_func.as_deref(), Some("f"));
        // From file scope: should resolve to the global $x.
        let global = idx.resolve_def("$x", None).expect("global resolved");
        assert!(global.scope_func.is_none());
    }

    #[test]
    fn resolve_def_falls_back_to_global_when_no_local() {
        let source = "Global $x = 1\nFunc F()\nReturn $x\nEndFunc\n";
        let idx = index_for(source);
        // Cursor inside F, but $x is only declared globally.
        let def = idx.resolve_def("$x", Some("f")).expect("resolved to global");
        assert!(def.scope_func.is_none());
    }

    #[test]
    fn resolve_def_returns_none_for_unknown_symbol() {
        let idx = index_for("Global $x = 1\n");
        assert!(idx.resolve_def("$undeclared", None).is_none());
    }

    #[test]
    fn resolve_def_for_function_ignores_scope() {
        // Functions are always global regardless of cursor scope.
        let source = "Func Foo()\nEndFunc\nFunc Bar()\n    Foo()\nEndFunc\n";
        let idx = index_for(source);
        let def = idx.resolve_def("foo", Some("bar")).expect("resolved");
        assert_eq!(def.kind, DefKind::Function);
        assert!(def.scope_func.is_none());
    }

    // ── find_refs ─────────────────────────────────────────────────────────────

    #[test]
    fn find_refs_for_global_returns_all_refs() {
        let source = "Global $x = 1\nFunc F()\n    Return $x\nEndFunc\n$x = 2\n";
        let idx = index_for(source);
        // $x used inside F and at file scope.
        let refs = idx.find_refs("$x", None);
        assert!(
            refs.len() >= 2,
            "got {} ref(s), expected ≥2",
            refs.len()
        );
    }

    #[test]
    fn find_refs_for_local_returns_only_in_function() {
        let source = "Func F()\n    Local $tmp = 1\n    Return $tmp\nEndFunc\n\
                      Func G()\n    Local $tmp = 9\n    Return $tmp\nEndFunc\n";
        let idx = index_for(source);
        // Refs scoped to F should only include those inside F.
        let refs = idx.find_refs("$tmp", Some("f"));
        assert!(
            refs.iter().all(|r| r.scope_func.as_deref() == Some("f")),
            "all refs should be inside F"
        );
    }

    #[test]
    fn find_refs_returns_empty_for_unknown_name() {
        let idx = index_for("Global $x = 1\n");
        assert!(idx.find_refs("$nope", None).is_empty());
    }

    // ── cursor_scope ─────────────────────────────────────────────────────────

    #[test]
    fn cursor_scope_inside_function_returns_func_name() {
        let source = "Func MyFunc()\n    Local $x = 1\nEndFunc\n";
        let tree = parse(source).expect("parse");
        // Line 1 is "    Local $x = 1" — inside MyFunc.
        let node = node_at_position(&tree, source, Position::new(1, 10)).expect("node");
        assert_eq!(cursor_scope(node, source).as_deref(), Some("myfunc"));
    }

    #[test]
    fn cursor_scope_at_file_level_returns_none() {
        let source = "Global $x = 1\n";
        let tree = parse(source).expect("parse");
        let node = node_at_position(&tree, source, Position::new(0, 8)).expect("node");
        assert!(cursor_scope(node, source).is_none());
    }

    #[test]
    fn cursor_scope_case_insensitive_key() {
        // Even if the function is declared as "MYFUNC", the scope key is lowercase.
        let source = "Func MYFUNC()\n    Return 1\nEndFunc\n";
        let tree = parse(source).expect("parse");
        let node = node_at_position(&tree, source, Position::new(1, 4)).expect("node");
        assert_eq!(cursor_scope(node, source).as_deref(), Some("myfunc"));
    }

    // ── scope_at_line ────────────────────────────────────────────────────────

    #[test]
    fn scope_at_line_inside_clean_function() {
        let source = "Func MyHelper()\n    Local $x = 1\n    Return $x\nEndFunc\n";
        let index = index_for(source);
        // Line 1 is "    Local $x = 1" — inside MyHelper (lines 0–3).
        assert_eq!(scope_at_line(&index, 1).as_deref(), Some("myhelper"));
    }

    #[test]
    fn scope_at_line_at_file_scope_returns_none() {
        let source = "Global $g = 1\nFunc F()\nEndFunc\n";
        let index = index_for(source);
        // Line 0 is "Global $g = 1" — file scope.
        assert!(scope_at_line(&index, 0).is_none());
    }

    #[test]
    fn scope_at_line_with_bare_dollar_inside_function() {
        // Source that simulates the user typing `$` mid-edit inside a function.
        // A bare `$` is invalid AutoIt syntax; tree-sitter must use error recovery.
        // scope_at_line must still return the enclosing function.
        let source = "Func MyHelper()\n    Local $x = 1\n    $\n    Return $x\nEndFunc\n";
        let index = index_for(source);
        // Line 2 is "    $" — should still be inside MyHelper (lines 0–4).
        let scope = scope_at_line(&index, 2);
        assert_eq!(
            scope.as_deref(),
            Some("myhelper"),
            "scope_at_line must detect the enclosing function even with a parse error on that line. \
             If this fails, tree-sitter error-recovery is truncating the function_declaration range."
        );
    }

    #[test]
    fn scope_at_line_with_bare_at_inside_function() {
        // Same as above but for `@` (macro trigger character).
        let source = "Func MyHelper()\n    Local $x = 1\n    @\n    Return $x\nEndFunc\n";
        let index = index_for(source);
        let scope = scope_at_line(&index, 2);
        assert_eq!(scope.as_deref(), Some("myhelper"));
    }
}
