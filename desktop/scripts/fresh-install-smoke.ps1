# fresh-install-smoke.ps1 — 装机冒烟验收（发版面验收机制闸 4 的 CI 化）
#
# 对 build 产物 setup.exe 做「装机 → 起壳 → 服务面 → 清理」全链冒烟：
#   1. 静默安装到临时目录          2. 关键件 10 项在位
#   3. 壳拉起六进程编排            4. wasm-host 9140 验签五 UDF verified
#   5. codegraph 9150 健康         6. context-inspector 19100 服务面
#   7. engine 服务面五 UDF 注册    8. ai-plugin 无 key 环境预期剔除（可见失败语义）
# 任何一步失败即 exit 1（fail-closed）；无论成败均清理进程与目录。
#
# 用法: powershell -File fresh-install-smoke.ps1 -SetupPath <setup.exe>

param(
    [Parameter(Mandatory = $true)]
    [string]$SetupPath
)

$ErrorActionPreference = 'Stop'
$workdir = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { $env:TEMP }
$install = Join-Path $workdir "smoke-install"
$dataDir = Join-Path $env:LOCALAPPDATA "com.evorule.evo-agent-desktop"

function Stop-SmokeShell {
    Get-Process | Where-Object { $_.Path -like "$install*" } | ForEach-Object {
        try { taskkill /PID $_.Id /T /F 2>$null | Out-Null } catch { }
    }
}

