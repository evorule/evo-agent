# 拉起 console dev server(5174 vite) - 幂等
$OpsDir = $PSScriptRoot
. (Join-Path $OpsDir '_common.ps1')
$cfg = Get-OpsConfig -OpsDir $OpsDir
$svc = $cfg.services.console

if (Test-OpsProbe -Probe $svc.probe) {
    Write-Host '[ops] console dev server 已在运行(探活通过), 跳过'
    exit 0
}

$p = Start-OpsDetached -Exe $svc.exe -ArgLine $svc.args -Workdir $svc.workdir `
    -StdoutLog (Join-Path $cfg.log_dir 'console.log')
Start-Sleep -Seconds 2
if (Wait-OpsProbe -Probe $svc.probe -TimeoutSec 20) {
    Write-Host "[ops] console dev server 已启动 pid=$($p.Id), 探活通过"
} else {
    Write-Host "[ops] console dev server 已拉起 pid=$($p.Id) 但探活未通过(vite 启动可能需数秒), 详见 $($cfg.log_dir)\console.stderr.log"
    exit 1
}
