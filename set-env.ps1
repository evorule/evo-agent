# Evo-Agent 环境变量设置脚本（PowerShell）
#
# 用法:
#   1. 编辑本文件,填入真实的 MINIMAX_API_KEY
#   2. 在 PowerShell 中 dot-source:
#      . .\set-env.ps1
#   3. 然后运行 evo-agent:
#      .\target\release\evo-agent.exe run "你的任务"

# ============================================================
# LLM API Key 配置(必填,任选一个 provider)
# ============================================================

# MiniMax(默认 provider)
# 获取地址: https://platform.minimaxi.com/
$env:MINIMAX_API_KEY = "在此填入你的 MiniMax API key"

# DeepSeek(可选,如需切换 provider)
# 获取地址: https://platform.deepseek.com/
# $env:DEEPSEEK_API_KEY = ""
# $env:EVO_AGENT_LLM__PROVIDER = "deepseek"
# $env:EVO_AGENT_LLM__MODEL = "deepseek-chat"
# $env:EVO_AGENT_LLM__API_BASE = "https://api.deepseek.com/v1/chat/completions"
# $env:EVO_AGENT_LLM__API_KEY = $env:DEEPSEEK_API_KEY

# OpenAI(可选,如需切换 provider)
# 获取地址: https://platform.openai.com/
# $env:OPENAI_API_KEY = ""
# $env:EVO_AGENT_LLM__PROVIDER = "openai"
# $env:EVO_AGENT_LLM__MODEL = "gpt-4o-mini"
# $env:EVO_AGENT_LLM__API_BASE = "https://api.openai.com/v1/chat/completions"
# $env:EVO_AGENT_LLM__API_KEY = $env:OPENAI_API_KEY

# ============================================================
# evorule-server 连接配置(默认指向本地 18082 端口)
# ============================================================
$env:EVO_AGENT_EVORULE__BASE_URL = "http://127.0.0.1:18082"

# ============================================================
# 日志级别(可选,debug 用于调试)
# ============================================================
# $env:EVO_AGENT_LOGGING__LEVEL = "debug"

Write-Host "✅ 环境变量已设置:"
Write-Host "  MINIMAX_API_KEY: $(if ($env:MINIMAX_API_KEY -and $env:MINIMAX_API_KEY -ne '在此填入你的 MiniMax API key') { '已配置 (' + $env:MINIMAX_API_KEY.Length + ' 字符)' } else { '❌ 未配置' })"
Write-Host "  EVORULE_BASE_URL: $env:EVO_AGENT_EVORULE__BASE_URL"
Write-Host ""
Write-Host "下一步:"
Write-Host "  1. 启动 evorule-server:"
Write-Host "     .\evorule-server.exe --addr 127.0.0.1:18082 --wal-dir .\.build\e2e-wal --auto-verify"
Write-Host "  2. 运行 evo-agent:"
Write-Host "     .\target\release\evo-agent.exe run '你的任务'"
Write-Host "  3. 或指定 agent:"
Write-Host "     .\target\release\evo-agent.exe run --agent researcher '你的任务'"
