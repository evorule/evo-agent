# Evo-Agent 5-minute demo script (Windows).
# One command: check env -> fetch + start evorule-server -> build evo-agent
# -> run a finance demo session (LLM + tools, fully audited) -> replay the
# fact chain with hash-chain verification.
#
# Success is dual-criteria: the audit chain must verify AND the business
# artifact must exist on disk (workspace/expenses_2026.json with the 45.50
# entry). A verified audit of "nothing happened" is a false success.
#
# Output is ASCII-only by design (PowerShell 5.1 GBK console safe).
# Prerequisites:
#   - Rust toolchain (cargo) on PATH
#   - an LLM API key, exported as an environment variable:
#       MiniMax (default):  MINIMAX_API_KEY
#       DeepSeek:           DEEPSEEK_API_KEY   (run with -Provider deepseek)
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File demo.ps1
#   powershell -ExecutionPolicy Bypass -File demo.ps1 -Provider deepseek
#
# All demo assets live under .demo\ (git-ignored). Delete the folder to reset.

param(
    [ValidateSet("minimax", "deepseek")]
    [string]$Provider = "minimax",
    # Override LLM endpoint (optional). MiniMax note: the default below is the
    # international endpoint; use https://api.minimax.cn/v1/text/chatcompletion_v2
    # for China-mainland keys.
    [string]$ApiBase = "",
    # evorule-server release to fetch (pinned for reproducibility)
    [string]$ServerVersion = "v0.5.0"
)

$ErrorActionPreference = "Stop"
$repo = $PSScriptRoot
Set-Location $repo
$demoDir = Join-Path $repo ".demo"
$serverDir = Join-Path $demoDir "server"
$serverPort = 18080
$serverUrl = "http://127.0.0.1:$serverPort"

$failed = $false

