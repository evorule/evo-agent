# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

"""
evo-agent serve 进程真实 LLM E2E(批 3 D6)

与 tests/llm_real_smoke.py(直连 API 冒烟)不同,本脚本走 serve 进程全链:
  evo-agent serve(WS 会话协议) + evorule-server(规则数据面) + 真实 MiniMax LLM

验证链(批 3 D6):
  P0  双服务探活(evo-agent /health + evorule-server /api/health)
  P1  GET /admin/llm-status 脱敏快照(configured/provider/present/hint/source,
      响应体不含密钥全值)
  P2  数据准备:创建 workspace + 2 条规则(1 条激活)——HTTP 直调 server
  P3  轮 1「查看生效规则并解释」:WS 会话帧 message → agent 真实调
      rule_list/rule_get 消费上下文 → LlmDelta 流式中文解释 → Done
  P4  轮 2「起草规则」:agent 调 rule_validate(G1-G7) → 输出最终草稿 JSON
  P5  提取草稿(fence/裸 JSON) + transform 结构断言 → 写产物文件
      (供 console RuleValidator 侧做 L_console 校验)

会话协议(console agent-client.ts 同款):
  连接 ws://{agent}/api/sessions/{id}/ws?agent_type=general(id 首连用 new)
  客户端帧 {"type":"message","content":...}
  服务端事件 SessionCreated/LlmDelta/ToolCall/ToolResult/Done/Error/Info

前置:
  - evorule-server 已运行: 127.0.0.1:18080(--insecure-serve,loopback 豁免)
  - evo-agent serve 已运行: 127.0.0.1:8081(--no-auth,env 注入
    MINIMAX_API_KEY / MINIMAX_MODEL / MINIMAX_API_BASE)
  - pip install httpx websockets

运行(仓库根目录):
  python tests/e2e_serve_llm.py
"""

import asyncio
import json
import os
import re
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

try:
    import httpx
    import websockets
except ImportError as e:
    print(f"ERROR: 缺少依赖({e.name})。运行: pip install httpx websockets")
    sys.exit(1)

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")

AGENT_BASE = os.environ.get("E2E_AGENT_BASE", "http://127.0.0.1:8081")
SERVER_BASE = os.environ.get("E2E_SERVER_BASE", "http://127.0.0.1:18080")
TURN_TIMEOUT = float(os.environ.get("E2E_TURN_TIMEOUT", "240"))
DRAFT_OUT = os.environ.get(
    "E2E_DRAFT_OUT", str(Path(os.environ.get("TEMP", "/tmp")) / "evo-agent-e2e-last-draft.json")
)

GREEN = "\033[92m"
RED = "\033[91m"
YELLOW = "\033[93m"
RESET = "\033[0m"

# seed 规则内容(transform 范式,与 console prompts.ts 同构)
SEED_RULE_ACTIVE = {
    "transform": [
        {
            "type": "branch",
            "params": {
                "domain": {"type": "exists", "path": "order.amount"},
                "on_true": [
                    {
                        "type": "set",
                        "params": {"attr": "order.level", "operation": "set", "value": "checked"},
                    }
                ],
                "on_false": [
                    {
                        "type": "set",
                        "params": {"attr": "order.level", "operation": "set", "value": "untouched"},
                    }
                ],
            },
        }
    ]
}
SEED_RULE_DRAFT = {
    "transform": [
        {
            "type": "set",
            "params": {"attr": "audit.last_touch", "operation": "set", "value": "e2e"},
        }
    ]
}


class E2ETest:
    def __init__(self) -> None:
        self.passed = 0
        self.failed = 0
        self.errors: List[str] = []

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


