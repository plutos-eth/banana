<#
.SYNOPSIS
    Enforces the two UI invariants that are cheap to break and expensive to fix later.

.DESCRIPTION
    1. NO HARD-CODED STYLE VALUES (spec sec.8, sec.12).
       Every colour, spacing step, radius, font size and font family lives in
       ui/src/tokens.css. A component containing "#1a1a1a" or "padding: 8px" turns the
       later design pass into a rewrite. Only tokens.css may hold literals.

    2. THE CONTENT SECURITY POLICY STAYS STRICT (spec sec.3.1, PLAN.md C1).
       The launch calldata contains an attacker-controlled logo URL. If the webview can
       fetch it, a token deployer learns the IP of every quarrel user watching their
       launch, in real time, before they buy. The CSP makes that impossible rather than
       merely tested-against: img-src allows only 'self' and data:, and connect-src
       allows no external host at all -- every RPC call goes through the Rust backend
       over IPC, so the webview never needs the network.

       Directive values are compared EXACTLY, not by substring. "img-src 'self' data:"
       and "img-src 'self' data: https:" differ by one token and by the entire point of
       the check, so any addition has to be a deliberate edit here.

       This check is the backstop. The CSP itself is the guarantee.

    Exits non-zero on violation. Run from the repository root.
#>

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$failures = @()

# --- 1. no hard-coded style values --------------------------------------------------
Write-Host "== ui style tokens ==" -ForegroundColor Cyan

# Each rule: Regex must match AND, when present, Unless must NOT match. The Unless
# clause exists because "font-family: var(--font-ui)" is the CORRECT usage and only a
# literal stack is a violation. A negative lookahead cannot express this reliably here:
# \s* backtracks and satisfies the lookahead anyway.
$rules = @(
    @{ Name = 'hex colour';         Regex = '#[0-9a-fA-F]{3,8}\b' },
    @{ Name = 'rgb()/hsl()';        Regex = '\b(rgba?|hsla?)\s*\(' },
    @{ Name = 'px value';           Regex = '\b\d+(\.\d+)?px\b' },
    @{ Name = 'rem/em value';       Regex = '\b\d+(\.\d+)?(rem|em)\b' },
    @{ Name = 'literal font stack'; Regex = 'font-family\s*:'; Unless = 'var\(' }
)

$files = Get-ChildItem -Path 'ui/src' -Recurse -File -Include *.ts, *.tsx, *.css, *.scss -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -ne 'tokens.css' }

foreach ($f in $files) {
    $rel = Resolve-Path -Relative $f.FullName
    $lineNo = 0
    foreach ($line in (Get-Content -LiteralPath $f.FullName)) {
        $lineNo++
        # An explicit opt-out for the rare justified case; must carry a reason.
        if ($line -match 'tokens-exempt:') { continue }
        foreach ($r in $rules) {
            if ($line -notmatch $r.Regex) { continue }
            if ($r.ContainsKey('Unless') -and $line -match $r.Unless) { continue }
            $failures += "$($r.Name) in ${rel}:${lineNo} -> $($line.Trim())"
        }
    }
}

$styleCount = $failures.Count
if ($styleCount -eq 0) {
    Write-Host "  ok  no hard-coded colours, sizes or font stacks outside tokens.css" -ForegroundColor Green
}

# --- 2. the CSP is present and strict -----------------------------------------------
Write-Host "== content security policy ==" -ForegroundColor Cyan

$confPath = 'crates/app/tauri.conf.json'
if (-not (Test-Path $confPath)) {
    $failures += "missing $confPath -- the CSP is the mechanism that enforces sec.3.1"
} else {
    $conf = Get-Content $confPath -Raw | ConvertFrom-Json
    $csp = $conf.app.security.csp

    if (-not $csp) {
        $failures += "tauri.conf.json has no app.security.csp. Without it the webview may fetch attacker-supplied URLs (PLAN.md C1)."
    } else {
        # Parse "a 'self'; b 'none'" into directive -> normalised value.
        $actual = @{}
        foreach ($part in ($csp -split ';')) {
            $t = $part.Trim()
            if (-not $t) { continue }
            $tokens = $t -split '\s+'
            $actual[$tokens[0].ToLower()] = (($tokens | Select-Object -Skip 1) -join ' ')
        }

        # Exact required values. Anything else is a weakening.
        $required = [ordered]@{
            'default-src'     = @{ Value = "'none'";                              Why = 'nothing loads unless explicitly allowed' }
            'script-src'      = @{ Value = "'self'";                              Why = 'no remote or inline script' }
            'img-src'         = @{ Value = "'self' data:";                        Why = 'blocks attacker-supplied logo URLs (sec.3.1, PLAN.md C1)' }
            'font-src'        = @{ Value = "'self'";                              Why = 'no remote webfonts' }
            'connect-src'     = @{ Value = "'self' ipc: http://ipc.localhost";    Why = 'the webview must not reach the network; RPC goes through Rust' }
            'object-src'      = @{ Value = "'none'";                              Why = 'no plugins' }
            'frame-ancestors' = @{ Value = "'none'";                              Why = 'not embeddable' }
            'base-uri'        = @{ Value = "'none'";                              Why = 'no base-tag redirection of relative URLs' }
            'form-action'     = @{ Value = "'none'";                              Why = 'no form can post anywhere' }
        }

        foreach ($d in $required.Keys) {
            $want = $required[$d].Value
            if (-not $actual.ContainsKey($d)) {
                $failures += "CSP is missing directive '$d' (want: $d $want) -- $($required[$d].Why)"
            } elseif ($actual[$d] -ne $want) {
                $failures += "CSP directive '$d' is '$($actual[$d])', want '$want' -- $($required[$d].Why)"
            }
        }

        # Belt and braces: no wildcard and no external origin anywhere in the policy.
        if ($csp -match '\*') {
            $failures += "CSP contains a wildcard: $csp"
        }
        foreach ($m in [regex]::Matches($csp, '[a-z]+://[^\s;]+')) {
            if ($m.Value -ne 'http://ipc.localhost') {
                $failures += "CSP names an external origin '$($m.Value)'; only http://ipc.localhost (Tauri IPC) is allowed"
            }
        }
        if ($conf.app.security.dangerousDisableAssetCspModification -ne $false) {
            $failures += "app.security.dangerousDisableAssetCspModification must be false"
        }
        if ($conf.app.security.assetProtocol.enable -ne $false) {
            $failures += "app.security.assetProtocol.enable must be false; the app serves no arbitrary local files"
        }
    }
    if ($failures.Count -eq $styleCount) {
        Write-Host "  ok  CSP present and strict; webview has no network reach" -ForegroundColor Green
    }
}

# --- verdict ------------------------------------------------------------------------
if ($failures) {
    Write-Host ""
    Write-Host "UI INVARIANTS VIOLATED" -ForegroundColor Red
    foreach ($f in $failures) { Write-Host "  - $f" -ForegroundColor Red }
    Write-Host ""
    Write-Host "Style values belong in ui/src/tokens.css (spec sec.8)." -ForegroundColor Yellow
    Write-Host "The CSP is the mechanism behind spec sec.3.1; loosening it needs a deliberate edit here." -ForegroundColor Yellow
    exit 1
}

Write-Host "ui invariants intact" -ForegroundColor Green
exit 0
