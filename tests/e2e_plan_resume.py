# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

"""plan-execute 中断恢复 E2E（真 kill 注入 + 显式恢复双跑；跨平台终止原语双形态）

前置条件与 e2e_plan_execute.py 相同：真实 LLM API key（.env）+ 运行中的
evorule-server + cargo 编译产物。

场景 K（kill 注入 + 恢复双跑）：

  1. 启动 `evo-agent workflow research_plan --plan-execute`（与
     e2e_plan_execute.py 场景 A 同一工作流），后台运行；
  2. 轮询 run 级账本（data/sessions/planrun-*.jsonl）直到 v1 计划已物化
     （PlanLoopCheckpointed 在账）且 ≥2 粒已完成（NodeCheckpointed）——
     即执行中段窗口；
  3. 强杀进程：Windows = `taskkill /F /PID`（树杀），POSIX = `kill -9`
     （双形态终止原语，对应比赛/生产两侧的真实死亡面）；
  4. 从账本文件名取得 run 会话名，显式恢复：
     `evo-agent workflow research_plan --plan-execute --resume-session <sid>`
  5. 断言：
     a. 恢复跑 EXIT=0，stderr 含 "resuming plan run"；
     b. run ledger session 与被杀 run 同会话（续写同一账本）；
     c. 合并账本内每个 node_id 至多一条 NodeCheckpointed（已完成粒零重执行）；
     d. 恢复跑产出非空 content。

诚实面（与设计档 §七一致）：真实 LLM 温度>0，两次运行的 content 文本不承诺
一致——「等价」判据为账本事实等价（零重执行 + 会话连续 + 终态可达）；内容级
hash 等价由确定性 compute 链的 UT 层双跑覆盖（引擎/驱动 UT 各一）。

用法：python tests/e2e_plan_resume.py [--evidence-dir DIR]
"""

from __future__ import annotations

import argparse
import json
import threading
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Dict, Optional

# ===== 颜色 =====
GREEN = "\033[92m"
RED = "\033[91m"
YELLOW = "\033[93m"
RESET = "\033[0m"

REPO_ROOT = Path(__file__).parent.parent
ENV_PATH = REPO_ROOT / ".env"
BINARY = REPO_ROOT / "target" / "debug" / "evo-agent.exe"
SESSIONS_DIR = REPO_ROOT / "data" / "sessions"
WORKFLOW_ID = "research_plan"
POLL_TIMEOUT_SECS = 600
POLL_INTERVAL_SECS = 2.0


def load_env_file(env_path: Path) -> Dict[str, str]:
    """从 .env 文件加载 key=value（不依赖 python-dotenv）"""
    env: Dict[str, str] = {}
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


def check(ok: bool, pass_msg: str, fail_msg: str) -> bool:
    if ok:
        print(f"  {GREEN}PASS{RESET}  {pass_msg}")
    else:
        print(f"  {RED}FAIL{RESET}  {fail_msg}")
    return ok


def kill_hard(pid: int) -> None:
    """跨平台强杀原语（双形态）：Windows=taskkill /F（树杀）；POSIX=kill -9"""
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(pid)],
            capture_output=True,
            check=False,
        )
    else:
        import signal

        os.kill(pid, signal.SIGKILL)


def journal_events(path: Path) -> list[dict]:
    """读 run 账本全部事件（行级 JSON；坏行跳过——kill 窗口允许半行尾巴）"""
    out: list[dict] = []
    if not path.exists():
        return out
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line))
        except ValueError:
            continue
    return out


def existing_planrun_sessions() -> set:
    """当前全部 planrun 会话名（场景隔离基线：轮询只认本场景新增的会话）"""
    if not SESSIONS_DIR.exists():
        return set()
    return {p.stem for p in SESSIONS_DIR.glob("planrun-*.jsonl")}


def newest_planrun_session(exclude: Optional[set] = None) -> Optional[str]:
    """最新 planrun-*.jsonl 的会话名（可排除既有集合；无则 None）"""
    if not SESSIONS_DIR.exists():
        return None
    candidates = sorted(
        (p for p in SESSIONS_DIR.glob("planrun-*.jsonl") if p.stem not in (exclude or set())),
        key=lambda p: p.stat().st_mtime,
    )
    if not candidates:
        return None
    return candidates[-1].stem


