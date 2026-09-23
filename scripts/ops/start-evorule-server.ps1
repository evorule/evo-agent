# 拉起 evorule-server(18080) - 幂等: 探活通过则跳过
$OpsDir = $PSScriptRoot
. (Join-Path $OpsDir '_common.ps1')
$cfg = Get-OpsConfig -OpsDir $OpsDir
$svc = $cfg.services.'evorule-server'

if (Test-OpsProbe -Probe $svc.probe) {
    Write-Host '[ops] evorule-server 已在运行(探活通过), 跳过'
    exit 0
}

$p = Start-OpsDetached -Exe $svc.exe -ArgLine $svc.args -Workdir $svc.workdir `
    -StdoutLog (Join-Path $cfg.log_dir 'evorule-server.log')
Start-Sleep -Seconds 1
if (Wait-OpsProbe -Probe $svc.probe -TimeoutSec 10) {
    Write-Host "[ops] evorule-server 已启动 pid=$($p.Id), 探活通过"
} else {
    Write-Host "[ops] evorule-server 已拉起 pid=$($p.Id) 但探活未通过(可能仍在启动), 详见 $($cfg.log_dir)\evorule-server.stderr.log"
    exit 1
}
