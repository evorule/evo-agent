# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

"""
自进化全链 真实 LLM E2E（信号 → 提名 → 审批 → 落盘 → 再违规）

走双服务全链: evo-agent serve(WS 会话协议) + evorule-server(规则数据面/治理面)
+ 真实 MiniMax LLM。验证自进化闭环的六个环节:

  E0  双服务探活(evo-agent /health + evorule-server /api/health)
  E1  直调 server 制造种子违规: POST /api/sessions + /command(robot_move) →
      审计链出现 Violation 事实(reason 命中种子哨兵文案)
  E2  直调 GET /api/sessions/{id}/evolution-signals: 违规信号可见
      (total_violations>=1, signals[0] kind/rule_ref/last_instr_type 断言)
  E3  agent WS 会话真实调用 evolution_signals 工具 → 摘要含信号明细
  E4  agent WS 会话起草约束层草稿并调用 rule_promote 工具 →
      治理队列出现 pending 的 meta_promotion 条目(kind 硬编码防旁路)
  E5  直调 POST /api/publish/queue/{id}/review approved → bundle 落盘,
      GET /api/bundles/active 可观测(blake3 content_hash)
  E6  同会话再违规(信号计数递增) + 全新会话独立信号 + 审计链验证通过

会话协议(console agent-client.ts 同款):
  连接 ws://{agent}/api/sessions/new/ws?agent_type=general
  客户端帧 {"type":"message","content":...}
  服务端事件 SessionCreated/LlmDelta/ToolCall/ToolResult/Done/Error/Info

前置(由运行方准备,脚本只做验证侧):
  - evorule-server 已运行: 127.0.0.1:18080
    (--insecure-serve --rules-dir <含 00_constraint_ 演练种子的目录>
     --workspace-db <临时路径> --no-rate-limit)
  - evo-agent serve 已运行: 127.0.0.1:8081(--no-auth,env 注入
    EVO_AGENT_LLM__API_KEY / EVO_AGENT_LLM__MODEL / EVO_AGENT_LLM__API_BASE,
    EVO_AGENT_EVORULE__BASE_URL 指向 evorule-server)
  - pip install httpx websockets

运行(仓库根目录):
  python tests/e2e_evolution_loop.py
"""

import asyncio
import json
import os
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
POLL_TIMEOUT = float(os.environ.get("E2E_POLL_TIMEOUT", "20"))
RESULT_OUT = os.environ.get(
    "E2E_EVOLUTION_RESULT_OUT",
    str(Path(os.environ.get("TEMP", "/tmp")) / "evorule-e2e-evolution-result.json"),
)

# 种子哨兵期望值(与启动 server 的 rules_dir 中 00_constraint_ 种子一致)
SEED_REASON = "运动指令缺少安全清场前置"
SEED_INSTR_TYPE = "robot_move"

# E4 提名草稿(跟踪型约束:不含 enforce 强制原语——enforce 仅限治理链晋升的
# L2 元规则文件;bundle 导入期携带 enforce 会被硬拒收,故草稿为留痕型)
DRAFT_META_RULE = {
    "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
    "kind": "rule_set",
    "id": "com.evorule.demo.meta_guard.evolution.draft",
    "version": "0.1.0",
    "metadata": {
        "tier": "constraint",
        "title": "自进化演练草稿：运动留痕哨兵",
        "description": "对 robot_move 指令做事后留痕跟踪（跟踪型约束，不含强制原语）。",
    },
    "transform": [
        {
            "rule_id": "evo_draft_motion_tracking",
            "type": "branch",
            "params": {
                "domain": {
                    "type": "instruction",
                    "instruction_type": "robot_move",
                },
                "on_true": [
                    {
                        "type": "set",
                        "params": {
                            "attr": "audit.meta_guard.tracked",
                            "operation": "set",
                            "value": True,
                        },
                    }
                ],
                "on_false": [],
            },
        }
    ],
}

GREEN = "\033[92m"
RED = "\033[91m"
YELLOW = "\033[93m"
RESET = "\033[0m"


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

    @property
    def all_passed(self) -> bool:
        return self.failed == 0


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


def tool_names(turn: Dict[str, Any]) -> List[str]:
    return [c.get("name") for c in turn["tool_calls"]]