def journal_has_terminal(path: Path) -> bool:
    """账本是否已落终态标记（已完成/已终断 run 不构成注入窗口）"""
    return any(e.get("type") == "plan_loop_finished" for e in journal_events(path))


def count_events(events: list[dict], kind: str) -> int:
    return sum(1 for e in events if e.get("type") == kind)


def checkpoint_node_ids(events: list[dict]) -> list[str]:
    return [e["payload"]["node_id"] for e in events if e.get("type") == "node_checkpointed"]


def scenario_k(env: Dict[str, str], evidence_dir: Optional[Path]) -> bool:
    print("\n=== 场景 K: kill 注入 + 显式恢复双跑 ===")
    if not BINARY.exists():
        print(f"  {RED}FAIL{RESET}  编译产物缺失: {BINARY}")
        return False

    proc_env = {**os.environ, **env}
    started_at = time.time()

    # ① 首跑(后台):与场景 A 同一 plan-execute 工作流。
    # 子进程输出落文件(不接 PIPE——tracing 日志量大,管道缓冲灌满后子进程
    # 阻塞在写 stderr 上永不推进,注入窗口永不到达;落文件顺带成为证据)
    evidence_dir = evidence_dir or REPO_ROOT / "data" / "e2e_evidence"
    evidence_dir.mkdir(parents=True, exist_ok=True)
    first_out = open(evidence_dir / "scenarioK_first_stdout.txt", "w", encoding="utf-8")
    first_err = open(evidence_dir / "scenarioK_first_stderr.txt", "w", encoding="utf-8")
    proc = subprocess.Popen(
        [str(BINARY), "workflow", WORKFLOW_ID, "--plan-execute"],
        cwd=str(REPO_ROOT),
        env=proc_env,
        stdout=first_out,
        stderr=first_err,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    print(f"  ···  首跑已启动 pid={proc.pid}（等待执行中段窗口后强杀）")

    # ② 轮询账本:本场景新增会话 + v1 计划已物化 + ≥2 粒完成 + 无终态
    #(场景隔离:既有会话一律排除,防止旧 run 满足窗口造成假注入)
    before = existing_planrun_sessions()
    sid: Optional[str] = None
    deadline = time.time() + POLL_TIMEOUT_SECS
    try:
        while time.time() < deadline:
            if proc.poll() is not None:
                print(f"  {RED}FAIL{RESET}  首跑在注入前自行退出（exit={proc.returncode}）")
                return False
            sid = newest_planrun_session(exclude=before)
            if sid:
                journal_path = SESSIONS_DIR / f"{sid}.jsonl"
                if not journal_has_terminal(journal_path):
                    events = journal_events(journal_path)
                    # ≥1 粒即可:run 账本自 plan v1 起(probe 不入账),首粒完成
                    # =执行中段真实窗口;若按 v1 全粒数等,两粒工作流的窗口与
                    # 自然完成瞬间重合,注入永远赶不上
                    if count_events(events, "plan_loop_checkpointed") >= 1 and count_events(
                        events, "node_checkpointed"
                    ) >= 1:
                        break
                sid = None
            time.sleep(POLL_INTERVAL_SECS)
        else:
            kill_hard(proc.pid)
            print(f"  {RED}FAIL{RESET}  轮询超时（{POLL_TIMEOUT_SECS}s）未达注入窗口，已强杀")
            return False
    finally:
        first_out.close()
        first_err.close()
    window_secs = time.time() - started_at
    events = journal_events(SESSIONS_DIR / f"{sid}.jsonl")
    print(
        f"  ···  注入窗口到达（{window_secs:.0f}s）: 会话 {sid}，"
        f"计划检查点 {count_events(events, 'plan_loop_checkpointed')} 条，"
        f"粒检查点 {count_events(events, 'node_checkpointed')} 条"
    )

    # ③ 强杀(双形态原语)
    kill_hard(proc.pid)
    proc.wait(timeout=30)
    print(f"  {GREEN}PASS{RESET}  进程已强杀（{'taskkill /F' if os.name == 'nt' else 'kill -9'}）")

    # ④ 显式恢复(人类持剑:续跑仅经旗标)
    resume_proc = subprocess.run(
        [
            str(BINARY),
            "workflow",
            WORKFLOW_ID,
            "--plan-execute",
            "--resume-session",
            sid,
        ],
        cwd=str(REPO_ROOT),
        env=proc_env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=900,
    )
    if evidence_dir is not None:
        evidence_dir.mkdir(parents=True, exist_ok=True)
        (evidence_dir / "scenarioK_resume_stderr.txt").write_text(
            resume_proc.stderr, encoding="utf-8"
        )
        (evidence_dir / "scenarioK_resume_stdout.txt").write_text(
            resume_proc.stdout, encoding="utf-8"
        )

    ok = check(
        resume_proc.returncode == 0,
        f"恢复跑 EXIT=0（{resume_proc.returncode}）",
        f"恢复跑失败: {resume_proc.stderr[-500:]}",
    )
    ok = check(
        "resuming plan run" in resume_proc.stderr,
        "stderr 含 'resuming plan run'（恢复路径生效）",
        "stderr 缺恢复标记",
    ) and ok

    # ⑤ 断言:同会话续写 + 零重执行 + 非空产出
    merged = journal_events(SESSIONS_DIR / f"{sid}.jsonl")
    ckpt_nodes = checkpoint_node_ids(merged)
    duplicates = sorted({n for n in ckpt_nodes if ckpt_nodes.count(n) > 1})
    ok = check(
        not duplicates,
        f"零重执行:每粒至多一条检查点（共 {len(ckpt_nodes)} 条）",
        f"发现重复检查点粒（重执行证据）: {duplicates}",
    )
    ok = check(
        f"run ledger session: {sid}" in resume_proc.stderr,
        "恢复跑续写同一 run 会话",
        "恢复跑会话与被杀 run 不一致",
    ) and ok
    ok = check(
        resume_proc.stdout.strip() != "",
        "恢复跑产出非空 content",
        "恢复跑 content 为空",
    ) and ok
    return ok




# ===== 场景 L:serve 面 kill 注入 + 扫尾 + 显式恢复(人类持剑全链) =====

SERVE_PORT = 18092
SERVE_BASE = f"http://127.0.0.1:{SERVE_PORT}"

# 与 e2e_plan_execute.py 场景 F 同一研究任务的等价形态(PlanFact 指示词随 goal 携带)
SCENARIO_L_GOAL = (
    "Goal: research the topic 'evorule deterministic workflow engine design' and produce "
    "a research digest. Produce a PlanFact JSON for this goal: 2-4 llm nodes with "
    'agent_type "researcher" (collect key facts, then synthesize a structured digest), '
    "exactly one sink node. Output ONLY the PlanFact JSON object."
)


def wait_serve_health(timeout_s: float = 60.0) -> Optional[str]:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"{SERVE_BASE}/health", timeout=3) as resp:
                if resp.status == 200:
                    return None
        except Exception:  # noqa: BLE001 — 轮询期连接拒绝/超时均属预期
            pass
        time.sleep(0.5)
    return f"serve health 未就绪（{timeout_s}s 超时）"


