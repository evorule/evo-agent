# evorule 本地运维脚本族 - 共享工具(配置加载/探活/分离启动/日志)
# 由各 start-*.ps1 与 watchdog-check.ps1 点源加载,不单独执行
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-OpsConfig {
    param([Parameter(Mandatory)][string]$OpsDir)
    $local = Join-Path $OpsDir 'ops.local.json'
    if (-not (Test-Path $local)) {
        Write-Host "[ops] 未找到本地配置: $local"
        Write-Host "[ops] 请复制 ops.local.example.json 为 ops.local.json,并按本机实际路径修改"
        exit 2
    }
    return Get-Content $local -Raw -Encoding utf8 | ConvertFrom-Json
}

function Test-OpsProbe {
    # 探活: http 用状态码 2xx-4xx 视为存活(5xx 视为不健康), tcp 用端口连通
    param([Parameter(Mandatory)]$Probe)
    try {
        if ($Probe.type -eq 'http') {
            $r = Invoke-WebRequest -Uri $Probe.url -UseBasicParsing -TimeoutSec $Probe.timeout_sec
            return ($r.StatusCode -ge 200 -and $r.StatusCode -lt 500)
        }
        if ($Probe.type -eq 'tcp') {
            $client = [System.Net.Sockets.TcpClient]::new()
            try {
                $ok = $client.ConnectAsync('127.0.0.1', [int]$Probe.port).Wait([TimeSpan]::FromSeconds([double]$Probe.timeout_sec))
                return ($ok -and $client.Connected)
            } finally { $client.Dispose() }
        }
        throw "[ops] 未知探活类型: $($Probe.type)"
    } catch { return $false }
}

function Wait-OpsProbe {
    # 轮询探活直到通过或超时(服务启动耗时可能数秒)
    param([Parameter(Mandatory)]$Probe,[Parameter(Mandatory)][int]$TimeoutSec)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-OpsProbe -Probe $Probe) { return $true }
        Start-Sleep -Seconds 1
    }
    return (Test-OpsProbe -Probe $Probe)
}

function Start-OpsDetached {
    # 分离启动: 子进程脱离调用方独立存活,stdout/stderr 落日志(上次日志轮转为 .prev)
    param(
        [Parameter(Mandatory)][string]$Exe,
        [Parameter(Mandatory)][AllowEmptyString()][string]$ArgLine,
        [string]$Workdir,
        [string]$StdoutLog,
        [hashtable]$ExtraEnv = @{}
    )
    foreach ($key in $ExtraEnv.Keys) { Set-Item -Path "env:$key" -Value $ExtraEnv[$key] }
    $resolved = if (Test-Path $Exe) { $Exe } elseif (Get-Command $Exe -ErrorAction SilentlyContinue) { $Exe } else { throw "[ops] 可执行文件不存在且不在 PATH: $Exe" }
    $argList = if ($Workdir) { @{ WorkingDirectory = $Workdir } } else { @{} }
    if ($ArgLine) { $argList.ArgumentList = $ArgLine }
    if ($StdoutLog) {
        $dir = Split-Path $StdoutLog -Parent
        if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
        $out = $StdoutLog -replace '\.log$', '.stdout.log'
        $err = $StdoutLog -replace '\.log$', '.stderr.log'
        foreach ($f in @($out, $err)) {
            if (Test-Path $f) {
                # Rotation fallback (a stale handle on .prev must not block startup):
                # try rename; if .prev is locked, clear it and retry; if still locked,
                # skip rotation and let Redirect overwrite the old log.
                try { Move-Item $f "$f.prev" -Force -ErrorAction Stop } catch {
                    try {
                        Remove-Item "$f.prev" -Force -ErrorAction Stop
                        Move-Item $f "$f.prev" -Force -ErrorAction Stop
                    } catch { Write-Warning "[ops] log rotation failed for '$f', old log will be overwritten: $_" }
                }
            }
        }
        return Start-Process -FilePath $resolved -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $out -RedirectStandardError $err @argList
    }
    return Start-Process -FilePath $resolved -WindowStyle Hidden -PassThru @argList
}

function Write-OpsLog {
    param([Parameter(Mandatory)][string]$LogDir,[Parameter(Mandatory)][string]$Message)
    if (-not (Test-Path $LogDir)) { New-Item -ItemType Directory -Path $LogDir -Force | Out-Null }
    $line = '{0} {1}' -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $Message
    Add-Content -Path (Join-Path $LogDir 'watchdog.log') -Value $line -Encoding utf8
    Write-Host $line
}

