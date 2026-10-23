# NSIS 安装包构建脚本：收集内嵌资源 → tauri build → 更新三件套归集。
#
# 资源布局（安装后 {install}\resources\...，由 tauri.conf.json bundle.resources 承接）：
#   resources\bin\evo-agent.exe             <- <repo>\target\release\evo-agent.exe
#   resources\bin\evorule-server.exe        <- <server-repo>\target\release\evorule-server.exe
#   resources\bin\evorule-ai-plugin.exe     <- <server-repo>\plugins\ai-plugin\target\release\（LLM 执行体）
#   resources\evo-agent\evo-agent.toml      <- desktop\templates\evo-agent.toml（发版模板，唯一源）
#   resources\evo-agent\agents\             <- <repo>\agents
#   resources\evo-agent\web\dist\           <- <repo>\web\dist
#   resources\evorule-server\resources\     <- <server-repo>\resources
#   resources\evorule-server\rules\         <- <server-repo>\rules
#   resources\evorule-server\agent-governance\rules\00_constraint_model_allowlist.json
#                                           <- <server-repo>\data\agent-governance\rules\（执行体白名单物料）
#
# <repo>       = 本仓根（本脚本所在 desktop 目录的上级）。
# <server-repo> = 引擎仓根，默认取本仓同级 evorule-server 目录，-ServerRepo 可覆盖。
# console 仓同理（-ConsoleRepo，默认同级 evorule-console-cloud）。
#
# 用法：powershell -File build-installer.ps1 [-SkipCollect] [-UpdateUrlBase <base>] [-ReleaseNotes <notes>]
# 产物：desktop\target\release\bundle\nsis\evo-agent-desktop_<ver>_x64-setup.exe（+.sig）
#       desktop\target\release\bundle\updater\{setup.exe, .sig, latest.json}（三件套归集）
#
# 签名要求：tauri build 需 TAURI_SIGNING_PRIVATE_KEY / TAURI_SIGNING_PRIVATE_KEY_PASSWORD
# env（私钥位置见发布留痕档）；无 env 时 build 产出 setup.exe 但 .sig 缺失，本脚本报错终止。
#
# latest.json 由本脚本程序化生成（signature 从 .sig 文件读原文注入，禁止手写——
# 手抄 base64 极易漂移）；url = UpdateUrlBase + 安装包文件名。缺省（不传参）=
# 按本版 tag 的 GitHub Releases permalink（.../releases/download/evo-agent-desktop-v<ver>）
# ——更新器端点取 releases/latest/latest.json 自动追踪最新发布，但 latest.json 内
# url 指向的安装包必须钉本版 tag（latest 解析到更新版本时文件名已变，钉 permalink
# 才不空指）；本地源调试传 http://127.0.0.1:18777 等显式 base 覆盖；上传 Release
# 由人工按发布步骤档执行（latest.json 随三件套上传）。

param(
    [switch]$SkipCollect,
    [switch]$SkipFreshness,
    [string]$ServerRepo = "",
    [string]$ConsoleRepo = "",
    [string]$UpdateUrlBase = "",
    [string]$ReleaseNotes = ""
)

$ErrorActionPreference = "Stop"
$desktop = $PSScriptRoot
$srcTauri = Join-Path $desktop "src-tauri"
$repoRoot = Split-Path $desktop -Parent
if (-not $ServerRepo) { $ServerRepo = Join-Path (Split-Path $repoRoot -Parent) "evorule-server" }
if (-not $ConsoleRepo) { $ConsoleRepo = Join-Path (Split-Path $repoRoot -Parent) "evorule-console-cloud" }

