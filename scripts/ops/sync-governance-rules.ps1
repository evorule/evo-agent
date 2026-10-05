# 治理规则正本单向同步: evo-agent/rules/governance(版本化正本) -> evorule-server 运行时 rules_dir。
# 部署纪律: repo->rules_dir 单向, data 目录手改=违规; 每份正本先 SHA256 对比后落盘复制,
# 不一致=提示人工确认 diff, 不静默覆盖; -Check 只校验不写(巡检/CI 用, 发现漂移退出码 1)。
# 正本缺失于 rules_dir 时: 若其 rule id 已以其他文件名(如 publish 晋升产物)在场则跳过
# (部署须经 publish 通道, 防同 id 双真相源), 否则视为新部署落盘复制。
# VIA-PUBLISH 命中时做版本弱校验: 晋升产物版本高于正本=孤儿晋升/正本未回填(报 issue);
# 正本高于晋升产物=修复后在途待再晋升(信息性, 不计 issue)。
param(
    [string]$RulesDir = 'D:\evorule-server\data\agent-governance\rules',
    [switch]$Check
)

$ErrorActionPreference = 'Stop'
$RepoDir = Join-Path $PSScriptRoot '..\..\rules\governance'
$RepoDir = [System.IO.Path]::GetFullPath($RepoDir)

if (-not (Test-Path $RepoDir)) {
    Write-Host "[ERROR] 正本目录不存在: $RepoDir"
    exit 1
}
if (-not (Test-Path $RulesDir)) {
    if ($Check) {
        Write-Host "[MISSING] 运行时目录不存在: $RulesDir"
        exit 1
    }
    Write-Host "[INIT] 运行时目录不存在, 创建: $RulesDir"
    New-Item -ItemType Directory -Path $RulesDir -Force | Out-Null
}

# 读取规则文件的 id 与 tier(缺失/非法 JSON 返回 $null)
function Get-RuleMeta {
    param([string]$Path)
    try {
        return Get-Content $Path -Raw -Encoding UTF8 | ConvertFrom-Json
    } catch {
        return $null
    }
}

# 查找某正本的 rule id 是否已以其他文件名存在于 rules_dir(publish 晋升产物等), 命中返回该文件对象
function Find-RuleIdFile {
    param([string]$SrcPath, [string]$Dir)
    $meta = Get-RuleMeta -Path $SrcPath
    if (-not $meta -or -not $meta.id) { return $null }
    foreach ($f in (Get-ChildItem $Dir -Filter *.json -File)) {
        try {
            if ((Get-RuleMeta -Path $f.FullName).id -eq $meta.id) { return $f }
        } catch { }
    }
    return $null
}

# 语义化版本分段数值比较(A>B 返回 1, 相等返回 0, A<B 返回 -1); 段缺失或非数字按 0 处理
function Compare-RuleVersion {
    param([string]$A, [string]$B)
    if (-not $A) { $A = '0' }
    if (-not $B) { $B = '0' }
    $sa = $A.Split('.')
    $sb = $B.Split('.')
    $n = [Math]::Max($sa.Count, $sb.Count)
    for ($i = 0; $i -lt $n; $i++) {
        $va = 0
        $vb = 0
        if ($i -lt $sa.Count) { [void][int]::TryParse($sa[$i], [ref]$va) }
        if ($i -lt $sb.Count) { [void][int]::TryParse($sb[$i], [ref]$vb) }
        if ($va -lt $vb) { return -1 }
        if ($va -gt $vb) { return 1 }
    }
    return 0
}

$issues = 0
$sources = Get-ChildItem $RepoDir -Filter *.json -File | Sort-Object Name
if (-not $sources) {
    Write-Host "[ERROR] 正本目录无规则文件: $RepoDir"
    exit 1
}
Write-Host "正本目录: $RepoDir"
Write-Host "运行时目录: $RulesDir"
if ($Check) { Write-Host "模式: 校验(不写)" } else { Write-Host "模式: 同步(写入需人工确认)" }
Write-Host ""

foreach ($src in $sources) {
    # 本链同步对象=约束链正本(tier=constraint); 其余(业务/留痕型)不经
    # agent-governance 约束目录部署, 信息性跳过
    $meta = Get-RuleMeta -Path $src.FullName
    if (-not $meta -or $meta.metadata.tier -ne 'constraint') {
        $tier = if ($meta) { $meta.metadata.tier } else { 'unknown' }
        Write-Host "[LOCAL-ONLY] $($src.Name) 非约束链正本(tier=$tier), 不走本同步链"
        continue
    }
    $dst = Join-Path $RulesDir $src.Name
    if (Test-Path $dst) {
        $hashSrc = (Get-FileHash $src.FullName -Algorithm SHA256).Hash
        $hashDst = (Get-FileHash $dst -Algorithm SHA256).Hash
        if ($hashSrc -eq $hashDst) {
            Write-Host "[OK]      $($src.Name) 与运行时一致"
            continue
        }
        $issues++
        if ($Check) {
            Write-Host "[DRIFT]   $($src.Name) 与运行时不一致(退出码 1, 请人工 diff)"
            continue
        }
        Write-Host "[DRIFT]   $($src.Name) 与运行时不一致, 请先人工确认 diff:"
        Write-Host "          正本:   $($src.FullName)"
        Write-Host "          运行时: $dst"
        $answer = Read-Host "          以正本覆盖运行时文件? (y/N)"
        if ($answer -eq 'y') {
            Copy-Item $src.FullName $dst -Force
            Write-Host "[SYNCED]  $($src.Name) 已以正本覆盖"
            $issues--
        } else {
            Write-Host "[SKIP]    保留运行时版本(未覆盖, 退出码 1)"
        }
    } else {
        $promoted = Find-RuleIdFile -SrcPath $src.FullName -Dir $RulesDir
        if ($promoted) {
            Write-Host "[VIA-PUBLISH] $($src.Name) 的 rule id 已以其他文件名在场(publish 晋升通道产物), 跳过"
            # 版本弱校验: 晋升产物版本高于正本=孤儿晋升/正本未回填(报 issue);
            # 正本高于晋升产物=修复后在途待再晋升(信息性, 不计 issue)
            $promotedMeta = Get-RuleMeta -Path $promoted.FullName
            $vSrc = [string]$meta.version
            $vDst = [string]$promotedMeta.version
            $cmp = Compare-RuleVersion -A $vDst -B $vSrc
            if ($cmp -gt 0) {
                $issues++
                Write-Host "[STALE-SOURCE] $($src.Name) 运行时晋升产物($($promoted.Name))版本($vDst)高于正本($vSrc), 正本未回填, 请核查(退出码 1)"
            } elseif ($cmp -lt 0) {
                Write-Host "[NOTE] $($src.Name) 正本版本($vSrc)高于运行时晋升产物($vDst), 正本在途待再晋升(信息性, 不计 issue)"
            }
            continue
        }
        if ($Check) {
            $issues++
            Write-Host "[MISSING] $($src.Name) 未部署于运行时(退出码 1)"
            continue
        }
        Copy-Item $src.FullName $dst
        Write-Host "[DEPLOYED] $($src.Name) 新部署落盘"
    }
}

Write-Host ""
if ($issues -gt 0) {
    Write-Host "存在 $issues 项未解决的漂移/缺失。"
    exit 1
}
Write-Host "全部一致。"
exit 0