def http_json(method: str, url: str, body: Optional[dict] = None, timeout: float = 900) -> tuple:
    """HTTP JSON 请求;返回 (status, parsed|None, raw_text)"""
    data = json.dumps(body).encode("utf-8") if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            text = resp.read().decode("utf-8", errors="replace")
            try:
                return resp.status, json.loads(text), text
            except ValueError:
                return resp.status, None, text
    except urllib.error.HTTPError as e:
        text = e.read().decode("utf-8", errors="replace")
        try:
            return e.code, json.loads(text), text
        except ValueError:
            return e.code, None, text
    except (ConnectionError, TimeoutError, OSError) as e:
        # serve 被杀瞬间的连接重置属预期(被杀 run 的后台线程)——静默哨兵
        return 0, None, f"connection error: {e}"


def scenario_l(env: Dict[str, str], evidence_dir: Optional[Path]) -> bool:
    """serve 面 kill 注入 + 扫尾 + 显式恢复。

    断言五条：
      ① kill serve 进程(taskkill /F 树杀)后重启(watchdog 拉起的脚本等价物)；
      ② GET /api/sessions/resumable 列出被杀 run(计划检查点在账且无终态)；
      ③ 带 resume_session_id 重放 → HTTP 200 且 success=true；
      ④ run_session_id 同会话续写;合并账本每粒至多一条检查点(零重执行)；
      ⑤ content 非空。
    """
    print("\n=== 场景 L: serve 面 kill 注入 + 扫尾 + 显式恢复 ===")
    if not BINARY.exists():
        print(f"  {RED}FAIL{RESET}  编译产物缺失: {BINARY}")
        return False
    proc_env = {**os.environ, **env}
    evidence_dir = evidence_dir or REPO_ROOT / "data" / "e2e_evidence"
    evidence_dir.mkdir(parents=True, exist_ok=True)

    def spawn_serve() -> subprocess.Popen:
        # serve 输出落文件(不接 DEVNULL——健康检查失败时无据可查)
        out = open(evidence_dir / "scenarioL_serve_stderr.log", "a", encoding="utf-8")
        return subprocess.Popen(
            [str(BINARY), "serve", "--workdir", str(REPO_ROOT),
             "--host", "127.0.0.1", "--port", str(SERVE_PORT), "--no-auth"],
            cwd=str(REPO_ROOT), env=proc_env,
            stdout=out, stderr=out,
        )

    serve = spawn_serve()
    err = wait_serve_health()
    if err:
        kill_hard(serve.pid)
        print(f"  {RED}FAIL{RESET}  serve 启动失败: {err}")
        return False
    print(f"  {GREEN}PASS{RESET}  serve 健康（{SERVE_BASE}/health）")

    # ① 后台发起 plan-execute run
    run_result: dict = {}

    def do_run(tag: str, body: dict) -> None:
        run_result[tag] = http_json(
            "POST", f"{SERVE_BASE}/agents/general/run", body
        )

    thread = threading.Thread(
        target=do_run,
        args=("initial", {"agent_type": "general", "goal": SCENARIO_L_GOAL,
                          "execution": {"mode": "plan_execute"}}),
        daemon=True,
    )
    thread.start()

    # ② 轮询注入窗口:本场景新增会话 + v1 计划在账 + ≥1 粒完成 + 无终态
    before = existing_planrun_sessions()
    sid: Optional[str] = None
    deadline = time.time() + POLL_TIMEOUT_SECS
    while time.time() < deadline:
        if not thread.is_alive():
            print(f"  {RED}FAIL{RESET}  run 在注入窗口前自行完成（无法注入）")
            kill_hard(serve.pid)
            return False
        sid = newest_planrun_session(exclude=before)
        if sid:
            journal_path = SESSIONS_DIR / f"{sid}.jsonl"
            if not journal_has_terminal(journal_path):
                events = journal_events(journal_path)
                if count_events(events, "plan_loop_checkpointed") >= 1 and count_events(
                    events, "node_checkpointed"
                ) >= 1:
                    break
            sid = None
        time.sleep(POLL_INTERVAL_SECS)
    else:
        kill_hard(serve.pid)
        print(f"  {RED}FAIL{RESET}  轮询超时未达注入窗口")
        return False
    print(f"  ···  注入窗口到达: 会话 {sid}")

    # ③ 强杀 serve 进程树(watchdog 拉起的脚本等价物=重启 serve)
    kill_hard(serve.pid)
    serve.wait(timeout=30)
    thread.join(timeout=10)
    print(f"  {GREEN}PASS{RESET}  serve 已强杀（{'taskkill /F' if os.name == 'nt' else 'kill -9'} 树杀）")

    serve = spawn_serve()
    err = wait_serve_health()
    if err:
        print(f"  {RED}FAIL{RESET}  serve 重启失败: {err}")
        return False
    print(f"  {GREEN}PASS{RESET}  serve 已重启（watchdog 拉起等价）")

    ok = True
    try:
        # ④ 扫尾:被杀 run 必须在列
        status, parsed, _ = http_json("GET", f"{SERVE_BASE}/api/sessions/resumable", timeout=30)
        listed = [
            r for r in (parsed or {}).get("resumable", [])
            if r.get("session_id") == sid
        ]
        ok = check(
            status == 200 and listed,
            "GET /api/sessions/resumable 列出被杀 run",
            f"被杀 run 未在扫尾列表: status={status} parsed={parsed}",
        ) and ok

        # ⑤ 显式恢复(人类持剑:续跑仅经 resume_session_id)
        status, parsed, raw = http_json(
            "POST", f"{SERVE_BASE}/agents/general/run",
            body={"agent_type": "general", "goal": SCENARIO_L_GOAL,
                  "execution": {"mode": "plan_execute"},
                  "resume_session_id": sid},
        )
        if evidence_dir is not None:
            evidence_dir.mkdir(parents=True, exist_ok=True)
            (evidence_dir / "scenarioL_resume_response.json").write_text(raw, encoding="utf-8")
        ok = check(
            status == 200 and (parsed or {}).get("success") is True,
            f"恢复跑 HTTP 200 且 success=true（status={status}）",
            f"恢复跑失败: status={status} body={raw[:300]}",
        ) and ok
        ok = check(
            (parsed or {}).get("run_session_id") == sid,
            "恢复跑续写同一 run 会话（run_session_id 透出）",
            f"会话不一致: {(parsed or {}).get('run_session_id')}",
        ) and ok
        ok = check(
            (parsed or {}).get("content", "") != "",
            "恢复跑产出非空 content",
            "恢复跑 content 为空",
        ) and ok

        # ⑥ 零重执行:合并账本每粒至多一条检查点
        merged = journal_events(SESSIONS_DIR / f"{sid}.jsonl")
        ckpt_nodes = checkpoint_node_ids(merged)
        duplicates = sorted({n for n in ckpt_nodes if ckpt_nodes.count(n) > 1})
        ok = check(
            not duplicates,
            f"零重执行:每粒至多一条检查点（共 {len(ckpt_nodes)} 条）",
            f"发现重复检查点粒: {duplicates}",
        ) and ok
    finally:
        kill_hard(serve.pid)
    return ok




