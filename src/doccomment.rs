//! Doc-comment extraction for user-defined AutoIt functions.
//!
//! Walks backwards from a `Func` declaration line, collects consecutive
//! `;`-prefixed comment lines, and renders them as Markdown for hover popups.
//!
//! ## Supported formats
//!
//! ### AutoDoc structured (used by official UDF libraries)
//!
//! ```autoit
//! ; #FUNCTION# ====================================================
//! ; Name...........: _ArrayAdd
//! ; Description ...: Adds a value at the end of an existing array
//! ; Parameters ....: $aArray - Array to modify
//! ;                  $vValue - Value to add
//! ; Return values .: Success - Returns the index of the added item
//! ; ================================================================
//! Func _ArrayAdd(ByRef $aArray, $vValue)
//! ```
//!
//! Detected by the presence of `...:` key-field lines.  Description,
//! Parameters, and Return values sections are extracted; other fields
//! (Author, Remarks, etc.) are silently skipped.
//!
//! ### Plain leading comments (most user code)
//!
//! ```autoit
//! ; Calculates the sum of two numbers.
//! ; $a - first number, $b - second number
//! Func Add($a, $b)
//! ```
//!
//! All non-separator `;` lines are joined as a description block.
//!
//! ## Rendered output (Markdown)
//!
//! AutoDoc:
//! ```text
//! Adds a value at the end of an existing array.
//!
//! **Parameters:**
//! - `$aArray` — Array to modify
//! - `$vValue` — Value to add
//!
//! **Returns:**
//! Success - Returns the index of the added item
//! ```
//!
//! Plain:
//! ```text
//! Calculates the sum of two numbers.
//! $a - first number, $b - second number
//! ```

// ─── Public entry point ───────────────────────────────────────────────────────

