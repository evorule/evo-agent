# fresh-install-smoke.ps1 — 装机冒烟验收（发版面验收机制闸 4 的 CI 化）
#
# 对 build 产物 setup.exe 做「装机 → 起壳 → 服务面 → 清理」全链冒烟：
#   1. 静默安装到临时目录          2. 关键件 10 项在位
#   3. 壳拉起六进程编排            4. wasm-host 9140 验签五 UDF verified
#   5. codegraph 9150 健康         6. context-inspector 19100 服务面
#   7. engine 健康面可达            8. ai-plugin 无 key 环境预期剔除（可见失败语义）
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
        "resources\bin\evo-agent.exe",
        "resources\bin\evorule-server.exe",
        "resources\bin\evorule-ai-plugin.exe",
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
    # PSObject.Properties 返回内部集合，.Count 走成员枚举（每成员 1）——@() 强转取真实数目
    $mods = @($health.integrity.PSObject.Properties)
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

    # ---- 7. engine health reachable (orchestration complete) ---------------
    # runtime.json 由编排链尾写出（lib.rs：engine→serve 探测→runtime.json），
    # 壳启动后需等编排完成——与第 4-6 步同款 deadline 等待，180s 覆盖 CI 冷启动
    $runtime = Join-Path $dataDir "runtime.json"
    $rtDeadline = (Get-Date).AddSeconds(180)
    while (-not (Test-Path $runtime) -and (Get-Date) -lt $rtDeadline) { Start-Sleep -Seconds 3 }
    if (-not (Test-Path $runtime)) { throw "runtime.json missing under $dataDir (orchestration not complete within 180s)" }
    $rt = Get-Content $runtime -Raw | ConvertFrom-Json
    # runtime.json 为嵌套结构（orchestrator.rs last_ports：serve.port / engine.port / llm_executor.port）
    $enginePort = $rt.engine.port
    if (-not $enginePort) { throw "engine_port missing in runtime.json" }
    # ---- 7. engine health reachable -----------------------------------------
    # 说明：装机冒烟的本职=安装完整性（进程起得来、HTTP 面活着），不背书
    # 服务目录内容。/api/services 在 fresh install 上合法为空（清单=
    # native+registry 绑定，装机首启无绑定；run 41596263340/41601796694
    # 两轮实测 0 条）；服务目录正确性属 evorule-server 仓自身测试面。
    # udf_* 服务经 wasm-host 插件自有面（:9140/services）暴露，实载查验
    # 由第 3 步承担（wasm-host 5 modules verified）。
    $deadline = (Get-Date).AddSeconds(30)
    $engineOk = $false
    while ((Get-Date) -lt $deadline) {
        try {
            $h = Invoke-WebRequest -Uri "http://127.0.0.1:$enginePort/api/health" -UseBasicParsing -TimeoutSec 5
            if ($h.StatusCode -eq 200) { $engineOk = $true; break }
        } catch { }
        Start-Sleep -Seconds 3
    }
    if (-not $engineOk) { throw "engine :$enginePort /api/health not reachable within 30s" }
    Write-Host "[smoke] engine :$enginePort /api/health: OK"

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
} catch {
    # 失败现场保留：dump 壳/引擎日志尾段到 CI 输出（finally 的清理会毁掉现场，
    # 不可诊断=每轮死因只能猜——run 41568353520 教训）
    Write-Host "[smoke] FAILED: $($_.Exception.Message)"
    $logDir = Join-Path $dataDir "logs"
    if (Test-Path $logDir) {
        foreach ($lf in Get-ChildItem $logDir -Filter *.log -ErrorAction SilentlyContinue) {
            Write-Host "===== $($lf.Name) (tail 40) ====="
            Get-Content $lf.FullName -Tail 40 -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "  $_" }
        }
    } else {
        Write-Host "[smoke] no logs dir under $dataDir"
        if (Test-Path $dataDir) { Get-ChildItem $dataDir -Recurse -ErrorAction SilentlyContinue | Select-Object -First 20 | ForEach-Object { Write-Host "  $($_.FullName)" } }
    }
    exit 1
} finally {
    Stop-SmokeShell
    Start-Sleep -Seconds 2
    if (Test-Path $install) { Remove-Item $install -Recurse -Force -ErrorAction SilentlyContinue }
    if (Test-Path $dataDir) { Remove-Item $dataDir -Recurse -Force -ErrorAction SilentlyContinue }
    Write-Host "[smoke] cleanup done"
}
