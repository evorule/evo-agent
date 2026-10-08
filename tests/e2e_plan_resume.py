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
import os
import re
import subprocess
import sys
import time
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


def newest_planrun_session() -> Optional[str]:
    """最新 planrun-*.jsonl 的会话名（无则 None）"""
    if not SESSIONS_DIR.exists():
        return None
    candidates = sorted(
        (p for p in SESSIONS_DIR.glob("planrun-*.jsonl")),
        key=lambda p: p.stat().st_mtime,
    )
    if not candidates:
        return None
    return candidates[-1].stem


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

    # ① 首跑(后台):与场景 A 同一 plan-execute 工作流
    proc = subprocess.Popen(
        [str(BINARY), "workflow", WORKFLOW_ID, "--plan-execute"],
        cwd=str(REPO_ROOT),
        env=proc_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    print(f"  ···  首跑已启动 pid={proc.pid}（等待执行中段窗口后强杀）")

    # ② 轮询账本:v1 计划已物化 + ≥2 粒完成
    sid: Optional[str] = None
    deadline = time.time() + POLL_TIMEOUT_SECS
    while time.time() < deadline:
        if proc.poll() is not None:
            print(f"  {RED}FAIL{RESET}  首跑在注入前自行退出（exit={proc.returncode}）")
            return False
        sid = newest_planrun_session()
        if sid:
            events = journal_events(SESSIONS_DIR / f"{sid}.jsonl")
            if count_events(events, "plan_loop_checkpointed") >= 1 and count_events(
                events, "node_checkpointed"
            ) >= 2:
                break
        time.sleep(POLL_INTERVAL_SECS)
    else:
        kill_hard(proc.pid)
        print(f"  {RED}FAIL{RESET}  轮询超时（{POLL_TIMEOUT_SECS}s）未达注入窗口，已强杀")
        return False
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


def main() -> int:
    parser = argparse.ArgumentParser(description="plan-execute 中断恢复 E2E（kill 注入+恢复双跑）")
    parser.add_argument(
        "--evidence-dir",
        type=Path,
        default=None,
        help="stdout/stderr 证据落盘目录（可选）",
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

    ok = scenario_k(env, args.evidence_dir)
    print(f"\n{'=' * 60}")
    print(f"结果: {'ALL PASS' if ok else 'FAILED'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
