# scripts/scrape-builtins.ps1
#
# Builds data/builtins.json: a structured catalog of every documented AutoIt v3
# function (core builtins + UDF library functions) scraped from the official
# online help at autoitscript.com/autoit3/docs/.
#
# The autoit-lsp server loads this JSON at startup and serves it via:
#   - textDocument/hover  (Sprint 1 Day 3)
#   - textDocument/completion  (Sprint 3)
#
# Usage:
#   .\scripts\scrape-builtins.ps1
#
# Optional:
#   -DelayMs N     pause between fetches in ms (default 100, polite to the server)
#   -RetryCount N  retries per failed fetch (default 2)
#   -MaxFunctions N  scrape only first N (smoke-test mode); 0 = no limit
#   -OutputFile    override data/builtins.json path
#
# Implementation notes
# - Uses Invoke-WebRequest with -UseBasicParsing (works on PS 5.1 + PS 7,
#   no IE dependency). The .Links property still parses correctly for href
#   discovery; for detail-page field extraction we regex the raw HTML
#   because -UseBasicParsing doesn't populate .ParsedHtml.
# - The AutoIt docs are auto-generated and structurally regular:
#     <h1>FunctionName</h1>
#     <p class="funcdesc">summary text<br /></p>
#     <p class="codeheader">Signature line<br /></p>
#     <h2>Parameters</h2><table>... <tr><td>name</td><td>desc</td></tr> ...</table>
#     <h2>Return Value</h2><table>...</table>
#   so regex against these anchors is reliable in practice. If a page
#   doesn't fit the pattern we log the URL and store nulls — the LSP
#   side falls back to "no info" rather than failing.
# - One-time runtime: ~15-30 min for ~1,500-2,500 functions, sequential.
#   Re-scrape annually or when AutoIt ships a major release.

[CmdletBinding()]
param(
    [int]$DelayMs = 100,
    [int]$RetryCount = 2,
    [int]$MaxFunctions = 0,
    [string]$OutputFile = (Join-Path $PSScriptRoot "..\data\builtins.json"),

    # Cache raw detail-page HTML under %TEMP%\autoit-builtins-cache\ so
    # subsequent runs can re-extract fields without re-fetching from the
    # network. Speeds up extraction-logic iteration from ~17 min to ~30
    # seconds. Pass -RefreshCache to force re-download (e.g. after AutoIt
    # ships a docs update). Cache files are organized by URL path so the
    # layout mirrors `functions/Abs.htm`, `libfunctions/_ArrayAdd.htm`, etc.
    [string]$CacheRoot = (Join-Path $env:TEMP "autoit-builtins-cache"),
    [switch]$RefreshCache
)

$ErrorActionPreference = 'Stop'

$BaseUrl = "https://www.autoitscript.com/autoit3/docs/"
$IndexPages = @(
    @{ Url = "functions.htm";    PathPrefix = "functions/";    Category = "core" },
    @{ Url = "libfunctions.htm"; PathPrefix = "libfunctions/"; Category = "udf"  }
)

# --- HTTP helpers -----------------------------------------------------------

function Get-Page {
    param([string]$Url)
    for ($attempt = 0; $attempt -le $RetryCount; $attempt++) {
        try {
            return Invoke-WebRequest -Uri $Url -UseBasicParsing -TimeoutSec 30
        } catch {
            if ($attempt -eq $RetryCount) { throw }
            Write-Verbose "Retry $($attempt+1) for $Url after error: $_"
            Start-Sleep -Milliseconds 500
        }
    }
}

# Cache stats for end-of-run reporting.
$script:cacheHits   = 0
$script:cacheMisses = 0

