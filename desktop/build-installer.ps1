# NSIS 安装包构建脚本：收集内嵌资源 → tauri build。
#
# 资源布局（安装后 {install}\resources\...，由 tauri.conf.json bundle.resources 承接）：
#   resources\bin\evo-agent.exe             <- D:\evo-agent\target\release\evo-agent.exe
#   resources\bin\evorule-server.exe        <- D:\evorule-server\target\release\evorule-server.exe
#   resources\evo-agent\evo-agent.toml      <- D:\evo-agent\evo-agent.toml（可选，用户配置透传基座）
#   resources\evo-agent\agents\             <- D:\evo-agent\agents
#   resources\evo-agent\web\dist\           <- D:\evo-agent\web\dist
#   resources\evorule-server\resources\     <- D:\evorule-server\resources
#   resources\evorule-server\rules\         <- D:\evorule-server\rules
#
# 用法：powershell -File build-installer.ps1 [-SkipCollect]
# 产物：desktop\target\release\bundle\nsis\evo-agent-desktop_<ver>_x64-setup.exe

param(
    [switch]$SkipCollect
)

$ErrorActionPreference = "Stop"
$desktop = $PSScriptRoot
$srcTauri = Join-Path $desktop "src-tauri"

if (-not $SkipCollect) {
    $pairs = @(
        @{ Src = "D:\evo-agent\target\release\evo-agent.exe";            Dst = "resources\bin\evo-agent.exe" },
        @{ Src = "D:\evorule-server\target\release\evorule-server.exe";  Dst = "resources\bin\evorule-server.exe" },
        @{ Src = "D:\evo-agent\agents";                                  Dst = "resources\evo-agent\agents" },
        @{ Src = "D:\evo-agent\web\dist";                                Dst = "resources\evo-agent\web\dist" },
        @{ Src = "D:\evorule-server\resources";                          Dst = "resources\evorule-server\resources" },
        @{ Src = "D:\evorule-server\rules";                              Dst = "resources\evorule-server\rules" }
    )

    foreach ($p in $pairs) {
        if (-not (Test-Path $p.Src)) { throw "missing source: $($p.Src)（先 cargo build --release 两个引擎仓）" }
        $dst = Join-Path $srcTauri $p.Dst
        $dstParent = Split-Path $dst -Parent
        if (-not (Test-Path $dstParent)) { New-Item -ItemType Directory -Path $dstParent -Force | Out-Null }
        if (Test-Path $dst -PathType Container) { Remove-Item $dst -Recurse -Force }
        Copy-Item $p.Src $dst -Recurse -Force
        Write-Host "collected $($p.Src) -> $dst"
    }

    # 用户配置透传基座（可选源，存在才收）
    $userToml = "D:\evo-agent\evo-agent.toml"
    if (Test-Path $userToml) {
        Copy-Item $userToml (Join-Path $srcTauri "resources\evo-agent\evo-agent.toml") -Force
        Write-Host "collected $userToml -> resources\evo-agent\evo-agent.toml"
    }
}

Set-Location $desktop
npm run tauri build
