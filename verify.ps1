# Evo-Agent verification script.
# Output is ASCII-only by design (PowerShell 5.1 GBK console safe).
# Phases: build -> test-compile -> test-run -> secret scan -> dependency contract.
# -Phase <name> runs a single phase; default "all" runs every phase in order
# (local one-shot behavior, unchanged). CI invokes one phase per workflow step
# so each completed step's log survives an infra kill mid-run (2026-10-21:
# ubuntu verify died 6x at ~48-52 min with the whole single-step log blob lost;
# splitting turns the next death into a bounded, attributable phase).
# NOTE: no 2>&1 redirection on native commands (PS 5.1 turns stderr into error
# records which abort under $ErrorActionPreference='Stop'); rely on $LASTEXITCODE.

param(
    [ValidateSet("all", "build", "test-compile", "test-run", "scan", "contract")]
    [string]$Phase = "all"
)

$ErrorActionPreference = "Continue"
$repo = $PSScriptRoot
Set-Location $repo
$failed = $false
# OS gate compatible with Windows PowerShell 5.1 ($IsWindows is pwsh-only).
$isWin = $env:OS -eq 'Windows_NT'

function Test-Phase([string]$name) { $Phase -eq 'all' -or $Phase -eq $name }

if (Test-Phase 'build') {
    Write-Host "== [1/5] cargo build =="
    # Wrap the heavy phases in coreutils timeout (Linux only): a hang becomes a
    # logged FAIL with full output instead of an infra kill. Windows unaffected.
    if ($isWin) {
        cargo build --quiet
    } else {
        timeout --kill-after=30 1500 cargo build --quiet
    }
    if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: cargo build"; $failed = $true } else { Write-Host "PASS: cargo build" }
}

if (Test-Phase 'test-compile') {
    Write-Host "== [2/5] cargo test --no-run (compile all test binaries) =="
    # CI diagnosis (2026-10-21): the ubuntu runner died repeatedly mid-verify,
    # most plausibly during test-binary compilation (build phase is capped at
    # 25 min by the timeout below, so a ~48 min death lands inside phase 2's
    # compile+link window). Compiling without running isolates the linker/rustc
    # memory spike from the test-execution phase in the step-split layout.
    if ($isWin) {
        cargo test --no-run
    } else {
        timeout --kill-after=30 2400 cargo test --no-run
    }
    if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: cargo test --no-run"; $failed = $true } else { Write-Host "PASS: cargo test --no-run" }
}

if (Test-Phase 'test-run') {
    Write-Host "== [3/5] cargo test =="
    # Failure details travel on stderr -- must keep the merged stream, otherwise
    # CI log carries only the summary line (2026-09-25 lesson: ubuntu had two red
    # tests with zero details in the log; local WSL repro was the only option).
    # $ErrorActionPreference is Continue at the top, so 2>&1 will not abort.
    # Tee mode (2026-10-22): stream every line to the step log as it arrives --
    # if cargo test hangs, the LAST log line names the stuck test. The full
    # stream is still kept in memory for the failure-detail tail below
    # (buffered-only output was the blind spot that hid the hang).
    $testLog = New-Object System.Collections.Generic.List[string]
    if ($isWin) {
        cargo test 2>&1 | ForEach-Object { $l = "$_"; Write-Host $l; $testLog.Add($l) }
    } else {
        # Resource watchdog (2026-10-22, permanent sentinel): a background
        # loop streams a snapshot every 15s (loadavg / mem / disk / top-mem
        # procs / process counts) alongside the test stream. Origin: the
        # ubuntu runner froze 49s into test-run and the box froze so hard
        # even the 35-min timeout never concluded -- a machine-level freeze,
        # not a single-test hang. Two diagnostic rounds with this watchdog
        # went green, and the same day a windows-latest image drift pointed
        # at a host/image regression; if the freeze ever recurs on main,
        # the watchdog's last streamed snapshots bracket it with the
        # resource curve and the culprit's process tree, even if the runner
        # itself dies and the final log upload is lost.
        bash -c '
            ( while true; do
                  echo "=== WD $(date -u +%H:%M:%S) load=$(cat /proc/loadavg) ==="
                  free -m | sed -n 2p
                  df -h / | tail -1
                  ps aux --sort=-%mem | head -8
                  echo "=== procs evo_agent=$(pgrep -c -f evo_agent) npm_or_node=$(pgrep -c -f "npm|node") sh=$(pgrep -c -x sh) total=$(ps -e --no-headers | wc -l)"
                  sleep 15
              done ) & WD=$!
            timeout --kill-after=30 2100 cargo test 2>&1
            RC=$?
            kill $WD 2>/dev/null
            exit $RC
        ' 2>&1 | ForEach-Object { $l = "$_"; Write-Host $l; $testLog.Add($l) }
        if ($LASTEXITCODE -eq 124) { Write-Host "TIMEOUT: cargo test after 35 min" }
    }
    $testLog | Select-String -Pattern "^test result"
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAIL: cargo test -- failure details (last 80 lines):"
        $testLog | Select-Object -Last 80 | ForEach-Object { Write-Host $_ }
        $failed = $true
    } else { Write-Host "PASS: cargo test" }
}

