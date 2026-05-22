//! autoit-run — thin launcher that streams AutoIt3.exe stdout to the
//! calling terminal.
//!
//! # Why this binary exists
//!
//! AutoIt3.exe is built as a Windows GUI-subsystem executable
//! (SUBSYSTEM:WINDOWS). `ConsoleWrite()` only writes to its stdout when it
//! receives a real *pipe* handle — not a console handle. When Zed's task
//! runner invokes a program via PowerShell, the spawned process inherits
//! a console handle for stdout, so `ConsoleWrite()` output is silently
//! discarded.
//!
//! This binary is a CONSOLE-subsystem wrapper. It spawns AutoIt3.exe with
//! `Stdio::piped()` (a real pipe for stdout), then reads and re-prints each
//! line as it arrives. The result: `ConsoleWrite()` output streams to Zed's
//! terminal in real time, exactly as it does in SciTE.
//!
//! # Usage
//!
//! ```text
//! autoit-run <script.au3> [script-args...]
//! ```
//!
//! # Discovery (from tasks.json, in priority order)
//!
//! 1. PATH: `autoit-run` found via `Get-Command` — dev / power-user override.
//! 2. Registry: `HKCU\SOFTWARE\zed-autoit\RunnerPath` — written by
//!    `autoit-lsp.exe` at startup when it finds `autoit-run.exe` in its own
//!    directory (i.e. the Zed extension cache after download).
//!
//! # Platform note
//!
//! `autoit-run.exe` is Windows-only. AutoIt itself only runs on Windows, so
//! there are no Linux/macOS builds of this binary in the CI matrix.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

// ─── AutoIt3.exe discovery ────────────────────────────────────────────────────
//
// Mirrors the lookup chain in `au3check.rs` and `tasks.json`:
//   1. HKLM\SOFTWARE\WOW6432Node\AutoIt v3\AutoIt  (64-bit Windows, ~all users)
//   2. HKLM\SOFTWARE\AutoIt v3\AutoIt              (32-bit Windows, rare)
//   3. C:\Program Files (x86)\AutoIt3              (hardcoded fallback)

#[cfg(windows)]
fn install_dir_from_registry() -> Option<PathBuf> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

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

/// Resolve the full path to `AutoIt3.exe`. Registry lookup first, then the
/// canonical default install path.
fn discover_autoit3() -> Option<PathBuf> {
    if let Some(dir) = install_dir_from_registry() {
        let candidate = dir.join("AutoIt3.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let fallback = PathBuf::from(r"C:\Program Files (x86)\AutoIt3\AutoIt3.exe");
    fallback.is_file().then_some(fallback)
}

// ─── Entry point ─────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("autoit-run: usage: autoit-run <script.au3> [args...]");
        std::process::exit(1);
    }

    let Some(autoit3) = discover_autoit3() else {
        eprintln!(
            "autoit-run: AutoIt3.exe not found. \
             Install AutoIt v3 from https://www.autoitscript.com, \
             or set the au3checkPath LSP setting to point to your install."
        );
        std::process::exit(1);
    };

    let mut cmd = Command::new(&autoit3);
    // Forward the script path and any additional arguments the user passes.
    cmd.args(&args[1..]);
    // Pipe stdout so we can stream it; inherit stderr so any AutoIt3.exe
    // error messages appear directly in the terminal without buffering.
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("autoit-run: failed to spawn {}: {e}", autoit3.display());
            std::process::exit(1);
        }
    };

    // Stream stdout line by line as it arrives. `BufReader::lines()` handles
    // both `\n` and `\r\n` line endings and strips them — clean output
    // regardless of what the AutoIt script writes via ConsoleWrite().
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => println!("{l}"),
                Err(_) => break,
            }
        }
    }

    // Mirror AutoIt3.exe's exit code so callers (and Zed's task runner) can
    // detect script failures. Fall back to 1 if the OS gives us nothing.
    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("autoit-run: error waiting for AutoIt3.exe: {e}");
            std::process::exit(1);
        }
    };
    std::process::exit(status.code().unwrap_or(1));
}