# Detail-page fetch with on-disk cache. Reads from $CacheRoot if present
# (and -RefreshCache not set), else fetches from the web, stores the raw
# HTML, and sleeps the politeness delay (only after a real fetch — cache
# hits are local file reads and don't need to throttle the site).
function Get-Detail-Html {
    param(
        [string]$Url,
        [string]$RelativePath
    )
    $cacheFile = Join-Path $CacheRoot $RelativePath
    if (-not $RefreshCache -and (Test-Path $cacheFile)) {
        $script:cacheHits++
        return [System.IO.File]::ReadAllText($cacheFile)
    }

    $script:cacheMisses++
    $resp = Get-Page -Url $Url
    $html = $resp.Content

    $cacheDir = Split-Path $cacheFile -Parent
    if (-not (Test-Path $cacheDir)) {
        New-Item -ItemType Directory -Path $cacheDir -Force | Out-Null
    }
    [System.IO.File]::WriteAllText($cacheFile, $html, [System.Text.UTF8Encoding]::new($false))

    Start-Sleep -Milliseconds $DelayMs
    return $html
}

# --- HTML field extractors --------------------------------------------------

# Strip HTML tags and normalize whitespace, PRESERVING semantic line breaks
# from <br>. Callers that emit markdown convert the surviving `\n` to `<br>`
# (markdown soft breaks render as space — line breaks inside bullet content
# need `<br>` or trailing-two-spaces to render as real breaks).
#
# Order matters:
#   1. <br> → \n (preserve line breaks).
#   2. Other tags stripped (entities still escaped here, so `&lt;Array.au3&gt;`
#      text isn't mistaken for HTML).
#   3. Entities decoded.
#   4. Runs of spaces/tabs collapsed (but not newlines).
#   5. Whitespace adjacent to newlines stripped (so `,\n   text` → `,\ntext`).
#   6. Multiple consecutive newlines collapsed to one.
function Clean-Text {
    param([string]$Html)
    if ([string]::IsNullOrEmpty($Html)) { return $null }
    # 0. Normalize CRLF/CR line endings to LF. AutoIt's docs are CRLF on
    #    disk and `<br />` is typically followed by a real newline, so
    #    without this normalization we end up with `\n\r\n` sequences that
    #    the later `\n+` collapse can't see as consecutive.
    $t = $Html -replace "`r`n?", "`n"
    # 1. <br> variants → newline. Done BEFORE tag-stripping so the structural
    #    info isn't lost.
    $t = $t -replace '<br\s*/?>', "`n"
    # 2. Drop remaining HTML tags.
    $t = $t -replace '<[^>]+>', ' '
    # 3. Decode entities.
    $t = $t -replace '&nbsp;', ' ' `
            -replace '&amp;', '&' `
            -replace '&lt;', '<' `
            -replace '&gt;', '>' `
            -replace '&quot;', '"' `
            -replace '&#39;', "'"
    # 4. Collapse runs of spaces/tabs (not newlines).
    $t = $t -replace '[ \t]+', ' '
    # 5. Strip whitespace adjacent to newlines.
    $t = $t -replace '[ \t]*\n[ \t]*', "`n"
    # 6. Collapse multiple consecutive newlines into one (the source often
    #    has `<br /><br />` for paragraph breaks; we lose that distinction
    #    intentionally — the hover popup doesn't need vertical-space tuning).
    $t = $t -replace "`n+", "`n"
    return $t.Trim()
}

# Pull <h1>NAME</h1> (page title — fallback if we trust the index name more).
function Extract-Name {
    param([string]$Content)
    if ($Content -match '<h1>\s*([^<]+?)\s*</h1>') { return $Matches[1] }
    return $null
}

# Pull <p class="funcdesc">summary<br /></p>.
function Extract-Summary {
    param([string]$Content)
    if ($Content -match '<p\s+class="funcdesc">(.*?)</p>') {
        return Clean-Text $Matches[1]
    }
    return $null
}

