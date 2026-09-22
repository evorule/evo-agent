# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

r"""
进化巡视任务模式 真实 LLM E2E（patrol 子命令：信号 → 起草 → 证据 → 提名 → 报告）

走真实双进程: evorule-server(HTTP) + evo-agent patrol 子命令(一次性任务,无需 serve)
+ 真实 MiniMax LLM。验证自进化二期的三个分支:

  E0  服务探活(evorule-server /api/health)
  E1  直调 server 制造种子违规: POST /api/sessions + /command(robot_move) →
      审计链出现 Violation 事实(同自进化全链演练种子)
  E2  patrol 全链提名: `evo-agent patrol --session S --workspace W` →
      exit 0 + 报告 status=nominated + actions 含 draft/gate_one_evidence/nominate
      + 治理队列出现 pending meta_promotion(workspace_id 过滤参数生效)
      + 队列项携带 tier=constraint 转写产物
  E3  重复提名被拒(去重门禁): 直接 POST 同 (workspace, kind, 目标规则) 提名 → 409
  E4  直调审批 approved → L2 约束文件落盘(/api/rules/l2-inventory 可见)
  E5  无信号静默: 全新无违规会话 patrol → exit 0 + status=no_signal + 零提名
  E6  审计链验证(种子会话 /audit/verify verified=true)
  E7  审批落盘后同目标再提名被拒(双态去重 published 分支, 98 号 D3) → 409
  E8  新会话违规归因=新约束(98 号核心断言: enforce 型晋升产物先于种子拦截,
      evolution-signals rule_ref=00_constraint_promoted_*.json#k)

前置(由运行方准备,脚本只做验证侧):
  - evorule-server 已运行: 127.0.0.1:18080
    (--insecure-serve --rules-dir <含 00_constraint_ enforce 种子的目录>
     --workspace-db <临时路径> --no-rate-limit;种子不进公开仓)
  - evo-agent debug 构建存在(target/debug/evo-agent.exe)
  - 仓库根 .env 含 MINIMAX_API_KEY(脚本读取后注入 patrol 子进程环境,
    绝不回显);evo-agent.toml 的 evorule.base_url 指向 18080
  - pip install httpx

运行(仓库根目录):
  python tests/e2e_evolution_patrol.py
"""

import asyncio
import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

try:
    import httpx
except ImportError as e:
    print(f"ERROR: 缺少依赖({e.name})。运行: pip install httpx")
    sys.exit(1)

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")

SERVER_BASE = os.environ.get("E2E_SERVER_BASE", "http://127.0.0.1:18080")
EVO_AGENT_DIR = os.environ.get(
    "E2E_EVO_AGENT_DIR", str(Path(__file__).resolve().parents[1])
)
PATROL_BIN = os.environ.get(
    "E2E_PATROL_BIN", str(Path(EVO_AGENT_DIR) / "target" / "debug" / "evo-agent.exe")
)
PATROL_TIMEOUT = float(os.environ.get("E2E_PATROL_TIMEOUT", "900"))
NO_SIGNAL_TIMEOUT = float(os.environ.get("E2E_NO_SIGNAL_TIMEOUT", "120"))
POLL_TIMEOUT = float(os.environ.get("E2E_POLL_TIMEOUT", "20"))

# 种子哨兵期望值(与启动 server 的 rules_dir 中 00_constraint_ 种子一致)
SEED_REASON = "运动指令缺少安全清场前置"
SEED_INSTR_TYPE = "robot_move"

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


async def poll_until(check, timeout_s: float, interval: float = 0.5):
    """轮询异步事实落链(命令经 channel 异步进反应器),直到 check 通过或超时。"""
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        last = await check()
        if last is not None:
            return last
        await asyncio.sleep(interval)
    return None


def load_minimax_key() -> Optional[str]:
    """从 evo-agent .env 读 MINIMAX_API_KEY(仅注入子进程,绝不回显)。"""
    env_path = Path(EVO_AGENT_DIR) / ".env"
    if not env_path.exists():
        return None
    for line in env_path.read_text(encoding="utf-8", errors="ignore").splitlines():
        line = line.strip()
        if line.startswith("MINIMAX_API_KEY="):
            val = line.split("=", 1)[1].strip().strip('"').strip("'")
            return val or None
    return None