async def poll_until(check, timeout_s: float, interval: float = 0.5):
    """轮询异步事实落链(命令经 channel 异步进反应器),直到 check 通过或超时。"""
    deadline = time.time() + timeout_s
    last = None
    while time.time() < deadline:
        last = await check()
        if last is not None:
            return last
        await asyncio.sleep(interval)
    return None


async def make_session(client: httpx.AsyncClient, t: E2ETest, label: str) -> Optional[int]:
    r = await client.post(f"{SERVER_BASE}/api/sessions")
    if r.status_code != 200:
        t.fail(f"创建会话({label})", f"status={r.status_code} body={r.text[:200]}")
        return None
    sid = r.json().get("session_id")
    t.ok(f"创建会话({label})", f"session_id={sid}")
    return sid


async def submit_robot_move(
    client: httpx.AsyncClient, t: E2ETest, sid: int, ts: int
) -> None:
    r = await client.post(
        f"{SERVER_BASE}/api/sessions/{sid}/command",
        json={"instruction": {"type": SEED_INSTR_TYPE, "params": {"timestamp": ts}}},
    )
    if r.status_code == 200:
        t.ok(f"提交 {SEED_INSTR_TYPE} 指令", f"session={sid} ts={ts}")
    else:
        t.fail(f"提交 {SEED_INSTR_TYPE} 指令", f"status={r.status_code} body={r.text[:200]}")


async def wait_violation(
    client: httpx.AsyncClient, sid: int, min_count: int
) -> Optional[List[Dict[str, Any]]]:
    """轮询审计链直到 Violation 事实 >= min_count 条(reason 命中种子文案)。"""

    async def check():
        r = await client.get(f"{SERVER_BASE}/api/sessions/{sid}/audit?include_content=true")
        if r.status_code != 200:
            return None
        body = r.json()
        if body.get("verified") is not True:
            return None
        entries = body.get("entries", [])
        hits = [
            e
            for e in entries
            if SEED_REASON in json.dumps(e, ensure_ascii=False)
            and "Violation" in json.dumps(e, ensure_ascii=False)
        ]
        if len(hits) >= min_count:
            return hits
        return None

    return await poll_until(check, POLL_TIMEOUT)


async def e0_probe(t: E2ETest, client: httpx.AsyncClient) -> None:
    t.header("E0 双服务探活")
    r = await client.get(f"{AGENT_BASE}/health")
    if r.status_code == 200 and "ok" in r.text:
        t.ok("evo-agent /health", r.text.strip()[:20])
    else:
        t.fail("evo-agent /health", f"status={r.status_code} body={r.text[:100]}")
    r = await client.get(f"{SERVER_BASE}/api/health")
    if r.status_code == 200:
        t.ok("evorule-server /api/health")
    else:
        t.fail("evorule-server /api/health", f"status={r.status_code} body={r.text[:100]}")


async def e1_seed_violation(
    t: E2ETest, client: httpx.AsyncClient
) -> Optional[int]:
    t.header("E1 直调 server 制造种子违规(enforce 拦截 → Violation 事实落链)")
    sid = await make_session(client, t, "E1")
    if sid is None:
        return None
    await submit_robot_move(client, t, sid, ts=1)
    hits = await wait_violation(client, sid, min_count=1)
    if hits:
        t.ok("审计链出现 Violation 事实", f"count={len(hits)}")
        reason = json.dumps(hits[-1], ensure_ascii=False)
        t.info(f"reason 片段: {reason[:120]}")
    else:
        t.fail("审计链出现 Violation 事实", f"轮询 {POLL_TIMEOUT}s 未出现种子违规")
    return sid


