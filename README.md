# autoit-lsp

A Language Server Protocol implementation for [AutoIt v3](https://www.autoitscript.com/site/autoit/).

Companion to the [zed-autoit](https://github.com/sumit-m/zed-autoit) Zed editor extension. The Zed extension launches `autoit-lsp` as its language server; users typing in `.au3` files get live diagnostics in their editor.

## Status

**v0.3.0 — diagnostics + outline + hover.** Layered on top of the v0.2.1 Au3Check diagnostics baseline:
- An in-memory tree-sitter parse tree per open document (linked from the sibling `tree-sitter-autoit` crate), updated on every edit. Foundation for everything below.
- `textDocument/documentSymbol` walks the tree to emit a hierarchical outline: functions with parameter children, top-level `Global`/`Const` declarations, `Global Enum` members, `#Region` blocks with their contents nested inside.
- `textDocument/hover` looks up identifiers under the cursor in a static catalog of ~3,500 documented AutoIt functions (core builtins + UDF library functions) scraped from the official help, and renders a Markdown popup with signature, summary, parameters, return value, and a documentation link.

Au3Check diagnostics from v0.2.1 still work the same way — refresh on open, save, and after the user stops typing (configurable debounce, default 400ms; in-memory buffer is staged to a temp file so Au3Check can lint unsaved edits).

Still planned: completion (scope-aware), go-to-definition, find-references, cross-file `#include` resolution.

## Requirements

- Windows (Au3Check is a Windows-only executable).
- AutoIt v3 installed.

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