# Pull <p class="codeheader">...</p>. UDF library pages have *two* lines inside
# the codeheader:
#     #include <Lib.au3>
#     _LibFunc ( ... )
# Core function pages have only the signature line. Split on <br /> and emit:
#   include   — text of the #include directive (or null for core)
#   signature — the function signature line (always present)
# If we accidentally end up with one line that itself starts with `#include `
# (defensive), peel that prefix off so the signature stays clean.
function Extract-CodeHeader {
    param([string]$Content)
    if ($Content -notmatch '(?s)<p\s+class="codeheader">(.*?)</p>') {
        return [PSCustomObject]@{ include = $null; signature = $null }
    }
    $raw = $Matches[1]
    # Normalize <br /> / <br> / <br/> to a single delimiter, then split.
    $normalized = $raw -replace '<br\s*/?>', "`n"
    $lines = $normalized -split "`n" `
        | ForEach-Object { Clean-Text $_ } `
        | Where-Object { $_ -ne $null -and $_ -ne '' }

    $include   = $null
    $signature = $null
    foreach ($line in $lines) {
        if ($null -eq $signature -and $line -match '^#include\b') {
            $include = $line
        } elseif ($null -eq $signature) {
            $signature = $line
        }
    }
    # Defensive: if signature still starts with `#include ` (collapsed page
    # variant), peel it off into include.
    if ($signature -and $signature -match '^(#include\s+\S+)\s+(.+)$') {
        $include   = $Matches[1]
        $signature = $Matches[2]
    }
    return [PSCustomObject]@{ include = $include; signature = $signature }
}

# Pull the first <table>...</table> after <h2>SectionName</h2>, stopping at
# the next <h2> or end of body. Returns the raw table HTML for further parsing
# (or $null if the section / table isn't present).
function Extract-SectionTable {
    param(
        [string]$Content,
        [string]$SectionName
    )
    $headerPattern = "<h2>\s*$([regex]::Escape($SectionName))\s*</h2>"
    $idx = [regex]::Match($Content, $headerPattern, 'IgnoreCase')
    if (-not $idx.Success) { return $null }
    $after = $Content.Substring($idx.Index + $idx.Length)
    # Cut off at next <h2> so we don't pick up tables from later sections.
    $nextH2 = [regex]::Match($after, '<h2>')
    if ($nextH2.Success) {
        $after = $after.Substring(0, $nextH2.Index)
    }
    # Grab first <table>...</table>.
    $tableMatch = [regex]::Match($after, '(?s)<table[^>]*>(.*?)</table>')
    if ($tableMatch.Success) {
        return $tableMatch.Groups[1].Value
    }
    return $null
}

# Parse param rows from the Parameters <table>. Each <tr> has two <td>s:
# name, description. Some rows label optional params with <strong>[optional]</strong>
# inside the description; we keep that text as-is — it's useful context.
function Extract-Parameters {
    param([string]$Content)
    $tableBody = Extract-SectionTable -Content $Content -SectionName 'Parameters'
    if (-not $tableBody) { return @() }

    $params = New-Object System.Collections.Generic.List[object]
    $rowPattern = '(?s)<tr[^>]*>\s*<td[^>]*>(.*?)</td>\s*<td[^>]*>(.*?)</td>\s*</tr>'
    foreach ($m in [regex]::Matches($tableBody, $rowPattern)) {
        $name = Clean-Text $m.Groups[1].Value
        $desc = Clean-Text $m.Groups[2].Value
        # Skip header rows that have <strong>-wrapped column labels.
        if ($name -match '^(Constant Name|Button Pressed)$') { continue }
        # Leave embedded `\n` in the description as-is. The hover formatter
        # converts them to markdown hard-break sequences when emitting (the
        # renderer escapes raw `<br>` to literal text, so we go through
        # CommonMark `  \n` for hard breaks instead).
        $params.Add([PSCustomObject]@{ name = $name; description = $desc })
    }
    # Use .ToArray() rather than @($params): wrapping a Generic.List<T> with the
    # @() operator throws ArgumentException "Argument types do not match" on
    # Windows PowerShell 5.1. .ToArray() is the safe equivalent.
    return ,$params.ToArray()
}

