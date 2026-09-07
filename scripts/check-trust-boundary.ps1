<#
.SYNOPSIS
    Enforces the trust boundary of spec sec.3.8 and sec.4.

.DESCRIPTION
    Three properties, checked mechanically because a comment cannot enforce them:

      1. `quarrel-live` -- the only crate that reads PRIVATE_KEY or signs -- is depended on
         by `quarrel-app` and by nothing else. In particular NOT by `quarrel-cli`, which
         is what makes "no CLI command needs a key" (sec.10, PLAN.md C6) a fact rather than
         a promise.

      2. `quarrel-core` has no network and no database dependency, so the rule engine and
         curve math stay fully testable offline (sec.4, phase 1).

      3. No crate outside `crates/live/` mentions PRIVATE_KEY.

    Exits non-zero on violation. Run from the repository root.
#>

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$failures = @()

Write-Host "== trust boundary ==" -ForegroundColor Cyan

# --- 1. who depends on quarrel-live? ------------------------------------------------
$tree = cargo tree --invert quarrel-live --edges normal 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Host $tree
    throw "cargo tree failed; cannot verify the trust boundary"
}

# Package names appear as the first token of each line, after the tree drawing glyphs.
$dependents = @()
foreach ($line in $tree) {
    # Lines look like "|-- quarrel-app v0.1.0 (C:\...)" with box-drawing glyphs that
    # vary by terminal. Matching the first "<name> v<digit>" pair is glyph-agnostic.
    if ($line -match '([a-z0-9_\-]+)\s+v\d+\.') {
        $name = $Matches[1]
        if ($name -ne 'quarrel-live') { $dependents += $name }
    }
}
$dependents = $dependents | Sort-Object -Unique

$allowed = @('quarrel-app')
$illegal = $dependents | Where-Object { $allowed -notcontains $_ }

if ($illegal) {
    $failures += "quarrel-live is depended on by: $($illegal -join ', '). Only 'quarrel-app' may depend on it (spec sec.3.8)."
} else {
    Write-Host "  ok  quarrel-live dependents: $($dependents -join ', ')" -ForegroundColor Green
}

# --- 2. quarrel-core stays offline --------------------------------------------------
$coreTree = cargo tree --package quarrel-core --edges normal 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Host $coreTree
    throw "cargo tree failed for quarrel-core"
}

# Crates that would mean core has grown an I/O dependency. alloy-primitives is
# deliberately allowed: it is U256 and Address, pure types with no network.
$banned = @(
    'tokio', 'reqwest', 'hyper', 'rusqlite', 'libsqlite3-sys', 'tauri',
    'alloy-provider', 'alloy-transport', 'alloy-transport-http', 'alloy-rpc-client',
    'ureq', 'curl', 'tungstenite', 'tokio-tungstenite'
)
$found = @()
foreach ($line in $coreTree) {
    foreach ($b in $banned) {
        if ($line -match "(^|[^\w\-])$([regex]::Escape($b))\s+v\d") { $found += $b }
    }
}
$found = $found | Sort-Object -Unique

if ($found) {
    $failures += "quarrel-core has acquired I/O dependencies: $($found -join ', '). It must stay pure (spec sec.4)."
} else {
    Write-Host "  ok  quarrel-core has no network or database dependency" -ForegroundColor Green
}

# --- 3. the key is named in exactly one crate ---------------------------------------
$keyHits = Get-ChildItem -Path 'crates' -Recurse -Include *.rs -File |
    Where-Object { $_.FullName -notmatch '\\crates\\live\\' } |
    Select-String -Pattern 'PRIVATE_KEY' -SimpleMatch

if ($keyHits) {
    foreach ($h in $keyHits) { $failures += "PRIVATE_KEY referenced outside crates/live: $($h.Path):$($h.LineNumber)" }
} else {
    Write-Host "  ok  PRIVATE_KEY appears only under crates/live" -ForegroundColor Green
}

# --- verdict ------------------------------------------------------------------------
if ($failures) {
    Write-Host ""
    Write-Host "TRUST BOUNDARY VIOLATED" -ForegroundColor Red
    foreach ($f in $failures) { Write-Host "  - $f" -ForegroundColor Red }
    exit 1
}

Write-Host "trust boundary intact" -ForegroundColor Green
exit 0