def extract_json_candidate(text: str) -> Optional[str]:
    """从 LLM 文本提取草稿 JSON 候选(fence 优先,其次贪心花括号片段)。

    与 console src/lib/agent/rule-draft-extract.ts 同语义(TS 侧做 L_console 校验,
    这里仅负责把 agent 产物取出来)。
    """
    fenced = re.search(r"```(?:json)?\s*\n?([\s\S]*?)```", text)
    candidates: List[str] = []
    if fenced:
        candidates.append(fenced[1])
    candidates.append(text)
    for candidate in candidates:
        s = candidate.strip()
        # 贪心片段:首个 { 到最后一个 }(或 [ ... ])
        for opener, closer in (("{", "}"), ("[", "]")):
            lo, hi = s.find(opener), s.rfind(closer)
            if lo != -1 and hi > lo:
                frag = s[lo : hi + 1]
                try:
                    parsed = json.loads(frag)
                    if isinstance(parsed, (dict, list)):
                        return json.dumps(parsed, ensure_ascii=False, indent=2)
                except json.JSONDecodeError:
                    continue
    return None


def looks_like_rule(parsed: Any) -> bool:
    """console RuleValidator G0 归一化口径:{transform:[...]} / 顶层数组 / 单条 {type}"""
    if isinstance(parsed, list):
        return len(parsed) > 0
    if isinstance(parsed, dict):
        if isinstance(parsed.get("transform"), list):
            return len(parsed["transform"]) > 0
        return isinstance(parsed.get("type"), str)
    return False


async def collect_turn(
    ws: "websockets.WebSocketClientProtocol", content: str
) -> Dict[str, Any]:
    """发送一轮 message 帧,收集事件直到 Done/Error。返回事件汇总。"""
    await ws.send(json.dumps({"type": "message", "content": content}, ensure_ascii=False))
    deltas: List[str] = []
    tool_calls: List[Dict[str, Any]] = []
    tool_results: List[Dict[str, Any]] = []
    infos: List[str] = []
    done: Optional[Dict[str, Any]] = None
    error: Optional[str] = None
    session_id: Optional[str] = None
    deadline = time.time() + TURN_TIMEOUT
    while True:
        remaining = deadline - time.time()
        if remaining <= 0:
            raise TimeoutError(f"turn exceeded {TURN_TIMEOUT}s")
        raw = await asyncio.wait_for(ws.recv(), timeout=remaining)
        try:
            ev = json.loads(raw)
        except json.JSONDecodeError:
            continue
        t = ev.get("type")
        if t == "SessionCreated":
            session_id = ev.get("session_id")
        elif t == "LlmDelta":
            deltas.append(ev.get("text", ""))
        elif t == "ToolCall":
            tool_calls.append(ev)
        elif t == "ToolResult":
            tool_results.append(ev)
        elif t == "Info":
            infos.append(str(ev.get("message", "")))
        elif t == "Done":
            done = ev
            break
        elif t == "Error":
            error = ev.get("error", "unknown")
            break
    return {
        "session_id": session_id,
        "text": "".join(deltas),
        "tool_calls": tool_calls,
        "tool_results": tool_results,
        "infos": infos,
        "done": done,
        "error": error,
    }


def prepare_data(client: httpx.Client, tag: str) -> Dict[str, Any]:
    """P2:创建 workspace + 2 条规则 + 激活 1 条。返回上下文。"""
    resp = client.post(
        f"{SERVER_BASE}/api/workspaces",
        json={"name": f"e2e-llm-{tag}", "owner_id": "e2e", "description": "serve LLM E2E"},
    )
    resp.raise_for_status()
    ws_id = resp.json()["id"]

    def create_rule(name: str, content: dict) -> str:
        r = client.post(
            f"{SERVER_BASE}/api/workspaces/{ws_id}/rules",
            json={
                "name": name,
                "content": json.dumps(content, ensure_ascii=False),
                "created_by": "e2e",
                "description": "serve LLM E2E seed rule",
            },
        )
        r.raise_for_status()
        return r.json()["id"]

    rule_active = create_rule("order-level-check", SEED_RULE_ACTIVE)
    create_rule("audit-touch", SEED_RULE_DRAFT)
    r = client.post(f"{SERVER_BASE}/api/workspaces/{ws_id}/rules/{rule_active}/activate")
    r.raise_for_status()
    return {"ws_id": ws_id, "rule_active": rule_active}