# Pull the Return Value section. Core function pages have a structured 2-col
# `<table>` (Success: / Failure: rows) immediately under `<h2>Return Value</h2>`,
# often followed by SUPPLEMENTARY tables (button constants, error codes) that
# aren't return-value info — concatenating everything together produces an
# unreadable blob, so we extract only the first table and format it as
# markdown bullets. UDF pages typically use a single paragraph instead;
# we fall back to flat text for those.
#
# Output format:
#   Structured pages → "- Success: the ID of the button pressed.\n- Failure: $IDTIMEOUT (-1) ..."
#   Paragraph pages  → "Returns 1 on success and 0 on failure."
#
# The hover formatter renders this on its own line after `**Returns:**`, so
# markdown bullets become a real bullet list.
function Extract-ReturnValue {
    param([string]$Content)
    $headerPattern = '<h2>\s*Return\s+Value\s*</h2>'
    $idx = [regex]::Match($Content, $headerPattern, 'IgnoreCase')
    if (-not $idx.Success) { return $null }
    $after = $Content.Substring($idx.Index + $idx.Length)
    $nextH2 = [regex]::Match($after, '<h2>')
    if ($nextH2.Success) {
        $after = $after.Substring(0, $nextH2.Index)
    }

    # First try: structured 2-column table form. Extract just the first
    # `<table>...</table>` after the heading — anything after that on the
    # same page is supplementary (button constants, decimal/hex lookups, etc.)
    # and not part of the return-value contract.
    $tableMatch = [regex]::Match($after, '(?s)<table[^>]*>(.*?)</table>')
    if ($tableMatch.Success) {
        $tableBody = $tableMatch.Groups[1].Value
        $rowPattern = '(?s)<tr[^>]*>\s*<td[^>]*>(.*?)</td>\s*<td[^>]*>(.*?)</td>\s*</tr>'
        $rows = [regex]::Matches($tableBody, $rowPattern)
        if ($rows.Count -gt 0) {
            $lines = New-Object System.Collections.Generic.List[string]
            foreach ($r in $rows) {
                $label = Clean-Text $r.Groups[1].Value
                $value = Clean-Text $r.Groups[2].Value
                # Skip header rows where the cells are column labels in <strong>
                # (autoitscript.com mixes these into the value table sometimes).
                if (-not $label -or -not $value) { continue }
                if ($label -match '^(Constant Name|Button Pressed)$') { continue }
                # AutoIt's label cells already include the trailing colon
                # ("Success:", "Failure:"), so we don't need to add one.
                # Embedded newlines in $value (rare) stay as `\n`; the hover
                # formatter handles them.
                $lines.Add("- $label $value")
            }
            if ($lines.Count -gt 0) {
                return ($lines -join "`n")
            }
        }
    }

    # Fallback: no usable table, treat the whole region as paragraph text.
    # Embedded `\n` stays in the JSON; hover formatter converts to markdown
    # hard breaks at render time.
    return Clean-Text $after
}

# --- Index page discovery --------------------------------------------------

