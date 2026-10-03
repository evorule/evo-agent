# 拉起 evo-agent serve(8081) - 幂等; 自动注入 .env 环境变量(LLM 密钥等,只打印键名不打印值)
$OpsDir = $PSScriptRoot
. (Join-Path $OpsDir '_common.ps1')
$cfg = Get-OpsConfig -OpsDir $OpsDir
$svc = $cfg.services.'evo-agent-serve'

if (Test-OpsProbe -Probe $svc.probe) {
    Write-Host '[ops] evo-agent serve 已在运行(探活通过), 跳过'
    exit 0
}

# Binary freshness gate: refuse to start a stale/missing exe (config section
# "freshness"; policy "warn" downgrades STALE to a warning, UNKNOWN never blocks).
$freshProp = $svc.PSObject.Properties['freshness']
if ($freshProp) {
    $f = $freshProp.Value
    $policy = if ($f.PSObject.Properties['policy']) { $f.policy } else { 'enforce' }
    if (-not $f.PSObject.Properties['repo'] -or -not $f.PSObject.Properties['paths']) {
        Write-Host '[ops] 配置错误: freshness 节缺少 repo/paths, 跳过新鲜度核对'
    } else {
        $fr = Test-OpsBinaryFreshness -ExePath $svc.exe -RepoPath $f.repo -Paths @($f.paths)
        if (($fr.Status -eq 'STALE' -or $fr.Status -eq 'MISSING') -and $policy -ne 'warn') {
            Write-Host "[ops] 拒绝拉起: $($fr.Status) $($fr.Detail)"
            Write-Host "[ops] 修复: 先停服 → scripts\ops\build-rust-service.ps1 -Repo <仓路径> -Exe <exe路径> -StopServe → 重跑本启动器"
            exit 1
        }
        if ($fr.Status -ne 'FRESH') {
            Write-Host "[ops] 警告: 运行体新鲜度 $($fr.Status) $($fr.Detail)(policy=$policy)"
        }
    }
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