function Repair-OpsStaleBinary {
    # 二进制陈旧自愈: 启动器新鲜度门判 STALE/MISSING 时自动重建, 复核与拉起仍由调用方负责。
    # 接线缘起(2026-10-21): 此前「改码→exe 陈旧→看门狗每次拉起被门挡死→进程一断永不自愈」——
    # 门挡陈旧代码是对的, 缺的是自动重建环节。构建限档 -j 2(本机内存红线)。
    param(
        [Parameter(Mandatory)][string]$ExePath,
        [Parameter(Mandatory)][string]$RepoPath,
        [Parameter(Mandatory)][string[]]$Paths,
        [Parameter(Mandatory)][string]$SvcName,
        [Parameter(Mandatory)][string]$Status,
        [Parameter(Mandatory)][string]$LogDir
    )
    Write-OpsLog -LogDir $LogDir -Message "[watchdog] $SvcName 二进制 $Status, 自动重建(-j 2 限档)..."
    $buildScript = Join-Path $PSScriptRoot 'build-rust-service.ps1'
    $prevJobs = $env:CARGO_BUILD_JOBS
    $env:CARGO_BUILD_JOBS = '2'
    $ok = $false
    try {
        & $buildScript -Repo $RepoPath -Exe $ExePath -StopServe *>> (Join-Path $LogDir 'autobuild.log')
        $ok = ($LASTEXITCODE -eq 0)
    } catch {
        Write-OpsLog -LogDir $LogDir -Message "[watchdog] $SvcName 自动重建异常: $($_.Exception.Message)"
    } finally {
        if ($null -ne $prevJobs) { $env:CARGO_BUILD_JOBS = $prevJobs } else { Remove-Item env:CARGO_BUILD_JOBS -ErrorAction SilentlyContinue }
    }
    if ($ok) {
        Write-OpsLog -LogDir $LogDir -Message "[watchdog] $SvcName 自动重建完成, 复核新鲜度后继续"
    } else {
        Write-OpsLog -LogDir $LogDir -Message "[watchdog] $SvcName 自动重建失败 — 详见 autobuild.log, 拒绝拉起"
    }
    return $ok
}

function Test-OpsBinaryFreshness {
    # Compare the built binary's mtime against the last commit time that touched
    # the paths which feed the binary (source + manifests). Guards against a
    # stale exe silently serving old code after an in-place build failed to link
    # (e.g. target locked by a running process: cargo may print "Finished"
    # without actually linking).
    # Returns Status: STALE | FRESH | UNKNOWN | MISSING (UNKNOWN never blocks).
    param(
        [Parameter(Mandatory)][string]$ExePath,
        [Parameter(Mandatory)][string]$RepoPath,
        [Parameter(Mandatory)][string[]]$Paths
    )
    if (-not (Test-Path $ExePath)) {
        return [pscustomobject]@{ Status = 'MISSING'; Detail = "exe 不存在(需先构建): $ExePath";
                                 ExeMtimeEpoch = $null; CodeCommitEpoch = $null; CodeCommitShortSha = $null }
    }
    $unknown = { return [pscustomobject]@{ Status = 'UNKNOWN'; Detail = 'git 不可用或非 git 仓, 跳过新鲜度核对(放行)';
                                           ExeMtimeEpoch = $null; CodeCommitEpoch = $null; CodeCommitShortSha = $null } }
    try {
        $null = & git -C $RepoPath rev-parse --is-inside-work-tree 2>$null
        if ($LASTEXITCODE -ne 0) { return & $unknown }
    } catch { return & $unknown }
    # Last commit that touched binary-relevant paths (docs/tests are excluded).
    $out = $null
    try { $out = & git -C $RepoPath log -1 --format='%h %ct' -- $Paths 2>$null } catch { return & $unknown }
    if ($LASTEXITCODE -ne 0 -or -not $out) { return & $unknown }
    $parts = ("$out".Trim() -split '\s+')
    if ($parts.Count -lt 2 -or $parts[1] -notmatch '^\d+$') { return & $unknown }
    $codeEpoch = [int64]$parts[1]
    # PS 5.1 (.NET Framework) DateTime lacks ToUnixTimeSeconds -- convert via
    # DateTimeOffset(ticks, zero offset); ticks of a UTC DateTime are UTC scale.
    $exeDt = [IO.File]::GetLastWriteTimeUtc($ExePath)
    $exeEpoch = [DateTimeOffset]::new($exeDt.Ticks, [TimeSpan]::Zero).ToUnixTimeSeconds()
    $status = if ($exeEpoch -lt $codeEpoch) { 'STALE' } else { 'FRESH' }
    $detail = 'exe mtime={0} vs 代码 commit {1}({2})' -f $exeEpoch, $parts[0], $codeEpoch
    return [pscustomobject]@{ Status = $status; Detail = $detail;
                             ExeMtimeEpoch = $exeEpoch; CodeCommitEpoch = $codeEpoch; CodeCommitShortSha = $parts[0] }
}
