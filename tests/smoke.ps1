# Local smoke test for autoit-lsp.  *** WIP — DOES NOT PASS YET ***
#
# Builds a minimal LSP frame sequence (initialize + initialized + didOpen)
# as a binary file and pipes it into the server via cmd's stdin
# redirection. Intended to verify a textDocument/publishDiagnostics
# notification appears in stdout, proving the wires are connected end-to-
# end before plugging into Zed.
#
# Current state (2026-05-20): the server's `initialize` request gets a
# proper response, but the `initialized` and `did_open` notification
# handlers never fire — confirmed by a temporary `panic!` in the
# `initialized` handler not crashing the server. tower-lsp 0.20's codec
# logs all three inbound frames, so the bytes reach the server; the
# dispatch never happens. Looks like a tower-lsp ordering quirk when
# stdin EOFs shortly after the frames arrive. End-to-end verification is
# being deferred to "open a .au3 in Zed" (Zed walks the protocol state
# machine correctly).
#
# Notes preserved for the next pass at fixing this:
#   - $proc.StandardInput.BaseStream.Write corrupts the byte stream
#     somehow; cmd redirection works cleanly. Use cmd /c pipelines.
#   - Setting RUST_LOG via $psi.EnvironmentVariables doesn't reliably
#     propagate to the autoit-lsp child through the cmd pipe; use
#     `set RUST_LOG=...` inline in the cmd command or `$env:` in the
#     parent shell.
#   - `(type file & ping -n 3 > nul) | binary` holds stdin open ~2s
#     after frames are delivered, but tower-lsp still doesn't dispatch
#     the queued notifications. Issue likely needs an actual sleep
#     inside Server::serve's main loop, not just keeping the pipe open.
#
# Usage: pwsh -File tests/smoke.ps1 [-Binary <path>]

param(
    [string]$Binary = (Resolve-Path "$PSScriptRoot\..\target\debug\autoit-lsp.exe").Path,
    [string]$TestFile = (Resolve-Path "$PSScriptRoot\smoke_input.au3").Path
)

function Build-LspFrame([string]$Json) {
    $bytes = [System.Text.Encoding]::UTF8.GetByteCount($Json)
    return "Content-Length: $bytes`r`n`r`n$Json"
}

# LSP file URI: file:/// + forward-slashed path with %20 for spaces.
$uri = "file:///" + ($TestFile -replace '\\', '/' -replace ' ', '%20')

$frames =
    (Build-LspFrame (@{
        jsonrpc = '2.0'; id = 1; method = 'initialize'
        params = @{ processId = $PID; rootUri = $null; capabilities = @{} }
    } | ConvertTo-Json -Compress -Depth 5)) +
    (Build-LspFrame (@{
        jsonrpc = '2.0'; method = 'initialized'; params = @{}
    } | ConvertTo-Json -Compress)) +
    (Build-LspFrame (@{
        jsonrpc = '2.0'; method = 'textDocument/didOpen'
        params = @{
            textDocument = @{
                uri = $uri; languageId = 'autoit'; version = 1
                # Read as plain string — Get-Content returns a PSObject
                # that ConvertTo-Json serializes as {value, PSPath, …}.
                text = [System.IO.File]::ReadAllText($TestFile)
            }
        }
    } | ConvertTo-Json -Compress -Depth 5))
# Deliberately NOT sending shutdown/exit. tower-lsp cancels pending
# requests when exit arrives, so the didOpen handler's async work would
# be dropped before publishDiagnostics fires. Instead, we hold stdin
# open via `ping` so the server keeps reading; we kill it from outside
# after collecting the output.

$inputBin = Join-Path $env:TEMP 'autoit-lsp-smoke.bin'
[System.IO.File]::WriteAllBytes($inputBin, [System.Text.Encoding]::UTF8.GetBytes($frames))

# Run via cmd's stdin redirection so the pipe stays raw bytes.
# `timeout` lets us bound the run since the LSP would otherwise wait
# on stdin forever after consuming our frames.
$outFile = Join-Path $env:TEMP 'autoit-lsp-smoke-out.txt'
$errFile = Join-Path $env:TEMP 'autoit-lsp-smoke-err.txt'
# `type ... & ping -n 3 > nul` keeps stdin open ~2s after the frames are
# delivered, giving the server time to process the didOpen notification
# and publish diagnostics before hitting EOF. RUST_LOG is set via cmd's
# inline `set` so the autoit-lsp child inherits it through the pipe.
$cmdArgs = "set RUST_LOG=autoit_lsp=debug,tower_lsp=info & (type `"$inputBin`" & ping 127.0.0.1 -n 3 > nul) | `"$Binary`" > `"$outFile`" 2> `"$errFile`""
$job = Start-Job -ScriptBlock { param($a) cmd /c $a } -ArgumentList $cmdArgs
if (-not (Wait-Job $job -Timeout 7)) { Stop-Job $job }
Receive-Job $job | Out-Null
Remove-Job $job -Force

$stdout = Get-Content -Raw $outFile -ErrorAction SilentlyContinue
$stderr = Get-Content -Raw $errFile -ErrorAction SilentlyContinue

Write-Host "=== stderr (server logs) ===" -ForegroundColor Cyan
Write-Host $stderr
Write-Host "=== stdout (LSP frames) ===" -ForegroundColor Cyan
Write-Host $stdout

if ($stdout -match 'textDocument/publishDiagnostics') {
    Write-Host "`nSMOKE TEST PASSED" -ForegroundColor Green
    exit 0
} else {
    Write-Host "`nSMOKE TEST FAILED: no publishDiagnostics in stdout." -ForegroundColor Red
    exit 1
}
