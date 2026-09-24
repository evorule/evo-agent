#!/usr/bin/env python3
"""TCB 约束前置门（BUG-P0-005）在位性 A/B 探测。

对一个运行中的 evorule-server 实例：建会话 → 提交一条故意违规的 call_external
（model 不在白名单）→ 轮询 history 找 Violation 事实。

- 违规指令返回 Violation  ⇒ 约束前置门在位（enforce E2E 可真实触发）
- 违规指令返回 IoRequest   ⇒ server 仍消费旧 TCB（遮蔽未修复，enforce E2E 不可行）

用法：python probe_tcb_gate.py [base_url]（缺省 http://127.0.0.1:18080）
只创建测试会话与提交测试指令，不改动规则集。
"""

import json
import sys
import time

import httpx

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18080"

VIOLATING = {
    "type": "call_external",
    "params": {
        "model": "probe-nonallowlisted-model",
        "messages": [{"role": "user", "content": "hi"}],
    },
}


def main() -> int:
    c = httpx.Client(timeout=15.0)
    r = c.post(f"{BASE}/api/sessions", json={})
    r.raise_for_status()
    sid = str(r.json()["session_id"])
    print(f"session_id={sid}")

    r = c.post(f"{BASE}/api/sessions/{sid}/command", json={"instruction": VIOLATING})
    print(f"command status={r.status_code} body={r.text[:200]}")

    # 轮询 history 最长 5s 等 Violation/IoRequest 出现
    verdict = "TIMEOUT"
    for _ in range(10):
        time.sleep(0.5)
        h = c.get(f"{BASE}/api/sessions/{sid}/history").raise_for_status().json()
        facts = h if isinstance(h, list) else h.get("history", h.get("facts", []))
        types = [f.get("type") or f.get("fact", {}).get("type") for f in facts]
        if "Violation" in types:
            v = next(f for f in facts if (f.get("type") or f.get("fact", {}).get("type")) == "Violation")
            print(f"VIOLATION fact found: {json.dumps(v, ensure_ascii=False)[:400]}")
            verdict = "GATE_PRESENT"
            break
        if "IoRequest" in types:
            print(f"IoRequest fact found (no Violation): types={types}")
            verdict = "GATE_ABSENT"
            break

    sig = c.get(f"{BASE}/api/sessions/{sid}/evolution-signals")
    if sig.is_success:
        print(f"evolution-signals: {json.dumps(sig.json(), ensure_ascii=False)[:300]}")
    print(f"VERDICT={verdict}")
    return 0 if verdict == "GATE_PRESENT" else 1


if __name__ == "__main__":
    sys.exit(main())
