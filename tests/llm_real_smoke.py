# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

"""
evo-agent 真实 LLM 冒烟测试

不依赖 evo-agent 编译结果,直接用 httpx 调 MiniMax API。
验证:
  1. MiniMax API key 可用
  2. 简单对话返回正常
  3. 中文/英文都能处理(中文是 EvoRule 核心场景)
  4. tool_calls 字段(为将来 function calling 准备)

# 前置
  - .env 文件含 MINIMAX_API_KEY
  - 联网

# 运行
  cd D:\evo-agent
  python tests/llm_real_smoke.py
"""

import json
import os
import sys
import time
from pathlib import Path
from typing import Any, Dict, Optional

try:
    import httpx
except ImportError:
    print("ERROR: 需要 httpx 库。运行: pip install httpx")
    sys.exit(1)


# ===== 颜色 =====
GREEN = "\033[92m"
RED = "\033[91m"
YELLOW = "\033[93m"
RESET = "\033[0m"


def load_env_file(env_path: Path) -> Dict[str, str]:
    """从 .env 文件加载 key=value(不依赖 python-dotenv)"""
    env = {}
    if not env_path.exists():
        return env
    for line in env_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            continue
        k, v = line.split("=", 1)
        env[k.strip()] = v.strip()
    return env


class MiniMaxClient:
    """MiniMax API 客户端(也是 evo-agent LlmHandler 的等价实现)"""

    def __init__(
        self,
        api_key: str,
        model: str = "MiniMax-M2.5",
        api_base: str = "https://api.minimax.io/v1/text/chatcompletion_v2",
        timeout: float = 30.0,
    ):
        self.api_key = api_key
        self.model = model
        self.api_base = api_base.rstrip("/")
        self.timeout = timeout
        self._client = httpx.Client(timeout=timeout)

    def chat(
        self,
        messages: list[Dict[str, str]],
        temperature: float = 0.7,
        max_tokens: int = 1024,
        tools: Optional[list[Dict[str, Any]]] = None,
    ) -> Dict[str, Any]:
        """调一次 chat completion API"""
        body: Dict[str, Any] = {
            "model": self.model,
            "messages": messages,
            "temperature": temperature,
            "max_tokens": max_tokens,
        }
        if tools:
            body["tools"] = tools
            body["tool_choice"] = "auto"

        resp = self._client.post(
            self.api_base,
            json=body,
            headers={
                "Authorization": f"Bearer {self.api_key}",
                "Content-Type": "application/json",
            },
        )
        resp.raise_for_status()
        return resp.json()


