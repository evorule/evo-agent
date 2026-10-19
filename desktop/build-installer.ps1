# NSIS 安装包构建脚本：收集内嵌资源 → tauri build → 更新三件套归集。
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
# 用法：powershell -File build-installer.ps1 [-SkipCollect] [-UpdateUrlBase <base>] [-ReleaseNotes <notes>]
# 产物：desktop\target\release\bundle\nsis\evo-agent-desktop_<ver>_x64-setup.exe（+.sig）
#       desktop\target\release\bundle\updater\{setup.exe, .sig, latest.json}（三件套归集）
#
# 签名要求：tauri build 需 TAURI_SIGNING_PRIVATE_KEY / TAURI_SIGNING_PRIVATE_KEY_PASSWORD
# env（私钥位置见发布留痕档）；无 env 时 build 产出 setup.exe 但 .sig 缺失，本脚本报错终止。
#
# latest.json 由本脚本程序化生成（signature 从 .sig 文件读原文注入，禁止手写——
# 手抄 base64 极易漂移）；url = UpdateUrlBase + 安装包文件名。发布时传真实
# base（如 https://gitee.com/{owner}/{repo}/releases/download/latest），
# 缺省本地调试形态 http://127.0.0.1:18777；上传 Release 由人工按发布步骤档执行。

param(
    [switch]$SkipCollect,
    [string]$UpdateUrlBase = "http://127.0.0.1:18777",
    [string]$ReleaseNotes = ""
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

    # console 静态产物（可选源，存在才收；W6：在位即启用，缺位 = 安装包不带
    # console 通道，壳启动探测缺位后静默不启用）。源 = console-cloud 仓
    # CONSOLE_BASE=/console/ 构建产物。
    $consoleBuild = "D:\evorule-console-cloud\build"
    if (Test-Path (Join-Path $consoleBuild "index.html")) {
        $consoleDst = Join-Path $srcTauri "resources\console"
        if (Test-Path $consoleDst -PathType Container) { Remove-Item $consoleDst -Recurse -Force }
        New-Item -ItemType Directory -Path $consoleDst -Force | Out-Null
        Copy-Item $consoleBuild (Join-Path $consoleDst "build") -Recurse -Force
        Write-Host "collected $consoleBuild -> resources\console\build"
    } else {
        Write-Warning "console build missing (run: CONSOLE_BASE=/console/ npm --prefix D:\evorule-console-cloud run build); installer ships without console"
    }
}

Set-Location $desktop
npm run tauri build
if ($LASTEXITCODE -ne 0) { throw "tauri build failed ($LASTEXITCODE)" }

# ---- 更新三件套归集（M4.2）：setup.exe + .sig + latest.json -> bundle\updater\ ----
$nsisDir = Join-Path $desktop "target\release\bundle\nsis"
$setup = Get-ChildItem $nsisDir -Filter "*_x64-setup.exe" |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $setup) { throw "no setup.exe found in $nsisDir" }
$sigPath = "$($setup.FullName).sig"
if (-not (Test-Path $sigPath)) {
    throw "updater signature missing: $sigPath（需 TAURI_SIGNING_PRIVATE_KEY env 重跑）"
}

$sig = (Get-Content $sigPath -Raw).Trim()
if ($setup.Name -notmatch '^evo-agent-desktop_(.+)_x64-setup\.exe$') {
    throw "unexpected setup name: $($setup.Name)"
}
$version = $Matches[1]
$latest = @{
    version  = $version
    notes    = $ReleaseNotes
    pub_date = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    platforms = @{
        "windows-x86_64" = @{
            signature = $sig
            url       = "$UpdateUrlBase/$($setup.Name)"
        }
    }
} | ConvertTo-Json -Depth 5

$outDir = Join-Path $desktop "target\release\bundle\updater"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
[System.IO.File]::WriteAllText((Join-Path $outDir "latest.json"), $latest)
Copy-Item $setup.FullName $outDir -Force
Copy-Item $sigPath $outDir -Force

# 机器校验：回读 latest.json 与 .sig 原文逐字节比对，防生成环节漂移
$check = Get-Content (Join-Path $outDir "latest.json") -Raw | ConvertFrom-Json
if ($check.platforms.'windows-x86_64'.signature -cne $sig) {
    throw "latest.json signature mismatch after generation"
}

Write-Host "updater artifacts (v$version) -> $outDir"
Write-Host "  $($setup.Name)"
Write-Host "  $($setup.Name).sig"
Write-Host "  latest.json (url=$UpdateUrlBase/$($setup.Name))"
