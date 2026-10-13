#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# =============================================================================
# check_model_allowlist_sync.py — 会话模型准入白名单「双副本」同步守卫
#
# 背景（SG-0 件2③，对应 O-385 BOM 教训）：
#   00_constraint_model_allowlist.json 是 00_constraint_* 治理规则资产，存在两份：
#     - 正本：evo-agent/rules/governance/            （git 跟踪，评审与发布走这条）
#     - 部署副本：evorule-server/data/agent-governance/rules/
#                 （server 运行时资产，.gitignore 管辖，不入仓）
#   两份都是「真实存在且能读」的独立文件，没有任何机制保证它们不漂移：
#   改了一份忘了另一份，不会报错、不会告警，只在规则真正拦截模型调用时才暴露。
#   O-385 的 BOM 事故就是这个形状 —— 双份物料各改各的，对账时才发现不一致。
#
# 本守卫做三件事（全部显式断言，不做「没报错就算过」）：
#   1. 正本存在 + 是合法 JSON + 具备规则资产必备形状（kind/id/version/transform）
#   2. 部署副本存在
#   3. 两份逐字节一致（sha256 对账）
#
# 用法：
#   python scripts/check_model_allowlist_sync.py
#       # 严格模式（默认）：副本缺失 = 红。这是本机开发环境应有的姿态。
#   python scripts/check_model_allowlist_sync.py --allow-missing-peer
#       # 副本不在（CI 全新检出等）时只校验正本；副本在则仍强制对账。
#   python scripts/check_model_allowlist_sync.py --peer <path>
#       # 显式指定副本路径
#   python scripts/check_model_allowlist_sync.py --json
#       # JSON 输出（机器消费）
#
# 退出码：
#   0 = 对账一致（或按 --allow-missing-peer 只校验了正本）
#   1 = 漂移 / 正本形状非法 / 副本缺失（严格模式）
#   2 = 环境错误（路径不可读等）
# =============================================================================

import argparse
import hashlib
import json
import sys
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

REPO_ROOT = Path(__file__).resolve().parent.parent

CANONICAL_REL = "rules/governance/00_constraint_model_allowlist.json"
DEFAULT_PEER_REL = "../evorule-server/data/agent-governance/rules/00_constraint_model_allowlist.json"

# 规则资产必备形状（00_constraint_* 家族共有）
REQUIRED_TOP_KEYS = ("kind", "id", "version", "transform")