def run_patrol(session_id: int, workspace_id: str, timeout_s: float) -> Dict[str, Any]:
    """调起 patrol 子命令,返回 {exit_code, report(dict|None), stderr}。"""
    env = os.environ.copy()
    key = load_minimax_key()
    if key:
        env["MINIMAX_API_KEY"] = key
    env["EVO_AGENT_EVORULE__BASE_URL"] = SERVER_BASE
    proc = subprocess.run(
        [
            PATROL_BIN,
            "patrol",
            "--session",
            str(session_id),
            "--workspace",
            workspace_id,
        ],
        cwd=EVO_AGENT_DIR,
        env=env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=timeout_s,
    )
    report = None
    for line in reversed((proc.stdout or "").splitlines()):
        s = line.strip()
        if s.startswith("{"):
            try:
                report = json.loads(s)
                break
            except json.JSONDecodeError:
                continue
    return {"exit_code": proc.returncode, "report": report, "stderr": proc.stderr or ""}


async def make_session(client: httpx.AsyncClient, t: E2ETest, label: str) -> Optional[int]:
    r = await client.post(f"{SERVER_BASE}/api/sessions")
    if r.status_code != 200:
        t.fail(f"创建会话({label})", f"status={r.status_code} body={r.text[:200]}")
        return None
    sid = r.json().get("session_id")
    t.ok(f"创建会话({label})", f"session_id={sid}")
    return sid


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


async def e0_probe(t: E2ETest, client: httpx.AsyncClient) -> bool:
    t.header("E0 服务探活")
    r = await client.get(f"{SERVER_BASE}/api/health")
    if r.status_code == 200:
        t.ok("evorule-server /api/health")
        return True
    t.fail("evorule-server /api/health", f"status={r.status_code} body={r.text[:100]}")
    return False


async def e1_seed_violation(t: E2ETest, client: httpx.AsyncClient) -> Optional[int]:
    t.header("E1 直调 server 制造种子违规(enforce 拦截 → Violation 事实落链)")
    sid = await make_session(client, t, "E1")
    if sid is None:
        return None
    r = await client.post(
        f"{SERVER_BASE}/api/sessions/{sid}/command",
        json={"instruction": {"type": SEED_INSTR_TYPE, "params": {"timestamp": 1}}},
    )
    if r.status_code == 200:
        t.ok(f"提交 {SEED_INSTR_TYPE} 指令", f"session={sid}")
    else:
        t.fail(f"提交 {SEED_INSTR_TYPE} 指令", f"status={r.status_code} body={r.text[:200]}")
        return sid
    hits = await wait_violation(client, sid, min_count=1)
    if hits:
        t.ok("审计链出现 Violation 事实", f"count={len(hits)}")
    else:
        t.fail("审计链出现 Violation 事实", f"轮询 {POLL_TIMEOUT}s 未出现种子违规")
    return sid


