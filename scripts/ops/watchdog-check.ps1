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
} finally {
    if ($created) { $mutex.ReleaseMutex() | Out-Null }
    $mutex.Dispose()
}