async def e2_signals_endpoint(
    t: E2ETest, client: httpx.AsyncClient, sid: int
) -> Dict[str, Any]:
    t.header("E2 直调 GET /api/sessions/{id}/evolution-signals(信号可见)")
    r = await client.get(f"{SERVER_BASE}/api/sessions/{sid}/evolution-signals?limit=10")
    if r.status_code != 200:
        t.fail("evolution-signals 可达", f"status={r.status_code} body={r.text[:200]}")
        return {}
    sig = r.json()
    t.ok("evolution-signals 200", f"total_violations={sig.get('total_violations')}")
    if sig.get("total_violations", 0) >= 1:
        t.ok("total_violations>=1")
    else:
        t.fail("total_violations>=1", f"响应={json.dumps(sig, ensure_ascii=False)[:200]}")
        return sig
    signals = sig.get("signals", [])
    if not signals:
        t.fail("signals 非空", "违规已落链但信号列表为空")
        return sig
    s0 = signals[0]
    if s0.get("kind") == "violation":
        t.ok("signals[0].kind=violation")
    else:
        t.fail("signals[0].kind=violation", f"实际 {s0.get('kind')}")
    if str(s0.get("rule_ref", "")).startswith("rule_index="):
        t.ok("signals[0].rule_ref 归因", str(s0.get("rule_ref")))
    else:
        t.fail("signals[0].rule_ref 归因", f"实际 {s0.get('rule_ref')}")
    if s0.get("last_instr_type") == SEED_INSTR_TYPE:
        t.ok("signals[0].last_instr_type", SEED_INSTR_TYPE)
    else:
        t.fail("signals[0].last_instr_type", f"实际 {s0.get('last_instr_type')}")
    if s0.get("count", 0) >= 1 and SEED_REASON in str(s0.get("reason_summary", "")):
        t.ok("signals[0] 计数与原因摘要")
    else:
        t.fail("signals[0] 计数与原因摘要", json.dumps(s0, ensure_ascii=False)[:200])
    return sig


async def e3_agent_tool(
    t: E2ETest, sid: int
) -> Optional[str]:
    t.header("E3 agent 会话真实调用 evolution_signals 工具")
    # 信号只读消费 + 治理提名两工具在 rule-copilot 档白名单(23 工具)内
    ws_url = f"{AGENT_BASE.replace('http', 'ws', 1)}/api/sessions/new/ws?agent_type=rule-copilot"
    async with websockets.connect(ws_url, open_timeout=15, max_size=16 * 1024 * 1024) as ws:
        turn = await collect_turn(
            ws,
            f"请调用 evolution_signals 工具查询会话 {sid} 的进化信号"
            f"(session_id 参数传 {sid}),然后用中文概括当前有哪些违规信号、"
            "治理队列现状如何。",
        )
        if turn["session_id"]:
            t.ok("SessionCreated", f"session_id={turn['session_id']}")
        else:
            t.fail("SessionCreated", "未收到 session_id")
        if turn["error"]:
            t.fail("轮次完成", f"Error 事件: {turn['error']}")
            return turn["session_id"]
        names = tool_names(turn)
        t.info("tool_calls: " + (", ".join(n for n in names if n) or "(无)"))
        if "evolution_signals" in names:
            t.ok("真实调用 evolution_signals", f"tools={names}")
        else:
            t.fail("真实调用 evolution_signals", f"tools={names}")
        combined = turn["text"] + json.dumps(turn["tool_results"], ensure_ascii=False)
        checks = [
            ("信号含归因", "rule_index=" in combined),
            ("信号含种子违规文案", SEED_REASON in combined),
            ("摘要含违规计数", "×1" in combined or "×2" in combined or "1 条违规" in combined),
        ]
        for name, passed in checks:
            if passed:
                t.ok(name)
            else:
                t.fail(name, f"文本前 300 字: {turn['text'][:300]}")
        return turn["session_id"]