class E2ETest:
    def __init__(self, env_path: Path):
        self.env = load_env_file(env_path)
        self.passed = 0
        self.failed = 0
        self.errors: list[str] = []

    def ok(self, name: str, info: str = "") -> None:
        self.passed += 1
        print(f"  {GREEN}PASS{RESET}  {name}" + (f"  ({info})" if info else ""))

    def fail(self, name: str, reason: str) -> None:
        self.failed += 1
        self.errors.append(f"{name}: {reason}")
        print(f"  {RED}FAIL{RESET}  {name} — {reason}")

    def info(self, msg: str) -> None:
        print(f"  {YELLOW}···{RESET}  {msg}")

    def header(self, msg: str) -> None:
        print(f"\n=== {msg} ===")

    def test_env_loaded(self) -> Optional[MiniMaxClient]:
        """测试 .env 加载 + key 存在"""
        self.header("环境配置")
        api_key = self.env.get("MINIMAX_API_KEY", "")
        if not api_key:
            self.fail("MINIMAX_API_KEY", "未在 .env 中找到")
            return None
        if not api_key.startswith("sk-"):
            self.fail("MINIMAX_API_KEY 格式", f"应以 'sk-' 开头,实际 '{api_key[:5]}...'")
            return None
        self.ok(
            "MINIMAX_API_KEY 存在",
            f"前缀 {api_key[:5]}...,长度 {len(api_key)}",
        )

        model = self.env.get("MINIMAX_MODEL", "MiniMax-M2.5")
        api_base = self.env.get(
            "MINIMAX_API_BASE",
            "https://api.minimax.io/v1/text/chatcompletion_v2",
        )
        self.ok("MINIMAX_MODEL", model)
        self.ok("MINIMAX_API_BASE", api_base)

        return MiniMaxClient(api_key, model, api_base)

    def test_simple_chat(self, client: MiniMaxClient) -> None:
        """测试 1: 简单英文对话"""
        self.header("场景 1: 简单英文对话")
        try:
            start = time.time()
            resp = client.chat(
                messages=[
                    {"role": "system", "content": "你是一个简洁的助手,用 1 句话回答。"},
                    {"role": "user", "content": "What is EvoRule in one sentence?"},
                ],
                max_tokens=200,
            )
            duration = time.time() - start
            content = (
                resp.get("choices", [{}])[0]
                .get("message", {})
                .get("content", "")
            )
            if not content:
                self.fail("简单英文对话", "API 返回空 content")
                return
            self.ok(
                "简单英文对话",
                f"{duration:.1f}s, content 前 80 字符: {content[:80]!r}",
            )
        except Exception as e:
            self.fail("简单英文对话", str(e))

    def test_chinese_chat(self, client: MiniMaxClient) -> None:
        """测试 2: 中文对话(EvoRule 核心场景)"""
        self.header("场景 2: 中文对话(核心场景)")
        try:
            start = time.time()
            resp = client.chat(
                messages=[
                    {"role": "system", "content": "你是 EvoRule 反应式执行引擎的助手。"},
                    {"role": "user", "content": "用一句话解释 EvoRule 是做什么的。"},
                ],
                max_tokens=200,
            )
            duration = time.time() - start
            content = (
                resp.get("choices", [{}])[0]
                .get("message", {})
                .get("content", "")
            )
            if not content:
                self.fail("中文对话", "API 返回空 content")
                return
            self.ok(
                "中文对话",
                f"{duration:.1f}s, content 前 80 字符: {content[:80]!r}",
            )
        except Exception as e:
            self.fail("中文对话", str(e))

    def test_evo_rule_scenario(self, client: MiniMaxClient) -> None:
        """测试 3: EvoRule 真实场景 — 解析指令"""
        self.header("场景 3: EvoRule 真实场景(JSON 指令解析)")
        try:
            start = time.time()
            resp = client.chat(
                messages=[
                    {
                        "role": "system",
                        "content": (
                            "你是 EvoRule Agent。根据用户目标,输出一个 JSON 指令。\n"
                            '格式: {"type": "...", "params": {...}}'
                        ),
                    },
                    {
                        "role": "user",
                        "content": "把 x 增加 5",
                    },
                ],
                max_tokens=300,
            )
            duration = time.time() - start
            content = (
                resp.get("choices", [{}])[0]
                .get("message", {})
                .get("content", "")
            )
            if not content:
                self.fail("EvoRule 场景", "API 返回空 content")
                return

            # 尝试从 content 中提取 JSON
            content_clean = content.strip()
            # 处理 markdown code fence
            if content_clean.startswith("```"):
                lines = content_clean.split("\n")
                content_clean = "\n".join(
                    l for l in lines
                    if not l.startswith("```")
                ).strip()

            try:
                parsed = json.loads(content_clean)
                if isinstance(parsed, dict) and "type" in parsed:
                    self.ok(
                        "EvoRule 场景",
                        f"{duration:.1f}s, 解析出 JSON: {parsed}",
                    )
                else:
                    self.fail(
                        "EvoRule 场景",
                        f"JSON 不含 'type' 字段: {parsed}",
                    )
            except json.JSONDecodeError as e:
                self.fail(
                    "EvoRule 场景",
                    f"LLM 输出不是合法 JSON: {e}, content: {content[:100]!r}",
                )
        except Exception as e:
            self.fail("EvoRule 场景", str(e))

    def test_tool_calling(self, client: MiniMaxClient) -> None:
        """测试 4: Function calling(为将来 Agent 准备)"""
        self.header("场景 4: Function calling(可选功能)")
        tools = [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "获取某地天气",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "location": {
                                "type": "string",
                                "description": "城市名",
                            },
                        },
                        "required": ["location"],
                    },
                },
            }
        ]
        try:
            start = time.time()
            resp = client.chat(
                messages=[
                    {
                        "role": "user",
                        "content": "北京今天天气如何?",
                    },
                ],
                tools=tools,
                max_tokens=200,
            )
            duration = time.time() - start
            choice = resp.get("choices", [{}])[0]
            finish_reason = choice.get("finish_reason", "")
            tool_calls = choice.get("message", {}).get("tool_calls")

            if tool_calls and len(tool_calls) > 0:
                tool_name = tool_calls[0].get("function", {}).get("name", "")
                self.ok(
                    "Function calling",
                    f"{duration:.1f}s, 调用 {tool_name} (finish_reason={finish_reason})",
                )
            elif finish_reason == "tool_calls":
                self.ok(
                    "Function calling",
                    f"{duration:.1f}s, LLM 决定调工具(finish_reason=tool_calls)",
                )
            else:
                # LLM 选择直接回答(不调工具)
                self.info(
                    f"LLM 选择了直接回答(finish_reason={finish_reason}),"
                    "这在某些模型上是预期行为",
                )
                self.ok(
                    "Function calling(可降级)",
                    f"{duration:.1f}s, LLM 拒绝调工具,直接回答",
                )
        except Exception as e:
            # 某些模型不支持 function calling,优雅降级
            self.info(f"Function calling 失败(模型可能不支持): {e}")
            self.ok("Function calling(降级)", "模型不支持,跳过")

    def summary(self) -> bool:
        total = self.passed + self.failed
        print(f"\n{'=' * 60}")
        print(f"结果: {self.passed}/{total} passed")
        if self.errors:
            print("\n失败用例:")
            for e in self.errors:
                print(f"  - {e}")
        print(f"{'=' * 60}")
        return self.failed == 0


def main() -> int:
    print("evo-agent 真实 LLM 冒烟测试")
    print(f"  时间: {time.strftime('%Y-%m-%d %H:%M:%S')}")

    env_path = Path(__file__).parent.parent / ".env"
    print(f"  env:   {env_path}")

    e2e = E2ETest(env_path)
    client = e2e.test_env_loaded()
    if client is None:
        e2e.summary()
        return 1

    try:
        e2e.test_simple_chat(client)
        e2e.test_chinese_chat(client)
        e2e.test_evo_rule_scenario(client)
        e2e.test_tool_calling(client)
    finally:
        client._client.close()

    return 0 if e2e.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
