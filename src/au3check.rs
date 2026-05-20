//! Au3Check.exe wrapper — discover the binary, invoke it, parse its
//! output into LSP `Diagnostic` structs.
//!
//! Au3Check is AutoIt's official syntax linter. We shell out to it and
//! re-publish its errors/warnings as LSP diagnostics. Invocation form:
//!
//! ```text
//! Au3Check.exe -q <script-path>
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

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use tokio::process::Command;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

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
        if let Ok(key) = hklm.open_subkey(sub) {
            if let Ok(dir) = key.get_value::<String, _>("InstallDir") {
                return Some(PathBuf::from(dir));
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn install_dir_from_registry() -> Option<PathBuf> {
    None
}

/// Run Au3Check on `target` and return its captured stdout.
///
/// `-q` suppresses the banner. Each entry in `include_dirs` is passed
/// as a separate `-I <path>` so quoted `#include "x.au3"` directives
/// fall through to the original file's directory when we're linting a
/// staged temp file. `cwd`, if provided, becomes the process working
/// directory — set it to the original file's directory as a fallback
/// for any cwd-relative resolution paths.
///
/// We don't pass `-d` (debug) or any `-w<n>` warning flags — defaults
/// are good enough for v0.2. Future versions may surface these as LSP
/// settings.
pub async fn run_au3check(
    au3check: &Path,
    target: &Path,
    include_dirs: &[&Path],
    cwd: Option<&Path>,
) -> std::io::Result<String> {
    let mut cmd = Command::new(au3check);
    cmd.arg("-q");
    for dir in include_dirs {
        cmd.arg("-I").arg(dir);
    }
    cmd.arg(target);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let output = cmd.output().await?;
    // Au3Check writes diagnostics to stdout; stderr is usually empty.
    // Use lossy decode — script paths can contain non-UTF8 bytes on
    // Windows codepages but our parser only cares about the ASCII parts.
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse Au3Check output into LSP diagnostics scoped to `target`.
///
/// Au3Check emits diagnostics for `#include`d files too, with their own
/// paths. We filter to only the file we asked about — Zed publishes
/// diagnostics per-URI, and surfacing include-file errors against the
/// current buffer's URI would put squigglies on the wrong lines.
pub fn parse_diagnostics(output: &str, target: &Path) -> Vec<Diagnostic> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        // The trailing `\r` from the source regex (loganch/AutoIt-VSCode)
        // isn't needed — `.lines()` strips line terminators for us.
        Regex::new(
            r#"^"(?P<path>.+)"\((?P<line>\d+),(?P<col>\d+)\)\s:\s(?P<sev>warning|error):\s(?P<msg>.+?)\s*$"#,
        )
        .expect("regex literal compiles")
    });

    let target_str = target.to_string_lossy();
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
        let severity = match &caps["sev"] {
            "error" => DiagnosticSeverity::ERROR,
            "warning" => DiagnosticSeverity::WARNING,
            _ => DiagnosticSeverity::INFORMATION,
        };
        diags.push(Diagnostic {
            range: Range {
                start: Position { line: line0, character: col0 },
                end: Position { line: line0, character: col0 + 1 },
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
        PathBuf::from(r"C:\Users\UPN\Documents\claude\zed-autoit\samples\hello.au3")
    }

    #[test]
    fn parses_error_line() {
        let out = r#""C:\Users\UPN\Documents\claude\zed-autoit\samples\hello.au3"(72,37) : error: syntax error
"C:\Users\UPN\Documents\claude\zed-autoit\samples\hello.au3" - 1 error(s), 0 warning(s)
"#;
        let diags = parse_diagnostics(out, &target());
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.range.start.line, 71); // 72 - 1
        assert_eq!(d.range.start.character, 36); // 37 - 1
        assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(d.message, "syntax error");
        assert_eq!(d.source.as_deref(), Some("Au3Check"));
    }

    #[test]
    fn parses_warning_line() {
        let out = r#""C:\Users\UPN\Documents\claude\zed-autoit\samples\hello.au3"(10,5) : warning: $foo: possibly used before declaration.
"#;
        let diags = parse_diagnostics(out, &target());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::WARNING));
        assert!(diags[0].message.contains("possibly used before declaration"));
    }

    #[test]
    fn clean_file_produces_no_diagnostics() {
        let out = r#"AutoIt3 Syntax Checker v3.3.18.0  Copyright (c) 2007-2025 Tylo & AutoIt Team

C:\Users\UPN\Documents\claude\zed-autoit\samples\hello.au3 - 0 error(s), 0 warning(s)
"#;
        let diags = parse_diagnostics(out, &target());
        assert!(diags.is_empty());
    }

    #[test]
    fn filters_out_include_file_diagnostics() {
        // Au3Check reports errors from #include'd files with the included
        // file's path. We only want diagnostics for the file we asked
        // about — otherwise squigglies land on the wrong buffer.
        let out = r#""C:\Users\UPN\Documents\claude\zed-autoit\samples\helpers.au3"(5,1) : error: missing EndFunc
"C:\Users\UPN\Documents\claude\zed-autoit\samples\hello.au3"(72,37) : error: syntax error
"#;
        let diags = parse_diagnostics(out, &target());
        assert_eq!(diags.len(), 1, "only the hello.au3 diagnostic should pass through");
        assert_eq!(diags[0].range.start.line, 71);
    }

    #[test]
    fn case_insensitive_path_match() {
        // Windows paths are case-insensitive. If the buffer URI lowercases
        // a path that Au3Check echoes back in mixed case (or vice versa),
        // we still want the diagnostic to publish.
        let out = r#""c:\users\upn\documents\claude\zed-autoit\samples\hello.au3"(1,1) : error: x
"#;
        let diags = parse_diagnostics(out, &target());
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn ignores_summary_and_banner_lines() {
        let out = r#"AutoIt3 Syntax Checker v3.3.18.0
some other unrelated noise
"C:\Users\UPN\Documents\claude\zed-autoit\samples\hello.au3" - 1 error(s), 0 warning(s)
"#;
        let diags = parse_diagnostics(out, &target());
        assert!(diags.is_empty());
    }
}
