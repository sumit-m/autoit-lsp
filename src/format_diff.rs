//! Minimal text diff for formatting edits.
//!
//! v0.6.0 bug fix. The Tidy formatter (`textDocument/formatting`) hands back a
//! fully re-formatted document. Returning that as a single whole-document
//! `TextEdit` (range `(0,0)..EOF`) makes Zed drop the cursor to end-of-file on
//! every format-on-save: the old cursor position cannot be mapped through a
//! total replacement, so the editor parks it at EOF.
//!
//! [`diff_edits`] instead returns a **multi-hunk** line diff: one small edit per
//! changed region, with all unchanged lines left untouched. This matters
//! because Tidy normalizes keyword/variable/function casing *throughout* the
//! file, so changes are scattered — a single-hunk (common-prefix/suffix-trim)
//! diff would still span almost the whole document and move the cursor whenever
//! it sat in the changed middle. With per-hunk edits, a cursor on any unchanged
//! line falls outside every edit range and is preserved regardless of scroll
//! position.
//!
//! Algorithm: strip common leading/trailing lines, then an LCS line diff over
//! the middle, grouping consecutive non-matching lines into replace/insert/
//! delete hunks. Capped to avoid O(n·m) blow-up on very large files — beyond
//! the cap it falls back to a single middle-spanning edit (correct, just not
//! minimal).

use tower_lsp::lsp_types::{Range, TextEdit};

use crate::tree::byte_to_position;

/// Largest LCS table we'll build (`old_mid.len() * new_mid.len()`). ~2000×2000
/// lines. Past this we fall back to a single middle-spanning edit to bound
/// memory/time; format-on-save on files that large is rare.
const MAX_LCS_CELLS: usize = 4_000_000;

