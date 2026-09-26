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
# 失败明细随 stderr 走——必须并流保留,否则 CI 日志只剩汇总行无法定位
# (2026-09-25 教训:ubuntu 两测试红灯但日志零明细,只能本地 WSL 复现)。
# $ErrorActionPreference 顶部已设 Continue,2>&1 不会中断脚本。
$testLog = cargo test 2>&1 | ForEach-Object { "$_" }
$testLog | Select-String -Pattern "^test result"
if ($LASTEXITCODE -ne 0) {
    Write-Host "FAIL: cargo test -- failure details (last 80 lines):"
    $testLog | Select-Object -Last 80 | ForEach-Object { Write-Host $_ }
    $failed = $true
} else { Write-Host "PASS: cargo test" }

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
# Since 2026-09-20: evo-agent is fully decoupled from the evorule main repo.
# The agent orchestration layer must not depend on main-repo crates (TCB /
# reactor / governance / cli) -- path OR version deps both banned. This check
# fails on any re-introduction of main-repo deps (regression guard).
#
# Scope note (2026-09-22): the ban targets MAIN-REPO crates. Governance shared
# components hosted in separate repos are allowed via explicit allowlist --
# they are governance-face libraries, not engine crates, and do not bypass
# evorule-server for rule execution. Current allowlist: evorule-constitution
# (from evorule-system-rules; schema validation with embedded data).
$separateRepoAllowlist = @('evorule-constitution')
$manifest = Get-Content (Join-Path $repo "Cargo.toml") -Raw
$depHits = [regex]::Matches($manifest, '(?m)^\s*(evorule-[\w-]+)\s*=') |
    Where-Object { $separateRepoAllowlist -notcontains $_.Groups[1].Value }
if ($depHits.Count -gt 0) {
    foreach ($h in $depHits) { Write-Host "FAIL: main-repo dependency found: $($h.Value)" }
    Write-Host "  (evo-agent is decoupled from the evorule main repo since 2026-09-20;"
    Write-Host "   agent layer must not depend on main-repo crates. Re-adding main-repo"
    Write-Host "   deps is a layering violation. Separate-repo governance components"
    Write-Host "   must be explicitly added to the allowlist.)"
    $failed = $true
} else {
    Write-Host "PASS: dependency contract (no main-repo deps -- decoupled)"
}

if ($failed) { Write-Host "== RESULT: FAIL =="; exit 1 }
Write-Host "== RESULT: ALL PASS =="
