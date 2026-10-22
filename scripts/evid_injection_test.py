#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""EVID 七连破坏注入验收——验证器对七类攻击的检出判定（红绿对照）。

对每类注入构造真实的篡改证据文档，经 evid_verify.py CLI（真实入口）
验证，断言退出码与篡改分类。全部通过 = 验收门通过。

  #0 前提    有效链+真实签名 → 0 / TRUSTED
  #1 改字段   field_modified → 1
  #2 删中间   record_deleted → 1
  #3 插入     record_inserted → 1
  #4 删尾     tail_truncated → 1
  #5 整链重算（自洽但旧签名不再覆盖新根） signature_invalid → 2
  #6 同密钥不同链（记录被换、真签名保留） chain_substituted → 1
  #7 验证器自检 bootstrap → 0（4/4）

依赖：blake3、pynacl（Ed25519 签名构造）。CI 中随 evid-parity job 运行。
"""

import json
import subprocess
import sys
import tempfile
from pathlib import Path

from blake3 import blake3

sys.path.insert(0, str(Path(__file__).parent))
from evid_verify import canonical_evid_json  # noqa: E402

VERIFIER = Path(__file__).parent / "evid_verify.py"

try:
    from nacl.signing import SigningKey
except ImportError:
    print("ERROR: pip install pynacl", file=sys.stderr)
    sys.exit(3)


def make_record(seq: int, prev: str, fact_type: str, fact_hash: str) -> dict:
    c = {
        "seq": seq,
        "step_id": f"s{seq}",
        "fact_type": fact_type,
        "cause": seq - 1 if seq > 1 else None,
        "previous_evid": prev,
        "fact_hash": fact_hash,
        "domain_check": None,
        "evid_hash": "",
        "timestamp": 1792586000 + seq,
    }
    c["evid_hash"] = "blake3:" + blake3(
        canonical_evid_json(c).encode("utf-8")).hexdigest()
    return c


def build_chain(n: int, tag: str) -> dict:
    records = []
    prev = ""
    for seq in range(1, n + 1):
        r = make_record(seq, prev, f"Event-{tag}", f"blake3:{tag}-{seq:061d}")
        prev = r["evid_hash"]
        records.append(r)
    return {
        "version": "1",
        "chain_id": f"chain-{tag}",
        "created_at": "2026-10-22T12:00:00+00:00",
        "records": records,
        "chain_root": records[-1]["evid_hash"],
        "chain_length": n,
    }


def sign_doc(doc: dict, sk: SigningKey) -> dict:
    payload = f"{doc['chain_root']}:{doc['chain_length']}".encode("utf-8")
    doc["signature"] = {
        "root_hash": doc["chain_root"],
        "signature": bytes(sk.sign(payload).signature).hex(),
        "public_key": sk.verify_key.encode().hex(),
        "chain_length": doc["chain_length"],
    }
    return doc


def run_verify(doc: dict, pubkey_hex: str) -> tuple[int, str]:
    with tempfile.NamedTemporaryFile(
            "w", suffix=".json", delete=False, encoding="utf-8") as f:
        json.dump(doc, f, ensure_ascii=False)
        path = f.name
    proc = subprocess.run(
        [sys.executable, "-X", "utf8", str(VERIFIER), path, "--pubkey", pubkey_hex],
        capture_output=True, text=True, encoding="utf-8",
    )
    return proc.returncode, proc.stdout + proc.stderr


RESULTS: list[tuple[str, bool, str]] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    RESULTS.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" — {detail}" if detail and not ok else ""))


def main() -> int:
    print("EVID 七连破坏注入验收：")
    sk = SigningKey.generate()
    vk_hex = sk.verify_key.encode().hex()

    # ── #0 前提：有效链 + 真实签名 → TRUSTED ──
    genuine = sign_doc(build_chain(5, "A"), sk)
    rc, out = run_verify(genuine, vk_hex)
    check("#0 前提：有效链+真实签名 TRUSTED (exit 0)",
          rc == 0 and "TRUSTED" in out, f"rc={rc}, out={out[-300:]}")

    # ── #1 改字段（不重算 evid_hash）→ field_modified ──
    d = json.loads(json.dumps(genuine))
    d["records"][2]["fact_type"] = "Forged"
    rc, out = run_verify(d, vk_hex)
    check("#1 field_modified (exit 1)",
          rc == 1 and "field_modified" in out, f"rc={rc}, out={out[-300:]}")

    # ── #2 删中间记录 → 悬空 prev → record_deleted ──
    d = json.loads(json.dumps(genuine))
    del d["records"][2]
    rc, out = run_verify(d, vk_hex)
    check("#2 record_deleted (exit 1)",
          rc == 1 and "record_deleted" in out, f"rc={rc}, out={out[-300:]}")

    # ── #3 插入记录（外来记录自洽、后继不重算）→ record_inserted ──
    d = json.loads(json.dumps(genuine))
    foreign = make_record(3, d["records"][1]["evid_hash"], "Injected",
                          "blake3:injected-" + "0" * 57)
    d["records"].insert(2, foreign)
    d["chain_length"] = 6  # 攻击者同步改长度声明，避开长度检测
    rc, out = run_verify(d, vk_hex)
    check("#3 record_inserted (exit 1)",
          rc == 1 and "record_inserted" in out, f"rc={rc}, out={out[-300:]}")

    # ── #4 删尾（chain_length 声明不变）→ tail_truncated ──
    d = json.loads(json.dumps(genuine))
    d["records"] = d["records"][:3]
    rc, out = run_verify(d, vk_hex)
    check("#4 tail_truncated (exit 1)",
          rc == 1 and "tail_truncated" in out, f"rc={rc}, out={out[-300:]}")

    # ── #5 整链重算（新链自洽，旧真实签名不再覆盖新根）→ signature_invalid ──
    forged = build_chain(5, "B")  # 攻击者全新自洽链
    forged["signature"] = genuine["signature"]  # 保留旧真签名
    rc, out = run_verify(forged, vk_hex)
    check("#5 signature_invalid 整链重算+旧签名 (exit 2)",
          rc == 2 and "signature_invalid" in out, f"rc={rc}, out={out[-300:]}")

    # ── #6 同密钥不同链（记录换成另一条真链，原签名与声称根保留）
    #      → 链层根失配 + 签名真实有效 → chain_substituted ──
    other = sign_doc(build_chain(5, "C"), sk)  # 同密钥签出的另一条真链
    d = json.loads(json.dumps(genuine))
    d["records"] = other["records"]  # 记录被换
    # chain_root / chain_length / signature 保持 genuine（签名真实覆盖声称根）
    rc, out = run_verify(d, vk_hex)
    check("#6 chain_substituted 同密钥不同链 (exit 1)",
          rc == 1 and "chain_substituted" in out, f"rc={rc}, out={out[-300:]}")

    # ── #7 验证器自检 bootstrap ──
    proc = subprocess.run(
        [sys.executable, "-X", "utf8", str(VERIFIER), "--self-test"],
        capture_output=True, text=True, encoding="utf-8")
    check("#7 验证器自检 bootstrap (exit 0, 4/4)",
          proc.returncode == 0 and "4/4 PASS" in proc.stdout,
          f"rc={proc.returncode}, out={proc.stdout[-300:]}")

    failed = [name for name, ok, _ in RESULTS if not ok]
    print(f"\n注入验收: {len(RESULTS) - len(failed)}/{len(RESULTS)} PASS"
          + (f"，失败: {failed}" if failed else " —— 全部检出"))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
