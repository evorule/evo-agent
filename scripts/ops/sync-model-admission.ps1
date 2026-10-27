# 模型准入白名单参数化翻译器（07 号已批方向落地：配置面管清单内容、
# 翻译器生成正本、执法仍在引擎+执行体双层不变）。
# 流程: 读 <ServerHome>\data\agent-governance\model-admission.json 的
#   {"allowed": [...]} → 生成 rule_set（00_constraint_model_allowlist.json，
#   版本=现运行时版本 patch+1）→ 落 rules_dir（即晋升）。
# 引擎 hot-reload 自动装载；ai-plugin（含物料热重载哨兵的版本）10 秒内
# 自动跟随，无需重启。验收纪律不变：新模型须先过 provider-acceptance.ps1。
# 用法: powershell -ExecutionPolicy Bypass -File sync-model-admission.ps1
#       [-ServerHome D:\evorule-server]
# 退出码: 0=已生成落盘 1=失败
param(
    [string]$ServerHome = "D:\evorule-server"
)

$ErrorActionPreference = "Stop"
$ConfigPath = Join-Path $ServerHome "data\agent-governance\model-admission.json"
$RulesDir = Join-Path $ServerHome "data\agent-governance\rules"
$Target = Join-Path $RulesDir "00_constraint_model_allowlist.json"

if (-not (Test-Path $ConfigPath)) {
    Write-Host "[ERROR] 配置面不存在: $ConfigPath（创建 {`"allowed`": [...]} 后重试）"
    exit 1
}
$config = Get-Content $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json
if (-not $config.allowed -or $config.allowed.Count -eq 0) {
    Write-Host "[ERROR] allowed 清单为空（白名单不允许空集——空集等于全拒，请用移除部署的方式下线）"
    exit 1
}
$allowed = @($config.allowed | ForEach-Object { ($_ -as [string]).Trim() } | Where-Object { $_ })
$dups = $allowed | Group-Object | Where-Object Count -gt 1
if ($dups) {
    Write-Host ("[ERROR] allowed 含重复项: " + (($dups | ForEach-Object Name) -join ", "))
    exit 1
}

# 版本递增：现运行时版本 patch+1；无现文件从 1.2.0 起（与出厂种子对齐）
$version = "1.2.0"
if (Test-Path $Target) {
    try {
        $cur = (Get-Content $Target -Raw -Encoding UTF8 | ConvertFrom-Json).version
        if ($cur -match '^(\d+)\.(\d+)\.(\d+)$') {
            $version = "{0}.{1}.{2}" -f $Matches[1], $Matches[2], ([int]$Matches[3] + 1)
        }
    } catch { Write-Host "[WARN] 现文件版本不可读，回退 1.2.0" }
}

$valuesJson = ($allowed | ForEach-Object { '"' + $_ + '"' }) -join ","
$reasonText = "会话模型不在本工作区准入白名单（仅允许 " + ($allowed -join "、") + "；由 model-admission.json 参数化生成）"
$rule = @"
{
  "`$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
  "kind": "rule_set",
  "id": "com.evorule.constraint.model_allowlist",
  "version": "$version",
  "metadata": {
    "tier": "constraint",
    "title": "会话模型准入白名单",
    "description": "本工作区仅允许准入白名单内的模型发起 LLM 会话（call_external）。白名单以 in 域集合成员判定表达；内容由 model-admission.json 配置面经本翻译器生成，执法仍在引擎+执行体双层。"
  },
  "transform": [
    {
      "type": "enforce",
      "params": {
        "domain": {
          "type": "all",
          "inner": [
            {
              "type": "instruction",
              "instruction_type": "call_external"
            },
            {
              "type": "not",
              "inner": {
                "type": "in",
                "path": "instruction.params.model",
                "values": [$valuesJson],
                "on_missing": "error"
              }
            }
          ]
        },
        "reason": "$reasonText"
      }
    }
  ]
}
"@

if (-not (Test-Path $RulesDir)) { New-Item -ItemType Directory -Path $RulesDir -Force | Out-Null }
[System.IO.File]::WriteAllText($Target, $rule, [System.Text.UTF8Encoding]::new($false))
Write-Host "[OK] 白名单物料已生成: $Target（version=$version, $($allowed.Count) 个模型）"
Write-Host "     引擎 hot-reload 自动装载；ai-plugin（热重载版本）10 秒内跟随。"
exit 0
