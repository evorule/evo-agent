# 拉起 evo-agent serve(8081) - 幂等; 自动注入 .env 环境变量(LLM 密钥等,只打印键名不打印值)
$OpsDir = $PSScriptRoot
. (Join-Path $OpsDir '_common.ps1')
$cfg = Get-OpsConfig -OpsDir $OpsDir
$svc = $cfg.services.'evo-agent-serve'

if (Test-OpsProbe -Probe $svc.probe) {
    Write-Host '[ops] evo-agent serve 已在运行(探活通过), 跳过'
    exit 0
}

$extra = @{}
if ($svc.env_file -and (Test-Path $svc.env_file)) {
    foreach ($line in Get-Content $svc.env_file -Encoding utf8) {
        if ($line -match '^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$') {
            $val = $Matches[2]
            if (($val.StartsWith('"') -and $val.EndsWith('"') -and $val.Length -ge 2) -or
                ($val.StartsWith("'") -and $val.EndsWith("'") -and $val.Length -ge 2)) {
                $val = $val.Substring(1, $val.Length - 2)
            }
            $extra[$Matches[1]] = $val
        }
    }
    Write-Host ("[ops] 已从 {0} 注入环境变量(仅键名): {1}" -f $svc.env_file, (($extra.Keys | Sort-Object) -join ', '))
} elseif ($svc.env_file) {
    Write-Host "[ops] 警告: env_file 不存在: $($svc.env_file), 将不带 LLM 密钥启动"
}

$p = Start-OpsDetached -Exe $svc.exe -ArgLine $svc.args -Workdir $svc.workdir `
    -StdoutLog (Join-Path $cfg.log_dir 'evo-agent-serve.log') -ExtraEnv $extra
Start-Sleep -Seconds 1
if (Wait-OpsProbe -Probe $svc.probe -TimeoutSec 10) {
    Write-Host "[ops] evo-agent serve 已启动 pid=$($p.Id), 探活通过"
} else {
    Write-Host "[ops] evo-agent serve 已拉起 pid=$($p.Id) 但探活未通过(可能仍在启动), 详见 $($cfg.log_dir)\evo-agent-serve.stderr.log"
    exit 1
}
