# 拉起 evorule-context-inspector(19100) - 幂等: 探活通过则跳过; 拉起前核对二进制新鲜度
$OpsDir = $PSScriptRoot
. (Join-Path $OpsDir '_common.ps1')
$cfg = Get-OpsConfig -OpsDir $OpsDir
$svc = $cfg.services.'context-inspector'

if (Test-OpsProbe -Probe $svc.probe) {
    Write-Host '[ops] context-inspector 已在运行(探活通过), 跳过'
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

$p = Start-OpsDetached -Exe $svc.exe -ArgLine $svc.args -Workdir $svc.workdir `
    -StdoutLog (Join-Path $cfg.log_dir 'context-inspector.log')
Start-Sleep -Seconds 1
if (Wait-OpsProbe -Probe $svc.probe -TimeoutSec 10) {
    Write-Host "[ops] context-inspector 已启动 pid=$($p.Id), 探活通过"
} else {
    Write-Host "[ops] context-inspector 已拉起 pid=$($p.Id) 但探活未通过(可能仍在启动), 详见 $($cfg.log_dir)\context-inspector.stderr.log"
    exit 1
}
