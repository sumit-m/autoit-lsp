//! Au3Check.exe wrapper — discover the binary, invoke it, parse its
//! output into LSP `Diagnostic` structs.
//!
//! Au3Check is AutoIt's official syntax linter. We shell out to it and
//! re-publish its errors/warnings as LSP diagnostics. Invocation form:
//!
//! ```text
//! Au3Check.exe -q [-I <path>...] <script-path>
//! ```
//!
//! Output line format (per loganch/AutoIt-VSCode, MIT):
//!
//! ```text
//! "C:\path\to\script.au3"(LINE,COL) : error: <message>
//! "C:\path\to\script.au3"(LINE,COL) : warning: <message>
//! ```
//!
//! Followed by a summary line: `0 error(s), 0 warning(s)` (or non-zero
//! counts). We only parse the diagnostic lines; the summary is ignored.
//!
//! Flag semantics (see https://www.autoitscript.com/autoit3/scite/docs/SciTE4AutoIt3/au3check.html):
//! - `-q`: quiet, suppress banner. Always passed.
//! - `-I <path>`: extra include search dir. Set to the original
//!   file's directory so quoted `#include "x.au3"` resolves from a
//!   staged temp path.
//!
//! Other flags (`-w<n>`, `-d`) are *not* exposed by the LSP because
//! SciTE already configures them and adding a layered LSP setting
//! didn't justify the UX cost.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use tokio::process::Command;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

/// Configuration for a single `run_au3check` invocation. Bundled into a
/// struct rather than a long parameter list so future flag additions
/// stay additive instead of rippling through callers.
#[derive(Debug, Clone)]
pub struct Au3CheckConfig<'a> {
    /// The file Au3Check should lint (a staged temp file in our case).
    pub target: &'a Path,
    /// Extra directories searched after the script's own dir for
    /// quoted `#include "x.au3"` resolution. Pass the original file's
    /// directory so includes still resolve from a staged temp path.
    pub include_dirs: &'a [&'a Path],
    /// Process working directory. Set to the original file's dir as a
    /// fallback for any cwd-relative behaviour in Au3Check.
    pub cwd: Option<&'a Path>,
    /// Extra raw arguments appended verbatim to the Au3Check argv,
    /// tokenized from the `au3checkExtraArgs` user setting (e.g.
    /// `["-w", "1", "-d"]`). Not validated — the user owns the contents.
    pub extra_args: &'a [String],
}

/// Discover the Au3Check.exe path.
///
/// Probes the Windows registry where the official AutoIt installer
/// writes its install dir (HKLM\SOFTWARE\WOW6432Node\AutoIt v3\AutoIt
/// on 64-bit Windows; HKLM\SOFTWARE\AutoIt v3\AutoIt on 32-bit), then
/// falls back to the canonical default path. Returns `None` only if
/// the binary isn't at any of those locations — typically a portable
/// install at a custom path. In that case the user sets the
/// `au3checkPath` LSP setting (handled in `main.rs`), which overrides
/// whatever this function returns.
pub fn discover_au3check() -> Option<PathBuf> {
    if let Some(dir) = install_dir_from_registry() {
        let candidate = dir.join("Au3Check.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let fallback = PathBuf::from(r"C:\Program Files (x86)\AutoIt3\Au3Check.exe");
    fallback.is_file().then_some(fallback)
}

#[cfg(windows)]
fn install_dir_from_registry() -> Option<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    // AutoIt v3 is a 32-bit application. On 64-bit Windows the installer
    // writes to WOW6432Node; on 32-bit Windows it writes to the native
    // path. Try both.
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    for sub in [
        r"SOFTWARE\WOW6432Node\AutoIt v3\AutoIt",
        r"SOFTWARE\AutoIt v3\AutoIt",
    ] {
        if let Ok(key) = hklm.open_subkey(sub)
            && let Ok(dir) = key.get_value::<String, _>("InstallDir")
        {
            return Some(PathBuf::from(dir));
        }
    }
    None
}

#[cfg(not(windows))]
fn install_dir_from_registry() -> Option<PathBuf> {
    None
}

/// Discover AutoIt's standard `Include\` directory (the search path for
/// `#include <File.au3>` directives). Returns `None` on non-Windows or
/// when AutoIt isn't installed.
pub fn discover_autoit_include_dir() -> Option<PathBuf> {
    let dir = install_dir_from_registry()
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)\AutoIt3"));
    let candidate = dir.join("Include");
    candidate.is_dir().then_some(candidate)
}