# --- step 0: environment checks -------------------------------------------
Write-Host "== [0/5] environment checks =="
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "FAIL: cargo not found on PATH. Install Rust: https://rustup.rs"
    exit 1
}
$keyEnv = if ($Provider -eq "minimax") { "MINIMAX_API_KEY" } else { "DEEPSEEK_API_KEY" }
if (-not [Environment]::GetEnvironmentVariable($keyEnv)) {
    Write-Host "FAIL: environment variable $keyEnv is not set."
    Write-Host "  export it first, e.g.:  `$env:$keyEnv = `"your-api-key`""
    exit 1
}
Write-Host "PASS: cargo found, $keyEnv set"

# --- step 1: ensure evorule-server is running ------------------------------
Write-Host "== [1/5] evorule-server =="
$serverUp = $false
try {
    $probe = Invoke-WebRequest -Uri $serverUrl -UseBasicParsing -TimeoutSec 3
    if ($probe.StatusCode -eq 200) { $serverUp = $true }
} catch { $serverUp = $false }

if ($serverUp) {
    Write-Host "PASS: server already running at $serverUrl (reusing it)"
    Write-Host "  note: if it requires auth, export EVORULE_AUTH_TOKEN first;"
    Write-Host "  a demo server started by this script manages the token for you."
} else {
    if (-not (Test-Path (Join-Path $serverDir "evorule-server.exe"))) {
        Write-Host "downloading evorule-server $ServerVersion from Gitee..."
        New-Item -ItemType Directory -Force -Path $demoDir | Out-Null
        # Gitee Release asset names carry the version without the "v" prefix
        # (tag v0.5.0 -> asset evorule-server-0.5.0-win64.zip)
        $ver = $ServerVersion.TrimStart("v")
        $zip = Join-Path $demoDir "evorule-server-$ver-win64.zip"
        $dl = "https://gitee.com/evorule/evorule-server/releases/download/$ServerVersion/evorule-server-$ver-win64.zip"
        Invoke-WebRequest -Uri $dl -OutFile $zip -UseBasicParsing
        Expand-Archive -Path $zip -DestinationPath $serverDir -Force
        Remove-Item $zip
        # flatten if the archive nests a single top-level directory
        $nested = Get-ChildItem $serverDir -Directory | Where-Object {
            Test-Path (Join-Path $_.FullName "evorule-server.exe") }
        if ($nested) {
            Get-ChildItem $nested.FullName -Force | Move-Item -Destination $serverDir -Force
            Remove-Item $nested.FullName -Force
        }
    }
    if (-not (Test-Path (Join-Path $serverDir "evorule-server.exe"))) {
        Write-Host "FAIL: evorule-server.exe not found after download/extract"
        exit 1
    }
    Write-Host "starting evorule-server (port $serverPort)..."
    # v0.5.0+ refuses to start with no auth on loopback unless a token is set;
    # demo uses a random per-run token passed to both server and evo-agent.
    $demoToken = [guid]::NewGuid().ToString("N")
    Push-Location $serverDir
    New-Item -ItemType Directory -Force -Path data | Out-Null
    Start-Process -FilePath ".\evorule-server.exe" -WindowStyle Hidden `
        -ArgumentList "--addr", "127.0.0.1:$serverPort", "--web-dir", "web", `
                      "--rules-dir", "rules", "--service-registry", "service_registry.json", `
                      # evo-agent's ReAct loop script is consumer-owned (locality
                      # principle): the server-bundled server_eval.json only carries
                      # single-shot bridge rules whose collect template expects a
                      # different tool_calls shape, so the agent ships its own
                      # constitution (assets/agent_constitution.json, full ReAct loop).
                      "--core-eval", (Join-Path $repo "assets\agent_constitution.json"), `
                      "--wal-dir", "./data/wal", "--wal-fsync", `
                      "--auth-token", $demoToken `
        -RedirectStandardError (Join-Path $serverDir "server-stderr.log")
    Pop-Location
    $env:EVORULE_AUTH_TOKEN = $demoToken
    # wait for readiness (up to ~20s)
    $ready = $false
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Seconds 1
        try {
            $probe = Invoke-WebRequest -Uri $serverUrl -UseBasicParsing -TimeoutSec 2
            if ($probe.StatusCode -eq 200) { $ready = $true; break }
        } catch { }
    }
    if ($ready) { Write-Host "PASS: server is up at $serverUrl" }
    else {
        Write-Host "FAIL: server did not become ready. Check $serverDir\server-stderr.log"
        exit 1
    }
}

# --- step 2: project config (evo-agent.toml) -------------------------------
Write-Host "== [2/5] project config =="
$apiBase = if ($ApiBase -ne "") { $ApiBase } elseif ($Provider -eq "minimax") {
    "https://api.minimaxi.com/v1/text/chatcompletion_v2"
} else { "https://api.deepseek.com/chat/completions" }
$model = if ($Provider -eq "minimax") { "MiniMax-M2.5" } else { "deepseek-chat" }
$apiKeyEnv = if ($Provider -eq "minimax") { "MINIMAX_API_KEY" } else { "DEEPSEEK_API_KEY" }
$toml = @"
[llm]
provider = "$Provider"
api_key = "`${ENV:$apiKeyEnv}"
model = "$model"
api_base = "$apiBase"

[evorule]
base_url = "$serverUrl"
"@
$tomlPath = Join-Path $repo "evo-agent.toml"
if (Test-Path $tomlPath) {
    Write-Host "SKIP: evo-agent.toml already exists (keeping yours)"
} else {
    Set-Content -Path $tomlPath -Value $toml -Encoding ascii
    Write-Host "PASS: wrote evo-agent.toml (provider=$Provider, evorule=$serverUrl)"
}

# --- step 3: build ----------------------------------------------------------
Write-Host "== [3/5] cargo build (first run takes a few minutes) =="
cargo build --quiet
if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: cargo build"; exit 1 }
Write-Host "PASS: build"

# --- step 4: run the finance demo session -----------------------------------
Write-Host "== [4/5] running finance demo session (streaming) =="
# file_write sandbox: only <workdir>/workspace/ is writable -> aim the goal there
New-Item -ItemType Directory -Force -Path (Join-Path $repo "workspace") | Out-Null
$goal = ("Register one expense: date 2026-09-07, item 'Team lunch', " +
         "amount 45.50 CNY, category 'Catering', submitter 'evo-agent-demo'. " +
         "Write it into workspace/expenses_2026.json as a JSON array " +
         "(create the file if missing; append if it exists). " +
         "Confirm the written path in one sentence.")
$stderrLog = Join-Path $demoDir "run-stderr.log"
# cmd /c wrapper: keeps raw stderr bytes (PS 5.1 would rewrite stderr into
# error records), we parse the session id from it below.
$cmd = ".\target\debug\evo-agent.exe run `"$goal`" --stream --agent general"
cmd /c "$cmd 2>`"$stderrLog`""
if ($LASTEXITCODE -ne 0) {
    Write-Host "FAIL: agent run failed. See $stderrLog"
    exit 1
}
$sessionLine = Select-String -Path $stderrLog -Pattern '\[session: (\d+)\]' | Select-Object -First 1
if (-not $sessionLine) {
    Write-Host "FAIL: session id not found in run output"
    exit 1
}
$sessionId = $sessionLine.Matches[0].Groups[1].Value
Write-Host "PASS: session created (id=$sessionId)"

# --- step 5: business artifact + audit chain verification -------------------
Write-Host ""
Write-Host "== [5/5] business artifact + audit chain verification =="
# dual-criteria (fail-fast): the run exit code alone can hide a false success
# (e.g. the LLM chats nicely but never executes the tool). Require the file.
$bizFile = Join-Path $repo "workspace\expenses_2026.json"
if (-not (Test-Path $bizFile)) {
    Write-Host "FAIL: business task not completed - $bizFile was not created"
    Write-Host "  (the audit report below may still verify; a verified audit of"
    Write-Host "   'nothing happened' is exactly the false success this check guards)"
    $failed = $true
} else {
    $biz = Get-Content $bizFile -Raw
    if ($biz -notmatch '45\.50') {
        Write-Host "FAIL: business task not completed - $bizFile exists but lacks the 45.50 entry"
        $failed = $true
    } else {
        Write-Host "PASS: business artifact written ($bizFile contains 45.50)"
    }
}
$headers = @{}
if ($env:EVORULE_AUTH_TOKEN) { $headers["Authorization"] = "Bearer $($env:EVORULE_AUTH_TOKEN)" }
try {
    # GET /api/sessions/{id}/audit/verify -> AuditVerify {verified, fact_count, last_hash}
    $verify = Invoke-RestMethod -Uri "$serverUrl/api/sessions/$sessionId/audit/verify" `
        -Headers $headers -TimeoutSec 10
    Write-Host ("audit chain verified: {0}  (facts: {1}, last_hash: {2})" -f `
        $verify.verified, $verify.fact_count, $verify.last_hash)
    if (-not $verify.verified) { $failed = $true }
} catch {
    Write-Host "FAIL: audit verify request: $_"
    $failed = $true
}
try {
    # GET /api/sessions/{id}/audit -> full fact chain
    $report = Invoke-RestMethod -Uri "$serverUrl/api/sessions/$sessionId/audit" `
        -Headers $headers -TimeoutSec 10
    Write-Host ""
    Write-Host "-- audit report (fact chain) --"
    Write-Host ($report | ConvertTo-Json -Depth 8)
} catch {
    Write-Host "WARN: audit report fetch failed: $_"
}

Write-Host ""
if (-not $failed) {
    Write-Host "============================================================="
    Write-Host " DEMO COMPLETE"
    Write-Host " - finance session ran with every LLM/tool call turned into"
    Write-Host "   auditable facts by the evorule engine"
    Write-Host " - server-side hash-chain verification passed (audit/verify)"
    Write-Host " - business artifact on disk: workspace/expenses_2026.json"
    Write-Host "   (45.50 entry present - the task really happened)"
    Write-Host " - browse the audit trail: $serverUrl"
    Write-Host " - reset anytime: delete the .demo folder"
    Write-Host "============================================================="
} else {
    Write-Host "== RESULT: FAIL (see messages above) =="
    exit 1
}