def sha256_of(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read_json(path: Path) -> Any:
    # 显式 utf-8：GBK 控制台下 cp1252/默认编码会把中文 description 解成乱码（PS 5.1 坑）
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def check_shape(doc: Any, label: str) -> Optional[str]:
    """返回 None = 形状合格；否则返回人类可读的错误原因。"""
    if not isinstance(doc, dict):
        return f"{label}: 顶层不是 JSON 对象（实际 {type(doc).__name__}）"
    missing = [k for k in REQUIRED_TOP_KEYS if k not in doc]
    if missing:
        return f"{label}: 缺少必备键 {missing}（现有键 {sorted(doc.keys())}）"
    if doc.get("kind") != "rule_set":
        return f"{label}: kind 应为 'rule_set'，实际 {doc.get('kind')!r}"
    if not isinstance(doc.get("transform"), list) or not doc["transform"]:
        return f"{label}: transform 必须是非空数组（规则集无任何规则 = 空壳）"
    for idx, rule in enumerate(doc["transform"]):
        if not isinstance(rule, dict) or "type" not in rule:
            return f"{label}: transform[{idx}] 缺少 'type' 字段"
    return None


def describe_rules(doc: Dict[str, Any]) -> str:
    """把规则集压成一行指纹，便于两侧对比时肉眼定位漂移。"""
    parts = []
    for rule in doc.get("transform", []):
        if isinstance(rule, dict):
            params = rule.get("params", {}) or {}
            domain = params.get("domain", {})
            try:
                parts.append(f"{rule.get('type')}:{json.dumps(domain, ensure_ascii=False, sort_keys=True)}")
            except (TypeError, ValueError):
                parts.append(f"{rule.get('type')}:<unserializable>")
    return " | ".join(parts)


def main() -> int:
    ap = argparse.ArgumentParser(description="模型准入白名单双副本同步守卫（哈希对账）")
    ap.add_argument("--peer", default=None, help="部署副本路径（默认取 ../evorule-server/...）")
    ap.add_argument(
        "--allow-missing-peer",
        action="store_true",
        help="副本不存在时只校验正本并 exit 0（CI 全新检出场景）",
    )
    ap.add_argument("--json", action="store_true", help="JSON 输出")
    args = ap.parse_args()

    canonical = REPO_ROOT / CANONICAL_REL
    peer = Path(args.peer).resolve() if args.peer else (REPO_ROOT / DEFAULT_PEER_REL).resolve()

    result: Dict[str, Any] = {
        "canonical": str(canonical),
        "peer": str(peer),
        "canonical_sha256": None,
        "peer_sha256": None,
        "in_sync": False,
        "errors": [],
        "peer_present": peer.is_file(),
    }

    # ---- 1. 正本 ----
    if not canonical.is_file():
        result["errors"].append(f"正本缺失: {canonical}")
        emit(result, args.json)
        return 1
    try:
        result["canonical_sha256"] = sha256_of(canonical)
        canonical_doc = read_json(canonical)
    except (OSError, json.JSONDecodeError) as exc:
        result["errors"].append(f"正本不可读或非法 JSON: {exc}")
        emit(result, args.json)
        return 2

    shape_err = check_shape(canonical_doc, "正本")
    if shape_err:
        result["errors"].append(shape_err)
        emit(result, args.json)
        return 1

    # ---- 2. 副本 ----
    if not peer.is_file():
        msg = f"部署副本缺失: {peer}"
        if args.allow_missing_peer:
            # 显式声明的例外：不是「静默通过」，输出里留痕
            result["errors"].append(msg + "（--allow-missing-peer：本次未对账）")
            result["in_sync"] = None
            emit(result, args.json)
            return 0
        result["errors"].append(msg)
        emit(result, args.json)
        return 1

    try:
        result["peer_sha256"] = sha256_of(peer)
        peer_doc = read_json(peer)
    except (OSError, json.JSONDecodeError) as exc:
        result["errors"].append(f"副本不可读或非法 JSON: {exc}")
        emit(result, args.json)
        return 2

    peer_shape_err = check_shape(peer_doc, "副本")
    if peer_shape_err:
        result["errors"].append(peer_shape_err)
        emit(result, args.json)
        return 1

    # ---- 3. 对账 ----
    if result["canonical_sha256"] == result["peer_sha256"]:
        result["in_sync"] = True
        emit(result, args.json)
        return 0

    result["errors"].append("双副本漂移：sha256 不一致")
    result["canonical_fingerprint"] = describe_rules(canonical_doc)
    result["peer_fingerprint"] = describe_rules(peer_doc)
    emit(result, args.json)
    return 1


def emit(result: Dict[str, Any], as_json: bool) -> None:
    if as_json:
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return

    print("=" * 74)
    print("模型准入白名单双副本同步守卫")
    print("=" * 74)
    print(f"正本     : {result['canonical']}")
    print(f"副本     : {result['peer']}")
    print(f"副本在位 : {'是' if result['peer_present'] else '否'}")
    if result["canonical_sha256"]:
        print(f"正本 sha256: {result['canonical_sha256']}")
    if result["peer_sha256"]:
        print(f"副本 sha256: {result['peer_sha256']}")

    if result["in_sync"] is True:
        print("\n结论: PASS —— 两份逐字节一致。")
    elif result["in_sync"] is None:
        print("\n结论: SKIPPED —— 仅校验了正本（副本不在位）。")
    else:
        print("\n结论: FAIL ——")
        for err in result["errors"]:
            print(f"  - {err}")
        if "canonical_fingerprint" in result:
            print("\n正本规则指纹:")
            print(f"  {result['canonical_fingerprint']}")
            print("副本规则指纹:")
            print(f"  {result['peer_fingerprint']}")
            print("\n修法：以正本为准修副本（部署副本由 gitignore 管辖，不入仓）：")
            print(f"  copy \"{result['canonical']}\" \"{result['peer']}\"")
            print("改完重跑本守卫确认转绿。")
    print("=" * 74)


if __name__ == "__main__":
    sys.exit(main())