# 注册 Windows 计划任务 EvoruleOpsWatchdog: 用户登录时 + 每 1 分钟重复巡检
# 卸载: Unregister-ScheduledTask -TaskName EvoruleOpsWatchdog -Confirm:$false
$OpsDir = $PSScriptRoot
$script = Join-Path $OpsDir 'watchdog-check.ps1'
if (-not (Test-Path $script)) { throw "未找到 $script" }
$pwsh = (Get-Command pwsh -ErrorAction Stop).Source
$ErrorActionPreference = 'Stop'

$action = New-ScheduledTaskAction -Execute $pwsh `
    -Argument "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$script`""
# 注: RepetitionDuration 用 3650 天(Task Scheduler 不接受无限时长 XML),到期前重新注册即可
# 巡检间隔 1 小时(项目方 2026-09-23 裁定:没必要太频繁;开机自启由 AtLogOn 触发保证)
$repeat = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) `
    -RepetitionInterval (New-TimeSpan -Hours 1) -RepetitionDuration (New-TimeSpan -Days 3650)
$logon = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable `
    -ExecutionTimeLimit (New-TimeSpan -Minutes 10) -MultipleInstances IgnoreNew `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries

Register-ScheduledTask -TaskName 'EvoruleOpsWatchdog' -Action $action `
    -Trigger @($repeat, $logon) -Settings $settings `
    -Description 'evorule 生态服务看门狗(18080/8081/5174 探活自愈,登录自启)' -Force | Out-Null
Write-Host '[ops] 计划任务 EvoruleOpsWatchdog 已注册(登录触发 + 每 1 小时巡检)'
Write-Host '[ops] 查看: Get-ScheduledTask EvoruleOpsWatchdog | Get-ScheduledTaskInfo'
