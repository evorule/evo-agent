# sync-service-tools.ps1 — 发版模板 service_tools 清单与执行侧 SSOT 对齐
#
# SSOT = {ServerRepo}\plugins\*\official_native_services.json（各家族服务声明
# 的并集；与治理侧嵌入副本同源，scripts/sync-native-services.ps1 链路管双侧）。
# 捆绑模板的清单漂移（少家族/漏新增服务）= 桌面 agent 无法路由对应服务——
# 0.3.0 的 60 项旧清单事故即此因，故本脚本进 build-installer 双闸。
#
# 用法：
#   缺省   = 再生成 desktop\templates\evo-agent.toml 的 service_tools 段（排序去重）
#   -Check = 只校验不落盘：漂移即 exit 1（清单漂移构建必红，杜绝复发）
param(
    # 缺省=本仓同级的 evorule-server 目录（与 build-installer.ps1 同款解析）
    [string]$ServerRepo = "",
    [switch]$Check
)

$ErrorActionPreference = "Stop"

if (-not $ServerRepo) {
    $repoRoot = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
    $ServerRepo = Join-Path (Split-Path $repoRoot -Parent) "evorule-server"
}

$templatePath = Join-Path $PSScriptRoot "..\templates\evo-agent.toml"
if (-not (Test-Path $templatePath)) {
    Write-Error "release template not found: $templatePath"
}

$declFiles = Get-ChildItem (Join-Path $ServerRepo "plugins") -Directory |
    Sort-Object Name |
    ForEach-Object { Join-Path $_.FullName "official_native_services.json" } |
    Where-Object { Test-Path $_ }
$names = foreach ($f in $declFiles) {
    (Get-Content $f -Raw -Encoding UTF8 | ConvertFrom-Json).services |
        ForEach-Object { $_.name }
}

# SSOT 双契约面收编：外部插件包（plugin.json.services）与 NativeService
# 家族并集——契约形态双轨（official_native_services.json vs plugin.json），
# 只扫前者的闸防不住「形态在扫描面外」的整族缺席（codegraph 案实证，
# 见发版面验收机制档 §2.1），故第二面必须纳入同一 SSOT。
#
# ⚠️ 白名单只能收编「确定随发版」的外部包：装机对账面由打包的 plugins
# 清单决定，收编未随包的服务=装机启动期整列 missing 点名（可见噪音）。
# 随发版外部包在此显式登记；新增包须同步 build-installer.ps1 collect
# 收编 + orchestrator 拉起（四联映射，发版面验收机制档 §4 闸 1）。
$ExternalPackages = @("codegraph-bridge")

$pluginFiles = Get-ChildItem (Join-Path $ServerRepo "plugins") -Directory |
    Where-Object { $ExternalPackages -contains $_.Name } |
    ForEach-Object { Join-Path $_.FullName "plugin.json" } |
    Where-Object { Test-Path $_ }
$pluginNames = foreach ($f in $pluginFiles) {
    (Get-Content $f -Raw -Encoding UTF8 | ConvertFrom-Json).services |
        ForEach-Object { $_.name }
}
$names = @($names) + @($pluginNames)
$sorted = @($names | Sort-Object -Unique)
if ($sorted.Count -eq 0) {
    Write-Error "SSOT union is empty — refusing to wipe the template list"
}
if ($names.Count -ne $sorted.Count) {
    Write-Warning "duplicate service names across families dropped: $($names.Count) -> $($sorted.Count)"
}

# 段形态与既有模板一致：每行一项带尾逗号（TOML 允许尾逗号）
$block = "service_tools = [`n" +
    (($sorted | ForEach-Object { "  `"$_`"," }) -join "`n") +
    "`n]"

$raw = Get-Content $templatePath -Raw -Encoding UTF8
$pattern = '(?s)service_tools\s*=\s*\[.*?\]'
$match = [regex]::Match($raw, $pattern)
if (-not $match.Success) {
    Write-Error "service_tools block not found in template: $templatePath"
}

if ($Check) {
    if ($match.Value -ne $block) {
        $current = [regex]::Matches($match.Value, '"([^"]+)"') |
            ForEach-Object { $_.Groups[1].Value }
        $missing = @($sorted | Where-Object { $current -notcontains $_ })
        $extra = @($current | Where-Object { $sorted -notcontains $_ })
        Write-Host "service_tools drift vs SSOT (template=$($current.Count), SSOT=$($sorted.Count)):"
        if ($missing) { Write-Host "  missing: $($missing -join ', ')" }
        if ($extra) { Write-Host "  extra:   $($extra -join ', ')" }
        if (-not $missing -and -not $extra) { Write-Host "  (order/format drift only)" }
        Write-Host "fix: run desktop\scripts\sync-service-tools.ps1 (no args) and commit"
        exit 1
    }
    Write-Host "service_tools OK ($($sorted.Count) items, matches SSOT)"
    exit 0
}

$updated = $raw.Remove($match.Index, $match.Length).Insert($match.Index, $block)
[IO.File]::WriteAllText($templatePath, $updated, [System.Text.UTF8Encoding]::new($false))
Write-Host "service_tools regenerated ($($sorted.Count) items) -> $templatePath"
