//! Tree-sitter parse-tree-per-document infrastructure.
//!
//! Sprint 1 day 1 PM foundation: every higher-level feature in the
//! v0.3+ roadmap (document symbols, hover, go-to-def, find-refs,
//! completion) operates against an in-memory parse tree of the open
//! buffer. This module is the only place that talks to the tree-sitter
//! runtime directly.
//!
//! Design choices for this initial cut:
//! - **No incremental edits yet.** The LSP advertises `TextDocumentSync::FULL`
//!   (the Au3Check staging path needs the full buffer anyway), so each
//!   `did_change` re-parses from scratch. tree-sitter is microseconds-fast
//!   on typical AutoIt files (<10k LOC), so this is a non-issue at v0.3.
//!   Switching to `INCREMENTAL` sync + `tree.edit(&InputEdit)` is a Sprint
//!   2+ optimization if profiling shows reparse cost matters.
//! - **Parser instances are ephemeral.** We construct a fresh `Parser` on
//!   each call rather than holding one per document. `Parser::new()` plus
//!   `set_language()` is cheap (sub-millisecond) and keeps `DocState` free
//!   of the `!Send`-ish `Parser` (it actually is Send, but holding state
//!   separately is simpler).
//! - **Position-to-byte conversion respects LSP's UTF-16 semantics.** Zed
//!   sends LSP `Position`s as `(line, utf16-code-unit)`. tree-sitter uses
//!   UTF-8 byte offsets. We convert by walking the line char-by-char
//!   counting UTF-16 units. Walking the whole document line-by-line is
//!   O(n) per call — acceptable for hover/definition (rare, user-driven),
//!   but cache line starts if we ever wire this into a high-frequency
//!   path like completion-while-typing.

use tower_lsp::lsp_types::Position;
use tree_sitter::{Node, Parser, Tree};

/// Construct a fresh `Parser` configured for the AutoIt grammar.
///
/// Cheap (microseconds); call per parse rather than caching.
fn new_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_autoit::LANGUAGE.into())
        .expect("AutoIt grammar should load — runtime/grammar ABI mismatch otherwise");
    parser
}

/// Parse `source` fully, ignoring any prior tree.
///
/// We deliberately don't accept an `old_tree` parameter at this point —
/// without applying `tree.edit(&InputEdit)`s for the actual change ranges
/// (which we don't have under FULL sync), passing an old tree gives
/// tree-sitter stale offsets and the results are inconsistent. Full reparse
/// is the safe default until INCREMENTAL sync arrives.
pub fn parse(source: &str) -> Option<Tree> {
    new_parser().parse(source, None)
}

/// Find the smallest node in `tree` whose byte range covers the byte that
/// `position` resolves to in `source`. Returns `None` if the position
/// falls outside the source.
pub fn node_at_position<'tree>(
    tree: &'tree Tree,
    source: &str,
    position: Position,
) -> Option<Node<'tree>> {
    let byte = position_to_byte(source, position)?;
    let root = tree.root_node();
    root.descendant_for_byte_range(byte, byte)
}

/// Convert a UTF-8 byte offset into `source` to an LSP [`Position`]
/// (line, UTF-16 code unit). Used to translate tree-sitter [`Node::start_byte`]
/// / [`Node::end_byte`] into LSP [`Range`]s for document-symbol responses,
/// hover ranges, etc.
///
/// If `byte_offset` falls past the end of `source`, returns the position
/// of the last byte (clamped).
pub fn byte_to_position(source: &str, byte_offset: usize) -> tower_lsp::lsp_types::Position {
    let clamped = byte_offset.min(source.len());

    // Find the line containing this byte by counting '\n' bytes before it.
    // `bytes()` is O(n) per call; outline construction does O(symbols) calls,
    // which is small. For high-frequency conversion (e.g. completion ranking
    // by line) we'd precompute a line-start table.
    let mut line: u32 = 0;
    let mut line_start: usize = 0;
    for (i, b) in source.as_bytes().iter().enumerate() {
        if i >= clamped {
            break;
        }
        if *b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }

    // Count UTF-16 code units from line_start to clamped. Use char_indices
    // to step in valid char boundaries, summing utf16 width per char.
    let mut character: u32 = 0;
    let line_slice = &source[line_start..clamped];
    for ch in line_slice.chars() {
        character += ch.len_utf16() as u32;
    }

    tower_lsp::lsp_types::Position { line, character }
}