async def e2_patrol_nominate(
    t: E2ETest, client: httpx.AsyncClient, sid: int, ws_id: str
) -> Optional[Dict[str, Any]]:
    t.header("E2 patrol 全链提名(真实 MiniMax 两轮制)")
    t.info(f"调起 patrol: session={sid} workspace={ws_id}(真实 LLM,需数分钟)")
    result = await asyncio.to_thread(run_patrol, sid, ws_id, PATROL_TIMEOUT)
    if result["exit_code"] != 0:
        diag = result["stderr"][-300:]
        if result.get("report"):
            diag += " report=" + json.dumps(result["report"], ensure_ascii=False)[:600]
        t.fail("patrol exit 0", f"exit={result['exit_code']} stderr 尾部: {diag}")
        return None
    t.ok("patrol exit 0")
    report = result["report"]
    if not report:
        t.fail("patrol 报告 JSON 可解析", f"stdout 尾部: {(result['stderr'] or '')[-200:]}")
        return None
    if report.get("status") == "nominated":
        t.ok("报告 status=nominated", f"queue_id={report.get('queue_id')}")
    else:
        t.fail("报告 status=nominated", f"实际 {report.get('status')} report={json.dumps(report, ensure_ascii=False)[:300]}")
        return None
    if isinstance(report.get("queue_id"), int) and isinstance(report.get("sandbox_id"), int):
        t.ok("报告携带 queue_id/sandbox_id", f"queue={report.get('queue_id')} sandbox={report.get('sandbox_id')}")
    else:
        t.fail("报告携带 queue_id/sandbox_id", json.dumps(report, ensure_ascii=False)[:200])
    actions = {a.get("action") for a in report.get("actions", [])}
    need = {"draft", "gate_one_evidence", "nominate"}
    if need.issubset(actions):
        t.ok("actions 三阶段齐全(draft/gate_one_evidence/nominate)")
    else:
        t.fail("actions 三阶段齐全(draft/gate_one_evidence/nominate)", f"实际 {actions}")
    if (report.get("signals") or {}).get("total_violations", 0) >= 1:
        t.ok("报告内嵌信号快照(total_violations>=1)")
    else:
        t.fail("报告内嵌信号快照(total_violations>=1)", json.dumps(report.get("signals"), ensure_ascii=False)[:200])

    # D3 过滤参数生效 + 队列项断言
    async def check():
        r = await client.get(
            f"{SERVER_BASE}/api/publish/queue", params={"status": "pending", "workspace_id": ws_id}
        )
        if r.status_code != 200:
            return None
        for item in r.json():
            if item.get("kind") == "meta_promotion" and item.get("workspace_id") == ws_id:
                return item
        return None

    item = await poll_until(check, POLL_TIMEOUT)
    if not item:
        t.fail("治理队列出现 pending meta_promotion(workspace 过滤)", f"轮询 {POLL_TIMEOUT}s 未出现")
        return None
    t.ok("治理队列出现 pending meta_promotion(workspace 过滤)", f"id={item.get('id')}")
    content = item.get("meta_rule_content") or ""
    if '"tier"' in content and "constraint" in content:
        t.ok("队列项携带转写产物(tier=constraint)")
    else:
        t.fail("队列项携带转写产物(tier=constraint)", f"content 前 120 字: {content[:120]}")
    if item.get("test_report_sandbox_id") is not None:
        t.ok("队列项关联闸门一沙盒证据", f"sandbox={item.get('test_report_sandbox_id')}")
    else:
        t.fail("队列项关联闸门一沙盒证据", "test_report_sandbox_id 为空")
    # 无过滤旧行为仍在(全量可见)
    r_all = await client.get(f"{SERVER_BASE}/api/publish/queue", params={"status": "pending"})
    if r_all.status_code == 200 and any(
        i.get("id") == item.get("id") for i in r_all.json()
    ):
        t.ok("无过滤旧行为不变(全量仍可见)")
    else:
        t.fail("无过滤旧行为不变(全量仍可见)", f"status={r_all.status_code}")
    return item


async def e3_duplicate_rejected(
    t: E2ETest, client: httpx.AsyncClient, ws_id: str, item: Dict[str, Any]
) -> None:
    t.header("E3 重复提名被拒(去重门禁, 同 workspace+kind+目标规则)")
    version_ids = _parse_promoted_from(item.get("meta_rule_content") or "")
    if not version_ids:
        t.fail("从队列项解析 promoted_from 版本集", "队列表单缺 promoted_from")
        return
    t.ok("从队列项解析 promoted_from 版本集", f"ids={version_ids}")
    r = await client.post(f"{SERVER_BASE}/api/publish/queue", json=_nominate_body(ws_id, item))
    if r.status_code == 409:
        t.ok("重复提名 409", r.text[:120].replace("\n", " "))
    else:
        t.fail("重复提名 409", f"status={r.status_code} body={r.text[:200]}")


