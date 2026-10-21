# 拉起 evorule-ai-plugin(9130, LLM 执行体) - 幂等: 探活通过则跳过
# 红线(实测勘定): 工作目录必须= evorule-server 仓根——模型白名单路径按 CWD 相对解析, 错 CWD 即 fail-closed 拒所有请求。
# key 由 plugins/ai-plugin/ai-plugin.json 携带(本地文件, git 已忽略), 无需 env 注入。
# 无 freshness 门(插件 exe 变更低频; 可用性优先, 避免重建整仓拖慢拉起)。
$OpsDir = $PSScriptRoot
. (Join-Path $OpsDir '_common.ps1')
$cfg = Get-OpsConfig -OpsDir $OpsDir
$svc = $cfg.services.'ai-plugin'

if (Test-OpsProbe -Probe $svc.probe) {
    Write-Host '[ops] ai-plugin 已在运行(探活通过), 跳过'
    exit 0
}

$p = Start-OpsDetached -Exe $svc.exe -ArgLine $svc.args -Workdir $svc.workdir `
    -StdoutLog (Join-Path $cfg.log_dir 'ai-plugin.log')
Start-Sleep -Seconds 1
if (Wait-OpsProbe -Probe $svc.probe -TimeoutSec 10) {
    Write-Host "[ops] ai-plugin 已启动 pid=$($p.Id), 探活通过"
} else {
    Write-Host "[ops] ai-plugin 已拉起 pid=$($p.Id) 但探活未通过(可能仍在启动), 详见 $($cfg.log_dir)\ai-plugin.stderr.log"
    exit 1
}