if (-not $SkipCollect) {
    # 新鲜度门（发版前置红线）:产物须新于源码最后提交(先定稿提交、再 release
    # 重建的发行纪律),源码树须干净(未提交 bits 不得入包)。判定逻辑与实测
    # 见 scripts/check-build-freshness.ps1;-SkipFreshness 仅限本地调试,发版禁用。
    if (-not $SkipFreshness) {
        & powershell -NoProfile -ExecutionPolicy Bypass -File `
            (Join-Path $desktop "scripts\check-build-freshness.ps1") `
            -RepoRoot $repoRoot -ServerRepo $ServerRepo -ConsoleRepo $ConsoleRepo
        if ($LASTEXITCODE -ne 0) {
            throw "build freshness gate failed(见上;先重建再打包,-SkipFreshness 仅限本地调试)"
        }
    }

    $pairs = @(
        @{ Src = (Join-Path $repoRoot "target\release\evo-agent.exe");        Dst = "resources\bin\evo-agent.exe" },
        @{ Src = (Join-Path $ServerRepo "target\release\evorule-server.exe"); Dst = "resources\bin\evorule-server.exe" },
        @{ Src = (Join-Path $ServerRepo "plugins\ai-plugin\target\release\evorule-ai-plugin.exe"); Dst = "resources\bin\evorule-ai-plugin.exe" },
        @{ Src = (Join-Path $repoRoot "agents");                              Dst = "resources\evo-agent\agents" },
        @{ Src = (Join-Path $repoRoot "web\dist");                            Dst = "resources\evo-agent\web\dist" },
        @{ Src = (Join-Path $ServerRepo "resources");                         Dst = "resources\evorule-server\resources" },
        @{ Src = (Join-Path $ServerRepo "rules");                             Dst = "resources\evorule-server\rules" },
        @{ Src = (Join-Path $ServerRepo "data\agent-governance\rules\00_constraint_model_allowlist.json"); Dst = "resources\evorule-server\agent-governance\rules\00_constraint_model_allowlist.json" }
    )

    foreach ($p in $pairs) {
        if (-not (Test-Path $p.Src)) { throw "missing source: $($p.Src)（先 cargo build --release 引擎仓与 ai-plugin）" }
        $dst = Join-Path $srcTauri $p.Dst
        $dstParent = Split-Path $dst -Parent
        if (-not (Test-Path $dstParent)) { New-Item -ItemType Directory -Path $dstParent -Force | Out-Null }
        if (Test-Path $dst -PathType Container) { Remove-Item $dst -Recurse -Force }
        Copy-Item $p.Src $dst -Recurse -Force
        Write-Host "collected $($p.Src) -> $dst"
    }

    # 发版模板（唯一源）：desktop\templates\evo-agent.toml；仓根 evo-agent.toml
    # 是开发本地配置（gitignore），不再是发版源。收集前先过 SSOT 校验——
    # service_tools 清单漂移（少家族/漏服务）= 桌面 agent 无法路由，构建必红
    # （子进程调用：校验脚本自身的 exit 码不经 & 传染本脚本）。
    $syncScript = Join-Path $desktop "scripts\sync-service-tools.ps1"
    & powershell -NoProfile -ExecutionPolicy Bypass -File $syncScript -ServerRepo $ServerRepo -Check
    if ($LASTEXITCODE -ne 0) {
        throw "service_tools drift vs SSOT — run desktop\scripts\sync-service-tools.ps1 and commit"
    }
    $templateToml = Join-Path $desktop "templates\evo-agent.toml"
    if (-not (Test-Path $templateToml)) { throw "release template missing: $templateToml" }
    Copy-Item $templateToml (Join-Path $srcTauri "resources\evo-agent\evo-agent.toml") -Force
    Write-Host "collected $templateToml -> resources\evo-agent\evo-agent.toml"

    # console 静态产物（可选源，存在才收；W6：在位即启用，缺位 = 安装包不带
    # console 通道，壳启动探测缺位后静默不启用）。源 = console 仓
    # CONSOLE_BASE=/console/ 构建产物。
    $consoleBuild = Join-Path $ConsoleRepo "build"
    if (Test-Path (Join-Path $consoleBuild "index.html")) {
        $consoleDst = Join-Path $srcTauri "resources\console"
        if (Test-Path $consoleDst -PathType Container) { Remove-Item $consoleDst -Recurse -Force }
        New-Item -ItemType Directory -Path $consoleDst -Force | Out-Null
        Copy-Item $consoleBuild (Join-Path $consoleDst "build") -Recurse -Force
        Write-Host "collected $consoleBuild -> resources\console\build"
    } else {
        Write-Warning "console build missing (run: CONSOLE_BASE=/console/ npm --prefix $ConsoleRepo run build); installer ships without console"
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
if (-not $UpdateUrlBase) {
    # 缺省=按本版 tag 的 GitHub Releases permalink（见文件头注释：latest.json
    # 内 url 必须钉本版 tag，更新器入口端点才可用 latest 自动追踪）
    $UpdateUrlBase = "https://github.com/evorule/evo-agent-desktop/releases/download/evo-agent-desktop-v$version"
}
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
# （-Encoding UTF8 必带：WriteAllText 产出无 BOM UTF-8，PS5.1 Get-Content 缺省
# 按 ANSI/GBK 误读，中文双字节配对吞引号会伪报 Unterminated string）
$check = Get-Content (Join-Path $outDir "latest.json") -Raw -Encoding UTF8 | ConvertFrom-Json
if ($check.platforms.'windows-x86_64'.signature -cne $sig) {
    throw "latest.json signature mismatch after generation"
}

Write-Host "updater artifacts (v$version) -> $outDir"
Write-Host "  $($setup.Name)"
Write-Host "  $($setup.Name).sig"
Write-Host "  latest.json (url=$UpdateUrlBase/$($setup.Name))"
