# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

"""plan-execute 真实 LLM E2E 测试（Phase 1-B 交付物 9 IT 级用例）

验证 plan-execute 外层驱动两条真实链路（真实 MiniMax LLM API + 运行中的 evorule-server）：

  场景 A（PlanExecute 全链路）：planning probe（planner 单节点 DAG）→ LLM 产出
    PlanFact v1 → 物化 → 执行 researcher 节点 → 产出研究摘要。
    断言：EXIT=0、stdout 含 "plan v1 materialized"、stderr 含 plan_versions=1 replans=0。

  场景 B（replan 触发链路）：Dsl v1 含 ghost_agent（不存在的 agent_type）节点
    → 节点失败 → should_replan Failure → planner 产出 PlanFact v2 → 物化重跑成功。
    断言：EXIT=0、stdout 含 "replan materialized"、stderr 含 plan_versions=2 replans=1。

# 前置（本脚本不进 CI——依赖真实 LLM key/运行中 server/已编译产物）
  1. .env 含 MINIMAX_API_KEY（O-095：evo-agent 只认进程环境变量，脚本负责注入）
  2. evorule-server 运行于 evo-agent.toml base_url（默认 http://127.0.0.1:18080）
  3. cargo build 已产出 target/debug/evo-agent.exe

# 运行
  cd <repo-root>(evo-agent 仓库根目录)
  python tests/e2e_plan_execute.py [--evidence-dir <目录>]

  --evidence-dir 指定后，两场景 stdout/stderr 落盘该目录（核销证据留痕用）。
"""

import argparse
import os
import re
import subprocess
import sys
import time
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
DEFAULT_SERVER = "http://127.0.0.1:18080"

# 场景统计行格式（cmd_workflow 汇总输出，stderr）
STATS_RE = re.compile(
    r"=== workflow '(\S+)' done \(plan_versions=(\d+) replans=(\d+) "
    r"nodes_executed=(\d+) wall_ms=(\d+)\) ==="
)


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


def check_server(base_url: str) -> Optional[str]:
    """健康检查 evorule-server；返回 None=OK，否则返回错误文本"""
    try:
        with urllib.request.urlopen(f"{base_url}/api/health", timeout=5) as resp:
            if resp.status == 200:
                return None
            return f"HTTP {resp.status}"
    except Exception as e:  # noqa: BLE001 — 诊断用途，原样呈现
        return str(e)


def read_server_url() -> str:
    """从 evo-agent.toml 读 base_url（极简解析，缺省 18080）"""
    toml = REPO_ROOT / "evo-agent.toml"
    if toml.exists():
        m = re.search(r'base_url\s*=\s*"([^"]+)"', toml.read_text(encoding="utf-8"))
        if m:
            return m.group(1)
    return DEFAULT_SERVER