async def e4_review_publish(t: E2ETest, client: httpx.AsyncClient, item: Dict[str, Any]) -> None:
    t.header("E4 直调审批 approved → L2 约束文件落盘可观测")
    r = await client.post(
        f"{SERVER_BASE}/api/publish/queue/{item.get('id')}/review",
        json={
            "decision": "approved",
            "comment": "patrol-e2e-approve",
            "reviewed_by": "admin-evo",
            "role": "admin",
        },
    )
    if r.status_code != 200:
        t.fail("审批通过 200", f"status={r.status_code} body={r.text[:200]}")
        return
    if r.json().get("status") == "published":
        t.ok("审批通过 → published")
    else:
        t.fail("审批通过 → published", f"实际 {r.json().get('status')}")

    async def check():
        r = await client.get(f"{SERVER_BASE}/api/rules/l2-inventory")
        if r.status_code != 200:
            return None
        body = r.json()
        promoted = [
            f.get("path")
            for f in body.get("files", [])
            if str(f.get("path", "")).startswith("00_constraint_promoted_")
        ]
        return promoted or None

    promoted = await poll_until(check, POLL_TIMEOUT)
    if promoted:
        t.ok("L2 约束文件落盘且 inventory 可见", f"promoted={promoted}")
    else:
        t.fail("L2 约束文件落盘且 inventory 可见", f"轮询 {POLL_TIMEOUT}s 未出现 promoted 文件")


def _parse_promoted_from(content: str) -> List[str]:
    """从转写产物 metadata.promoted_from 解析目标版本集(与 E3 同口径)。"""
    promoted_from = ""
    try:
        promoted_from = str(json.loads(content).get("metadata", {}).get("promoted_from", ""))
    except (json.JSONDecodeError, AttributeError):
        pass
    return [
        v.strip()
        for v in promoted_from.removeprefix("rule_version:").split(",")
        if v.strip()
    ]


def _nominate_body(ws_id: str, item: Dict[str, Any]) -> Dict[str, Any]:
    return {
        "workspace_id": ws_id,
        "rule_version_ids": _parse_promoted_from(item.get("meta_rule_content") or ""),
        "test_report_sandbox_id": item.get("test_report_sandbox_id"),
        "kind": "meta_promotion",
        "meta_rule_content": item.get("meta_rule_content") or "",
        # 与 patrol 同身份(evo-agent-patrol=workspace 属主):模拟巡视再次运行
        # 重复提名同一目标——成员校验通过后才会触达去重门禁(409)
        "submitted_by": "evo-agent-patrol",
        "role": "department_head",
        "description": "重复提名演练(应被去重门禁拒绝)",
    }


async def e7_published_renominate(
    t: E2ETest, client: httpx.AsyncClient, ws_id: str, item: Dict[str, Any]
) -> None:
    t.header("E7 审批落盘后同目标再提名被拒(双态去重 published 分支, 98 号 D3)")
    version_ids = _parse_promoted_from(item.get("meta_rule_content") or "")
    if not version_ids:
        t.fail("E7 promoted_from 版本集解析", "队列表单缺 promoted_from")
        return
    r = await client.post(f"{SERVER_BASE}/api/publish/queue", json=_nominate_body(ws_id, item))
    if r.status_code == 409 and "published" in r.text:
        t.ok("审批后再提名 409(published 同目标占用)", r.text[:120].replace("\n", " "))
    else:
        t.fail(
            "审批后再提名 409(published 同目标占用)",
            f"status={r.status_code} body={r.text[:200]}",
        )