if (Test-Phase 'scan') {
    Write-Host "== [4/5] secret scan =="
    $patterns = @(
        "sk-[A-Za-z0-9]{20,}",                       # OpenAI/DeepSeek style key
        "AKIA[0-9A-Z]{16}",                          # AWS access key id
        "ghp_[A-Za-z0-9]{30,}",                      # GitHub PAT
        "-----BEGIN [A-Z ]*PRIVATE KEY-----",        # PEM private key
        "(?i)(api[_-]?key|secret|token)\s*[=:]\s*""[A-Za-z0-9_\-]{20,}"""  # hardcoded key assignment
    )
    # Whitelist: documentation example values used by the exfiltration detector's
    # own rule corpus/tests in src/builtin_tools/exfil_scan.rs (the detector must
    # carry secret-shaped literals by construction; these are published sample
    # values, not real credentials). Entries are content-exact on purpose -- no
    # line numbers (they drift with edits).
    $secretScanWhitelist = @(
        "AKIAIOSFODNN7EXAMPLE",                      # AWS docs sample access key id
        "-----BEGIN RSA PRIVATE KEY-----"            # generic PEM header test fixture
    )
    $targets = @("src", "config-examples", "docs", "agents", "assets", "README.md", "NOTICE.md", "CHANGELOG.md")
    $hits = @()
    foreach ($t in $targets) {
        if (Test-Path $t) {
            foreach ($p in $patterns) {
                $m = Get-ChildItem $t -Recurse -File -ErrorAction SilentlyContinue |
                    Select-String -Pattern $p -ErrorAction SilentlyContinue
                foreach ($x in $m) {
                    $whitelisted = $false
                    foreach ($w in $secretScanWhitelist) {
                        if ($null -ne $x.Line -and $x.Line.Contains($w)) { $whitelisted = $true; break }
                    }
                    if (-not $whitelisted) { $hits += "$($x.Path):$($x.LineNumber)" }
                }
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
}

if (Test-Phase 'contract') {
    Write-Host "== [5/5] dependency contract assertion =="
    # Since 2026-09-20: evo-agent is fully decoupled from the evorule main repo.
    # The agent orchestration layer must not depend on main-repo crates (TCB /
    # reactor / governance / cli) -- path OR version deps both banned. This check
    # fails on any re-introduction of main-repo deps (regression guard).
    #
    # Scope note (2026-09-22): the ban targets MAIN-REPO crates. Governance shared
    # components hosted in separate repos are allowed via explicit allowlist --
    # they are governance-face libraries, not engine crates, and do not bypass
    # evorule-server for rule execution. Current allowlist:
    #   evorule-constitution (from evorule-system-rules; schema validation with
    #     embedded data);
    #   evorule-bundle (from evorule-bundle; DatasetBundle schema + blake3 content
    #     hash + import verify chain). Hash byte-compatibility with the
    #     evorule-server import side depends on the same crate serialization, so
    #     both repos must upgrade the crate version in lockstep.
    #   evorule-hash (from evorule-hash; canonical BLAKE3 hashing crate --
    #     hash discipline: all hashing goes through evorule-hash, self-written
    #     blake3 is banned); used by the skill registry content pinning.
    $separateRepoAllowlist = @('evorule-constitution', 'evorule-bundle', 'evorule-hash')
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

    # Zero local-model hard constraint: default builds must not pull the
    # ort/onnxruntime/tokenizers local-model stack. fastembed is allowed only
    # via the opt-in `semantic` feature; assert the default dependency graph
    # stays clean of it and its transitive model-runtime crates.
    $treeOut = & cargo tree -e normal
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAIL: cargo tree failed (exit $LASTEXITCODE)"
        $failed = $true
    } else {
        $modelHits = @($treeOut | Where-Object {
            $_ -match '(fastembed|\bort\b|ort-sys|onnxruntime|tokenizers|hf-hub)'
        })
        if ($modelHits.Count -gt 0) {
            foreach ($h in $modelHits) { Write-Host "FAIL: local-model crate in default graph: $h" }
            Write-Host "  (fastembed is allowed only via the opt-in 'semantic' feature;"
            Write-Host "   default and release builds must stay free of ort/onnx/tokenizers.)"
            $failed = $true
        } else {
            Write-Host "PASS: zero local-model crates in default dependency graph"
        }
    }
}

if ($failed) { Write-Host "== RESULT: FAIL =="; exit 1 }
Write-Host "== RESULT: ALL PASS =="
# Explicit exit 0: a natural script end leaves $LASTEXITCODE at whatever the
# last NATIVE command set it to, and CI steps propagate $LASTEXITCODE -- a
# stale non-zero from an earlier phase would false-red an ALL PASS run.
exit 0