async def e4_agent_promote(
    t: E2ETest, client: httpx.AsyncClient, ws_id: str
) -> Optional[int]:
    """E4 两轮制：轮A agent 起草三步(创建/提交/查版本) → harness 以治理操作者身份
    组装沙盒证据链(dataset→sandbox_start→close, 与 E5 审批同范式) → 轮B agent 携带
    证据 id 调 rule_promote 提名。"""
    t.header("E4 agent 起草+提名(治理队列 pending, 沙盒证据由操作者组装)")
    draft_str = json.dumps(DRAFT_META_RULE, ensure_ascii=False)
    src_rule = json.dumps(
        {
            "kind": "rule_set",
            "id": "com.evorule.demo.motion_tracking.source",
            "version": "1.0.0",
            "metadata": {"title": "自进化演练源规则：运动留痕"},
            "transform": [
                {
                    "rule_id": "e2e_motion_tracking_source",
                    "type": "branch",
                    "params": {
                        "domain": {"type": "exists", "path": "payload.motion"},
                        "on_true": [
                            {
                                "type": "set",
                                "params": {
                                    "attr": "audit.motion.tracked",
                                    "operation": "set",
                                    "value": True,
                                },
                            }
                        ],
                        "on_false": [],
                    },
                }
            ],
        },
        ensure_ascii=False,
    )
    prompt_a = (
        "请为一次约束层晋升提名完成起草,严格按以下步骤执行:\n"
        f"1) 调用 rule_create 在工作空间 {ws_id} 创建一条源规则: name 为"
        ' "自进化演练源规则", content 为下面 JSON 的字符串形式,'
        ' created_by 用 "evo-agent-e2e":\n'
        f"{src_rule}\n"
        "2) 调用 rule_submit 把该规则提交为候选 (workspace_id 为 "
        f"{ws_id}, rule_id 用上一步返回的规则 id)。\n"
        "3) 调用 rule_versions 查询该规则版本列表,取最新版本的版本 id。\n"
        "全部完成后,最后一行只输出 VERSION_ID=<上一步取到的版本 id>,不要输出其他内容。"
    )
    ws_url = f"{AGENT_BASE.replace('http', 'ws', 1)}/api/sessions/new/ws?agent_type=rule-copilot"
    vid = None
    async with websockets.connect(ws_url, open_timeout=15, max_size=16 * 1024 * 1024) as ws:
        turn = await collect_turn(ws, prompt_a)
        if turn["error"]:
            t.fail("起草轮完成", f"Error 事件: {turn['error']}")
            return None
        names = tool_names(turn)
        t.info("tool_calls: " + (", ".join(n for n in names if n) or "(无)"))
        missing = [n for n in ("rule_create", "rule_submit", "rule_versions") if n not in names]
        if missing:
            t.fail("起草轮三工具真实调用", f"缺失={missing} tools={names}")
            return None
        t.ok("起草轮三工具真实调用(rule_create/rule_submit/rule_versions)", f"tools={names}")
        for line in (turn["text"] or "").splitlines():
            s = line.strip().strip("`*")
            if s.startswith("VERSION_ID"):
                vid = s.split("=", 1)[1].strip().strip("`*\"' ")
        if vid:
            t.ok("取到源规则版本 id", f"VERSION_ID={vid}")
        else:
            t.fail("取到源规则版本 id", f"最终文本前 300 字: {turn['text'][:300]}")
            return None

    r = await client.post(
        f"{SERVER_BASE}/api/workspaces/{ws_id}/test-datasets",
        json={
            "name": "自进化演练数据集",
            "cases_json": '[{"motion": "forward"}]',
            "created_by": "evo-agent-e2e",
        },
    )
    if r.status_code not in (200, 201):
        t.fail("创建测试数据集", f"status={r.status_code} body={r.text[:200]}")
        return None
    ds_body = r.json()
    dataset_id = ds_body.get("id") if isinstance(ds_body, dict) else None
    t.ok("创建测试数据集", f"dataset_id={dataset_id}")

    r = await client.post(
        f"{SERVER_BASE}/api/workspaces/{ws_id}/sandboxes",
        json={
            "rule_version_ids": [vid],
            "test_dataset_id": dataset_id,
            "started_by": "evo-agent-e2e",
        },
    )
    if r.status_code not in (200, 201):
        t.fail("启动沙盒测试", f"status={r.status_code} body={r.text[:200]}")
        return None
    sb = r.json()
    sandbox_id = sb.get("sandbox_id")
    t.ok("启动沙盒测试", f"sandbox_id={sandbox_id} cases={sb.get('test_case_count')}")

    r = await client.post(
        f"{SERVER_BASE}/api/workspaces/{ws_id}/sandboxes/{sandbox_id}/close",
        json={"closed_by": "evo-agent-e2e"},
    )
    if r.status_code != 200:
        t.fail("关闭沙盒出报告", f"status={r.status_code} body={r.text[:200]}")
        return None
    t.ok("关闭沙盒出报告", f"body={r.text[:120]}")

    prompt_b = (
        "请调用 rule_promote 提交约束层晋升提名,参数如下:\n"
        f'- workspace_id: "{ws_id}"\n'
        f'- rule_version_ids: ["{vid}"]\n'
        "- meta_rule_content: 下面草稿 JSON 的字符串形式 (不要修改内容):\n"
        f"{draft_str}\n"
        f"- test_report_sandbox_id: {sandbox_id}\n"
        '- submitted_by: "evo-agent-e2e"\n'
        '- role: "department_head"\n'
        '- description: "自进化全链演练提名"\n'
        "完成后用中文简述提名结果。"
    )
    async with websockets.connect(ws_url, open_timeout=15, max_size=16 * 1024 * 1024) as ws:
        turn = await collect_turn(ws, prompt_b)
        if turn["error"]:
            t.fail("提名轮完成", f"Error 事件: {turn['error']}")
            return None
        names = tool_names(turn)
        t.info("tool_calls: " + (", ".join(n for n in names if n) or "(无)"))
        if "rule_promote" in names:
            t.ok("真实调用 rule_promote(携带沙盒证据 id)", f"tools={names}")
        else:
            t.fail("真实调用 rule_promote(携带沙盒证据 id)", f"tools={names}")
            return None

    async def check():
        r = await client.get(f"{SERVER_BASE}/api/publish/queue?status=pending")
        if r.status_code != 200:
            return None
        for item in r.json():
            if (
                str(item.get("kind", "")) == "meta_promotion"
                and item.get("workspace_id") == ws_id
            ):
                return item
        return None

    item = await poll_until(check, POLL_TIMEOUT)
    if item:
        t.ok("治理队列出现 pending meta_promotion", f"id={item.get('id')}")
        content = item.get("meta_rule_content") or ""
        if '"tier"' in content and "constraint" in content:
            t.ok("队列项携带转写产物(tier=constraint)")
        else:
            t.fail("队列项携带转写产物(tier=constraint)", f"content 前 120 字: {content[:120]}")
        return item.get("id")
    t.fail("治理队列出现 pending meta_promotion", f"轮询 {POLL_TIMEOUT}s 未出现")
    return None