# ===== 场景 M:react 会话 kill 注入 + 扫尾 + 显式恢复(悬挂工具处置面) =====

def newest_react_journal(exclude: set) -> Optional[str]:
    """最新非 planrun 的会话账本名(排除既有集合;无则 None)"""
    if not SESSIONS_DIR.exists():
        return None
    candidates = sorted(
        (p for p in SESSIONS_DIR.glob("*.jsonl") if not p.name.startswith("planrun-")
         and p.stem not in exclude),
        key=lambda p: p.stat().st_mtime,
    )
    if not candidates:
        return None
    return candidates[-1].stem


def scenario_m(env: Dict[str, str], evidence_dir: Optional[Path]) -> bool:
    """react 会话崩溃恢复:kill 注入 → 扫尾列表(kind=react)→ 显式恢复。

    断言四条：
      ① kill serve 进程树后重启,GET /api/sessions/resumable 列出该会话
         (kind=react,悬挂 turn 在账);
      ② 带 resume_session_id 重放(无 execution 字段=react 分支)→ HTTP 200
         且 success=true;
      ③ session_id 同会话(续写同一账本)且账内 session_resumed 在账;
      ④ content 非空。
    """
    print("\n=== 场景 M: react 会话 kill 注入 + 扫尾 + 显式恢复 ===")
    if not BINARY.exists():
        print(f"  {RED}FAIL{RESET}  编译产物缺失: {BINARY}")
        return False
    proc_env = {**os.environ, **env}
    evidence_dir = evidence_dir or REPO_ROOT / "data" / "e2e_evidence"
    evidence_dir.mkdir(parents=True, exist_ok=True)
    serve_port = 18093
    serve_base = f"http://127.0.0.1:{serve_port}"

    def spawn_serve() -> subprocess.Popen:
        out = open(evidence_dir / "scenarioM_serve_stderr.log", "a", encoding="utf-8")
        return subprocess.Popen(
            [str(BINARY), "serve", "--workdir", str(REPO_ROOT),
             "--host", "127.0.0.1", "--port", str(serve_port), "--no-auth"],
            cwd=str(REPO_ROOT), env=proc_env, stdout=out, stderr=out,
        )

    def wait_health(timeout_s: float = 60.0) -> Optional[str]:
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(f"{serve_base}/health", timeout=3) as resp:
                    if resp.status == 200:
                        return None
            except Exception:  # noqa: BLE001
                pass
            time.sleep(0.5)
        return f"serve health 未就绪（{timeout_s}s 超时）"

    serve = spawn_serve()
    err = wait_health()
    if err:
        kill_hard(serve.pid)
        print(f"  {RED}FAIL{RESET}  serve 启动失败: {err}")
        return False
    print(f"  {GREEN}PASS{RESET}  serve 健康（{serve_base}/health）")

    # ① 后台发起 react run(默认模式)。goal 强制工具轮(读文件——LLM 裸答
    # 不调工具的非确定性会让注入窗口失效;file_read 属幂等读,悬挂时恰好
    # 走重执行恢复路径)。
    react_goal = (
        "Use the file_read tool to read the file README.md in the workspace root, "
        "then answer: what is this project about? You MUST call file_read first."
    )
    before = {p.stem for p in SESSIONS_DIR.glob("*.jsonl")} if SESSIONS_DIR.exists() else set()
    # 基线快照=全部账本文件(react 会话是非 planrun 的数字命名,旧残留会话
    # 若不排除会被误认成本场景新会话)
    before = {p.stem for p in SESSIONS_DIR.glob("*.jsonl")} if SESSIONS_DIR.exists() else set()
    run_result: dict = {}

    def do_run() -> None:
        run_result["r"] = http_json(
            "POST", f"{serve_base}/agents/general/run",
            body={"agent_type": "general", "goal": react_goal},
        )

    thread = threading.Thread(target=do_run, daemon=True)
    thread.start()

    # ② 轮询注入窗口:新增 react 会话账本出现工具轮(≥1 tool_invoked)
    sid: Optional[str] = None
    deadline = time.time() + POLL_TIMEOUT_SECS
    while time.time() < deadline:
        if not thread.is_alive():
            print(f"  {RED}FAIL{RESET}  run 在注入窗口前自行完成（无法注入）")
            kill_hard(serve.pid)
            return False
        # 新 react 会话账本 = 非 planrun 且不在 run 前快照里的新文件
        candidates = []
        if SESSIONS_DIR.exists():
            for p in SESSIONS_DIR.glob("*.jsonl"):
                if p.name.startswith("planrun-") or p.stem in before:
                    continue
                candidates.append(p)
        candidates.sort(key=lambda p: p.stat().st_mtime)
        if candidates:
            journal_path = candidates[-1]
            events = journal_events(journal_path)
            if count_events(events, "tool_invoked") >= 1:
                sid = journal_path.stem
                break
        time.sleep(POLL_INTERVAL_SECS)
    else:
        kill_hard(serve.pid)
        print(f"  {RED}FAIL{RESET}  轮询超时未达注入窗口")
        return False
    print(f"  ···  注入窗口到达: react 会话 {sid}")

    # ②.5 前提落定:等 server 侧会话状态出现(崩溃早于首轮落账的会话无
    # server 状态,不可恢复——恢复面语义=显式指引重发)。注入窗口选在
    # server 状态落定之后,保证被杀会话处于「可恢复」象限
    state_deadline = time.time() + 300
    state_ok = False
    while time.time() < state_deadline:
        try:
            with urllib.request.urlopen(
                f"http://127.0.0.1:18080/api/sessions/{sid}/state", timeout=5
            ) as resp:
                if resp.status == 200:
                    state_ok = True
                    break
        except Exception:  # noqa: BLE001 — 未落定/连接问题均属轮询预期
            pass
        if not thread.is_alive():
            break
        time.sleep(POLL_INTERVAL_SECS)
    ok_premise = check(
        state_ok,
        "注入前提:server 侧会话状态已落定(state 200)",
        "server 侧状态 300s 未落定(会话处于不可恢复象限,场景前提不成立)",
    )
    if not ok_premise:
        kill_hard(serve.pid)
        return False

    # ③ 强杀 serve → 重启(watchdog 拉起等价)
    kill_hard(serve.pid)
    serve.wait(timeout=30)
    thread.join(timeout=10)
    print(f"  {GREEN}PASS{RESET}  serve 已强杀（{'taskkill /F' if os.name == 'nt' else 'kill -9'} 树杀）")
    serve = spawn_serve()
    err = wait_health()
    if err:
        print(f"  {RED}FAIL{RESET}  serve 重启失败: {err}")
        return False
    print(f"  {GREEN}PASS{RESET}  serve 已重启")

    ok = True
    try:
        # ④ 扫尾:该会话以 kind=react 列出
        status, parsed, _ = http_json("GET", f"{serve_base}/api/sessions/resumable", timeout=30)
        listed = [
            r for r in (parsed or {}).get("resumable", [])
            if r.get("session_id") == sid and r.get("kind") == "react"
        ]
        ok = check(
            status == 200 and listed,
            "扫尾列表列出被杀 react 会话(kind=react)",
            f"react 会话未在扫尾列表: status={status} parsed={parsed}",
        ) and ok

        # ⑤ 显式恢复(react 分支:无 execution 字段+resume_session_id)
        status, parsed, raw = http_json(
            "POST", f"{serve_base}/agents/general/run",
            body={"agent_type": "general", "goal": "",
                  "resume_session_id": sid},
        )
        if evidence_dir is not None:
            (evidence_dir / "scenarioM_resume_response.json").write_text(raw, encoding="utf-8")
        ok = check(
            status == 200 and (parsed or {}).get("success") is True,
            f"恢复跑 HTTP 200 且 success=true（status={status}）",
            f"恢复跑失败: status={status} body={raw[:300]}",
        ) and ok
        ok = check(
            (parsed or {}).get("session_id") == sid,
            "恢复跑续写同一会话",
            f"会话不一致: {(parsed or {}).get('session_id')}",
        ) and ok
        ok = check(
            (parsed or {}).get("content", "") != "",
            "恢复跑产出非空 content",
            "恢复跑 content 为空",
        ) and ok

        # ⑥ 账面:session_resumed 在账
        merged = journal_events(SESSIONS_DIR / f"{sid}.jsonl")
        ok = check(
            count_events(merged, "session_resumed") >= 1,
            "账内 session_resumed 在账(恢复标记)",
            "账内无恢复标记",
        ) and ok
    finally:
        kill_hard(serve.pid)
    return ok


