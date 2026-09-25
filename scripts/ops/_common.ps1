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
        [Parameter(Mandatory)][string]$ArgLine,
        [string]$Workdir,
        [string]$StdoutLog,
        [hashtable]$ExtraEnv = @{}
    )
    foreach ($key in $ExtraEnv.Keys) { Set-Item -Path "env:$key" -Value $ExtraEnv[$key] }
    $resolved = if (Test-Path $Exe) { $Exe } elseif (Get-Command $Exe -ErrorAction SilentlyContinue) { $Exe } else { throw "[ops] 可执行文件不存在且不在 PATH: $Exe" }
    $argList = if ($Workdir) { @{ WorkingDirectory = $Workdir } } else { @{} }
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
        return Start-Process -FilePath $resolved -ArgumentList $ArgLine -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput $out -RedirectStandardError $err @argList
    }
    return Start-Process -FilePath $resolved -ArgumentList $ArgLine -WindowStyle Hidden -PassThru @argList
}

function Write-OpsLog {
    param([Parameter(Mandatory)][string]$LogDir,[Parameter(Mandatory)][string]$Message)
    if (-not (Test-Path $LogDir)) { New-Item -ItemType Directory -Path $LogDir -Force | Out-Null }
    $line = '{0} {1}' -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $Message
    Add-Content -Path (Join-Path $LogDir 'watchdog.log') -Value $line -Encoding utf8
    Write-Host $line
}
