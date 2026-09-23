# 看门狗单次巡检: 三服务探活,不通则拉起。计划任务每分钟触发,也可手动运行当作"一键全启"。
$created = $false
$mutex = [System.Threading.Mutex]::new($false, 'Global\EvoruleOpsWatchdog')
try { $created = $mutex.WaitOne(0) } catch { $created = $true }
if (-not $created) { exit 0 }

try {
    $OpsDir = $PSScriptRoot
    . (Join-Path $OpsDir '_common.ps1')
    $cfg = Get-OpsConfig -OpsDir $OpsDir
    $starters = @{
        'evorule-server'  = 'start-evorule-server.ps1'
        'evo-agent-serve' = 'start-evo-agent-serve.ps1'
        'console'         = 'start-console.ps1'
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
} finally {
    if ($created) { $mutex.ReleaseMutex() | Out-Null }
    $mutex.Dispose()
}
