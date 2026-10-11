# ci-autofix.ps1 - CI red closed-loop authorized autofix machinery (manual mode).
#
# Rule-only machinery for the authorized autofix pipeline: the LLM session does
# attribution and rewrites; THIS script enforces the whitelist audit, the round
# circuit breaker, and the audit trail. Fail-closed: anything not explicitly
# whitelisted is blocked and escalates to human.
#
# Subcommands:
#   status                Show per-repo main CI state (no throttle, no alert side effects)
#   audit  -PatchFile X   Whitelist-audit a candidate unified diff patch
#                          [-AllowAllowlist] permits allowlist edits (one prior
#                          approval per batch, granted by the operator ordering the run)
#   record -Repo R -Sha S -Result ok|still-red|escalated [-Note "…"]
#                         Append the JSONL audit-trail line and update the round
#                         counter; frozen (>=2 rounds/day/repo) exits 2
#
# Exit codes: 0 PASS, 1 BLOCKED/found-red, 2 frozen, 3 usage error.

param(
    [Parameter(Position = 0)][string]$Cmd = 'status',
    [string]$PatchFile = '',
    [switch]$AllowAllowlist,
    [string]$Repo = '',
    [string]$Sha = '',
    [string]$Result = '',
    [string]$Note = ''
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '_common.ps1')
$cfg = Get-OpsConfig -OpsDir $PSScriptRoot
$logDir = $cfg.log_dir
$stateFile = Join-Path $logDir 'ci-autofix-state.json'
$trailFile = Join-Path $logDir 'ci-autofix-trail.jsonl'
$repos = @('evorule/evorule', 'evorule/evo-agent')

function Get-CiState([string]$slug) {
    try {
        $cr = Invoke-RestMethod -Uri ("https://api.github.com/repos/{0}/commits/main/check-runs" -f $slug) -TimeoutSec 20
        $runs = @($cr.check_runs)
        $bad = @($runs | Where-Object { $_.conclusion -eq 'failure' })
        if ($bad.Count -gt 0) { return @{ state = 'FAIL'; jobs = ($bad | ForEach-Object { $_.name }) -join ', ' } }
        if ($runs.Count -eq 0) { return @{ state = 'NONE'; jobs = 'no check runs on main' } }
        if (@($runs | Where-Object { -not $_.conclusion }).Count -gt 0) { return @{ state = 'PENDING'; jobs = '' } }
        return @{ state = 'PASS'; jobs = ('{0} checks all success' -f $runs.Count) }
    } catch { return @{ state = 'UNKNOWN'; jobs = $_.Exception.Message } }
}

function Test-Frozen([string]$slug) {
    if (-not (Test-Path $stateFile)) { return $false }
    try { $st = Get-Content $stateFile -Raw -Encoding utf8 | ConvertFrom-Json } catch { return $false }
    $today = Get-Date -Format 'yyyyMMdd'
    $rec = $st.PSObject.Properties[$slug + '@' + $today]
    return ($rec -and [int]$rec.Value -ge 2)
}

