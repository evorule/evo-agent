# Evo-Agent one-shot verification script.
# Output is ASCII-only by design (PowerShell 5.1 GBK console safe).
# Steps: cargo build -> cargo test -> secret scan -> source layout assertion.
# NOTE: no 2>&1 redirection on native commands (PS 5.1 turns stderr into error
# records which abort under $ErrorActionPreference='Stop'); rely on $LASTEXITCODE.

$ErrorActionPreference = "Continue"
$repo = $PSScriptRoot
Set-Location $repo
$failed = $false

Write-Host "== [1/4] cargo build =="
cargo build --quiet
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: cargo build"; $failed = $true } else { Write-Host "PASS: cargo build" }

Write-Host "== [2/4] cargo test =="
cargo test 2>$null | Select-String -Pattern "^test result"
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: cargo test"; $failed = $true } else { Write-Host "PASS: cargo test" }

Write-Host "== [3/4] secret scan =="
$patterns = @(
    "sk-[A-Za-z0-9]{20,}",                       # OpenAI/DeepSeek style key
    "AKIA[0-9A-Z]{16}",                          # AWS access key id
    "ghp_[A-Za-z0-9]{30,}",                      # GitHub PAT
    "-----BEGIN [A-Z ]*PRIVATE KEY-----",        # PEM private key
    "(?i)(api[_-]?key|secret|token)\s*[=:]\s*""[A-Za-z0-9_\-]{20,}"""  # hardcoded key assignment
)
$targets = @("src", "config-examples", "docs", "agents", "assets", "README.md", "NOTICE.md", "CHANGELOG.md")
$hits = @()
foreach ($t in $targets) {
    if (Test-Path $t) {
        foreach ($p in $patterns) {
            $m = Get-ChildItem $t -Recurse -File -ErrorAction SilentlyContinue |
                Select-String -Pattern $p -ErrorAction SilentlyContinue
            foreach ($x in $m) { $hits += "$($x.Path):$($x.LineNumber)" }
        }
    }
}
if ($hits.Count -gt 0) {
    Write-Host "FAIL: secret-like patterns found:"
    $hits | ForEach-Object { Write-Host "  $_" }
    Write-Host "  (review each hit; false positives must be whitelisted explicitly, never ignored silently)"
    $failed = $true
} else {
    Write-Host "PASS: secret scan (0 hits)"
}

Write-Host "== [4/4] dependency contract assertion =="
# EvoRule engine crates must come from crates.io (version deps), never from a
# local path -- path deps would break out-of-tree builds for external users.
$manifest = Get-Content (Join-Path $repo "Cargo.toml") -Raw
$depHits = [regex]::Matches($manifest, '(?m)^\s*(evorule-tcb|evorule-reactor)\s*=\s*\{\s*path\s*=')
if ($depHits.Count -gt 0) {
    foreach ($h in $depHits) { Write-Host "FAIL: path dependency found: $($h.Value)" }
    Write-Host "  (engine crates must be version deps like `evorule-tcb = `"0.x.y`"; see README 'dependency contract')"
    $failed = $true
} else {
    Write-Host "PASS: dependency contract (evorule-tcb / evorule-reactor resolved from crates.io)"
}

# Version alignment: engine crates must track the主线 supported minor
# (source of truth: evorule/SECURITY.md support table). Historical drift
# (locked 0.4.3 while主线 0.6.0) is exactly what this check prevents (N8).
$supportedMinor = "0.6"
$depVers = [regex]::Matches($manifest, '(?m)^\s*(evorule-tcb|evorule-reactor)\s*=\s*"(\d+)\.(\d+)\.\d+"')
$misaligned = @()
foreach ($v in $depVers) {
    $minor = "$($v.Groups[2].Value).$($v.Groups[3].Value)"
    if ($minor -ne $supportedMinor) {
        $misaligned += "$($v.Groups[1].Value)=$minor"
    }
}
if ($depVers.Count -eq 0) {
    Write-Host "FAIL: no pinned evorule-tcb/evorule-reactor version deps found in Cargo.toml"
    $failed = $true
} elseif ($misaligned.Count -gt 0) {
    Write-Host "FAIL: engine crates not aligned with supported minor ${supportedMinor}.x:"
    foreach ($m in $misaligned) { Write-Host "  $m" }
    Write-Host "  (upgrade evorule-tcb/evorule-reactor, then update 'supportedMinor' here;"
    Write-Host "   supported versions: evorule/SECURITY.md)"
    $failed = $true
} else {
    Write-Host "PASS: version alignment (evorule engine crates on ${supportedMinor}.x)"
}

if ($failed) { Write-Host "== RESULT: FAIL =="; exit 1 }
Write-Host "== RESULT: ALL PASS =="
