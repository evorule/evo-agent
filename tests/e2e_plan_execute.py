# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

"""plan-execute 真实 LLM E2E 测试（交付物 9 IT 级用例；Phase 1-B 场景 A/B + Phase 2 场景 C/D）

验证 plan-execute 外层驱动四条真实链路（真实 MiniMax LLM API + 运行中的 evorule-server）：

  场景 A（PlanExecute 全链路）：planning probe（planner 单节点 DAG）→ LLM 产出
    PlanFact v1 → 物化 → 执行 researcher 节点 → 产出研究摘要。
    断言：EXIT=0、stdout 含 "plan v1 materialized"、统计行 plan_versions=1
    replans=0、tokens_used>0（Phase 2 交付物 7 埋点真实验证）。

  场景 B（replan 触发链路）：Dsl v1 含 ghost_agent（不存在的 agent_type）节点
    → 节点失败 → should_replan Failure → planner 产出 PlanFact v2 → 物化重跑成功。
    断言：EXIT=0、stdout 含 "replan materialized"、统计行 plan_versions=2
    replans=1、replan_tokens>0（v2+ 成本埋点）。

  场景 C（D-01 enforce 终止，Phase 2 交付物 6）：Dsl 含 probe_violator 节点
    （model 非白名单）→ TCB 约束前置门产生 Violation 事实 → runner 固定前缀
    上抛 → 外层驱动判别后终止整个循环且不 replan（§9.5.1 选项 B）。
    断言：EXIT=1、stderr 含 "halted by enforce violation" 与
    "enforce violation: rule_index="、全程无 "replan materialized"。

  场景 D（replan 硬上限防抖终态）：Dsl warmup（合规 researcher 成功）→ boom
    （ghost_agent 必失败），--max-replan 0 → should_replan 判定序第 1 步硬上限
    直接终止、显式传播 Err。
    断言：EXIT=1、stderr 含 "replan budget exhausted"、无 "replan materialized"。

# 前置（本脚本不进 CI——依赖真实 LLM key/运行中 server/已编译产物）
  1. .env 含 MINIMAX_API_KEY（O-095：evo-agent 只认进程环境变量，脚本负责注入）
  2. evorule-server 运行于 evo-agent.toml base_url（默认 http://127.0.0.1:18080），
     且其 TCB 约束前置门在位（BUG-P0-005 修复后版本，场景 C 依赖）
  3. cargo build 已产出 target/debug/evo-agent.exe

# 运行
  cd <repo-root>(evo-agent 仓库根目录)
  python tests/e2e_plan_execute.py [--evidence-dir <目录>]

  --evidence-dir 指定后，四场景 stdout/stderr 落盘该目录（核销证据留痕用）。
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

# 场景统计行格式（cmd_workflow 汇总输出，stderr；Phase 2 扩展 3 个埋点字段）
STATS_RE = re.compile(
    r"=== workflow '(\S+)' done \(plan_versions=(\d+) replans=(\d+) "
    r"nodes_executed=(\d+) wall_ms=(\d+) repeated_nodes=(\d+) "
    r"tokens_used=(\d+) replan_tokens=(\d+)\) ==="
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
    tag: str,
    args: list,
    env: Dict[str, str],
    evidence_dir: Optional[Path],
    expect_exit: int = 0,
) -> tuple:
    """跑一个 workflow 场景，返回 (proc, ok)——场景专属断言由调用方做"""
    print(f"\n=== 场景 {name} ===")
    if not BINARY.exists():
        print(f"  {RED}FAIL{RESET}  编译产物缺失: {BINARY}")
        return None, False

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
        (evidence_dir / f"{tag}_stdout.txt").write_text(proc.stdout, encoding="utf-8")
        (evidence_dir / f"{tag}_stderr.txt").write_text(proc.stderr, encoding="utf-8")
        print(f"  {YELLOW}···{RESET}  证据落盘 {evidence_dir}/{tag}_*.txt")

    # 断言：退出码
    if proc.returncode != expect_exit:
        print(f"  {RED}FAIL{RESET}  EXIT={proc.returncode}（预期 {expect_exit}）")
        print(f"    stderr 尾部: {proc.stderr[-500:]}")
        return proc, False
    print(f"  {GREEN}PASS{RESET}  EXIT={expect_exit}（{duration:.1f}s）")
    return proc, True


def assert_stats_line(proc, label: str) -> Optional[re.Match]:
    """断言 stderr 含 Phase 2 统计行并打印埋点值；返回 match 或 None"""
    m = STATS_RE.search(proc.stderr)
    if not m:
        print(f"  {RED}FAIL{RESET}  stderr 未找到统计行（{label}）")
        print(f"    stderr 尾部: {proc.stderr[-500:]}")
        return None
    (
        wf_id, versions, replans, nodes, wall_ms,
        repeated, tokens, replan_tokens,
    ) = m.groups()
    print(
        f"  {YELLOW}···{RESET}  统计 plan_versions={versions} replans={replans}"
        f" nodes_executed={nodes} wall_ms={wall_ms} repeated_nodes={repeated}"
        f" tokens_used={tokens} replan_tokens={replan_tokens}"
    )
    return m


def check(cond: bool, ok_msg: str, fail_msg: str) -> bool:
    print(f"  {GREEN}PASS{RESET}  {ok_msg}" if cond else f"  {RED}FAIL{RESET}  {fail_msg}")
    return cond


def scenario_a(env: Dict[str, str], evidence_dir: Optional[Path]) -> bool:
    proc, ok = run_scenario(
        "A: PlanExecute 全链路（probe→PlanFact v1→物化→执行）",
        "scenarioA", ["research_plan", "--plan-execute"], env, evidence_dir,
    )
    if proc is None:
        return False
    m = assert_stats_line(proc, "A")
    ok = ok and m is not None
    if m:
        _, versions, replans, nodes, _, repeated, tokens, _ = m.groups()
        ok = ok and check(
            (versions, replans) == ("1", "0"),
            "plan_versions=1 replans=0", f"预期 plan_versions=1 replans=0，实得 {versions}/{replans}",
        )
        ok = ok and check(
            int(nodes) >= 1, "nodes_executed>=1", "nodes_executed=0（应有节点完成）",
        )
        ok = ok and check(
            int(tokens) > 0, "tokens_used>0（交付物 7 埋点真实生效）", "tokens_used=0（埋点未生效）",
        )
        ok = ok and check(
            int(repeated) == 0, "repeated_nodes=0", "repeated_nodes 非 0（首版不应有重复）",
        )
    ok = ok and check(
        "plan v1 materialized" in proc.stdout,
        "stdout 含 'plan v1 materialized'", "stdout 缺 'plan v1 materialized'",
    )
    return ok


def scenario_b(env: Dict[str, str], evidence_dir: Optional[Path]) -> bool:
    proc, ok = run_scenario(
        "B: replan 触发链路（v1 失败→Failure replan→v2 成功）",
        "scenarioB", ["replan_drill"], env, evidence_dir,
    )
    if proc is None:
        return False
    m = assert_stats_line(proc, "B")
    ok = ok and m is not None
    if m:
        _, versions, replans, _, _, _, _, replan_tokens = m.groups()
        ok = ok and check(
            (versions, replans) == ("2", "1"),
            "plan_versions=2 replans=1", f"预期 plan_versions=2 replans=1，实得 {versions}/{replans}",
        )
        ok = ok and check(
            int(replan_tokens) > 0,
            "replan_tokens>0（v2+ 版本成本埋点真实生效）",
            "replan_tokens=0（replan 成本埋点未生效）",
        )
    ok = ok and check(
        "replan materialized, re-executing" in proc.stdout,
        "stdout 含 'replan materialized, re-executing'",
        "stdout 缺 'replan materialized, re-executing'",
    )
    ok = ok and check(
        "ghost_agent" in proc.stdout,
        "stdout 含 v1 失败原因（ghost_agent）", "stdout 缺 v1 失败原因（ghost_agent）",
    )
    return ok


def scenario_c(env: Dict[str, str], evidence_dir: Optional[Path]) -> bool:
    proc, ok = run_scenario(
        "C: D-01 enforce 终止（Violation→固定前缀→driver 判别→终止不 replan）",
        "scenarioC", ["enforce_drill"], env, evidence_dir, expect_exit=1,
    )
    if proc is None:
        return False
    combined = proc.stdout + proc.stderr
    ok = ok and check(
        "halted by enforce violation" in combined,
        "含 'halted by enforce violation'（driver 判别终止）",
        "缺 'halted by enforce violation'（driver 未按 D-01 终止）",
    )
    ok = ok and check(
        "enforce violation: rule_index=" in combined,
        "含 'enforce violation: rule_index='（runner 固定前缀上抛）",
        "缺 'enforce violation: rule_index='（前缀契约未满足）",
    )
    ok = ok and check(
        "replan materialized" not in combined,
        "全程无 replan（§9.5.1-B：不开「换计划再试」通道）",
        "出现了 replan（违反 D-01 一票否决）",
    )
    return ok


def scenario_d(env: Dict[str, str], evidence_dir: Optional[Path]) -> bool:
    proc, ok = run_scenario(
        "D: replan 硬上限防抖终态（--max-replan 0→判定序第 1 步终止）",
        "scenarioD", ["ok_then_fail_drill", "--max-replan", "0"], env, evidence_dir,
        expect_exit=1,
    )
    if proc is None:
        return False
    combined = proc.stdout + proc.stderr
    ok = ok and check(
        "replan budget exhausted" in combined,
        "含 'replan budget exhausted'（硬上限显式传播 Err）",
        "缺 'replan budget exhausted'（硬上限未按判定序第 1 步终止）",
    )
    ok = ok and check(
        "replan materialized" not in combined,
        "全程无 replan（max_replan=0 拦截成功）",
        "出现了 replan（硬上限失效）",
    )
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

    print("plan-execute 真实 LLM E2E 测试（IT 级；Phase 1-B A/B + Phase 2 C/D）")
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

    ok_a = scenario_a(env, args.evidence_dir)
    ok_b = scenario_b(env, args.evidence_dir)
    ok_c = scenario_c(env, args.evidence_dir)
    ok_d = scenario_d(env, args.evidence_dir)

    total = ok_a and ok_b and ok_c and ok_d
    print(f"\n{'=' * 60}")
    print(f"结果: {'ALL PASS' if total else 'FAILED'}  (A={ok_a} B={ok_b} C={ok_c} D={ok_d})")
    print(f"{'=' * 60}")
    return 0 if total else 1


if __name__ == "__main__":
    sys.exit(main())