async def e8_new_session_attribution(t: E2ETest, client: httpx.AsyncClient) -> None:
    t.header("E8 新会话违规归因=新约束(enforce 型晋升产物先于种子拦截)")
    sid = await make_session(client, t, "E8-新约束生效")
    if sid is None:
        return
    r = await client.post(
        f"{SERVER_BASE}/api/sessions/{sid}/command",
        json={"instruction": {"type": SEED_INSTR_TYPE, "params": {"timestamp": 2}}},
    )
    if r.status_code == 200:
        t.ok(f"新会话提交 {SEED_INSTR_TYPE} 指令", f"session={sid}")
    else:
        t.fail(f"新会话提交 {SEED_INSTR_TYPE} 指令", f"status={r.status_code} body={r.text[:200]}")
        return

    async def check():
        r = await client.get(f"{SERVER_BASE}/api/sessions/{sid}/evolution-signals")
        if r.status_code != 200:
            return None
        signals = r.json().get("signals") or []
        hit = [
            s
            for s in signals
            if str(s.get("rule_ref", "")).startswith("00_constraint_promoted_")
            and "#" in str(s.get("rule_ref", ""))
        ]
        return hit or None

    hit = await poll_until(check, POLL_TIMEOUT)
    if not hit:
        r = await client.get(f"{SERVER_BASE}/api/sessions/{sid}/evolution-signals")
        got = (
            json.dumps(r.json().get("signals"), ensure_ascii=False)[:300]
            if r.status_code == 200
            else f"status={r.status_code}"
        )
        t.fail(
            "新会话违规归因=新约束(rule_ref=00_constraint_promoted_*#k)",
            f"轮询 {POLL_TIMEOUT}s 未出现 promoted 归因; signals={got}",
        )
        return
    t.ok("新会话违规归因=新约束(rule_ref=00_constraint_promoted_*#k)", f"rule_ref={hit[0].get('rule_ref')}")
    if hit[0].get("last_instr_type") == SEED_INSTR_TYPE:
        t.ok("归因信号指令类型=robot_move", f"reason={hit[0].get('reason_summary', '')[:60]}")
    else:
        t.fail("归因信号指令类型=robot_move", f"实际 {hit[0].get('last_instr_type')}")


async def e5_no_signal_silent(
    t: E2ETest, client: httpx.AsyncClient, ws_id: str
) -> None:
    t.header("E5 无信号巡视静默退出(零动作)")
    sid = await make_session(client, t, "E5-无违规")
    if sid is None:
        return
    result = await asyncio.to_thread(run_patrol, sid, ws_id, NO_SIGNAL_TIMEOUT)
    report = result["report"] or {}
    if result["exit_code"] == 0 and report.get("status") == "no_signal":
        t.ok("无信号 patrol exit 0 + status=no_signal")
    else:
        t.fail(
            "无信号 patrol exit 0 + status=no_signal",
            f"exit={result['exit_code']} report={json.dumps(report, ensure_ascii=False)[:200]}",
        )
    if report.get("actions") == []:
        t.ok("零动作(actions 为空)")
    else:
        t.fail("零动作(actions 为空)", f"actions={report.get('actions')}")


async def e6_verify_chain(t: E2ETest, client: httpx.AsyncClient, sid: int) -> None:
    t.header("E6 审计链验证")
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
    name = f"patrol-e2e-{int(time.time())}"
    r = await client.post(
        f"{SERVER_BASE}/api/workspaces",
        json={"name": name, "owner_id": "evo-agent-patrol", "description": "进化巡视演练"},
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
        healthy = await e0_probe(t, client)
        if not healthy:
            print("\n探活失败,终止(evorule-server 需先拉起)。")
            result["error"] = "probe failed"
            result["passed"] = t.passed
            result["failed"] = t.failed
            return 1

        sid = await e1_seed_violation(t, client)
        result["session_seed"] = sid
        ws_id = await ensure_workspace(client, t)
        item = None
        if sid is not None and ws_id:
            item = await e2_patrol_nominate(t, client, sid, ws_id)
            if item:
                await e3_duplicate_rejected(t, client, ws_id, item)
                await e4_review_publish(t, client, item)
                await e7_published_renominate(t, client, ws_id, item)
                await e8_new_session_attribution(t, client)
            else:
                t.fail("E3/E4 前置队列项", "E2 未产出队列项")
        else:
            t.fail("E2 前置", "种子会话或 workspace 创建失败")
        result["workspace_id"] = ws_id
        result["queue_id"] = (item or {}).get("id")

        if ws_id:
            await e5_no_signal_silent(t, client, ws_id)
        if sid is not None:
            await e6_verify_chain(t, client, sid)

    result["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    result["passed"] = t.passed
    result["failed"] = t.failed
    result["errors"] = t.errors
    out = os.environ.get(
        "E2E_PATROL_RESULT_OUT",
        str(Path(os.environ.get("TEMP", "/tmp")) / "evorule-e2e-patrol-result.json"),
    )
    Path(out).write_text(json.dumps(result, ensure_ascii=False, indent=2), "utf-8")

    t.header("收尾")
    print(f"  通过 {t.passed} 项 / 失败 {t.failed} 项;结果已写 {out}")
    return 0 if t.all_passed else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