async def ws_scenario(t: E2ETest, ctx: Dict[str, Any]) -> Optional[str]:
    """P3+P4:同一 WS 会话两轮(真实会话台协议)。返回草稿文本或 None。"""
    ws_url = f"{AGENT_BASE.replace('http', 'ws', 1)}/api/sessions/new/ws?agent_type=general"
    t.header("会话(WS 协议,agent_type=general)")
    draft_text: Optional[str] = None
    async with websockets.connect(ws_url, open_timeout=15, max_size=16 * 1024 * 1024) as ws:
        # 轮 1:上下文消费
        t.header("轮 1:查看生效规则并解释(上下文消费)")
        turn1 = await collect_turn(
            ws,
            (
                f"请调用 rule_list 工具查看工作区 {ctx['ws_id']} 里的规则,"
                "再用 rule_get 查看它们的内容,然后用中文逐条解释每条规则的作用。"
                "只查看和解释,不要创建或修改任何规则。"
            ),
        )
        t.info(
            "tool_calls: "
            + ", ".join(f"{c.get('name')}" for c in turn1["tool_calls"])
            + ("" if turn1["tool_calls"] else "(无)")
        )
        if turn1["session_id"]:
            t.ok("SessionCreated", f"session_id={turn1['session_id']}")
        else:
            t.fail("SessionCreated", "未收到 session_id")
        if turn1["error"]:
            t.fail("轮 1", f"Error 事件: {turn1['error']}")
        names1 = [c.get("name") for c in turn1["tool_calls"]]
        if "rule_list" in names1:
            t.ok("上下文消费 rule_list", f"tools={names1}")
        else:
            t.fail("上下文消费 rule_list", f"实际 tools={names1}")
        if turn1["done"] and turn1["done"].get("success"):
            t.ok(
                "轮 1 Done",
                f"steps={turn1['done'].get('steps')} duration={turn1['done'].get('duration_ms')}ms",
            )
        else:
            t.fail("轮 1 Done", f"done={turn1['done']}")
        text1 = turn1["text"].strip()
        if text1:
            t.ok("轮 1 中文解释非空", f"{len(text1)} 字符, 前 60: {text1[:60]!r}")
        else:
            t.fail("轮 1 中文解释非空", "LlmDelta 拼接为空")

        # 轮 2:产草稿(先 rule_validate,不入库)
        t.header("轮 2:起草规则(先 rule_validate,不入库)")
        turn2 = await collect_turn(
            ws,
            (
                "请起草一条规则:当 user.name 存在时,把 user.level 设为 verified,"
                "否则把 user.level 设为 guest。要求:1) 先调用 rule_validate 工具验证"
                "草稿;2) 验证通过后,在最终回答里只输出一个 ```json 代码块,内容为"
                '{"transform":[...]} 格式的规则 JSON;3) 不要调用 rule_create,'
                "草稿将转人工审核。"
            ),
        )
        t.info(
            "tool_calls: "
            + ", ".join(f"{c.get('name')}" for c in turn2["tool_calls"])
            + ("" if turn2["tool_calls"] else "(无)")
        )
        if turn2["error"]:
            t.fail("轮 2", f"Error 事件: {turn2['error']}")
        names2 = [c.get("name") for c in turn2["tool_calls"]]
        if "rule_validate" in names2:
            t.ok("草稿自检 rule_validate", f"tools={names2}")
        else:
            t.fail("草稿自检 rule_validate", f"实际 tools={names2}")
        if "rule_create" in names2:
            t.fail("不入库纪律", "出现了 rule_create 调用(违反人审边界)")
        else:
            t.ok("不入库纪律(无 rule_create)")
        if turn2["done"] and turn2["done"].get("success"):
            t.ok(
                "轮 2 Done",
                f"steps={turn2['done'].get('steps')} duration={turn2['done'].get('duration_ms')}ms",
            )
        else:
            t.fail("轮 2 Done", f"done={turn2['done']}")
        draft_text = turn2["text"]
    return draft_text


