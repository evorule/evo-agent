# Build a Rust service binary with guards against silent stale output:
#   1. Pre-check: refuse to build while the target exe is locked by a running
#      process (linker would fail or, worse, cargo may print "Finished" without
#      linking). Use -StopServe to stop the locking processes automatically.
#   2. Build: cargo build --release in the repo directory.
#   3. Post-check: verify the exe mtime advanced past the build start time,
#      otherwise fail (catches the "Finished but not linked" case).
# Usage: build-rust-service.ps1 -Repo <repo path> -Exe <exe path> [-StopServe]
param(
    [Parameter(Mandatory = $true)][string]$Repo,
    [Parameter(Mandatory = $true)][string]$Exe,
    [switch]$StopServe
)
$ErrorActionPreference = 'Continue'

if (-not (Test-Path $Repo)) { Write-Host "FAIL: repo 不存在: $Repo"; exit 1 }

if (Test-Path $Exe) {
    $locked = $false
    try {
        $fs = [IO.File]::Open($Exe, 'Open', 'ReadWrite', 'None')
        $fs.Close()
    } catch { $locked = $true }
    if ($locked) {
        $holders = @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Exe })
        $ids = ($holders | ForEach-Object { "$($_.Id) $($_.ProcessName)" }) -join '; '
        if (-not $StopServe) {
            Write-Host "FAIL: 目标 exe 被运行中进程占用, 链接必失败或被静默跳过: $Exe"
            Write-Host "  占用进程: $(if ($ids) { $ids } else { '(无法枚举, 可能权限不足)' })"
            Write-Host "  修复: 停服后重跑; 或加 -StopServe 由本脚本停止占用进程"
            exit 1
        }
        foreach ($p in $holders) {
            Write-Host "[build] -StopServe: 停止占用进程 pid=$($p.Id) ($($p.ProcessName))"
            try { Stop-Process -Id $p.Id -Force -ErrorAction Stop } catch { Write-Host "  停止失败: $_"; exit 1 }
        }
        Start-Sleep -Seconds 1
    }
} else {
    Write-Host "[build] 目标 exe 不存在(首次构建场景), 跳过占用预检: $Exe"
}

$startEpoch = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
Push-Location $Repo
try {
    & cargo build --release
    $rc = $LASTEXITCODE
} catch {
    Write-Host "FAIL: cargo 调用失败(是否在 PATH?): $_"
    $rc = 1
} finally {
    Pop-Location
}
if ($rc -ne 0) { Write-Host "FAIL: cargo build --release 退出码 $rc"; exit 1 }

if (-not (Test-Path $Exe)) {
    Write-Host "FAIL: 构建成功但 exe 未产出: $Exe"; exit 1
}
$exeDt = [IO.File]::GetLastWriteTimeUtc($Exe)
$exeEpoch = [DateTimeOffset]::new($exeDt.Ticks, [TimeSpan]::Zero).ToUnixTimeSeconds()
if ($exeEpoch -lt $startEpoch) {
    Write-Host "FAIL: cargo 报告成功但 exe mtime 未更新 (mtime=$exeEpoch < build start=$startEpoch) — 疑似链接被静默跳过, 请停服后重试"
    exit 1
}
Write-Host "PASS: 构建成功, exe mtime (epoch s): $exeEpoch — 如服务在运行请重启以加载新二进制"
exit 0