/// Discover `AutoIt3.exe` — the interpreter used to drive
/// `AutoIt3Wrapper.au3 /Tidy` for code formatting.
/// Same registry/fallback search order as `discover_au3check`.
pub fn discover_autoit3_exe() -> Option<PathBuf> {
    if let Some(dir) = install_dir_from_registry() {
        let candidate = dir.join("AutoIt3.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let fallback = PathBuf::from(r"C:\Program Files (x86)\AutoIt3\AutoIt3.exe");
    fallback.is_file().then_some(fallback)
}

/// Discover `AutoIt3Wrapper.au3`, which ships with SciTE4AutoIt3 and
/// provides the `/Tidy` code-formatting entry point.
/// Returns `None` when only the base AutoIt3 installer is present
/// (SciTE4AutoIt3 is a separate download).
pub fn discover_autoit3wrapper() -> Option<PathBuf> {
    if let Some(dir) = install_dir_from_registry() {
        let candidate = dir.join(r"SciTE\AutoIt3Wrapper\AutoIt3Wrapper.au3");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let fallback = PathBuf::from(
        r"C:\Program Files (x86)\AutoIt3\SciTE\AutoIt3Wrapper\AutoIt3Wrapper.au3",
    );
    fallback.is_file().then_some(fallback)
}

/// Discover `Tidy.exe`, the actual formatter binary that
/// `AutoIt3Wrapper.au3 /Tidy` calls internally.  Both
/// `AutoIt3Wrapper.au3` and `Tidy.exe` must be present for formatting
/// to work; checking both avoids advertising the capability on machines
/// where SciTE4AutoIt3 is only partially installed.
pub fn discover_tidy_exe() -> Option<PathBuf> {
    if let Some(dir) = install_dir_from_registry() {
        let candidate = dir.join(r"SciTE\Tidy\Tidy.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let fallback =
        PathBuf::from(r"C:\Program Files (x86)\AutoIt3\SciTE\Tidy\Tidy.exe");
    fallback.is_file().then_some(fallback)
}

/// Build the Au3Check argument list from a config. Extracted so unit
/// tests can verify flag generation without spawning the actual
/// process.
fn build_args(config: &Au3CheckConfig<'_>) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    args.push("-q".into());
    for dir in config.include_dirs {
        args.push("-I".into());
        args.push(dir.as_os_str().to_os_string());
    }
    // User-supplied extra flags (e.g. `-w 1 -d`), appended verbatim before
    // the target file. Not validated — see the `au3checkExtraArgs` setting.
    for arg in config.extra_args {
        args.push(arg.into());
    }
    args.push(config.target.as_os_str().to_os_string());
    args
}

/// Run Au3Check with the given config and return its captured stdout.
pub async fn run_au3check(
    au3check: &Path,
    config: Au3CheckConfig<'_>,
) -> std::io::Result<String> {
    let mut cmd = Command::new(au3check);
    cmd.args(build_args(&config));
    if let Some(cwd) = config.cwd {
        cmd.current_dir(cwd);
    }
    let output = cmd.output().await?;
    // Au3Check writes diagnostics to stdout; stderr is usually empty.
    // Use lossy decode — script paths can contain non-UTF8 bytes on
    // Windows codepages but our parser only cares about the ASCII parts.
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// True if `b` can appear in an AutoIt identifier: alphanumeric, `_`, `$`, `@`, `.`.
#[inline]
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'@' || b == b'.'
}

/// Walk forward from `col0` in `line` while the byte is part of an
/// identifier / sigil-prefixed name. Returns the end column (one past
/// the last identifier byte).
///
/// Heuristic only — assumes the source is ASCII-clean. AutoIt files
/// are conventionally ASCII or Windows-1252.
///
/// Returns `col0 + 1` if the position lands on a non-identifier
/// character or past the line end.
fn token_end_col(line: &str, col0: u32) -> u32 {
    let bytes = line.as_bytes();
    let start = col0 as usize;
    if start >= bytes.len() {
        return col0 + 1;
    }
    let mut end = start;
    while end < bytes.len() && is_ident_byte(bytes[end]) {
        end += 1;
    }
    if end == start {
        col0 + 1
    } else {
        end as u32
    }
}

/// Extract a leading AutoIt identifier from the start of a diagnostic
/// message. Au3Check messages commonly start with the offending name:
///
/// - `"ThisFunctionDoesNotExist(): undefined function."` → `Some("ThisFunctionDoesNotExist")`
/// - `"$foo: possibly used before declaration."` → `Some("$foo")`
/// - `"@MyMacro: unknown macro."` → `Some("@MyMacro")`
/// - `"Statement cannot be just an expression."` → `Some("Statement")` (false positive; harmless — search won't find a useful match)
/// - `"syntax error"` → `Some("syntax")` (likewise harmless)
/// - `""` → `None`
///
/// Identifier shape: optional `$` or `@` sigil, then one alpha or `_`,
/// then any number of alphanumerics or `_`. ASCII only.
fn extract_leading_identifier(msg: &str) -> Option<&str> {
    let bytes = msg.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut i = 0;
    let first = bytes[0];
    if first == b'$' || first == b'@' {
        i = 1;
        // The sigil alone isn't an identifier; need at least one
        // alpha/underscore after it.
        if i >= bytes.len() || !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
            return None;
        }
    } else if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    Some(&msg[..i])
}

/// Compute the start/end column of the diagnostic range, in 0-based
/// LSP column units. Four-tier fallback:
///
/// 1. **Forward walk** from `col0`: Au3Check landed on the *first* byte of
///    the offending token → walk right and return the full span.
/// 2. **Backward walk** from `col0`: Au3Check landed on the *last* byte of
///    the offending token (e.g. "Statement cannot be just an expression."
///    for a bare `$var` or `@MACRO`) → walk left to find the token start,
///    confirm with a forward walk from there.
/// 3. **Message-based lookup**: extract the leading identifier from the
///    diagnostic message and find it in the line (catches Au3Check's habit
///    of positioning "undefined function" diagnostics past the closing
///    paren — both forward and backward walks fail there).
/// 4. **Single character** at `col0` (worst-case fallback).
///
/// Returns `(start_col, end_col)` so callers can build an LSP Range.
fn diagnostic_range(line: &str, col0: u32, msg: &str) -> (u32, u32) {
    let bytes = line.as_bytes();

    // Tier 1: forward walk. Handles identifiers whose first char is reported.
    if (col0 as usize) < bytes.len() {
        let end = token_end_col(line, col0);
        if end > col0 + 1 {
            return (col0, end);
        }
    }

    // Tier 2: backward walk.  Au3Check sometimes reports the LAST character
    // of the offending token.  Example: bare `$components` on its own line
    // gets "Statement cannot be just an expression." with col pointing at
    // the trailing `s`.  Forward walk from `s` gives exactly 1 char (false),
    // so we walk left instead, find the token start, then confirm forward.
    {
        // If col0 is past EOL treat it as pointing at the last valid byte.
        let effective = if (col0 as usize) >= bytes.len() {
            bytes.len().saturating_sub(1)
        } else {
            col0 as usize
        };
        if !bytes.is_empty() && is_ident_byte(bytes[effective]) {
            let mut start = effective;
            while start > 0 && is_ident_byte(bytes[start - 1]) {
                start -= 1;
            }
            if start < effective {
                // Backed up at least one byte — we're inside a multi-char token.
                let end = token_end_col(line, start as u32);
                if end > start as u32 + 1 {
                    return (start as u32, end);
                }
            }
        }
    }

    // Tier 3: message-based identifier lookup.
    // Case-insensitive: Au3Check normalises function names to their canonical
    // casing in messages (e.g. emits `FileOpen` even when source says
    // `fileopen`).  A case-sensitive find would miss the off-case spelling.
    if let Some(id) = extract_leading_identifier(msg)
        && let Some(byte_start) = find_ascii_case_insensitive(line, id)
    {
        let start = byte_start as u32;
        let end = start + id.len() as u32;
        return (start, end);
    }

    // Tier 4: single character at the reported column.
    (col0, col0 + 1)
}

/// Find the first occurrence of `needle` in `haystack`, comparing
/// case-insensitively in ASCII. Iterates over char boundaries to avoid
/// false matches inside multi-byte UTF-8 sequences in `haystack` (e.g.
/// non-ASCII characters in strings or comments). `needle` is assumed
/// to be ASCII (true for AutoIt identifiers).
fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() {
        return Some(0);
    }
    let haystack_bytes = haystack.as_bytes();
    for (i, _) in haystack.char_indices() {
        let end = i + needle_bytes.len();
        if end > haystack_bytes.len() {
            break;
        }
        if haystack_bytes[i..end].eq_ignore_ascii_case(needle_bytes) {
            return Some(i);
        }
    }
    None
}

/// Parse Au3Check output into LSP diagnostics scoped to `target`.
///
/// `source` is the text of the file being linted — used to size the
/// diagnostic range to the offending token (instead of a single
/// character). Pass `""` if source isn't available; the range falls
/// back to the v0.1 single-character behaviour.
///
/// Au3Check emits diagnostics for `#include`d files too, with their
/// own paths. We filter to only the file we asked about — Zed
/// publishes diagnostics per-URI, and surfacing include-file errors
/// against the current buffer's URI would put squigglies on the
/// wrong lines.
pub fn parse_diagnostics(output: &str, target: &Path, source: &str) -> Vec<Diagnostic> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        // The trailing `\r` from the source regex (loganch/AutoIt-VSCode)
        // isn't needed — `.lines()` strips line terminators for us.
        Regex::new(
            r#"^"(?P<path>.+)"\((?P<line>\d+),(?P<col>\d+)\)\s:\s(?P<sev>warning|error):\s(?P<msg>.+?)\s*$"#,
        )
        .expect("regex literal compiles")
    });

    let target_str = target.to_string_lossy();
    // Indexing into `source.lines()` is O(n) per call; build a vec
    // once so 50 diagnostics in the same file don't trigger 50 full
    // re-scans. Empty source produces an empty vec, which token_end_col
    // handles by falling back to col0+1.
    let source_lines: Vec<&str> = source.lines().collect();
    let mut diags = Vec::new();
    for line in output.lines() {
        let Some(caps) = RE.captures(line) else { continue };
        // Path equality is case-insensitive on Windows; do an ASCII-fold
        // compare since the path bytes here are always ASCII or paths
        // returned verbatim from the OS.
        if !caps["path"].eq_ignore_ascii_case(&target_str) {
            continue;
        }
        // Au3Check is 1-based; LSP positions are 0-based. saturating_sub
        // guards against a hypothetical (0,0) sentinel we've never seen
        // in practice.
        let row: u32 = caps["line"].parse().unwrap_or(1);
        let col: u32 = caps["col"].parse().unwrap_or(1);
        let line0 = row.saturating_sub(1);
        let col0 = col.saturating_sub(1);
        let line_text = source_lines.get(line0 as usize).copied().unwrap_or("");
        let (start_col, end_col) = if line_text.is_empty() {
            (col0, col0 + 1)
        } else {
            diagnostic_range(line_text, col0, &caps["msg"])
        };
        let severity = match &caps["sev"] {
            "error" => DiagnosticSeverity::ERROR,
            "warning" => DiagnosticSeverity::WARNING,
            _ => DiagnosticSeverity::INFORMATION,
        };
        diags.push(Diagnostic {
            range: Range {
                start: Position { line: line0, character: start_col },
                end: Position { line: line0, character: end_col },
            },
            severity: Some(severity),
            source: Some("Au3Check".into()),
            message: caps["msg"].to_string(),
            ..Default::default()
        });
    }
    diags
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> PathBuf {
        // Neutral fixture path — these tests parse synthetic Au3Check
        // output, they don't actually invoke the linter. Path matters
        // only for the case-insensitive match check below.
        PathBuf::from(r"C:\test\sample.au3")
    }

    #[test]
    fn parses_error_line() {
        let out = r#""C:\test\sample.au3"(72,37) : error: syntax error
"C:\test\sample.au3" - 1 error(s), 0 warning(s)
"#;
        // Empty source → range falls back to single character (col+1).
        let diags = parse_diagnostics(out, &target(), "");
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.range.start.line, 71); // 72 - 1
        assert_eq!(d.range.start.character, 36); // 37 - 1
        assert_eq!(d.range.end.character, 37); // col0+1 fallback
        assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(d.message, "syntax error");
        assert_eq!(d.source.as_deref(), Some("Au3Check"));
    }

    #[test]
    fn parses_warning_line() {
        let out = r#""C:\test\sample.au3"(10,5) : warning: $foo: possibly used before declaration.
"#;
        let diags = parse_diagnostics(out, &target(), "");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::WARNING));
        assert!(diags[0].message.contains("possibly used before declaration"));
    }

    #[test]
    fn clean_file_produces_no_diagnostics() {
        let out = r#"AutoIt3 Syntax Checker v3.3.18.0  Copyright (c) 2007-2025 Tylo & AutoIt Team

C:\test\sample.au3 - 0 error(s), 0 warning(s)
"#;
        let diags = parse_diagnostics(out, &target(), "");
        assert!(diags.is_empty());
    }

    #[test]
    fn filters_out_include_file_diagnostics() {
        // Au3Check reports errors from #include'd files with the included
        // file's path. We only want diagnostics for the file we asked
        // about — otherwise squigglies land on the wrong buffer.
        let out = r#""C:\test\helpers.au3"(5,1) : error: missing EndFunc
"C:\test\sample.au3"(72,37) : error: syntax error
"#;
        let diags = parse_diagnostics(out, &target(), "");
        assert_eq!(diags.len(), 1, "only the sample.au3 diagnostic should pass through");
        assert_eq!(diags[0].range.start.line, 71);
    }

    #[test]
    fn case_insensitive_path_match() {
        // Windows paths are case-insensitive. If the buffer URI lowercases
        // a path that Au3Check echoes back in mixed case (or vice versa),
        // we still want the diagnostic to publish.
        let out = r#""c:\test\sample.au3"(1,1) : error: x
"#;
        let diags = parse_diagnostics(out, &target(), "");
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn ignores_summary_and_banner_lines() {
        let out = r#"AutoIt3 Syntax Checker v3.3.18.0
some other unrelated noise
"C:\test\sample.au3" - 1 error(s), 0 warning(s)
"#;
        let diags = parse_diagnostics(out, &target(), "");
        assert!(diags.is_empty());
    }

    // -- A4: multi-character squiggle range --

    #[test]
    fn token_end_col_extends_across_identifier() {
        // Line: `Local $foobar = 5`, diagnostic at col 7 ($)
        let line = "Local $foobar = 5";
        assert_eq!(token_end_col(line, 6), 13); // $foobar spans cols 6..13
    }

    #[test]
    fn token_end_col_handles_at_macro() {
        // Line: `ConsoleWrite(@CRLF)`, diagnostic at the @ macro
        let line = "ConsoleWrite(@CRLF)";
        assert_eq!(token_end_col(line, 13), 18); // @CRLF spans cols 13..18
    }

    #[test]
    fn token_end_col_returns_single_char_on_punctuation() {
        // Diagnostic lands on `(`, a non-identifier char.
        let line = "Func Broken(";
        assert_eq!(token_end_col(line, 11), 12); // just the `(`
    }

    #[test]
    fn token_end_col_handles_past_eol() {
        // Diagnostic position past the end of the line — fallback.
        let line = "short";
        assert_eq!(token_end_col(line, 100), 101);
    }

    #[test]
    fn parse_diagnostics_uses_source_for_range() {
        // Au3Check reports col=7 (1-based) — position of `$` in
        // `Local $foobar = 5`. Forward walk picks up the full
        // identifier.
        let source = "Local $foobar = 5\n";
        let out = format!(
            "\"{}\"(1,7) : warning: $foobar: possibly used before declaration.\n",
            target().to_string_lossy()
        );
        let diags = parse_diagnostics(&out, &target(), source);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.range.start.character, 6); // 7 - 1 (where $foobar starts)
        assert_eq!(d.range.end.character, 13); // walked to end of $foobar
    }

    #[test]
    fn parse_diagnostics_falls_back_to_msg_identifier_at_eol() {
        // The real-world failure: Au3Check positions "undefined
        // function" diagnostics AFTER the closing paren, at EOL.
        // Forward-walk bails out (1-char fallback); message-based
        // lookup finds the function name in the line and uses it
        // as the range instead.
        let source = "ThisFunctionDoesNotExist(\"hello\")\n";
        // Line length = 33; Au3Check reports col=34 (1-based, past EOL).
        let out = format!(
            "\"{}\"(1,34) : error: ThisFunctionDoesNotExist(): undefined function.\n",
            target().to_string_lossy()
        );
        let diags = parse_diagnostics(&out, &target(), source);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        // Repositioned from col 33 to col 0 (start of identifier in
        // the line). Range spans the full identifier.
        assert_eq!(d.range.start.character, 0);
        assert_eq!(d.range.end.character, 24); // len of "ThisFunctionDoesNotExist"
    }

    #[test]
    fn parse_diagnostics_msg_identifier_lookup_is_case_insensitive() {
        // AutoIt identifiers are case-insensitive; Au3Check normalises to
        // the builtin's canonical casing in messages (e.g. "FileOpen"
        // even when the source spells it "fileopen"). The Tier 2 lookup
        // must match across cases so the squiggle lands on the function
        // name, not on the closing paren via Tier 3 fallback.
        let source = "fileopen()\n";
        let out = format!(
            "\"{}\"(1,10) : error: FileOpen() [built-in] called with wrong number of args.\n",
            target().to_string_lossy()
        );
        let diags = parse_diagnostics(&out, &target(), source);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        // Should span "fileopen" (8 chars), not the trailing ')'.
        assert_eq!(d.range.start.character, 0);
        assert_eq!(d.range.end.character, 8);
    }

    #[test]
    fn parse_diagnostics_uses_msg_identifier_for_punctuation_col() {
        // col0 lands on `(` (not an identifier char). Forward walk
        // returns col+1 (1-char). Message starts with the function
        // name, so we relocate to it.
        let source = "foo()\n";
        let out = format!(
            "\"{}\"(1,4) : warning: foo: something.\n",
            target().to_string_lossy()
        );
        let diags = parse_diagnostics(&out, &target(), source);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        // col 4 → col0 = 3 (the `(`). Forward walk: 1 char.
        // Message lookup: "foo" found at col 0, length 3.
        assert_eq!(d.range.start.character, 0);
        assert_eq!(d.range.end.character, 3);
    }

    #[test]
    fn parse_diagnostics_backward_walk_for_bare_expression() {
        // Reproduces the reported bug: bare `$components` on its own line.
        // Au3Check reports col=11 (1-based) = the last character `s` (col0=10).
        // The backward walk should expand the range to cover the full token.
        let source = "$components\n";
        let out = format!(
            "\"{}\"(1,11) : error: Statement cannot be just an expression.\n",
            target().to_string_lossy()
        );
        let diags = parse_diagnostics(&out, &target(), source);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.range.start.character, 0);
        assert_eq!(d.range.end.character, 11); // full `$components`
    }

    #[test]
    fn parse_diagnostics_backward_walk_for_bare_macro() {
        // Same for a bare `@CRLF` — col=5 (1-based) = last char `F` (col0=4).
        let source = "@CRLF\n";
        let out = format!(
            "\"{}\"(1,5) : error: Statement cannot be just an expression.\n",
            target().to_string_lossy()
        );
        let diags = parse_diagnostics(&out, &target(), source);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.range.start.character, 0);
        assert_eq!(d.range.end.character, 5); // full `@CRLF`
    }

    #[test]
    fn parse_diagnostics_single_char_when_no_msg_match() {
        // col0 on whitespace, message identifier not in line —
        // worst-case fallback: single character at reported col.
        let source = "    \n";
        let out = format!(
            "\"{}\"(1,1) : error: syntax error\n",
            target().to_string_lossy()
        );
        let diags = parse_diagnostics(&out, &target(), source);
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        // "syntax" is extracted from msg but won't be found in "    "
        // → fall through to single char at col0=0.
        assert_eq!(d.range.start.character, 0);
        assert_eq!(d.range.end.character, 1);
    }

    // -- extract_leading_identifier behaviour --

    #[test]
    fn extract_leading_identifier_function() {
        assert_eq!(
            extract_leading_identifier("ThisFunctionDoesNotExist(): undefined function."),
            Some("ThisFunctionDoesNotExist")
        );
    }

    #[test]
    fn extract_leading_identifier_variable() {
        assert_eq!(
            extract_leading_identifier("$foo: possibly used before declaration."),
            Some("$foo")
        );
    }

    #[test]
    fn extract_leading_identifier_macro() {
        assert_eq!(
            extract_leading_identifier("@CRLF: something"),
            Some("@CRLF")
        );
    }

    #[test]
    fn extract_leading_identifier_underscored() {
        assert_eq!(
            extract_leading_identifier("_GUICtrlListView_AddItem: argument count."),
            Some("_GUICtrlListView_AddItem")
        );
    }

    #[test]
    fn extract_leading_identifier_returns_none_for_empty() {
        assert!(extract_leading_identifier("").is_none());
    }

    #[test]
    fn extract_leading_identifier_returns_none_for_punctuation_start() {
        assert!(extract_leading_identifier(".something").is_none());
        assert!(extract_leading_identifier("123abc").is_none());
        assert!(extract_leading_identifier("$$invalid").is_none());
    }

    #[test]
    fn extract_leading_identifier_lone_sigil_is_none() {
        // `$` alone isn't a valid identifier.
        assert!(extract_leading_identifier("$").is_none());
        assert!(extract_leading_identifier("@").is_none());
    }

    // -- diagnostic_range integration --

    #[test]
    fn diagnostic_range_forward_walk_wins() {
        // col0 on an identifier — forward walk finds multi-char range,
        // backward walk and message lookup not consulted.
        let line = "Local $foobar = 5";
        let (s, e) = diagnostic_range(line, 6, "irrelevant msg");
        assert_eq!((s, e), (6, 13));
    }

    #[test]
    fn diagnostic_range_backward_walk_at_end_of_variable() {
        // Au3Check reports col at the LAST character of the offending token for
        // "Statement cannot be just an expression." — e.g. a bare `$components`
        // on its own line where col0 = 10 (the `s`, 0-based).
        let line = "$components";
        let (s, e) = diagnostic_range(line, 10, "Statement cannot be just an expression.");
        assert_eq!((s, e), (0, 11));
    }

    #[test]
    fn diagnostic_range_backward_walk_at_end_of_macro() {
        // Same for a bare `@CRLF` — col0 = 4 (the `F`, last char, 0-based).
        let line = "@CRLF";
        let (s, e) = diagnostic_range(line, 4, "Statement cannot be just an expression.");
        assert_eq!((s, e), (0, 5));
    }

    #[test]
    fn diagnostic_range_msg_lookup_at_eol() {
        // Tier 3: backward walk fails (col past EOL lands on `)`, not ident),
        // so message lookup finds the function name.
        let line = "ThisFunctionDoesNotExist(\"hello\")";
        let (s, e) = diagnostic_range(
            line,
            33, // past EOL
            "ThisFunctionDoesNotExist(): undefined function.",
        );
        assert_eq!((s, e), (0, 24));
    }

    #[test]
    fn diagnostic_range_full_fallback() {
        let line = "    ";
        let (s, e) = diagnostic_range(line, 1, "no identifier here");
        assert_eq!((s, e), (1, 2));
    }

    // -- A2: Au3Check flag generation --

    #[test]
    fn build_args_minimal() {
        let target = PathBuf::from(r"C:\tmp\x.au3");
        let cfg = Au3CheckConfig {
            target: &target,
            include_dirs: &[],
            cwd: None,
            extra_args: &[],
        };
        let args = build_args(&cfg);
        // -q, then target.
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "-q");
        assert_eq!(args[1], target.as_os_str());
    }

    #[test]
    fn build_args_with_include_dirs() {
        let target = PathBuf::from(r"C:\tmp\x.au3");
        let inc1 = PathBuf::from(r"C:\proj");
        let inc2 = PathBuf::from(r"C:\lib");
        let include_dirs: &[&Path] = &[&inc1, &inc2];
        let cfg = Au3CheckConfig {
            target: &target,
            include_dirs,
            cwd: None,
            extra_args: &[],
        };
        let args = build_args(&cfg);
        // -q -I C:\proj -I C:\lib <target>
        assert_eq!(args.len(), 6);
        assert_eq!(args[0], "-q");
        assert_eq!(args[1], "-I");
        assert_eq!(args[2], inc1.as_os_str());
        assert_eq!(args[3], "-I");
        assert_eq!(args[4], inc2.as_os_str());
        assert_eq!(args[5], target.as_os_str());
    }

    #[test]
    fn build_args_appends_extra_args_before_target() {
        let target = PathBuf::from(r"C:\tmp\x.au3");
        let extra = vec!["-w".to_string(), "1".to_string(), "-d".to_string()];
        let cfg = Au3CheckConfig {
            target: &target,
            include_dirs: &[],
            cwd: None,
            extra_args: &extra,
        };
        let args = build_args(&cfg);
        // -q -w 1 -d <target>
        assert_eq!(args.len(), 5);
        assert_eq!(args[0], "-q");
        assert_eq!(args[1], "-w");
        assert_eq!(args[2], "1");
        assert_eq!(args[3], "-d");
        assert_eq!(args[4], target.as_os_str());
    }
}