switch ($Cmd) {
    'status' {
        $anyRed = $false
        foreach ($slug in $repos) {
            $s = Get-CiState $slug
            Write-Output ("{0} = {1} ({2})" -f $slug, $s.state, $s.jobs)
            if ($s.state -eq 'FAIL') { $anyRed = $true }
        }
        if (Test-Frozen 'evorule/evorule') { Write-Output 'frozen: evorule/evorule (round limit reached today)' }
        if (Test-Frozen 'evorule/evo-agent') { Write-Output 'frozen: evorule/evo-agent (round limit reached today)' }
        exit $(if ($anyRed) { 1 } else { 0 })
    }
    'audit' {
        if (-not $PatchFile -or -not (Test-Path $PatchFile)) { Write-Output 'ERROR: -PatchFile required'; exit 3 }
        if ($Repo -and (Test-Frozen $Repo)) { Write-Output ("BLOCK: autofix frozen for {0} (round limit reached); escalate to human" -f $Repo); exit 2 }
        $lines = Get-Content $PatchFile
        $curFile = ''
        $files = @{}
        foreach ($ln in $lines) {
            if ($ln -match '^diff --git') { $curFile = ''; continue }
            if ($ln -match '^\+\+\+ b/(.*)$') { $curFile = $Matches[1]; if (-not $files.ContainsKey($curFile)) { $files[$curFile] = @{ add = 0; del = 0; bad = @() } }; continue }
            if ($curFile -eq '') { continue }
            $f = $files[$curFile]
            if ($ln.StartsWith('+') -and -not $ln.StartsWith('+++')) { $f.add++; if (-not ($ln -match '^\+\s*$')) { $f.bad += ('+' + $ln.Substring(1)) } }
            elseif ($ln.StartsWith('-') -and -not $ln.StartsWith('---')) { $f.del++; if (-not ($ln -match '^-\s*$')) { $f.bad += ('-' + $ln.Substring(1)) } }
        }
        $verdicts = @()
        $total = 0
        foreach ($name in $files.Keys) {
            $f = $files[$name]
            $total += $f.add + $f.del
            $ext = [System.IO.Path]::GetExtension($name).ToLower()
            $isDoc = ($ext -in '.md', '.txt') -or ($name -like 'docs/*')
            $isAllowlist = ($name -like '*scan_public_face_allowlist.txt')
            $isWordlistPy = ($name -like '*scan_public_face.py')
            if ($isAllowlist -and -not $AllowAllowlist) {
                $verdicts += ("BLOCK {0}: allowlist edit without prior per-item approval (re-run with -AllowAllowlist only after operator grants it)" -f $name); continue
            }
            if ($isWordlistPy) {
                foreach ($bl in $f.bad) {
                    if ($bl -notmatch '^[-+]\s*(["'']).*\1,?\s*$' -and $bl -notmatch '^[-+]\s*#') { $verdicts += ("BLOCK {0}: non-list-literal line {1}" -f $name, $bl.Trim().Substring(0, [Math]::Min(60, $bl.Trim().Length))) }
                }
                continue
            }
            if ($isDoc) { continue }
            if ($ext -eq '.rs') { $cre = '\s*(//|/\*|\*)' }
            elseif ($ext -in '.py', '.ps1') { $cre = '^\s*#' }
            else { $verdicts += ("BLOCK {0}: path not whitelisted (code/workflow files need a human batch)" -f $name); continue }
            foreach ($bl in $f.bad) {
                if ($bl -notmatch ('^[-+]' + $cre)) { $verdicts += ("BLOCK {0}: non-comment line {1}" -f $name, $bl.Trim().Substring(0, [Math]::Min(60, $bl.Trim().Length))) }
            }
        }
        if ($files.Count -gt 5) { $verdicts += ("BLOCK: {0} files changed (max 5)" -f $files.Count) }
        if ($total -gt 20) { $verdicts += ("BLOCK: {0} changed lines (max 20)" -f $total) }
        if ($verdicts.Count -gt 0) { $verdicts | ForEach-Object { Write-Output $_ }; Write-Output 'AUDIT: BLOCKED'; exit 1 }
        Write-Output ("AUDIT: PASS ({0} file(s), {1} changed line(s), whitelist ok)" -f $files.Count, $total)
        exit 0
    }
    'record' {
        if (-not $Repo -or -not $Sha -or -not $Result) { Write-Output 'ERROR: -Repo -Sha -Result required'; exit 3 }
        if (Test-Frozen $Repo) { Write-Output ("FROZEN: {0} reached the 2-rounds/day limit; escalate to human" -f $Repo); exit 2 }
        $today = Get-Date -Format 'yyyyMMdd'
        $key = $Repo + '@' + $today
        $st = if (Test-Path $stateFile) { try { Get-Content $stateFile -Raw -Encoding utf8 | ConvertFrom-Json } catch { New-Object PSObject } } else { New-Object PSObject }
        if ($Result -eq 'ok') {
            if ($st.PSObject.Properties[$key]) { $st.PSObject.Properties.Remove($key) }
            $rounds = 0
        } else {
            $rounds = 1
            if ($st.PSObject.Properties[$key]) { $rounds = [int]$st.$key + 1 }
            if ($rounds -gt 2) { Write-Output ("FROZEN: {0} would exceed the 2-rounds/day limit; attempt denied, escalate to human" -f $Repo); exit 2 }
            $st | Add-Member -NotePropertyName $key -NotePropertyValue $rounds -Force
        }
        $st | ConvertTo-Json | Set-Content -Path $stateFile -Encoding UTF8
        $entry = @{ ts = (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'); repo = $Repo; sha = $Sha; round = $rounds; result = $Result; note = $Note } | ConvertTo-Json -Compress
        Add-Content -Path $trailFile -Value $entry -Encoding UTF8
        Write-Output ("recorded: round {0} for {1} ({2})" -f $rounds, $Repo, $Result)
        if ($rounds -ge 2) { Write-Output 'WARNING: one autofix round left today for this repo' }
        exit 0
    }
    default { Write-Output 'ERROR: unknown subcommand (status | audit | record)'; exit 3 }
}