/// Build a minimal set of [`TextEdit`]s that transform `old` into `new`,
/// touching only the lines that changed. Returns an empty vec when the texts
/// are identical.
pub fn diff_edits(old: &str, new: &str) -> Vec<TextEdit> {
    if old == new {
        return Vec::new();
    }

    // Keep each line's trailing newline attached so byte offsets sum exactly
    // and CRLF vs LF is preserved verbatim.
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();

    // Byte offset of the start of each old line: old_byte[k] = sum of lengths
    // of old_lines[0..k]. old_byte[old_lines.len()] == old.len().
    let old_byte: Vec<usize> = {
        let mut v = Vec::with_capacity(old_lines.len() + 1);
        let mut acc = 0usize;
        v.push(0);
        for l in &old_lines {
            acc += l.len();
            v.push(acc);
        }
        v
    };

    // Helper: build one edit replacing old lines [o_start, o_end) (indices into
    // old_lines) with new lines [n_start, n_end) (indices into new_lines).
    let mk_edit = |o_start: usize, o_end: usize, n_start: usize, n_end: usize| TextEdit {
        range: Range {
            start: byte_to_position(old, old_byte[o_start]),
            end: byte_to_position(old, old_byte[o_end]),
        },
        new_text: new_lines[n_start..n_end].concat(),
    };

    // Common leading / trailing lines (cheap; shrinks the LCS problem).
    let mut p = 0;
    while p < old_lines.len() && p < new_lines.len() && old_lines[p] == new_lines[p] {
        p += 1;
    }
    let mut s = 0;
    while s < old_lines.len() - p
        && s < new_lines.len() - p
        && old_lines[old_lines.len() - 1 - s] == new_lines[new_lines.len() - 1 - s]
    {
        s += 1;
    }

    let old_mid = &old_lines[p..old_lines.len() - s];
    let new_mid = &new_lines[p..new_lines.len() - s];
    let m = old_mid.len();
    let n = new_mid.len();

    // Fallback for very large diffs: one edit over the whole changed middle.
    if m.saturating_mul(n) > MAX_LCS_CELLS {
        return vec![mk_edit(p, old_lines.len() - s, p, new_lines.len() - s)];
    }

    // LCS lengths: dp[i][j] = LCS(old_mid[i..], new_mid[j..]).
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] = if old_mid[i] == new_mid[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    // Walk the alignment, grouping runs of non-matching lines into hunks.
    // Hunk indices are into old_mid / new_mid; convert to old_lines / new_lines
    // by adding the prefix `p`.
    let mut edits = Vec::new();
    let mut i = 0;
    let mut j = 0;
    let mut hunk_open = false;
    let mut h_old = 0; // hunk start in old_mid
    let mut h_new = 0; // hunk start in new_mid

    while i < m && j < n {
        if old_mid[i] == new_mid[j] {
            if hunk_open {
                edits.push(mk_edit(p + h_old, p + i, p + h_new, p + j));
                hunk_open = false;
            }
            i += 1;
            j += 1;
        } else {
            if !hunk_open {
                hunk_open = true;
                h_old = i;
                h_new = j;
            }
            // Follow the LCS: prefer the direction that keeps more common lines.
            if dp[i + 1][j] >= dp[i][j + 1] {
                i += 1; // treat old_mid[i] as deleted
            } else {
                j += 1; // treat new_mid[j] as inserted
            }
        }
    }

    // Tail: any remaining old (deletes) and/or new (inserts) lines form a final
    // hunk, merged with an already-open one.
    if hunk_open || i < m || j < n {
        let o_start = if hunk_open { h_old } else { i };
        let n_start = if hunk_open { h_new } else { j };
        edits.push(mk_edit(p + o_start, p + m, p + n_start, p + n));
    }

    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::position_to_byte;

    /// Apply `edits` to `old` and return the result. Edits are non-overlapping
    /// and in increasing document order; apply highest-offset first so earlier
    /// byte offsets stay valid.
    fn apply(old: &str, edits: &[TextEdit]) -> String {
        let mut spans: Vec<(usize, usize, &str)> = edits
            .iter()
            .map(|e| {
                let s = position_to_byte(old, e.range.start).expect("start byte");
                let en = position_to_byte(old, e.range.end).expect("end byte");
                (s, en, e.new_text.as_str())
            })
            .collect();
        spans.sort_by_key(|&(s, _, _)| std::cmp::Reverse(s));
        let mut result = old.to_string();
        for (s, en, txt) in spans {
            result.replace_range(s..en, txt);
        }
        result
    }

    /// Lines (0-based) that fall strictly inside some edit's range — these are
    /// the lines whose cursor position would NOT be preserved.
    fn touched_lines(edits: &[TextEdit]) -> Vec<u32> {
        let mut out = Vec::new();
        for e in edits {
            // A line L is "touched" if start.line <= L < end.line (whole-line
            // replacements have end at the start of the following line).
            for l in e.range.start.line..e.range.end.line {
                out.push(l);
            }
        }
        out
    }

    #[test]
    fn identical_text_yields_no_edits() {
        assert!(diff_edits("a\nb\n", "a\nb\n").is_empty());
    }

    #[test]
    fn localized_middle_change_touches_only_that_line() {
        let old = "Func Foo()\n    msgbox(0, \"x\", \"y\")\nEndFunc\n";
        let new = "Func Foo()\n    MsgBox(0, \"x\", \"y\")\nEndFunc\n";
        let edits = diff_edits(old, new);
        assert_eq!(edits.len(), 1);
        assert_eq!(touched_lines(&edits), vec![1]);
        assert_eq!(apply(old, &edits), new);
    }

    #[test]
    fn scattered_changes_produce_multiple_hunks_leaving_middle_untouched() {
        // Lines 0 and 4 change (re-casing); lines 1-3 are unchanged. This is
        // the cursor-jump repro: a cursor on line 2 must stay put.
        let old = "msgbox(0)\nA\nB\nC\nconsolewrite(1)\n";
        let new = "MsgBox(0)\nA\nB\nC\nConsoleWrite(1)\n";
        let edits = diff_edits(old, new);
        assert_eq!(edits.len(), 2, "expected two separate hunks: {edits:?}");
        let touched = touched_lines(&edits);
        assert!(!touched.contains(&1), "line 1 must be untouched");
        assert!(!touched.contains(&2), "line 2 (cursor) must be untouched");
        assert!(!touched.contains(&3), "line 3 must be untouched");
        assert_eq!(apply(old, &edits), new);
    }

    #[test]
    fn change_at_start_preserves_tail() {
        let old = "local $x=1\nConsoleWrite($x)\n";
        let new = "Local $x = 1\nConsoleWrite($x)\n";
        let edits = diff_edits(old, new);
        assert_eq!(touched_lines(&edits), vec![0]);
        assert_eq!(apply(old, &edits), new);
    }

    #[test]
    fn appended_lines_insert_at_eof() {
        let old = "Local $x = 1\n";
        let new = "Local $x = 1\nLocal $y = 2\n";
        assert_eq!(apply(old, &diff_edits(old, new)), new);
    }

    #[test]
    fn removed_blank_lines_round_trip() {
        let old = "a\n\n\nb\n";
        let new = "a\nb\n";
        let edits = diff_edits(old, new);
        assert_eq!(apply(old, &edits), new);
    }

    #[test]
    fn no_trailing_newline_round_trips() {
        let old = "a\nbqux";
        let new = "a\nbar";
        assert_eq!(apply(old, &diff_edits(old, new)), new);
    }

    #[test]
    fn whole_document_reindent_round_trips() {
        // Every line changes (worst case). Verify correctness; minimality is
        // not required here.
        let old = "Func F()\nIf $a Then\nx()\nEndIf\nEndFunc\n";
        let new = "Func F()\n    If $a Then\n        x()\n    EndIf\nEndFunc\n";
        assert_eq!(apply(old, &diff_edits(old, new)), new);
    }

    #[test]
    fn mismatched_line_endings_diff_everything_hence_normalize() {
        // LF old vs CRLF new: every line "differs" (\n vs \r\n) → a degenerate
        // whole-document diff. This is exactly why the formatting handler
        // normalizes both sides to LF before calling diff_edits (and why
        // returning CRLF triggers the Zed cursor-jump bug, zed#39547).
        let old_lf = "a\nb\nc\n";
        let new_crlf = "a\r\nb\r\nc\r\n";
        assert!(
            !diff_edits(old_lf, new_crlf).is_empty(),
            "mismatched endings produce a (degenerate) diff"
        );
        // Normalized to LF, there is no real change → no edits.
        assert!(diff_edits(old_lf, &new_crlf.replace("\r\n", "\n")).is_empty());
    }

    #[test]
    fn interleaved_changes_keep_unchanged_lines_untouched() {
        // Change every other line; the unchanged ones must never be touched.
        let old = "a1\nKEEP1\nb1\nKEEP2\nc1\n";
        let new = "A1\nKEEP1\nB1\nKEEP2\nC1\n";
        let edits = diff_edits(old, new);
        let touched = touched_lines(&edits);
        assert!(!touched.contains(&1), "KEEP1 (line 1) untouched");
        assert!(!touched.contains(&3), "KEEP2 (line 3) untouched");
        assert_eq!(apply(old, &edits), new);
    }
}