def main() -> int:
    t = E2ETest()
    print("evo-agent serve 进程真实 LLM E2E")
    print(f"  agent:  {AGENT_BASE}")
    print(f"  server: {SERVER_BASE}")

    client = httpx.Client(timeout=30.0)

    # P0 探活
    t.header("P0 双服务探活")
    try:
        client.get(f"{AGENT_BASE}/health").raise_for_status()
        t.ok("evo-agent /health")
    except Exception as e:
        t.fail("evo-agent /health", f"{e}(请先启动 serve 进程)")
        print(f"\n结果: {t.passed}/{t.passed + t.failed} passed")
        return 1
    try:
        client.get(f"{SERVER_BASE}/api/health").raise_for_status()
        t.ok("evorule-server /api/health")
    except Exception as e:
        t.fail("evorule-server /api/health", f"{e}(请先启动 server)")
        print(f"\n结果: {t.passed}/{t.passed + t.failed} passed")
        return 1

    # P1 llm-status 脱敏
    t.header("P1 /admin/llm-status 脱敏快照")
    try:
        resp = client.get(f"{AGENT_BASE}/admin/llm-status")
        resp.raise_for_status()
        body = resp.text
        snap = resp.json()
        if snap.get("configured") is True:
            t.ok("configured=true", f"provider={snap.get('provider')} model={snap.get('model')}")
        else:
            t.fail("configured", f"快照: {snap}")
        key = snap.get("api_key", {})
        if key.get("present") is True:
            t.ok("api_key.present=true", f"hint={key.get('hint')} source={key.get('source')}")
        else:
            t.fail("api_key.present", f"快照: {snap}")
        raw_key = os.environ.get("MINIMAX_API_KEY", "")
        if raw_key and raw_key in body:
            t.fail("脱敏铁律", "响应体含密钥全值")
        elif raw_key:
            t.ok("响应体不含密钥全值")
        else:
            t.info("本进程未注入 MINIMAX_API_KEY,跳过全值比对")
    except Exception as e:
        t.fail("GET /admin/llm-status", str(e))

    # P2 数据准备
    t.header("P2 数据准备(workspace + seed 规则)")
    ctx: Dict[str, Any] = {}
    try:
        ctx = prepare_data(client, tag=str(int(time.time())))
        t.ok("workspace + 2 规则 + 激活 1 条", f"ws_id={ctx['ws_id']}")
    except Exception as e:
        t.fail("数据准备", str(e))
        print(f"\n结果: {t.passed}/{t.passed + t.failed} passed")
        return 1

    # P3+P4 WS 两轮
    try:
        draft_text = asyncio.run(ws_scenario(t, ctx))
    except Exception as e:
        t.fail("WS 会话场景", f"{type(e).__name__}: {e}")
        draft_text = None

    # P5 草稿提取
    draft_path: Optional[str] = None
    if draft_text:
        t.header("P5 草稿提取")
        candidate = extract_json_candidate(draft_text)
        if candidate is None:
            t.fail("草稿提取", "LLM 回答中未找到 JSON")
        else:
            try:
                parsed = json.loads(candidate)
            except json.JSONDecodeError as e:
                parsed = None
                t.fail("草稿 JSON 解析", str(e))
            if parsed is not None:
                if looks_like_rule(parsed):
                    t.ok("transform 结构(G0 口径)")
                    Path(DRAFT_OUT).write_text(candidate, encoding="utf-8")
                    draft_path = DRAFT_OUT
                    t.ok("草稿已存档", f"{DRAFT_OUT}")
                else:
                    t.fail("transform 结构(G0 口径)", f"结构不像规则: {candidate[:120]!r}")

    client.close()
    print("\n" + "=" * 60)
    print(f"结果: {t.passed}/{t.passed + t.failed} passed")
    if t.errors:
        print("失败项:")
        for err in t.errors:
            print(f"  - {err}")
    if draft_path:
        print(f"下一步: 用 console RuleValidator 校验 {draft_path}(L_console)")
    print("=" * 60)
    return 0 if t.failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