async def e5_review_publish(
    t: E2ETest, client: httpx.AsyncClient, queue_id: int
) -> None:
    t.header("E5 直调审批 approved → L2 约束文件落盘可观测")
    r = await client.post(
        f"{SERVER_BASE}/api/publish/queue/{queue_id}/review",
        json={
            "decision": "approved",
            "comment": "e2e-approve",
            "reviewed_by": "admin-evo",
            "role": "admin",
        },
    )
    if r.status_code != 200:
        t.fail("审批通过 200", f"status={r.status_code} body={r.text[:200]}")
        return
    body = r.json()
    if body.get("status") == "published":
        t.ok("审批通过 → published")
    else:
        t.fail("审批通过 → published", f"实际 {body.get('status')} body={json.dumps(body, ensure_ascii=False)[:200]}")

    # meta_promotion 的落盘产物是 rules_dir 的 L2 约束文件
    # (00_constraint_promoted_<hash16>.json), 观测口 = /api/rules/l2-inventory;
    # bundles/active 是 L3 域, 不在 meta 晋升路径上。
    async def check():
        r = await client.get(f"{SERVER_BASE}/api/rules/l2-inventory")
        if r.status_code != 200:
            return None
        body = r.json()
        for f in body.get("files", []):
            p = str(f.get("path", ""))
            if p.startswith("00_constraint_promoted_") and p.endswith(".json"):
                return body
        return None

    inv = await poll_until(check, POLL_TIMEOUT)
    if inv:
        promoted = [
            f.get("path")
            for f in inv.get("files", [])
            if str(f.get("path", "")).startswith("00_constraint_promoted_")
        ]
        t.ok("L2 约束文件落盘且 inventory 可见", f"promoted={promoted}")
    else:
        t.fail("L2 约束文件落盘且 inventory 可见", f"轮询 {POLL_TIMEOUT}s 未出现 promoted 文件")