def run_scenario(
    name: str,
    args: list,
    env: Dict[str, str],
    evidence_dir: Optional[Path],
) -> bool:
    """跑一个 workflow 场景并断言结果"""
    print(f"\n=== 场景 {name} ===")
    if not BINARY.exists():
        print(f"  {RED}FAIL{RESET}  编译产物缺失: {BINARY}")
        return False

    # O-095：evo-agent 只认进程环境变量，不自动加载 .env——此处注入
    proc_env = {**os.environ, **env}

    start = time.time()
    proc = subprocess.run(
        [str(BINARY), "workflow", *args],
        cwd=str(REPO_ROOT),
        env=proc_env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=900,
    )
    duration = time.time() - start

    if evidence_dir is not None:
        evidence_dir.mkdir(parents=True, exist_ok=True)
        tag = "scenarioA" if "--plan-execute" in args else "scenarioB"
        (evidence_dir / f"{tag}_stdout.txt").write_text(proc.stdout, encoding="utf-8")
        (evidence_dir / f"{tag}_stderr.txt").write_text(proc.stderr, encoding="utf-8")
        print(f"  {YELLOW}···{RESET}  证据落盘 {evidence_dir}/{tag}_*.txt")

    ok = True

    # 断言 1：退出码
    if proc.returncode != 0:
        print(f"  {RED}FAIL{RESET}  EXIT={proc.returncode}（预期 0）")
        print(f"    stderr 尾部: {proc.stderr[-500:]}")
        ok = False
    else:
        print(f"  {GREEN}PASS{RESET}  EXIT=0（{duration:.1f}s）")

    # 断言 2：stderr 统计行
    m = STATS_RE.search(proc.stderr)
    if not m:
        print(f"  {RED}FAIL{RESET}  stderr 未找到统计行")
        print(f"    stderr 尾部: {proc.stderr[-500:]}")
        return False
    wf_id, versions, replans, nodes, wall_ms = m.groups()
    print(
        f"  {YELLOW}···{RESET}  统计 plan_versions={versions} replans={replans}"
        f" nodes_executed={nodes} wall_ms={wall_ms}"
    )

    # 断言 3：场景专属链路证据
    if "--plan-execute" in args:
        # 场景 A：probe 物化 v1 + 单版无 replan
        if "plan v1 materialized" not in proc.stdout:
            print(f"  {RED}FAIL{RESET}  stdout 缺 'plan v1 materialized'")
            ok = False
        else:
            print(f"  {GREEN}PASS{RESET}  stdout 含 'plan v1 materialized'")
        if (versions, replans) != ("1", "0"):
            print(f"  {RED}FAIL{RESET}  预期 plan_versions=1 replans=0")
            ok = False
        else:
            print(f"  {GREEN}PASS{RESET}  plan_versions=1 replans=0")
        if int(nodes) < 1:
            print(f"  {RED}FAIL{RESET}  nodes_executed=0（应有节点完成）")
            ok = False
    else:
        # 场景 B：v1 失败 → replan v2 → 成功
        if "replan materialized, re-executing" not in proc.stdout:
            print(f"  {RED}FAIL{RESET}  stdout 缺 'replan materialized, re-executing'")
            ok = False
        else:
            print(f"  {GREEN}PASS{RESET}  stdout 含 'replan materialized, re-executing'")
        if "ghost_agent" not in proc.stdout:
            print(f"  {RED}FAIL{RESET}  stdout 缺 v1 失败原因（ghost_agent）")
            ok = False
        else:
            print(f"  {GREEN}PASS{RESET}  stdout 含 v1 失败原因（ghost_agent）")
        if (versions, replans) != ("2", "1"):
            print(f"  {RED}FAIL{RESET}  预期 plan_versions=2 replans=1，实得 {versions}/{replans}")
            ok = False
        else:
            print(f"  {GREEN}PASS{RESET}  plan_versions=2 replans=1")

    return ok


def main() -> int:
    parser = argparse.ArgumentParser(description="plan-execute 真实 LLM E2E")
    parser.add_argument(
        "--evidence-dir",
        type=Path,
        default=None,
        help="stdout/stderr 证据落盘目录（可选）",
    )
    args = parser.parse_args()

    print("plan-execute 真实 LLM E2E 测试（Phase 1-B IT 级）")
    print(f"  时间: {time.strftime('%Y-%m-%d %H:%M:%S')}")
    print(f"  env:  {ENV_PATH}")

    env = load_env_file(ENV_PATH)
    if not env.get("MINIMAX_API_KEY"):
        print(f"  {RED}FAIL{RESET}  .env 缺 MINIMAX_API_KEY")
        return 1
    print(f"  {GREEN}PASS{RESET}  MINIMAX_API_KEY 存在（前缀 {env['MINIMAX_API_KEY'][:5]}...）")

    server_url = read_server_url()
    err = check_server(server_url)
    if err:
        print(f"  {RED}FAIL{RESET}  evorule-server 不可达（{server_url}）: {err}")
        print("            请先启动 server（本脚本不负责拉起）")
        return 1
    print(f"  {GREEN}PASS{RESET}  evorule-server 健康（{server_url}/api/health）")

    ok_a = run_scenario(
        "A: PlanExecute 全链路（probe→PlanFact v1→物化→执行）",
        ["research_plan", "--plan-execute"],
        env,
        args.evidence_dir,
    )
    ok_b = run_scenario(
        "B: replan 触发链路（v1 失败→Failure replan→v2 成功）",
        ["replan_drill"],
        env,
        args.evidence_dir,
    )

    total = ok_a and ok_b
    print(f"\n{'=' * 60}")
    print(f"结果: {'ALL PASS' if total else 'FAILED'}")
    print(f"{'=' * 60}")
    return 0 if total else 1


if __name__ == "__main__":
    sys.exit(main())
