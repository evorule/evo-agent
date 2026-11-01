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
#                                           <- 本仓 desktop\installer-assets\agent-governance\rules\
#                                              （执行体白名单物料快照，版本入 git；源物料由 server 侧
#                                              model-admission 配置面经翻译器生成，server 的 data\ 目录
#                                              系运行态不入版本控制，故安装器自带已验证快照。更新物料
#                                              时以新版本 JSON 覆盖本快照并随批提交。）
#
# <repo>       = 本仓根（本脚本所在 desktop 目录的上级）。
# <server-repo> = 引擎仓根，默认取本仓同级 evorule-server 目录，-ServerRepo 可覆盖。
# console 仓同理（-ConsoleRepo，默认同级 evorule-console-cloud）。
#
# 用法：powershell -File build-installer.ps1 [-SkipCollect] [-UpdateUrlBase <base>] [-ReleaseNotes <notes>]
# 五件套来源：THIRD-PARTY-NOTICES.md 与 LICENSES/third-party/ 为本仓自持件
# （桌面分发面仓集子集，由根仓 gen_notice.py --repos 生成并随本仓版本化），不再取自其他仓。
# 产物：desktop\target\release\bundle\nsis\evo-agent-desktop_<ver>_x64-setup.exe（+.sig）
#       desktop\target\release\bundle\updater\{setup.exe, .sig, latest.json, 源码 tarball}（发布归集）
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
    [string]$InspectorRepo = "",
    [string]$UpdateUrlBase = "",
    [string]$ReleaseNotes = ""
)

