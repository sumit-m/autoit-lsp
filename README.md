# autoit-lsp

A Language Server Protocol implementation for [AutoIt v3](https://www.autoitscript.com/site/autoit/).

Companion to the [zed-autoit](https://github.com/sumit-m/zed-autoit) Zed editor extension. The Zed extension launches `autoit-lsp` as its language server; users typing in `.au3` files get live diagnostics in their editor.

## Status

**v0.6.0 — full-featured language server.** Built on an in-memory tree-sitter parse tree per open document (linked from the sibling `tree-sitter-autoit` crate), updated on every edit, plus a static catalog of ~3,500 documented AutoIt functions (core builtins + UDF library) scraped from the official help, a per-file symbol index, an `#include`-graph workspace index, and a project-wide index of every `.au3` under the workspace root.

LSP capabilities served:

- **Diagnostics** (`publishDiagnostics`) — wraps `Au3Check.exe`; refresh on open, save, and after the user stops typing (configurable debounce, default 400ms; the in-memory buffer is staged to a temp file so Au3Check can lint unsaved edits). Extra Au3Check flags via the `au3checkExtraArgs` setting.
- **Hover** (`textDocument/hover`) — catalog popup for builtins/UDF-library functions; signature popup with doc-comments for user-defined functions.
- **Document symbols** (`textDocument/documentSymbol`) — hierarchical outline.
- **Go-to-definition / find-references** (`textDocument/definition`, `textDocument/references`) — within the file, across the `#include` graph, and project-wide (upward callers in files that include the current one).
- **Completion** (`textDocument/completion`) — scope-aware: variables, macros, functions, cross-file symbols, and `#include` paths.
- **Signature help** (`textDocument/signatureHelp`) and **inlay hints** (`textDocument/inlayHint`) — parameter names for builtins and UDFs.
- **Code actions** (`textDocument/codeAction`) — add missing `#include`, fix function casing.
- **Formatting** (`textDocument/formatting`) — via AutoIt3Wrapper's `/Tidy` mode.
- **Document highlight** (`textDocument/documentHighlight`) — same-symbol read/write highlighting.
- **Folding ranges** (`textDocument/foldingRange`) — functions, regions, control blocks, block comments.
- **Document color** (`textDocument/documentColor` + `colorPresentation`) — swatches for `0x…` literals in color-taking GUI calls, per-function RGB/BGR decoding.
- **Semantic tokens** (`textDocument/semanticTokens/full`) — UDF vs builtin, parameter/local/global/const/macro classification.
- **Call hierarchy** (`textDocument/prepareCallHierarchy` + incoming/outgoing) — dual-index (`#include` graph + project-wide).
- **`workspace/didChangeWatchedFiles`** — keeps the project index fresh as `.au3` files change on disk.

Diagnostics, formatting, and task execution are Windows-only (they invoke AutoIt's Windows-only `.exe` binaries); every other capability is cross-platform.

## Requirements

- The server builds and runs on Windows, Linux, and macOS.
- Windows + AutoIt v3 installed is required for the diagnostics, formatting, and task features (they invoke AutoIt's Windows-only `.exe` binaries). All other features work on any platform.

## Au3Check discovery

The server probes for `Au3Check.exe` in this order:

1. The `initializationOptions.au3checkPath` setting (if the client provides one).
2. `HKLM\SOFTWARE\WOW6432Node\AutoIt v3\AutoIt\InstallDir` (where the standard AutoIt installer writes its location on 64-bit Windows).
3. `HKLM\SOFTWARE\AutoIt v3\AutoIt\InstallDir` (32-bit Windows fallback).
4. The canonical default install path: `C:\Program Files (x86)\AutoIt3\Au3Check.exe`.

Users who installed AutoIt with the official MSI fall under (2) or (3) — no configuration needed. Users with a portable / unzipped AutoIt at a non-default location should set `au3checkPath` in their editor.

In Zed, add this to your `%APPDATA%\Zed\settings.json` (or workspace `.zed/settings.json`):

```json
{
  "lsp": {
    "autoit-lsp": {
      "settings": {
        "au3checkPath": "D:\\Tools\\AutoIt3\\Au3Check.exe"
      }
    }
  }
}
```

The server also accepts the same payload via `initializationOptions` for LSP clients that forward init-time options — but the `settings` key above is the path that's been verified to work with current Zed.

If `au3checkPath` is set but points to a file that doesn't exist, the server logs a warning and falls through to the registry / default discovery — so a stale setting doesn't break the LSP for users who later install AutoIt normally.

## Build

```sh
cargo build --release
```

Produces `target\release\autoit-lsp.exe`. The Zed extension downloads this binary from a tagged GitHub release at install time via `zed_extension_api::latest_github_release()`.

## Acknowledgments

The Au3Check invocation pattern and stdout parsing regex are adapted from [loganch/AutoIt-VSCode](https://github.com/loganch/AutoIt-VSCode) (MIT, Copyright (c) 2018 Logan Hampton). Their prior art validated that wrapping Au3Check produces useful diagnostics and let us skip a lot of trial-and-error around output format and edge cases.

The built-in function metadata served by hover (and used later for completion) is derived from the official AutoIt v3 documentation at <https://www.autoitscript.com/autoit3/docs/> via the scraper in `scripts/scrape-builtins.ps1` — only structured fields (name, signature, parameters, summary, return value) are extracted, not the full prose. Re-run the scraper after major AutoIt releases to refresh `data/builtins.json`.

## License

MIT — see [LICENSE](LICENSE).