async def e6_recount_and_verify(
    t: E2ETest, client: httpx.AsyncClient, sid1: int
) -> None:
    t.header("E6 同会话再违规(信号计数递增) + 全新会话独立信号 + 审计链验证")
    await submit_robot_move(client, t, sid1, ts=2)
    hits = await wait_violation(client, sid1, min_count=2)
    if hits:
        t.ok("同会话第二次违规落链", f"violation count={len(hits)}")
    else:
        t.fail("同会话第二次违规落链", f"轮询 {POLL_TIMEOUT}s 未达 2 条")

    r = await client.get(f"{SERVER_BASE}/api/sessions/{sid1}/evolution-signals")
    if r.status_code == 200:
        sig = r.json()
        if sig.get("total_violations", 0) >= 2:
            t.ok("信号计数递增(total_violations>=2)", f"实际 {sig.get('total_violations')}")
        else:
            t.fail("信号计数递增(total_violations>=2)", f"实际 {sig.get('total_violations')}")
        agg = sig.get("signals", [])
        if agg and agg[0].get("count", 0) >= 2:
            t.ok("同归因聚合计数>=2", f"count={agg[0].get('count')}")
        else:
            t.fail("同归因聚合计数>=2", json.dumps(agg, ensure_ascii=False)[:200])
    else:
        t.fail("evolution-signals 复查", f"status={r.status_code}")

    sid2 = await make_session(client, t, "E6-新会话")
    if sid2 is not None:
        await submit_robot_move(client, t, sid2, ts=3)
        hits2 = await wait_violation(client, sid2, min_count=1)
        if hits2:
            t.ok("全新会话独立违规落链")
        else:
            t.fail("全新会话独立违规落链", f"轮询 {POLL_TIMEOUT}s 未出现")
        r = await client.get(f"{SERVER_BASE}/api/sessions/{sid2}/evolution-signals")
        if r.status_code == 200 and r.json().get("total_violations", 0) >= 1:
            t.ok("新会话独立信号", f"total_violations={r.json().get('total_violations')}")
        else:
            t.fail("新会话独立信号", f"status={r.status_code} body={r.text[:150]}")

    for sid in (sid1, sid2):
        r = await client.get(f"{SERVER_BASE}/api/sessions/{sid}/audit/verify")
        ok_flag = False
        if r.status_code == 200:
            body = r.json()
            ok_flag = body.get("verified", body.get("valid")) is True
        if ok_flag:
            t.ok(f"审计链验证通过(session {sid})")
        else:
            t.fail(f"审计链验证通过(session {sid})", f"status={r.status_code} body={r.text[:150]}")


async def ensure_workspace(client: httpx.AsyncClient, t: E2ETest) -> Optional[str]:
    """创建演练 workspace(提名 provenance 归属);名字带时间戳保幂等。"""
    name = f"evolution-e2e-{int(time.time())}"
    r = await client.post(
        f"{SERVER_BASE}/api/workspaces",
        json={"name": name, "owner_id": "evo-agent-e2e", "description": "自进化全链演练"},
    )
    if r.status_code in (200, 201):
        ws_id = r.json().get("id") or r.json().get("workspace_id")
        t.ok("创建演练 workspace", f"id={ws_id}")
        return ws_id
    t.fail("创建演练 workspace", f"status={r.status_code} body={r.text[:200]}")
    return None


async def main() -> int:
    t = E2ETest()
    result: Dict[str, Any] = {"started_at": time.strftime("%Y-%m-%dT%H:%M:%S")}
    async with httpx.AsyncClient(timeout=30) as client:
        await e0_probe(t, client)
        if t.failed:
            print("\n探活失败,终止(双服务需先拉起)。")
            result["error"] = "probe failed"
            result["passed"] = t.passed
            result["failed"] = t.failed
            Path(RESULT_OUT).write_text(json.dumps(result, ensure_ascii=False, indent=2), "utf-8")
            return 1

        sid1 = await e1_seed_violation(t, client)
        result["session_seed"] = sid1
        if sid1 is None:
            Path(RESULT_OUT).write_text(json.dumps(result, ensure_ascii=False, indent=2), "utf-8")
            return 1

        sig = await e2_signals_endpoint(t, client, sid1)
        result["total_violations_after_e1"] = sig.get("total_violations")

        await e3_agent_tool(t, sid1)
        ws_id = await ensure_workspace(client, t)
        queue_id = None
        if ws_id:
            queue_id = await e4_agent_promote(t, client, ws_id)
        else:
            t.fail("E4 前置 workspace", "workspace 创建失败,提名无法进行")
        result["workspace_id"] = ws_id
        result["queue_id"] = queue_id

        if queue_id is not None:
            await e5_review_publish(t, client, queue_id)
        else:
            t.fail("E5 审批", "无队列项可审(依赖 E4)")

        await e6_recount_and_verify(t, client, sid1)

    result["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    result["passed"] = t.passed
    result["failed"] = t.failed
    result["errors"] = t.errors
    Path(RESULT_OUT).write_text(json.dumps(result, ensure_ascii=False, indent=2), "utf-8")

    t.header("收尾")
    print(f"  通过 {t.passed} 项 / 失败 {t.failed} 项;结果已写 {RESULT_OUT}")
    return 0 if t.all_passed else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