def main() -> int:
    parser = argparse.ArgumentParser(description="plan-execute 中断恢复 E2E（kill 注入+恢复双跑）")
    parser.add_argument(
        "--evidence-dir",
        type=Path,
        default=None,
        help="stdout/stderr 证据落盘目录（可选）",
    )
    parser.add_argument(
        "--only",
        choices=["K", "L", "M"],
        default=None,
        help="只跑单个场景（调试用；缺省 K+L+M 全量）",
    )
    args = parser.parse_args()

    print("plan-execute 中断恢复 E2E（真 kill 注入 + 显式恢复双跑；跨平台终止原语双形态）")
    print(f"  时间: {time.strftime('%Y-%m-%d %H:%M:%S')}")
    print(f"  env:  {ENV_PATH}")

    env = load_env_file(ENV_PATH)
    if not env.get("MINIMAX_API_KEY"):
        print(f"  {RED}FAIL{RESET}  .env 缺 MINIMAX_API_KEY")
        return 1
    print(f"  {GREEN}PASS{RESET}  MINIMAX_API_KEY 存在（前缀 {env['MINIMAX_API_KEY'][:5]}...）")
    if not SESSIONS_DIR.exists():
        print(f"  ···  账本目录不存在，首跑将创建: {SESSIONS_DIR}")

    ok = True
    if args.only in (None, "K"):
        ok = scenario_k(env, args.evidence_dir)
    if args.only in (None, "L") and (ok or args.only in ("L", "M")):
        ok = scenario_l(env, args.evidence_dir) and ok
    if args.only in (None, "M") and (ok or args.only == "M"):
        ok = scenario_m(env, args.evidence_dir) and ok
    print(f"\n{'=' * 60}")
    print(f"结果: {'ALL PASS' if ok else 'FAILED'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
