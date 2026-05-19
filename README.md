# autoit-lsp

A Language Server Protocol implementation for [AutoIt v3](https://www.autoitscript.com/site/autoit/).

Companion to the [zed-autoit](https://github.com/smadan/zed-autoit) Zed editor extension. The Zed extension launches `autoit-lsp` as its language server; users typing in `.au3` files get live diagnostics in their editor.

## Status

**v0.1 — diagnostics only (Phase 7 Option 1).** The server wraps AutoIt's official linter, `Au3Check.exe`, and surfaces its output as LSP `textDocument/publishDiagnostics`. No completion, hover, or go-to-definition yet — those are planned for v0.2+ as the tree-sitter parse tree gets wired in (see the zed-autoit repo's `PLAN.md` Phase 7 v0.3+ roadmap).

## Requirements

- Windows (Au3Check is a Windows-only executable).
- AutoIt v3 installed. Default path probed: `C:\Program Files (x86)\AutoIt3\Au3Check.exe`. Override via the LSP `initializationOptions.au3check_path` field.

## Build

```sh
cargo build --release
```

Produces `target\release\autoit-lsp.exe`. The Zed extension downloads this binary from a tagged GitHub release at install time via `zed_extension_api::latest_github_release()`.

## Acknowledgments

The Au3Check invocation pattern and stdout parsing regex are adapted from [loganch/AutoIt-VSCode](https://github.com/loganch/AutoIt-VSCode) (MIT, Copyright (c) 2018 Logan Hampton). Their prior art validated that wrapping Au3Check produces useful diagnostics and let us skip a lot of trial-and-error around output format and edge cases.

## License

MIT — see [LICENSE](LICENSE).