/// Build an LSP [`Range`] from a tree-sitter [`Node`]'s byte span.
pub fn node_range(node: &Node, source: &str) -> tower_lsp::lsp_types::Range {
    tower_lsp::lsp_types::Range {
        start: byte_to_position(source, node.start_byte()),
        end: byte_to_position(source, node.end_byte()),
    }
}

/// Convert an LSP [`Position`] (UTF-16 code units) to a UTF-8 byte offset
/// into `source`. Returns `None` if `position.line` is past the last line.
///
/// LSP spec is unambiguous: positions are UTF-16 code units, line-relative.
/// When `position.character` would land inside a surrogate pair, we round
/// down to the start of the char (matches the "best effort" behaviour of
/// most servers — surrogate-pair-splitting edits don't happen in practice).
pub(crate) fn position_to_byte(source: &str, position: Position) -> Option<usize> {
    let target_line = position.line as usize;
    let target_char = position.character as usize;

    // Walk line-by-line, accumulating UTF-8 byte offsets. `split_inclusive`
    // keeps the `\n` (or `\r\n`) attached so the byte counts add up correctly.
    let mut byte = 0usize;
    let mut iter = source.split_inclusive('\n');
    for _ in 0..target_line {
        match iter.next() {
            Some(line) => byte += line.len(),
            None => return None,
        }
    }
    let line = iter.next().unwrap_or("");

    // Within the line, walk char-by-char counting UTF-16 code units.
    // Two cases per iteration:
    //   - target_char falls AT the start of this char (utf16 == target_char)
    //     → return the current byte offset.
    //   - target_char falls INSIDE this char's UTF-16 units (the surrogate-
    //     split pathological case) → round down to this char's start.
    // Otherwise advance utf16 by this char's UTF-16 width and continue.
    let mut utf16 = 0usize;
    for (offset, ch) in line.char_indices() {
        if utf16 == target_char {
            return Some(byte + offset);
        }
        let next_utf16 = utf16 + ch.len_utf16();
        if target_char < next_utf16 {
            return Some(byte + offset);
        }
        utf16 = next_utf16;
    }
    // `target_char` is past the last char on this line — clamp to end-of-line.
    Some(byte + line.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke: the grammar loads and a trivial program parses without
    /// errors. Mirrors the same-named test in tree-sitter-autoit, but
    /// here it also exercises the path-dep wiring.
    #[test]
    fn parses_trivial_program() {
        let source = "Func Hello()\n    ConsoleWrite(\"hi\" & @CRLF)\nEndFunc\n";
        let tree = parse(source).expect("parse succeeded");
        assert_eq!(tree.root_node().kind(), "source_file");
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn position_at_start_maps_to_byte_zero() {
        assert_eq!(
            position_to_byte("Func F()\nEndFunc\n", Position::new(0, 0)),
            Some(0)
        );
    }

    #[test]
    fn position_within_first_line() {
        // 'F' 'u' 'n' 'c' ' ' '|F'
        let source = "Func F()\nEndFunc\n";
        assert_eq!(position_to_byte(source, Position::new(0, 5)), Some(5));
    }

    #[test]
    fn position_on_second_line() {
        let source = "Func F()\nEndFunc\n";
        // "Func F()\n" is 9 bytes; "E" on line 1 starts at byte 9.
        assert_eq!(position_to_byte(source, Position::new(1, 0)), Some(9));
        // 'd' is at line-1-char-2.
        assert_eq!(position_to_byte(source, Position::new(1, 2)), Some(11));
    }

    #[test]
    fn position_past_end_of_line_clamps_to_newline() {
        let source = "ab\ncd\n";
        // Line 0 only has "ab" (2 chars) + newline. char=5 is past EOL.
        // Should clamp to the byte just after the newline (3).
        assert_eq!(position_to_byte(source, Position::new(0, 5)), Some(3));
    }

    #[test]
    fn position_past_last_line_returns_none() {
        let source = "ab\ncd\n";
        // After "ab\ncd\n" there's an implicit empty line 2; line 3 doesn't exist.
        assert_eq!(position_to_byte(source, Position::new(3, 0)), None);
    }

    #[test]
    fn position_in_line_with_multibyte_char() {
        // 'a' (1 byte / 1 utf16) 'é' (2 bytes / 1 utf16) 'b' (1 byte / 1 utf16)
        let source = "aéb\n";
        // char 0 → byte 0 ('a')
        assert_eq!(position_to_byte(source, Position::new(0, 0)), Some(0));
        // char 1 → byte 1 ('é')
        assert_eq!(position_to_byte(source, Position::new(0, 1)), Some(1));
        // char 2 → byte 3 ('b', because 'é' is 2 bytes)
        assert_eq!(position_to_byte(source, Position::new(0, 2)), Some(3));
    }

    #[test]
    fn position_in_line_with_surrogate_pair() {
        // U+1F600 GRINNING FACE: 4 UTF-8 bytes, 2 UTF-16 code units (surrogate pair).
        let source = "a😀b\n";
        // char 0 → byte 0 ('a')
        assert_eq!(position_to_byte(source, Position::new(0, 0)), Some(0));
        // char 1 → byte 1 (start of '😀')
        assert_eq!(position_to_byte(source, Position::new(0, 1)), Some(1));
        // char 2 → byte 1 (middle of surrogate pair — round down to 😀 start)
        assert_eq!(position_to_byte(source, Position::new(0, 2)), Some(1));
        // char 3 → byte 5 ('b', after '😀's 4 bytes)
        assert_eq!(position_to_byte(source, Position::new(0, 3)), Some(5));
    }

    #[test]
    fn node_at_position_finds_function_name() {
        let source = "Func Hello()\nEndFunc\n";
        let tree = parse(source).unwrap();
        // 'H' of "Hello" is at line 0, char 5.
        let node = node_at_position(&tree, source, Position::new(0, 6))
            .expect("expected a node at position");
        // The descendant should be the identifier "Hello".
        assert_eq!(node.kind(), "identifier");
        assert_eq!(node.utf8_text(source.as_bytes()).unwrap(), "Hello");
    }

    #[test]
    fn node_at_position_inside_string() {
        let source = "ConsoleWrite(\"hello\")\n";
        // Position inside the string literal.
        let tree = parse(source).unwrap();
        let node = node_at_position(&tree, source, Position::new(0, 16)).unwrap();
        assert_eq!(node.kind(), "string");
    }

    #[test]
    fn byte_to_position_at_start() {
        assert_eq!(byte_to_position("Func\nEndFunc\n", 0), Position::new(0, 0));
    }

    #[test]
    fn byte_to_position_on_line_1() {
        // "Func\n" is 5 bytes; byte 5 is 'E' on line 1, col 0.
        assert_eq!(byte_to_position("Func\nEndFunc\n", 5), Position::new(1, 0));
        // byte 7 is 'd' on line 1, col 2.
        assert_eq!(byte_to_position("Func\nEndFunc\n", 7), Position::new(1, 2));
    }

    #[test]
    fn byte_to_position_multibyte() {
        // 'a' 1B, 'é' 2B, 'b' 1B. byte 3 = 'b' starting at utf16 col 2.
        assert_eq!(byte_to_position("aéb\n", 3), Position::new(0, 2));
    }

    #[test]
    fn byte_to_position_roundtrip() {
        let source = "Func Hello()\n    ConsoleWrite(\"hi\")\nEndFunc\n";
        for byte in [0, 5, 9, 13, 30, source.len()] {
            let pos = byte_to_position(source, byte);
            let back = position_to_byte(source, pos).unwrap();
            assert_eq!(back, byte.min(source.len()), "roundtrip at byte {byte}");
        }
    }

    #[test]
    fn node_range_for_function_declaration() {
        let source = "Func Hello()\nEndFunc\n";
        let tree = parse(source).unwrap();
        let func = tree.root_node().child(0).unwrap();
        assert_eq!(func.kind(), "function_declaration");
        let range = node_range(&func, source);
        assert_eq!(range.start, Position::new(0, 0));
        // "Func Hello()\nEndFunc" — end at the close of "EndFunc" on line 1.
        assert_eq!(range.end, Position::new(1, 7));
    }
}
