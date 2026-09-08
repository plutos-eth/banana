<#
.SYNOPSIS
    Enforces the trust boundary of spec sec.3.8 and sec.4.

.DESCRIPTION
    Three properties, checked mechanically because a comment cannot enforce them:

      1. `banana-live` -- the only crate that reads PRIVATE_KEY or signs -- is depended on
         by `banana-app` and by nothing else. In particular NOT by `banana-cli`, which
         is what makes "no CLI command needs a key" (sec.10, PLAN.md C6) a fact rather than
         a promise.

      2. `banana-core` has no network and no database dependency, so the rule engine and
         curve math stay fully testable offline (sec.4, phase 1).

      3. No crate outside `crates/live/` mentions PRIVATE_KEY.

    Exits non-zero on violation. Run from the repository root.
#>

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$failures = @()

Write-Host "== trust boundary ==" -ForegroundColor Cyan

# --- 1. who depends on banana-live? ------------------------------------------------
# Read from `cargo metadata`, not from `cargo tree` text. The tree is drawn for a human:
# its glyphs vary by terminal, `--invert` behaves differently depending on which package
# is selected, and on a Linux runner it listed all 500-odd transitive crates as
# "dependents" — a check that fails open on one platform and closed on another is not a
# check. The JSON says exactly which workspace crate declares which dependency.
$metaJson = cargo metadata --format-version 1 --no-deps 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Host $metaJson
    throw "cargo metadata failed; cannot verify the trust boundary"
}
$meta = $metaJson | ConvertFrom-Json

$dependents = @()
foreach ($pkg in $meta.packages) {
    foreach ($dep in $pkg.dependencies) {
        if ($dep.name -eq 'banana-live') { $dependents += $pkg.name }
    }
}
$dependents = @($dependents | Sort-Object -Unique)

$allowed = @('banana-app')
$illegal = @($dependents | Where-Object { $allowed -notcontains $_ })

if ($illegal.Count -gt 0) {
    $failures += "banana-live is depended on by: $($illegal -join ', '). Only 'banana-app' may depend on it (spec sec.3.8)."
} elseif ($dependents.Count -eq 0) {
    # Nothing depending on it means the query broke, not that the boundary is perfect.
    $failures += "no crate declares a dependency on banana-live, which cannot be right -- the metadata query is broken and this check is not running."
} else {
    Write-Host "  ok  banana-live dependents: $($dependents -join ', ')" -ForegroundColor Green
}

# --- 2. banana-core stays offline --------------------------------------------------
# `cargo tree` text is fine here, unlike in check 1: the package is named explicitly with
# `--package`, so there is no ambiguity about what is being walked, and the test is only
# "does a banned name appear anywhere in the output". Reading the JSON instead was tried
# and is worse -- Windows PowerShell's ConvertFrom-Json is case-insensitive about keys and
# refuses the metadata outright, because some crate declares both `Default` and `default`
# features.
$coreTree = cargo tree --package banana-core --edges normal 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Host $coreTree
    throw "cargo tree failed for banana-core"
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
$found = @($found | Sort-Object -Unique)

if ($found.Count -gt 0) {
    $failures += "banana-core has acquired I/O dependencies: $($found -join ', '). It must stay pure (spec sec.4)."
} else {
    Write-Host "  ok  banana-core has no network or database dependency" -ForegroundColor Green
}

# --- 3. the key is named in exactly one crate ---------------------------------------
# The separator is normalised before matching. This read `\crates\live\`, which matches
# nothing on Linux or macOS -- so every legitimate mention inside the crate that is
# *supposed* to hold the key was reported as a violation, and the check only ever worked
# on the machine it was written on.
$keyHits = Get-ChildItem -Path 'crates' -Recurse -Include *.rs -File |
    Where-Object { $_.FullName.Replace('\', '/') -notmatch '/crates/live/' } |
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
