# 拉起 echo 验证服务(9100) - 幂等: 探活通过则跳过
# 依赖说明: evorule-server 的 service_registry.json 将 call-service/echo_svc
# 指向 127.0.0.1:9100/api/echo —— 宪法桥接(call_service)与桥接探针的外部端点。
# echo 不在线时 serve 仍健康(探活只查 18080), 但桥接 dispatch 一律失败,
# 故 watchdog 将其纳入管理面与 evorule-server 同组自愈。
$OpsDir = $PSScriptRoot
. (Join-Path $OpsDir '_common.ps1')
$cfg = Get-OpsConfig -OpsDir $OpsDir
$svc = $cfg.services.'echo'

if (Test-OpsProbe -Probe $svc.probe) {
    Write-Host '[ops] echo 已在运行(探活通过), 跳过'
    exit 0
}

$p = Start-OpsDetached -Exe $svc.exe -ArgLine $svc.args -Workdir $svc.workdir `
    -StdoutLog (Join-Path $cfg.log_dir 'echo.log')
Start-Sleep -Seconds 1
if (Wait-OpsProbe -Probe $svc.probe -TimeoutSec 10) {
    Write-Host "[ops] echo 已启动 pid=$($p.Id), 探活通过"
} else {
    Write-Host "[ops] echo 已拉起 pid=$($p.Id) 但探活未通过(可能仍在启动), 详见 $($cfg.log_dir)\echo.stderr.log"
    exit 1
}