$ErrorActionPreference = "Stop"
$desktop = $PSScriptRoot
$srcTauri = Join-Path $desktop "src-tauri"
$repoRoot = Split-Path $desktop -Parent
if (-not $ServerRepo) { $ServerRepo = Join-Path (Split-Path $repoRoot -Parent) "evorule-server" }
if (-not $ConsoleRepo) { $ConsoleRepo = Join-Path (Split-Path $repoRoot -Parent) "evorule-console-cloud" }
# inspector 仓同理（-InspectorRepo，默认同级 evorule-context-inspector）；
# exe 由此仓构建产物收集，plugin.json 取 server 仓布局面（双源一致，SSOT 布局面）。
if (-not $InspectorRepo) { $InspectorRepo = Join-Path (Split-Path $repoRoot -Parent) "evorule-context-inspector" }

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
        @{ Src = (Join-Path $repoRoot "desktop\installer-assets\agent-governance\rules\00_constraint_model_allowlist.json"); Dst = "resources\evorule-server\agent-governance\rules\00_constraint_model_allowlist.json" },
        # 外部插件包随发版（A 方向裁定 2026-10-30）：目录资产=server.mjs(零外部
        # 依赖,仅 node: 内建)+vendor(7.7MB 自足)；与 sync-service-tools.ps1 的
        # $ExternalPackages 登记列表联动（四联映射：随包↔白名单↔壳拉起↔接线模板）
        @{ Src = (Join-Path $ServerRepo "plugins\codegraph-bridge");          Dst = "resources\evorule-server\plugins\codegraph-bridge" },
        # ai-plugin 服务面（基线接线批 2026-10-30）：exe 已走 bin 对；plugin.json
        # 单文件入包供服务面注册（ai_plugin_chat 对账可见）。口契约=9130（壳钉口）
        @{ Src = (Join-Path $ServerRepo "plugins\ai-plugin\plugin.json");     Dst = "resources\evorule-server\plugins\ai-plugin\plugin.json" },
        # wasm-host 随发版（外部包接线批二 2026-10-30）：exe/plugin.json/trusted-keys
        # 三件入包 + plugins\wasm 模块资产（*.wasm+*.wasm.sig+*.plugin-module.json）；
        # 缺省签名策略 enforce（fail-closed），信任锚与 .sig 配对出厂。口契约=9140
        @{ Src = (Join-Path $ServerRepo "plugins\wasm-host\target\release\evorule-wasm-host.exe"); Dst = "resources\evorule-server\plugins\wasm-host\evorule-wasm-host.exe" },
        @{ Src = (Join-Path $ServerRepo "plugins\wasm-host\plugin.json");     Dst = "resources\evorule-server\plugins\wasm-host\plugin.json" },
        @{ Src = (Join-Path $ServerRepo "plugins\wasm-host\trusted-keys.json"); Dst = "resources\evorule-server\plugins\wasm-host\trusted-keys.json" },
        @{ Src = (Join-Path $ServerRepo "plugins\wasm");                      Dst = "resources\evorule-server\plugins\wasm" },
        # context-inspector 随发版（B-053c 2026-10-31）：exe 取 inspector 仓构建
        # 产物（path 依赖 evorule/evorule-hash 等三仓并置，CI 检出布局见
        # desktop-release.yml）；plugin.json 取 server 仓布局面（与 inspector 仓
        # 根双源一致）。口契约=19100（admin 服务面）+19101（逐会话代理基址，
        # 本批仅占位预检，绑定接线为后续增量）。锚点流自持（Ed25519 出厂
        # 自生成于运行期 data 目录），无需 trusted-keys 等价物
        @{ Src = (Join-Path $InspectorRepo "target\release\evorule-context-inspector.exe"); Dst = "resources\evorule-server\plugins\evorule-context-inspector\evorule-context-inspector.exe" },
        @{ Src = (Join-Path $ServerRepo "plugins\evorule-context-inspector\plugin.json"); Dst = "resources\evorule-server\plugins\evorule-context-inspector\plugin.json" },
        @{ Src = (Join-Path $repoRoot "desktop\templates\plugins-manifest.json"); Dst = "resources\evorule-server\plugins-manifest-template.json" }
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

    # 外部插件包瘦身：node_modules（开发依赖,server.mjs 仅用 node: 内建,装机
    # 不需要）与服务化脚本（nssm 形态,桌面装机由壳拉起,不装系统服务）不入包
    $cgDst = Join-Path $srcTauri "resources\evorule-server\plugins\codegraph-bridge"
    foreach ($junk in @("node_modules", "start.ps1", "install-service.ps1", "fix-and-start.ps1")) {
        $j = Join-Path $cgDst $junk
        if (Test-Path $j) { Remove-Item $j -Recurse -Force; Write-Host "pruned $junk from codegraph-bridge" }
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

    # 许可证合规五件套（AGPL §4）：LICENSE/NOTICE.md 用本仓自带；THIRD-PARTY-NOTICES.md
    # 与 LICENSES/third-party/ 为本仓自持件（桌面分发面仓集子集：evo-agent +
    # evorule-server + evorule-console-cloud 并集，由根仓 gen_notice.py --repos
    # 生成并随本仓版本化，依赖变化时重跑生成脚本更新）；SOURCE.md 按本次构建
    # 动态生成（§13 指纹表填实值，内嵌组件源码入口一并列明）。落
    # src-tauri\resources\license\，由 tauri.conf.json bundle.resources 承接入
    # 安装包（安装目录 resources\license\）。不取自任何其他仓checkout——
    # 应用分发面与内核仓声明面解耦（架构红线 2026-10-26）。
    if (-not (Test-Path (Join-Path $repoRoot "THIRD-PARTY-NOTICES.md")) -or
        -not (Test-Path (Join-Path $repoRoot "LICENSES\third-party"))) {
        throw "self-held license files missing: $repoRoot（需 THIRD-PARTY-NOTICES.md + LICENSES\third-party\；由根仓 gen_notice.py --repos 生成后随本仓版本化）"
    }
    $licDst = Join-Path $srcTauri "resources\license"
    if (Test-Path $licDst) { Remove-Item $licDst -Recurse -Force }
    New-Item -ItemType Directory -Path (Join-Path $licDst "LICENSES") -Force | Out-Null
    Copy-Item (Join-Path $desktop "LICENSE") (Join-Path $licDst "LICENSE") -Force
    Copy-Item (Join-Path $repoRoot "NOTICE.md") (Join-Path $licDst "NOTICE.md") -Force
    Copy-Item (Join-Path $repoRoot "THIRD-PARTY-NOTICES.md") $licDst -Force
    Copy-Item (Join-Path $repoRoot "LICENSES\third-party") (Join-Path $licDst "LICENSES\third-party") -Recurse -Force

    # §13 源码 tarball（git archive 于本仓 HEAD 生成）+ 指纹，落 updater 归集目录，
    # 随三件套一并人工上传；内嵌组件（引擎/ai-plugin/console）源码各随其仓发版，
    # 入口在 SOURCE.md 内嵌组件表列明。
    $version = (Get-Content (Join-Path $srcTauri "tauri.conf.json") -Raw | ConvertFrom-Json).version
    $commitSha = (git -C $repoRoot rev-parse HEAD).Trim()
    $srcTgz = Join-Path $desktop "target\release\bundle\updater\evo-agent-desktop-$version-source.tar.gz"
    New-Item -ItemType Directory -Force -Path (Split-Path $srcTgz) | Out-Null
    git -C $repoRoot archive --format=tar.gz --prefix="evo-agent-desktop-$version/" -o $srcTgz HEAD
    if ($LASTEXITCODE -ne 0) { throw "git archive failed ($LASTEXITCODE)" }
    $srcSha = (Get-FileHash $srcTgz -Algorithm SHA256).Hash.ToLower()
    $srcTarballName = Split-Path $srcTgz -Leaf
    # here-string 终止符必须在行首（PS 语法约束），本段不缩进
    $sourceMd = @"
<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# evo-agent Desktop — 源码提供入口（AGPL-3.0 §13）

本安装包对应 evo-agent Desktop v$version，以 AGPL-3.0-or-later 授权。本文件随包
分发（安装目录 resources\license\），用于定位并校验与该包对应的完整源码
（AGPL-3.0 第 13 条）。

| 字段 | 值 |
|---|---|
| 产品版本 | $version |
| 构建 commit（evo-agent 仓） | $commitSha |
| 源码仓库（权威源） | <https://github.com/evorule/evo-agent> |
| 源码 tarball | $srcTarballName（随发布 Release 附件分发） |
| 源码 tarball SHA256 | $srcSha |

## 获取与校验方式

1. ``git clone`` 上表任一仓库，``git checkout $commitSha``
2. 复现归档：``git archive --format=tar.gz --prefix=evo-agent-desktop-$version/ -o $srcTarballName HEAD``
3. 计算 SHA256 并与上表值比对

## 内嵌组件

- evo-agent（本机智能体外壳）与 sdk：本仓（上表构建 commit）
- evorule-server 引擎 / evorule-ai-plugin：<https://github.com/evorule/evorule-server>
- console 用户面板（如随包分发）：<https://github.com/evorule/evorule-console-cloud>

## 第三方组件

许可正文与归属声明见本目录 THIRD-PARTY-NOTICES.md 与 LICENSES/third-party/。
"@
    [System.IO.File]::WriteAllText((Join-Path $licDst "SOURCE.md"), $sourceMd)
    Write-Host "collected license five-piece (self-held, $repoRoot) + SOURCE.md"
    Write-Host "  source tarball: $srcTarballName sha256=$srcSha"

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
