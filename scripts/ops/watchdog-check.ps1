# 看门狗单次巡检: 四服务探活,不通则拉起。计划任务每 5 分钟触发(2026-09-30 由每小时提频),也可手动运行当作"一键全启"。
$created = $false
$mutex = [System.Threading.Mutex]::new($false, 'Global\EvoruleOpsWatchdog')
try { $created = $mutex.WaitOne(0) } catch { $created = $true }
if (-not $created) { exit 0 }

try {
    $OpsDir = $PSScriptRoot
    . (Join-Path $OpsDir '_common.ps1')
    $cfg = Get-OpsConfig -OpsDir $OpsDir
    $starters = @{
        'evorule-server'      = 'start-evorule-server.ps1'
        'echo'                = 'start-echo.ps1'
        'evo-agent-serve'     = 'start-evo-agent-serve.ps1'
        'console'             = 'start-console.ps1'
        'context-inspector'   = 'start-context-inspector.ps1'
    }
    foreach ($name in $cfg.services.PSObject.Properties.Name) {
        $svc = $cfg.services.$name
        if (Test-OpsProbe -Probe $svc.probe) { continue }
        Write-OpsLog -LogDir $cfg.log_dir -Message "[watchdog] $name 探活失败, 执行拉起..."
        try {
            & (Join-Path $OpsDir $starters[$name]) *>> (Join-Path $cfg.log_dir 'watchdog.log')
        } catch {
            Write-OpsLog -LogDir $cfg.log_dir -Message "[watchdog] $name 拉起失败: $($_.Exception.Message)"
        }
    }

    # 北极星部署态巡检门禁（2026-10-11 接线）：只巡检告警不做拉起（服务探活由上方循环负责）。
    # 每次覆盖写 last-patrol-status.txt（现态可查不刷日志），FAIL 另记 watchdog.log。
    # 巡检件目录由本地配置提供（ops.local.json g1_adapter_dir，不入库）——公开仓不落私有路径。
    $patrol = if ($cfg.g1_adapter_dir) { Join-Path $cfg.g1_adapter_dir 'northstar_patrol.py' } else { $null }
    if ($patrol -and (Test-Path $patrol)) {
        $py = Join-Path $cfg.g1_adapter_dir '.venv\Scripts\python.exe'
        if (-not (Test-Path $py)) { $py = 'python' }
        try {
            # 任务上下文无 PYTHONUTF8，管道输出退回 GBK——自检子件含 ↔/✓ 等字符即 UnicodeEncodeError 崩溃，统一 UTF-8。
            $env:PYTHONUTF8 = '1'
            try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }
            $out = & $py $patrol 2>&1 | Out-String
            $code = $LASTEXITCODE
            $stamp = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
            $summary = "$stamp exit=$code"
            Set-Content -Path (Join-Path $cfg.log_dir 'last-patrol-status.txt') -Value ($summary + "`r`n" + $out.Trim()) -Encoding UTF8
            if ($code -ne 0) {
                Write-OpsLog -LogDir $cfg.log_dir -Message "[watchdog] 北极星巡检 FAIL (exit=$code)——详见 last-patrol-status.txt"
            }
        } catch {
            Write-OpsLog -LogDir $cfg.log_dir -Message "[watchdog] 北极星巡检执行异常: $($_.Exception.Message)"
        }
    }

    # main CI 红巡检触点（2026-10-11 接线）：只核对告警不做修复。30 分钟节流
    # （GitHub 匿名 API 限额 60 次/时，两仓各一次远低于限额），覆盖写
    # last-ci-status.txt（现态可查不刷日志），有红另记 watchdog.log。
    # PENDING/UNKNOWN/无 check runs 均不告警（push 窗口与网络抖动不当红处理）。
    $ciMarker = Join-Path $cfg.log_dir 'last-ci-check.txt'
    $ciDue = $true
    if (Test-Path $ciMarker) {
        try { $ciDue = ((Get-Date).Ticks - [int64](Get-Content $ciMarker)) -ge 18000000000 } catch { $ciDue = $true }
    }
    if ($ciDue) {
        Set-Content -Path $ciMarker -Value (Get-Date).Ticks -Encoding UTF8
        try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch { }
        $ciLines = @()
        $ciRed = $false
        foreach ($repo in @('evorule/evorule', 'evorule/evo-agent')) {
            $state = 'UNKNOWN'; $detail = ''
            try {
                $cr = Invoke-RestMethod -Uri ("https://api.github.com/repos/{0}/commits/main/check-runs" -f $repo) -TimeoutSec 20
                $runs = @($cr.check_runs)
                $bad = @($runs | Where-Object { $_.conclusion -eq 'failure' })
                $running = @($runs | Where-Object { -not $_.conclusion })
                if ($bad.Count -gt 0) {
                    $state = 'FAIL'; $detail = ($bad | ForEach-Object { $_.name }) -join ', '
                } elseif ($running.Count -gt 0) {
                    $state = 'PENDING'; $detail = ($running | ForEach-Object { $_.name }) -join ', '
                } elseif ($runs.Count -eq 0) {
                    $state = 'NONE'; $detail = 'no check runs on main'
                } else {
                    $state = 'PASS'; $detail = ('{0} checks all success' -f $runs.Count)
                }
            } catch { $state = 'UNKNOWN'; $detail = $_.Exception.Message }
            $ciLines += ('{0} = {1} ({2})' -f $repo, $state, $detail)
            if ($state -eq 'FAIL') { $ciRed = $true }
        }
        Set-Content -Path (Join-Path $cfg.log_dir 'last-ci-status.txt') -Value ((Get-Date -Format 'yyyy-MM-dd HH:mm:ss') + "`r`n" + ($ciLines -join "`r`n")) -Encoding UTF8
        if ($ciRed) {
            Write-OpsLog -LogDir $cfg.log_dir -Message '[watchdog] main CI 红——详见 last-ci-status.txt'
        }
    }
} finally {
    if ($created) { $mutex.ReleaseMutex() | Out-Null }
    $mutex.Dispose()
}
