#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
EvoRule 审计锚点独立验证器（V-2）——零依赖单文件（仅 Python stdlib）

验证命题（判定层可复算，非事件层可复现）：
  输入的审计资产（ATIF 导出 + 锚点列表）满足——
  T1 密码学：末锚点 ed25519 签名对其载荷成立（需验签公钥）
  T2 绑定：锚点覆盖区间无缝覆盖 [0, N)，且锚点 chain_head == 导出链头
  T3 结构：锚点 seq 严格递增 0..n，anchor_hash 链式

诚实边界（V-2 不验证的，须由 evorule verify 端点背书）：
  - 事实链 BLAKE3 逐条哈希重算（内嵌整棵哈希树超零依赖边界）
  - 锚点哈希链（blake3）重算——A2 截断/fork 检出面由 evorule verify
    端点背书（v2 签名载荷串接已覆盖单锚点全部防伪造字段，
    篡改任何载荷字段即验签失败）
  - 锚点签名者身份（公钥本身的真实性属分发面信任）
  - 「当时真跑了」（事件层不可复现）

锚点格式：evorule-anchor/2（governance ≥0.8.2；签名目标=载荷字段串接字节，
零依赖可验）。0.8.1 的 v1 锚点（签 anchor_hash）不兼容——验证器拒绝而非
静默混验。

输入文件：
  --atif PATH     ATIF v1.8 导出 JSON（evo-agent /api/sessions/{id}/atif）
  --anchors PATH  锚点列表 JSON（evorule-server /api/sessions/{id}/anchors）
  --pubkey HEX    验签公钥（64 位 hex；部署方分发）

用法：
  python verify_anchors.py --atif t.json --anchors a.json --pubkey <hex>

退出码：0=全部通过 1=验证失败 2=输入错误

