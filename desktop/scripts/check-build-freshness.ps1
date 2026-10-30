# check-build-freshness.ps1 — 发版构建产物新鲜度与源码树干净度校验
#
# 背景:build-installer 只收集磁盘上的既有 release 产物,不负责重建——
# 「推了代码忘重建=安装包静默装旧 bits」曾无任何提示。本脚本为发版前置门:
#   ① 干净度:相关仓不得有未提交的已跟踪改动(未跟踪杂物不拦;行尾归一化
#      幻影不算——git diff --quiet 判定,不吃 porcelain 的 M 幻影)
#   ② 新鲜度:每笔预构建产物须**晚于**其编译输入源码树的最后提交时间
#      (路径按编译输入收窄,docs-only 提交不误报)。语义=「先定稿提交、
#      后重建产物」的发行纪律——提交后改过的源码必然触发重建要求。
# build-installer 内嵌调用;本地调试可经 -SkipFreshness 跳过(发版禁用)。
param(
    [Parameter(Mandatory = $true)] [string]$RepoRoot,
    [Parameter(Mandatory = $true)] [string]$ServerRepo,
    [string]$ConsoleRepo = ""
)

# 不设 ErrorActionPreference=Stop:PS5.1 下原生命令(git)的 stderr 提示
# (如 CRLF 归一化 warning)会被包装成 ErrorRecord 直接触发终止——本脚本
# 判定全部走退出码/LASTEXITCODE,无需 Stop。
$script:violations = @()

function Head-Time([string]$repo, [string[]]$spec) {
    # pathspec 必须数组透传:含空格的单串会被 git 当成一个带空格的路径
    # (空匹配=静默漏检,本门禁曾有此 bug 被 exe 判定空转实证)
    $a = @("-C", $repo, "log", "-1", "--format=%cI")
    if ($spec) { $a += "--"; $a += $spec }
    $out = & git @a 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $out) { return $null }
    try { return [datetimeoffset]::Parse($out.Trim()) } catch { return $null }
}

function Assert-Clean([string]$name, [string]$repo) {
    & git -C $repo diff --quiet 2>$null
    $unstaged = $LASTEXITCODE
    & git -C $repo diff --cached --quiet 2>$null
    $staged = $LASTEXITCODE
    if ($unstaged -ne 0 -or $staged -ne 0) {
        Write-Host ("[freshness-diag] {0} exits: diff={1} diff --cached={2} autocrlf=[{3}] git=[{4}]" -f $name, $unstaged, $staged, (& git -C $repo config core.autocrlf 2>$null), (& git --version 2>$null))
        Write-Host "[freshness-diag] ${name} git status --porcelain (first 5):"
        & git -C $repo status --porcelain 2>&1 | Select-Object -First 5 | ForEach-Object { Write-Host "  $_" }
        Write-Host "[freshness-diag] ${name} git diff --stat (first 8, stderr merged):"
        & git -C $repo diff --stat 2>&1 | Select-Object -First 8 | ForEach-Object { Write-Host "  $_" }
    }
    if ($unstaged -ne 0) {
        $script:violations += "${name}: 存在未提交改动(git diff 非空)——发版不得携带无版本 bits"
    }
    if ($staged -ne 0) {
        $script:violations += "${name}: 存在已暂存未提交(git diff --cached 非空)"
    }
}

function Assert-Fresh([string]$what, [string]$artifact, $headTime) {
    if ($null -eq $headTime) { return }
    if (-not (Test-Path $artifact)) { return } # 缺件由收集段的存在性检查报
    $t = (Get-Item $artifact).LastWriteTimeUtc
    if ($t -le $headTime.UtcDateTime) {
        $script:violations += ("{0}: 产物陈旧(构建 {1:u} <= 源码最后提交 {2:u})——先定稿提交,再 release 重建" -f `
                $what, $t, $headTime.UtcDateTime)
    }
}

function Newest-Under([string]$dir) {
    if (-not (Test-Path $dir)) { return $null }
    Get-ChildItem $dir -Recurse -File -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
}

# ① 干净度
Assert-Clean "evo-agent" $RepoRoot
Assert-Clean "evorule-server" $ServerRepo
if ($ConsoleRepo -and (Test-Path $ConsoleRepo)) { Assert-Clean "console-cloud" $ConsoleRepo }

# ② 新鲜度(编译输入路径收窄)
Assert-Fresh "evo-agent.exe(serve)" `
    (Join-Path $RepoRoot "target\release\evo-agent.exe") `
    (Head-Time $RepoRoot @("src", "Cargo.toml", "Cargo.lock"))
Assert-Fresh "evorule-server.exe(engine)" `
    (Join-Path $ServerRepo "target\release\evorule-server.exe") `
    (Head-Time $ServerRepo @("src", "plugins", "Cargo.toml", "Cargo.lock"))
Assert-Fresh "evorule-ai-plugin.exe(执行体)" `
    (Join-Path $ServerRepo "plugins\ai-plugin\target\release\evorule-ai-plugin.exe") `
    (Head-Time $ServerRepo "plugins/ai-plugin")
Assert-Fresh "evorule-wasm-host.exe(UDF 宿)" `
    (Join-Path $ServerRepo "plugins\wasm-host\target\release\evorule-wasm-host.exe") `
    (Head-Time $ServerRepo "plugins/wasm-host")
$distNewest = Newest-Under (Join-Path $RepoRoot "web\dist")
if ($distNewest) {
    Assert-Fresh ("web/dist({0})" -f $distNewest.Name) $distNewest.FullName (Head-Time $RepoRoot "web")
}
if ($ConsoleRepo -and (Test-Path (Join-Path $ConsoleRepo "build\index.html"))) {
    $consoleNewest = Newest-Under (Join-Path $ConsoleRepo "build")
    if ($consoleNewest) {
        Assert-Fresh ("console build({0})" -f $consoleNewest.Name) $consoleNewest.FullName (Head-Time $ConsoleRepo "")
    }
}

if ($script:violations.Count -gt 0) {
    Write-Host "build freshness FAIL:"
    $script:violations | ForEach-Object { Write-Host "  $_" }
    Write-Host "修复:相关仓按发版手册前置条件重建(cargo build --release x3 + npm --prefix web run build + console build)后重跑;"
    Write-Host "     本地调试可 -SkipFreshness 跳过本门(发版禁用)"
    exit 1
}
Write-Host "build freshness OK(产物均新于源码最后提交,源码树干净)"
exit 0