function Get-FunctionLinks {
    param(
        [string]$IndexUrl,
        [string]$PathPrefix
    )
    $resp = Get-Page -Url $IndexUrl
    # .Links is populated even with -UseBasicParsing.
    $links = $resp.Links `
        | Where-Object { $_.href -and $_.href -like "$PathPrefix*" -and $_.href -like '*.htm' } `
        | ForEach-Object {
            # outerHTML is the inner text of <a>. Strip any nested tags
            # (rare but possible) and use as the canonical function name.
            $name = Clean-Text $_.outerHTML
            # If Clean-Text returned the whole outerHTML (because the tag
            # filter zeroed it out), fall back to deriving the name from the
            # href: PathPrefix/Name.htm -> Name.
            if (-not $name -or $name -match '<') {
                $name = [System.IO.Path]::GetFileNameWithoutExtension($_.href)
            }
            [PSCustomObject]@{
                Name = $name
                Href = $_.href
            }
        } `
        | Sort-Object Name -Unique
    return @($links)
}

# --- Main loop --------------------------------------------------------------

$startedAt = Get-Date
$result = [ordered]@{}
$failed = New-Object System.Collections.Generic.List[object]

foreach ($idx in $IndexPages) {
    $indexUrl = $BaseUrl + $idx.Url
    Write-Host ""
    Write-Host "Discovering functions on $indexUrl ..."
    $links = Get-FunctionLinks -IndexUrl $indexUrl -PathPrefix $idx.PathPrefix
    Write-Host "  Found $($links.Count) function links."

    if ($MaxFunctions -gt 0 -and $links.Count -gt $MaxFunctions) {
        $links = $links | Select-Object -First $MaxFunctions
        Write-Host "  Limiting to first $MaxFunctions (smoke-test mode)."
    }

    $i = 0
    foreach ($link in $links) {
        $i++
        if (($i % 50) -eq 0) {
            $elapsed = (Get-Date) - $startedAt
            $rate = if ($elapsed.TotalSeconds -gt 0) { [math]::Round($result.Count / $elapsed.TotalSeconds, 1) } else { 0 }
            Write-Host "  $i / $($links.Count) ($rate funcs/sec)"
        }

        $detailUrl = $BaseUrl + $link.Href
        try {
            # Cached fetch — first run populates the cache, later runs read
            # locally so re-extraction takes seconds instead of minutes.
            $html = Get-Detail-Html -Url $detailUrl -RelativePath $link.Href
            $header = Extract-CodeHeader -Content $html
            # `[object[]]` cast forces ConvertTo-Json to serialize as an
            # array even when the parameter list has 0 or 1 entries.
            # Without it PowerShell 5.1 collapses 0-element arrays to `{}`
            # and 1-element arrays to a bare object, breaking strict
            # consumers (serde_json sees a type mismatch and bails out).
            $entry = [ordered]@{
                name        = $link.Name
                category    = $idx.Category
                url         = $detailUrl
                include     = $header.include
                signature   = $header.signature
                summary     = Extract-Summary -Content $html
                parameters  = [object[]](Extract-Parameters -Content $html)
                returnValue = Extract-ReturnValue -Content $html
            }
            # Lowercase key for case-insensitive lookup. AutoIt itself is
            # case-insensitive; the canonical name is stored in `name`.
            $key = $link.Name.ToLowerInvariant()
            if ($result.Contains($key)) {
                Write-Verbose "Duplicate key '$key' (already from $($result[$key].category)); keeping first."
            } else {
                $result[$key] = $entry
            }
        } catch {
            Write-Host "  ! Failed $detailUrl : $_" -ForegroundColor Yellow
            $failed.Add([PSCustomObject]@{ Url = $detailUrl; Error = $_.Exception.Message })
        }
        # No outer sleep — politeness delay is inside Get-Detail-Html, so cache
        # hits proceed at memory speed and only real fetches throttle.
    }
}

$elapsed = (Get-Date) - $startedAt

Write-Host ""
Write-Host "=== Summary ==="
Write-Host "Total functions: $($result.Count)"
Write-Host "Failed:          $($failed.Count)"
Write-Host "Cache:           $($script:cacheHits) hits, $($script:cacheMisses) fetches"
Write-Host "Cache root:      $CacheRoot"
Write-Host "Elapsed:         $([math]::Round($elapsed.TotalSeconds, 1))s"

if ($failed.Count -gt 0) {
    Write-Host ""
    Write-Host "Failed URLs:"
    $failed | ForEach-Object { Write-Host "  $($_.Url)  -> $($_.Error)" }
}

# Wrap in a top-level object so the JSON can grow new sibling keys later
# (version, scrapedAt, sourceVersion, etc.) without breaking consumers.
$payload = [ordered]@{
    scrapedAt    = (Get-Date).ToString('o')
    sourceUrl    = $BaseUrl
    functions    = $result
    functionCount = $result.Count
}

# Ensure data dir exists.
$outDir = Split-Path $OutputFile -Parent
if (-not (Test-Path $outDir)) {
    New-Item -ItemType Directory -Path $outDir | Out-Null
}

# ConvertTo-Json default depth is 2; we have nested arrays inside the
# function objects so bump it.
#
# Write via WriteAllText with UTF-8-without-BOM. Set-Content -Encoding utf8
# emits a BOM on Windows PowerShell 5.1, which serde_json::from_str rejects
# (it expects a leading `{`, not 0xEF 0xBB 0xBF). PS 7 has -Encoding utf8NoBOM
# but we want this script to work on both versions, so go through .NET directly.
$json = $payload | ConvertTo-Json -Depth 10
[System.IO.File]::WriteAllText($OutputFile, $json, [System.Text.UTF8Encoding]::new($false))

Write-Host ""
Write-Host "Wrote $OutputFile"