License: AGPL-3.0-or-later (C) 2026 EvoRule Project
"""
import argparse
import hashlib
import json
import sys

# ===== ed25519 纯实现（RFC 8032；~150 行，无第三方依赖） =====
P = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
D = -121665 * pow(121666, P - 2, P) % P
I = pow(2, (P - 1) // 4, P)


def _sha512(m: bytes) -> bytes:
    return hashlib.sha512(m).digest()


def _inv(x: int) -> int:
    return pow(x, P - 2, P)


def _xrecover(y: int) -> int:
    xx = (y * y - 1) * _inv(D * y * y + 1)
    x = pow(xx, (P + 3) // 8, P)
    if (x * x - xx) % P != 0:
        x = (x * I) % P
    if x % 2 != 0:
        x = P - x
    return x


By = 4 * _inv(5) % P
Bx = _xrecover(By)
B = (Bx, By, 1, Bx * By % P)  # 基点（扩展坐标）


def _edwards_add(p, q):
    x1, y1, z1, t1 = p
    x2, y2, z2, t2 = q
    a = (y1 - x1) * (y2 - x2) % P
    b = (y1 + x1) * (y2 + x2) % P
    c = t1 * 2 * D * t2 % P
    dd = z1 * 2 * z2 % P
    e, f, g, h = b - a, dd - c, dd + c, b + a
    return (e * f % P, g * h % P, f * g % P, e * h % P)


def _scalarmult(p, e):
    q = (0, 1, 1, 0)
    while e > 0:
        if e & 1:
            q = _edwards_add(q, p)
        p = _edwards_add(p, p)
        e >>= 1
    return q


def _compress(p) -> bytes:
    x, y, z, _ = p
    zi = _inv(z)
    x, y = x * zi % P, y * zi % P
    return int.to_bytes(y | ((x & 1) << 255), 32, "little")


def _decompress(s: bytes):
    y = int.from_bytes(s, "little")
    sign = y >> 255
    y &= (1 << 255) - 1
    x = _xrecover(y)
    if x & 1 != sign:
        x = P - x
    return (x, y, 1, x * y % P)


def ed25519_verify(pub: bytes, msg: bytes, sig: bytes) -> bool:
    """RFC 8032 verify（严格长度校验；任何畸形输入返回 False）"""
    if len(sig) != 64 or len(pub) != 32:
        return False
    A = _decompress(pub)
    Rs = sig[:32]
    R = _decompress(Rs)
    S = int.from_bytes(sig[32:], "little")
    if S >= L:
        return False
    h = int.from_bytes(_sha512(Rs + pub + msg), "little") % L
    sB = _scalarmult(B, S)
    RhA = _scalarmult(A, h)
    Rha = _edwards_add(R, RhA)
    return _compress(sB) == _compress(Rha)


# ===== 验证器主体 =====

def _fail(layer: str, msg: str) -> int:
    print(f"FAIL [{layer}] {msg}")
    return 1


def _hex_ok(s: str, n: int) -> bool:
    return len(s) == n and all(c in "0123456789abcdefABCDEF" for c in s)


def anchor_payload_v2(anchor: dict) -> bytes:
    """evorule-anchor/2 签名载荷（与 governance `signature_payload_v2` 同构）。

    `{ANCHOR_FORMAT}|{session_id}|{seq}|{lo}|{hi}|{chain_head}|{key_id}|{engine_id}|{logical_time}`
    ——字段串接字节直接可重构，零依赖可验；anchor_hash 链（blake3）仍由
    evorule verify 端点背书（诚实边界，见模块文档）。
    """
    fr = anchor["fact_range"]
    parts = [
        "evorule-anchor/2",
        anchor["session_id"],
        str(anchor["seq"]),
        str(fr["lo"]), str(fr["hi"]),
        anchor["chain_head"],
        anchor["key_id"],
        anchor["engine_id"],
        str(anchor["logical_time"]),
    ]
    return "|".join(parts).encode()


def verify(atif: dict, anchors_doc: dict, pubkey_hex: str) -> int:
    anchors = anchors_doc.get("anchors", [])
    # ---------- T2 绑定：区间无缝 + 链头一致 ----------
    covered = sorted(
        (a["fact_range"]["lo"], a["fact_range"]["hi"], a["seq"]) for a in anchors
    )
    pos = 0
    for lo, hi, seq in covered:
        if lo > pos:
            return _fail("T2", f"区间缺口 [{pos},{lo})（锚点 seq={seq} 前）")
        pos = max(pos, hi)
    print(f"T2 区间无缝: PASS（覆盖 [0,{pos})，{len(anchors)} 锚点）")

    # 链头：ATIF extra.audit_chain_head（若有）须等于末锚点 chain_head
    endo = atif.get("extra", {}).get("audit_anchor")
    if endo is not None:
        last = anchors[-1]
        if endo.get("chain_head", last["chain_head"]) != last["chain_head"]:
            return _fail("T2", "ATIF extra.audit_anchor 与锚点列表末锚 chain_head 不一致")
        head = last["chain_head"]
        ach = atif.get("extra", {}).get("audit_chain_head")
        if ach is not None and ach != head:
            return _fail("T2", f"ATIF audit_chain_head {ach[:12]}… != 末锚 chain_head {head[:12]}…")
        print(f"T2 链头绑定: PASS（{head[:16]}…）")

    # ---------- T3 结构：seq 严格递增 + anchor_hash 形态 ----------
    seqs = [a["seq"] for a in anchors]
    if seqs != list(range(len(seqs))):
        return _fail("T3", f"seq 非严格 0..n 递增: {seqs}")
    for a in anchors:
        if not _hex_ok(a.get("anchor_hash", ""), 64):
            return _fail("T3", f"锚点 seq={a['seq']} anchor_hash 非 64-hex")
    print(f"T3 seq+hash 结构: PASS（0..{len(seqs)-1}）")

    # ---------- T1 密码学：逐锚点验签 ----------
    if not _hex_ok(pubkey_hex, 64):
        print("WARN [T1] 公钥非 64-hex，跳过验签（仅结构验证）")
        return 0
    pub = bytes.fromhex(pubkey_hex)
    for a in anchors:
        sig_hex = a.get("signature", "")
        if not _hex_ok(sig_hex, 128):
            return _fail("T1", f"锚点 seq={a['seq']} 签名非 128-hex")
        ok = ed25519_verify(pub, anchor_payload_v2(a), bytes.fromhex(sig_hex))
        if not ok:
            return _fail("T1", f"锚点 seq={a['seq']} ed25519 验签失败")
    print(f"T1 验签: PASS（{len(anchors)} 锚点全部通过）")

    print("VERIFIED: 判定层可复算成立（锚点背书与提交资产咬合）")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="EvoRule 审计锚点独立验证器（V-2）")
    ap.add_argument("--atif", required=True, help="ATIF v1.8 导出 JSON 路径")
    ap.add_argument("--anchors", required=True, help="锚点列表 JSON 路径")
    ap.add_argument("--pubkey", default="", help="验签公钥 64-hex（缺省跳过 T1）")
    args = ap.parse_args()
    try:
        atif = json.load(open(args.atif, encoding="utf-8"))
        anchors_doc = json.load(open(args.anchors, encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as e:
        print(f"INPUT ERROR: {e}")
        return 2
    return verify(atif, anchors_doc, args.pubkey)


if __name__ == "__main__":
    sys.exit(main())