# 无论成败都清理（进程 + 安装目录；数据目录一并清保证 runner 零遗留）
try {
    # ---- 1. clean slate + silent install -----------------------------------
    if (Test-Path $install) { Remove-Item $install -Recurse -Force }
    if (Test-Path $dataDir) { Remove-Item $dataDir -Recurse -Force }
    Write-Host "[smoke] silent install -> $install"
    $p = Start-Process -FilePath $SetupPath -ArgumentList "/S", "/D=$install" -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw "silent install exit code $($p.ExitCode)" }

    # ---- 2. key files in place ---------------------------------------------
    $keyFiles = @(
        "evo-agent-desktop.exe",
        "resources\evorule-server\evorule-server.exe",
        "resources\evorule-server\plugins\ai-plugin\evorule-ai-plugin.exe",
        "resources\evorule-server\plugins\ai-plugin\plugin.json",
        "resources\evorule-server\plugins\wasm-host\evorule-wasm-host.exe",
        "resources\evorule-server\plugins\wasm-host\plugin.json",
        "resources\evorule-server\plugins\wasm-host\trusted-keys.json",
        "resources\evorule-server\plugins\wasm\udf_finance_tax_calc.wasm",
        "resources\evorule-server\plugins\evorule-context-inspector\evorule-context-inspector.exe",
        "resources\evorule-server\plugins\evorule-context-inspector\plugin.json"
    )
    foreach ($rel in $keyFiles) {
        $full = Join-Path $install $rel
        if (-not (Test-Path $full)) { throw "key file missing: $rel" }
    }
    Write-Host "[smoke] key files: $($keyFiles.Count)/$($keyFiles.Count) OK"

    # ---- 3. launch shell ----------------------------------------------------
    Write-Host "[smoke] launching shell"
    Start-Process (Join-Path $install "evo-agent-desktop.exe") | Out-Null

    # ---- 4. wasm-host 9140 + signature integrity ---------------------------
    $deadline = (Get-Date).AddSeconds(90)
    $health = $null
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 3
        try {
            $health = Invoke-RestMethod -Uri "http://127.0.0.1:9140/health" -TimeoutSec 3
            break
        } catch { }
    }
    if (-not $health) { throw "wasm-host :9140 not healthy within 90s" }
    $mods = $health.integrity.PSObject.Properties
    if ($mods.Count -lt 5) { throw "wasm-host integrity modules = $($mods.Count), expect >= 5" }
    $unverified = @($mods | Where-Object { $_.Value.state -ne 'verified' })
    if ($unverified.Count -gt 0) {
        throw ("wasm-host integrity not verified: " + (($unverified | ForEach-Object { $_.Name }) -join ", "))
    }
    Write-Host "[smoke] wasm-host 9140: $($mods.Count) modules verified"

    # ---- 5. codegraph 9150 --------------------------------------------------
    $deadline = (Get-Date).AddSeconds(30)
    $cgOk = $false
    while ((Get-Date) -lt $deadline) {
        try {
            $r = Invoke-WebRequest -Uri "http://127.0.0.1:9150/health" -UseBasicParsing -TimeoutSec 3
            if ($r.StatusCode -eq 200) { $cgOk = $true; break }
        } catch { Start-Sleep -Seconds 3 }
    }
    if (-not $cgOk) { throw "codegraph :9150 not healthy within 30s" }
    Write-Host "[smoke] codegraph 9150: OK"

    # ---- 6. context-inspector 19100 admin service surface -------------------
    $deadline = (Get-Date).AddSeconds(30)
    $ciOk = $false
    while ((Get-Date) -lt $deadline) {
        try {
            $r = Invoke-WebRequest -Uri "http://127.0.0.1:19100/health" -UseBasicParsing -TimeoutSec 3
            if ($r.StatusCode -eq 200) { $ciOk = $true; break }
        } catch { Start-Sleep -Seconds 3 }
    }
    if (-not $ciOk) { throw "context-inspector :19100 not healthy within 30s" }
    Write-Host "[smoke] context-inspector 19100: OK"

    # ---- 7. engine service surface: five UDFs registered -------------------
    $runtime = Join-Path $dataDir "runtime.json"
    if (-not (Test-Path $runtime)) { throw "runtime.json missing under $dataDir" }
    $rt = Get-Content $runtime -Raw | ConvertFrom-Json
    $enginePort = $rt.engine_port
    if (-not $enginePort) { throw "engine_port missing in runtime.json" }
    $deadline = (Get-Date).AddSeconds(30)
    $udfs = @()
    while ((Get-Date) -lt $deadline) {
        try {
            $svc = (Invoke-WebRequest -Uri "http://127.0.0.1:$enginePort/api/services" -UseBasicParsing -TimeoutSec 5).Content |
                ConvertFrom-Json -AsHashtable
            $names = @($svc | ForEach-Object { $_.name })
            $udfs = @($names | Where-Object { $_ -like "udf_*" })
            if ($udfs.Count -ge 5) { break }
        } catch { }
        Start-Sleep -Seconds 3
    }
    if ($udfs.Count -lt 5) { throw "engine :$enginePort service surface has $($udfs.Count) udf_* entries, expect >= 5" }
    Write-Host "[smoke] engine :$enginePort service surface: $($udfs.Count) UDFs [$($udfs -join ', ')]"

    # ---- 8. ai-plugin expected exclusion (no key on CI runner) --------------
    $manifestPath = Join-Path $dataDir "plugins-manifest.json"
    if (Test-Path $manifestPath) {
        $mf = Get-Content $manifestPath -Raw | ConvertFrom-Json
        $pkgNames = @($mf.plugins.PSObject.Properties.Name)
        if ($pkgNames -contains "ai-plugin") {
            Write-Host "[smoke] NOTE: ai-plugin present in manifest (key available on runner) - acceptable"
        } else {
            Write-Host "[smoke] ai-plugin excluded from manifest (no LLM key) - visible-failure semantics OK"
        }
        if ($pkgNames -notcontains "wasm-host") { throw "wasm-host missing from plugins manifest" }
        if ($pkgNames -notcontains "codegraph-bridge") { throw "codegraph-bridge missing from plugins manifest" }
        if ($pkgNames -notcontains "evorule-context-inspector") { throw "evorule-context-inspector missing from plugins manifest" }
    } else {
        throw "plugins-manifest.json missing under $dataDir"
    }

    Write-Host "[smoke] ALL CHECKS PASSED"
    exit 0
} finally {
    Stop-SmokeShell
    Start-Sleep -Seconds 2
    if (Test-Path $install) { Remove-Item $install -Recurse -Force -ErrorAction SilentlyContinue }
    if (Test-Path $dataDir) { Remove-Item $dataDir -Recurse -Force -ErrorAction SilentlyContinue }
    Write-Host "[smoke] cleanup done"
}