/// Extract and format the doc-comment block immediately preceding the function
/// declaration at `func_start_line` (0-based).
///
/// Returns `None` when no comment block is found directly above the `Func`
/// keyword (blank lines or non-comment code between the comment and the
/// declaration cause the block to be ignored).
pub fn extract_doc_comment(source: &str, func_start_line: usize) -> Option<String> {
    if func_start_line == 0 {
        return None;
    }

    let all_lines: Vec<&str> = source.lines().collect();
    let mut comment_lines: Vec<&str> = Vec::new();

    // Walk backwards from the line immediately above the Func keyword,
    // collecting consecutive `;`-prefixed lines.
    let mut i = func_start_line - 1;
    loop {
        let raw = match all_lines.get(i) {
            Some(l) => l,
            None => break,
        };
        let trimmed = raw.trim();
        if trimmed.starts_with(';') {
            comment_lines.push(raw);
        } else {
            // Blank or non-comment line — stop. Even a single blank line
            // between a comment block and the Func keyword means the comment
            // is not associated with this function.
            break;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }

    if comment_lines.is_empty() {
        return None;
    }
    // Restore top-to-bottom order (we collected bottom-to-top).
    comment_lines.reverse();

    // Detect AutoDoc by the presence of at least one key-field line
    // (pattern: `; Word......: value`).
    let is_autodoc = comment_lines.iter().any(|l| is_autodoc_key(l.trim()));

    if is_autodoc {
        render_autodoc(&comment_lines)
    } else {
        render_plain(&comment_lines)
    }
}

// ─── AutoDoc parser ───────────────────────────────────────────────────────────

/// True if `s` (already trimmed) is an AutoDoc key-field line.
/// AutoDoc keys look like: `; Description ...: ` or `; Return values .: `
/// — semicolon, word(s), one or more dots, colon.
fn is_autodoc_key(s: &str) -> bool {
    if !s.starts_with(';') {
        return false;
    }
    // Require at least one dot immediately before a colon after the semicolon.
    // `Return values .:` has one dot; `Description ...:` has three — both match.
    s[1..].contains(".:")
}

/// True if `s` (trimmed) is a visual separator line: `; ====`, `; ----`, etc.
fn is_separator(s: &str) -> bool {
    let rest = s.trim_start_matches(';').trim();
    !rest.is_empty() && rest.chars().all(|c| matches!(c, '=' | '-' | '#' | '~' | '*' | ' '))
}

/// Identify which logical section a key-field line belongs to.
/// Returns `None` for non-key lines and for key lines we don't care about
/// (Author, Remarks, Link, etc.).
fn autodoc_section(s: &str) -> Option<Section> {
    if !is_autodoc_key(s) {
        return None;
    }
    let lower = s.to_lowercase();
    if lower.contains("description") {
        Some(Section::Description)
    } else if lower.contains("parameter") {
        Some(Section::Parameters)
    } else if lower.contains("return") {
        Some(Section::Returns)
    } else {
        Some(Section::Other)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    Description,
    Parameters,
    Returns,
    Other,
}

/// Parse an AutoDoc comment block into a Markdown string.
fn render_autodoc(lines: &[&str]) -> Option<String> {
    let mut description: Vec<String> = Vec::new();
    let mut parameters: Vec<String> = Vec::new();
    let mut returns: Vec<String> = Vec::new();
    let mut current = Section::Other;

    for raw in lines {
        let t = raw.trim();

        if is_separator(t) {
            // Visual separator lines (`; ====`) are skipped entirely.
            continue;
        }

        if let Some(section) = autodoc_section(t) {
            current = section;
            if current == Section::Other {
                continue;
            }
            // Extract the value that appears after the colon on the key line.
            if let Some(colon) = t.rfind(':') {
                let value = t[colon + 1..].trim();
                if !value.is_empty() {
                    push_to_section(value, current, &mut description, &mut parameters, &mut returns);
                }
            }
        } else if t.starts_with(';') && current != Section::Other {
            // Continuation line — strip the leading `;` and any padding spaces.
            let value = t[1..].trim();
            if !value.is_empty() {
                push_to_section(value, current, &mut description, &mut parameters, &mut returns);
            }
        }
    }

    if description.is_empty() && parameters.is_empty() && returns.is_empty() {
        return None;
    }

    let mut out = String::new();

    if !description.is_empty() {
        out.push_str(&description.join(" "));
        out.push('\n');
    }

    if !parameters.is_empty() {
        out.push_str("\n**Parameters:**\n");
        for p in &parameters {
            if let Some((name, desc)) = split_param(p) {
                out.push_str(&format!("- `{name}` — {desc}\n"));
            } else {
                out.push_str(&format!("- {p}\n"));
            }
        }
    }

    if !returns.is_empty() {
        out.push_str("\n**Returns:**\n");
        out.push_str(&returns.join("\n"));
        out.push('\n');
    }

    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn push_to_section(
    value: &str,
    section: Section,
    description: &mut Vec<String>,
    parameters: &mut Vec<String>,
    returns: &mut Vec<String>,
) {
    match section {
        Section::Description => description.push(value.to_string()),
        Section::Parameters => parameters.push(value.to_string()),
        Section::Returns => returns.push(value.to_string()),
        Section::Other => {}
    }
}

/// Try to split `"$ParamName - description text"` into `(name, desc)`.
/// Recognises ` - ` (space-dash-space) as the separator.
fn split_param(s: &str) -> Option<(&str, &str)> {
    let t = s.trim();
    if !t.starts_with('$') {
        return None;
    }
    t.find(" - ").map(|pos| (&t[..pos], t[pos + 3..].trim()))
}

// ─── Plain comment renderer ───────────────────────────────────────────────────

/// Render plain (non-AutoDoc) comment lines as a Markdown description block.
fn render_plain(lines: &[&str]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for raw in lines {
        let t = raw.trim();
        // Skip separators and bare semicolons.
        if t == ";" || is_separator(t) {
            continue;
        }
        // Strip leading `; ` or bare `;`.
        let content = if let Some(rest) = t.strip_prefix("; ") {
            rest
        } else if let Some(rest) = t.strip_prefix(';') {
            rest.trim_start()
        } else {
            t
        };
        if !content.is_empty() {
            parts.push(content.to_string());
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("\n"))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Plain comments ────────────────────────────────────────────────────────

    #[test]
    fn plain_single_line_description() {
        let source = "; Calculates the sum of two numbers.\nFunc Add($a, $b)\nEndFunc\n";
        let result = extract_doc_comment(source, 1).expect("should extract");
        assert!(result.contains("Calculates the sum of two numbers."));
    }

    #[test]
    fn plain_multi_line_description() {
        let source = "; First line.\n; Second line.\nFunc Foo()\nEndFunc\n";
        let result = extract_doc_comment(source, 2).expect("should extract");
        assert!(result.contains("First line."));
        assert!(result.contains("Second line."));
    }

    #[test]
    fn blank_line_between_comment_and_func_returns_none() {
        // A blank line between the comment and Func means no association.
        let source = "; Some comment.\n\nFunc Foo()\nEndFunc\n";
        assert!(extract_doc_comment(source, 2).is_none());
    }

    #[test]
    fn no_comment_returns_none() {
        let source = "Func Foo()\nEndFunc\n";
        assert!(extract_doc_comment(source, 0).is_none());
    }

    #[test]
    fn func_on_first_line_returns_none() {
        let source = "Func Foo()\nEndFunc\n";
        assert!(extract_doc_comment(source, 0).is_none());
    }

    #[test]
    fn separator_only_returns_none() {
        let source = "; =====================\nFunc Foo()\nEndFunc\n";
        assert!(extract_doc_comment(source, 1).is_none());
    }

    #[test]
    fn plain_strips_semicolon_prefix() {
        let source = "; My description\nFunc F()\nEndFunc\n";
        let result = extract_doc_comment(source, 1).unwrap();
        // Should not start with ";"
        assert!(!result.trim_start().starts_with(';'));
        assert!(result.contains("My description"));
    }

    // ── AutoDoc comments ──────────────────────────────────────────────────────

    #[test]
    fn autodoc_extracts_description() {
        let source = concat!(
            "; #FUNCTION# ============================\n",
            "; Description ...: Adds a value to an array\n",
            "; ========================================\n",
            "Func _Add()\nEndFunc\n",
        );
        let result = extract_doc_comment(source, 3).expect("should extract");
        assert!(result.contains("Adds a value to an array"));
    }

    #[test]
    fn autodoc_extracts_parameters() {
        let source = concat!(
            "; Description ...: Does something\n",
            "; Parameters ....: $arr - The array\n",
            ";                  $val - The value\n",
            "Func F($arr, $val)\nEndFunc\n",
        );
        let result = extract_doc_comment(source, 3).expect("should extract");
        assert!(result.contains("**Parameters:**"));
        assert!(result.contains("`$arr`"));
        assert!(result.contains("The array"));
        assert!(result.contains("`$val`"));
    }

    #[test]
    fn autodoc_extracts_returns() {
        let source = concat!(
            "; Description ...: Does something\n",
            "; Return values .: Success - 1\n",
            ";                  Failure - 0\n",
            "Func F()\nEndFunc\n",
        );
        let result = extract_doc_comment(source, 3).expect("should extract");
        assert!(result.contains("**Returns:**"));
        assert!(result.contains("Success"));
    }

    #[test]
    fn autodoc_skips_author_and_remarks() {
        let source = concat!(
            "; Description ...: Core function\n",
            "; Author ........: Someone\n",
            "; Remarks .......: None\n",
            "Func F()\nEndFunc\n",
        );
        let result = extract_doc_comment(source, 3).expect("should extract");
        assert!(result.contains("Core function"));
        // Author and Remarks should not appear as headers
        assert!(!result.contains("**Author"));
        assert!(!result.contains("**Remarks"));
    }

    #[test]
    fn autodoc_separator_lines_are_skipped() {
        let source = concat!(
            "; ====================================\n",
            "; Description ...: Useful function\n",
            "; ====================================\n",
            "Func F()\nEndFunc\n",
        );
        let result = extract_doc_comment(source, 3).expect("should extract");
        assert!(result.contains("Useful function"));
        // No raw `=` characters should bleed into the output
        assert!(!result.contains("===="));
    }

    // ── split_param ───────────────────────────────────────────────────────────

    #[test]
    fn split_param_with_dash_separator() {
        let (name, desc) = split_param("$aArray - Array to modify").unwrap();
        assert_eq!(name, "$aArray");
        assert_eq!(desc, "Array to modify");
    }

    #[test]
    fn split_param_non_variable_returns_none() {
        assert!(split_param("not a param").is_none());
    }
}
