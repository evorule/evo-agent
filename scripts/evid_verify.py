#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""EVID 零依赖独立验证器——evo-agent 证据链的离线校验工具。

只依赖 Python 标准库 + blake3（pip install blake3）。
不需要安装 EvoRule / evo-agent / evorule-server。

三层验证（各司其职）：
  链完整性   逐条 evid_hash 重算 + previous_evid 链式引用 + seq 连续
  链根一致性 chain_root = 末条 evid_hash 重算 + chain_length 一致
  链根签名   Ed25519 验签（root_hash:chain_length 载荷，长度防移植）

用法：
  python evid_verify.py evidence.json                        # 验链
  python evid_verify.py evidence.json --pubkey <64hex>       # 验链 + 验签
  python evid_verify.py --self-test                          # 验证器自检
  python evid_verify.py --vectors evid_golden_vectors.json   # 跨语言黄金向量对拍

退出码：0=全绿 / 1=链破坏（含 chain_substituted）/ 2=签名无效 / 3=输入错误
"""

import argparse
import json
import sys

try:
    from blake3 import blake3
except ImportError:
    print("ERROR: pip install blake3", file=sys.stderr)
    sys.exit(3)

GENESIS = ""
EVID_GENESIS_ROOT = "blake3:" + blake3(b"evid_genesis").hexdigest()
CAUSAL_KEYS = ("seq", "step_id", "fact_type", "cause",
               "previous_evid", "fact_hash", "domain_check")


# ═══════════════════════════════════════════════════════════════════════
# 哈希契约（与 Rust evid_hash 同源——跨语言黄金向量锁定）
# ═══════════════════════════════════════════════════════════════════════

def canonical_evid_json(causal: dict) -> str:
    """7 字段因果元组 → canonical JSON 数组（字节级确定性）。"""
    arr = [
        str(causal["seq"]),
        causal["step_id"],
        causal["fact_type"],
        causal["cause"],           # null or number
        causal["previous_evid"],
        causal["fact_hash"],
        causal["domain_check"],    # null or string
    ]
    return json.dumps(arr, ensure_ascii=False, separators=(",", ":"))


def evid_hash(causal: dict) -> str:
    canonical = canonical_evid_json(causal)
    return "blake3:" + blake3(canonical.encode("utf-8")).hexdigest()


# ═══════════════════════════════════════════════════════════════════════
# 链验证
# ═══════════════════════════════════════════════════════════════════════

def verify_chain(doc: dict) -> tuple[bool, list[tuple[str, str]]]:
    """验证 EVID 链完整性。返回 (是否通过, [(篡改分类, 失败描述)])。

    分类：field_modified / record_deleted / record_inserted /
          tail_truncated / chain_root_mismatch / chain_length_mismatch / seq_gap
    """
    records = doc.get("records", [])
    failures: list[tuple[str, str]] = []
    prev_evid = GENESIS

    for i, rec in enumerate(records):
        seq = rec.get("seq", 0)
        if seq != i + 1:
            # 记录但不 break——精确分类靠下方链式引用检查
            # （删记录/插记录都会表现为 seq 错位，链式引用才能区分）
            failures.append(("seq_gap", f"expected {i + 1}, found {seq} at record[{i}]"))

        # 重算 evid_hash（只用因果字段——排除 evid_hash/timestamp 展示字段）
        causal = {k: rec[k] for k in CAUSAL_KEYS}
        recomputed = evid_hash(causal)
        stored = rec.get("evid_hash", "")

        if recomputed != stored:
            # 记录自身哈希失配：字段被改（无论改的是因果字段还是存储哈希值）
            failures.append(
                ("field_modified", f"at seq {seq}: evid_hash 重算失配"))
            break

        # previous_evid 链式引用检查
        expected_prev = records[i - 1].get("evid_hash", GENESIS) if i > 0 else GENESIS
        if causal["previous_evid"] != expected_prev:
            stored_prev = causal["previous_evid"]
            if stored_prev in {r.get("evid_hash") for r in records}:
                # 引用了链上存在但非紧邻前驱的哈希 → 外来记录/分叉
                failures.append(
                    ("record_inserted",
                     f"at seq {seq}: previous_evid 指向非紧邻前驱（外来记录或分叉）"))
            else:
                # 引用了链上不存在的哈希 → 前驱记录被删（悬空引用）
                failures.append(
                    ("record_deleted",
                     f"at seq {seq}: previous_evid 悬空（引用的记录不在链上）"))
            break

        prev_evid = stored

    # 链长度验证（声明 > 实际 = 删尾——根失配是删尾的必然后果，不重复报）
    declared_length = doc.get("chain_length", 0)
    if declared_length > len(records):
        failures.append(
            ("tail_truncated",
             f"declared={declared_length}, actual={len(records)}（尾部记录被删）"))
        return (False, failures)

    # 链根验证（长度一致时根必重算对齐——失配即替换/篡改）
    root = doc.get("chain_root", "")
    expected_root = prev_evid if records else EVID_GENESIS_ROOT
    if root != expected_root:
        failures.append(
            ("chain_root_mismatch",
             f"doc={root[:20]}… expected={expected_root[:20]}…"))

    if declared_length != len(records):
        failures.append(
            ("chain_length_mismatch",
             f"declared={declared_length}, actual={len(records)}"))

    return (len(failures) == 0, failures)


# ═══════════════════════════════════════════════════════════════════════
# 签名验证（Ed25519——cryptography 或 PyNaCl 二者其一）
# ═══════════════════════════════════════════════════════════════════════

def _ed25519_verify(pubkey_hex: str, payload: bytes, signature_hex: str) -> tuple[bool, str]:
    """Ed25519 验签（cryptography 优先，PyNaCl 回落）。"""
    try:
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
        pk = Ed25519PublicKey.from_public_bytes(bytes.fromhex(pubkey_hex))
        pk.verify(bytes.fromhex(signature_hex), payload)
        return True, ""
    except ImportError:
        try:
            import nacl.signing
            verify_key = nacl.signing.VerifyKey(bytes.fromhex(pubkey_hex))
            verify_key.verify(payload, bytes.fromhex(signature_hex))
            return True, ""
        except ImportError:
            return False, "需要 pip install cryptography 或 pynacl 进行 Ed25519 验签"
        except Exception as e:  # noqa: BLE001 — 验签失败原因原样透出
            return False, f"signature verify failed: {e}"
    except Exception as e:  # noqa: BLE001 — 验签失败原因原样透出
        return False, f"signature verify failed: {e}"


def verify_signature(doc: dict, pubkey_hex: str) -> tuple[bool, str]:
    """验证链根签名（Ed25519 + 长度防移植 + 文档绑定）。

    签名必须覆盖**本**文档：root_hash == doc.chain_root 且
    chain_length == doc.chain_length，任一不符即 signature_invalid。
    """
    sig_data = doc.get("signature")
    if not sig_data:
        return False, "no signature in evidence document"

    root_hash = sig_data.get("root_hash", "")
    sig_length = sig_data.get("chain_length", 0)

    doc_pk = sig_data.get("public_key", "")
    if doc_pk and doc_pk != pubkey_hex:
        return False, (
            f"public_key mismatch: document claims {doc_pk[:16]}…, "
            f"verifier given {pubkey_hex[:16]}…"
        )

    if sig_length != doc.get("chain_length", 0):
        return False, (
            f"chain_length mismatch: signature says {sig_length}, "
            f"document says {doc.get('chain_length')}"
        )
    if root_hash != doc.get("chain_root", ""):
        return False, (
            f"root_hash mismatch: signature covers {root_hash[:20]}…, "
            f"document chain_root is {str(doc.get('chain_root'))[:20]}…"
        )

    payload = f"{root_hash}:{sig_length}".encode("utf-8")
    return _ed25519_verify(pubkey_hex, payload, sig_data.get("signature", ""))


# ═══════════════════════════════════════════════════════════════════════
# 综合判定（chain_substituted 精化）
# ═══════════════════════════════════════════════════════════════════════

def classify(chain_ok: bool, chain_failures: list,
             sig_checked: bool, sig_ok: bool) -> None:
    """chain_substituted 精化：链层红（仅根失配）+ 签名真实覆盖声称根
    → 同密钥不同链（记录被替换而签名与声称根保留）。"""
    if (not chain_ok and sig_checked and sig_ok
            and chain_failures
            and all(c in ("chain_root_mismatch", "chain_length_mismatch")
                    for c, _ in chain_failures)):
        # 替换标签：根失配+签名有效=链被替换的强证据
        for idx, (cat, msg) in enumerate(chain_failures):
            if cat == "chain_root_mismatch":
                chain_failures[idx] = ("chain_substituted", msg + "（签名真实有效——同密钥不同链）")


# ═══════════════════════════════════════════════════════════════════════
# 验证器自检（bootstrap——证明验证器自身可信）
# ═══════════════════════════════════════════════════════════════════════

def _build_test_chain(n: int = 3) -> dict:
    records = []
    prev = GENESIS
    for seq in range(1, n + 1):
        c = {
            "seq": seq,
            "step_id": f"s{seq}",
            "fact_type": "Command",
            "cause": seq - 1 if seq > 1 else None,
            "previous_evid": prev,
            "fact_hash": f"blake3:{seq:064x}",
            "domain_check": None,
        }
        c["evid_hash"] = evid_hash(c)
        prev = c["evid_hash"]
        records.append(c)
    return {
        "version": "1",
        "chain_id": "selftest",
        "records": records,
        "chain_root": prev,
        "chain_length": n,
    }


def self_test() -> bool:
    """4 项自检：有效链通过 / 篡改链拒绝 / 确定性 / 删尾检测。"""
    doc = _build_test_chain()

    # 1. 有效链通过
    valid, errors = verify_chain(doc)
    if not valid:
        print(f"  [FAIL] self-test 1: valid chain rejected: {errors}")
        return False
    print("  [PASS] self-test 1: valid chain accepted")

    # 2. 篡改链拒绝（改中间字段）
    tampered = json.loads(json.dumps(doc))
    tampered["records"][1]["fact_type"] = "Tampered"
    valid, failures = verify_chain(tampered)
    if valid or failures[0][0] != "field_modified":
        print(f"  [FAIL] self-test 2: tampered chain accepted or misclassified: {failures}")
        return False
    print("  [PASS] self-test 2: tampered chain rejected (field_modified)")

    # 3. 确定性（同输入同哈希）
    h1 = evid_hash(doc["records"][0])
    h2 = evid_hash(doc["records"][0])
    if h1 != h2:
        print("  [FAIL] self-test 3: hash not deterministic")
        return False
    print("  [PASS] self-test 3: deterministic")

    # 4. 删尾检测（chain_length 声明 > 实际记录数）
    truncated = json.loads(json.dumps(doc))
    truncated["records"] = truncated["records"][:1]
    valid, failures = verify_chain(truncated)
    if valid or failures[0][0] != "tail_truncated":
        print(f"  [FAIL] self-test 4: truncated chain accepted or misclassified: {failures}")
        return False
    print("  [PASS] self-test 4: truncated chain detected (tail_truncated)")

    return True


# ═══════════════════════════════════════════════════════════════════════
# 跨语言黄金向量对拍
# ═══════════════════════════════════════════════════════════════════════

def run_vectors(path: str) -> bool:
    """对拍黄金向量：Python 重算 vs 期望值（期望值由 Rust 侧同源断言）。"""
    with open(path, encoding="utf-8") as f:
        data = json.load(f)

    ok = True
    vectors = data.get("vectors", [])
    for v in vectors:
        actual = evid_hash(v["causal"])
        if actual != v["expect_evid_hash"]:
            print(f"  [FAIL] {v['name']}: expected {v['expect_evid_hash']}, got {actual}")
            ok = False
        else:
            print(f"  [PASS] {v['name']}")

    genesis = data.get("genesis_root")
    if genesis:
        if EVID_GENESIS_ROOT != genesis:
            print(f"  [FAIL] genesis_root: expected {genesis}, got {EVID_GENESIS_ROOT}")
            ok = False
        else:
            print("  [PASS] genesis_root")

    total = len(vectors) + (1 if genesis else 0)
    passed = total if ok else "FAILED"
    print(f"  vectors: {passed}/{total}")
    return ok


# ═══════════════════════════════════════════════════════════════════════
# CLI
# ═══════════════════════════════════════════════════════════════════════

def main() -> int:
    parser = argparse.ArgumentParser(
        description="EVID 零依赖独立验证器（evo-agent 证据链离线校验）"
    )
    parser.add_argument("evidence_file", nargs="?", help="evidence chain JSON 文件")
    parser.add_argument("--pubkey", default=None,
                        help="签名者公钥（64 hex），提供时验签")
    parser.add_argument("--self-test", action="store_true", help="验证器自检")
    parser.add_argument("--vectors", metavar="FILE",
                        help="跨语言黄金向量对拍（evid_golden_vectors.json）")
    args = parser.parse_args()

    if args.self_test:
        print("EVID verifier self-test:")
        if self_test():
            print("  4/4 PASS")
            return 0
        return 1

    if args.vectors:
        print("EVID cross-language golden vectors:")
        try:
            return 0 if run_vectors(args.vectors) else 1
        except (json.JSONDecodeError, OSError, KeyError) as e:
            print(f"ERROR: {e}", file=sys.stderr)
            return 3

    if not args.evidence_file:
        parser.print_help()
        return 3

    try:
        with open(args.evidence_file, encoding="utf-8") as f:
            doc = json.load(f)
    except (json.JSONDecodeError, OSError) as e:
        print(f"ERROR: {e}", file=sys.stderr)
        return 3

    print(f"chain_id: {doc.get('chain_id', '?')}")
    print(f"chain_length: {doc.get('chain_length', '?')}")

    chain_ok, failures = verify_chain(doc)
    sig_checked = args.pubkey is not None
    sig_ok = False
    if sig_checked:
        sig_ok, sig_err = verify_signature(doc, args.pubkey)

    # chain_substituted 精化（链层红 + 签名真实覆盖声称根）——先精化再输出
    if not chain_ok:
        classify(chain_ok, failures, sig_checked, sig_ok)

    if chain_ok:
        print("  chain integrity: PASS")
    else:
        for cat, msg in failures:
            print(f"  CHAIN FAILURE [{cat}]: {msg}")

    if sig_checked:
        if sig_ok:
            print("  signature: PASS")
        else:
            print(f"  SIGNATURE FAILURE [signature_invalid]: {sig_err}")

    if chain_ok and (not sig_checked or sig_ok):
        print("VERDICT: TRUSTED")
        return 0
    if not chain_ok:
        print("VERDICT: UNTRUSTED (chain broken)")
        return 1
    print("VERDICT: UNTRUSTED (signature invalid)")
    return 2


if __name__ == "__main__":
    sys.exit(main())
