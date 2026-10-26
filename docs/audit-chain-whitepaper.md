# evo-agent 审计链：可独立验证的可信性白皮书

> **一句话**：你的 AI 助手的每一步操作都被记录在一条防篡改的密码学链条上。你不需要信任我们——你可以亲自验证。

---

## 第一层 · 三十秒读完（给每一个使用者）

当你使用 evo-agent 完成一次任务——问一个问题、执行一个操作、生成一份文档——背后发生了什么？

AI 模型的每次调用、每次返回、每条规则的命中、每次状态变更，都会被追加写入一条**只增不删**的密码学链条。链条上每一环都携带前环的哈希指纹，任何一环被篡改，后续所有环的指纹都会失配——**篡改即红**。

但"我们说它安全"毫无意义。真正重要的是：**你可以自己验**。

一条命令，不装我们的软件也行，用 Python 也行，用任何语言写二十行代码也行——把你手中的审计记录重算一遍哈希，跟链尾值对比。一致，则可信；不一致，则被动过。

更进一步：整条审计链可以导出为一份**标准化证据文档（EVID）**——一个 JSON 文件交给任何第三方，对方用一个单文件 Python 脚本、装一个 blake3 库，就能离线验完。不需要我们的软件，不需要访问我们的服务器。

这不是承诺，是数学。

---

## 第二层 · 五分钟读完（给工程师与技术决策者）

### 它是什么

evo-agent 内置一套三层审计体系加一层公开投影，记录 AI 代理执行过程中的**全部事实**：

| 层 | 解决的问题 | 一句话 |
|---|---|---|
| **事实哈希链** | 记录被改了怎么办 | 每条事实带 BLAKE3 哈希 + 前条链接，改动任何一条即链断 |
| **签名锚点链** | 整条链被重写了怎么办 | 周期性对链尾做 Ed25519 签名封条——整链重算也无法通过绑定验证 |
| **运行时哨兵** | 没人盯着怎么办 | 定时扫描 + 跨扫描基线比对——纯追加与历史被改后重算可精确区分 |
| **EVID 公开投影** | 外部第三方怎么验 | 审计链一键导出为标准化证据文档；单文件验证器离线验链+验签（无密钥设计——投影须与签名锚点联用） |

### 实际记录了什么

以一次真实会话为例——用户向 AI 提问"1+1等于几？"，审计链自动记录了 **20 条事实**：

| 链上事实 | 记录了什么 |
|---|---|
| Command | 用户发送的原始指令（逐字在链） |
| IoRequest → IoResponse | 实际发送给模型的消息集合 + 模型的真实返回（逐字在链） |
| StateTransition | 每次状态变更的完整快照 |
| TransitionTrace | 每条规则的命中/未命中判定（含域归因） |
| Stable | 收敛标记 |
| 会话摘要与偏好沉淀 | 自动捕获的用户偏好（受保护域写入） |
| journal 锚 | 本地操作日志的链尾指纹（绑定到引擎链） |

**没有抽样，没有日志级别过滤**——全部事实，逐条落链。

### 篡改检测实测

我们对这条真实链做了四种攻击测试：

| 攻击方式 | 难度 | 结果 |
|---|---|---|
| 修改一条回答的内容（"2"改"3"，不重算哈希） | 低 | ✅ **检出**：链哈希失配 |
| 删除末尾记录 | 中 | ✅ **检出**：哨兵基线"记录回退" |
| 修改内容 + **重算全部哈希**（使链自身自洽） | 高 | ✅ **检出**：签名锚点绑定失败——锚点声称的链头 ≠ 重算链头 |
| 伪造签名锚点 | 极高 | 需持有签名私钥；密钥不落审计资产 |

**关键**：第三种攻击（整链重算）是哈希链的天然盲区——攻击者改完内容后重算全部哈希，链自身完全自洽。签名锚点链的存在正是为了封堵这一缺口：锚点在链外签名，攻击者没有私钥就造不出能通过绑定验证的假锚。

**证据文档面同样实测**：对 EVID 证据文档的七连破坏注入（改字段 / 删中间记录 / 插入记录 / 删尾 / 整链重算+旧签名 / 同密钥不同链 / 验证器自检）全部检出（8/8），并作为 CI 常驻验收门。

### 怎么验

**方式一：CLI 验证（推荐）**

```bash
# 从 server 导出审计链
curl http://your-server:port/api/sessions/{id}/audit?include_content=true > audit.json

# 验证哈希链 + 签名锚点
evorule verify-chain audit.json
evorule verify-anchors audit.json --pubkey <你的公钥>
```

**方式二：任何语言，二十行代码**

审计链的哈希算法是公开的：`fact_hash = blake3(stable_json_utf8)`，`chain_step = blake3(prev + content)`。你不需要安装 evo-agent——用 Python、Go、JavaScript 任何语言重算一遍即可。我们提供了 11 组跨语言黄金向量（含中文、边界值）供你校验实现正确性。

**方式三：HTTP 端点**

```
GET /api/sessions/{id}/audit/verify     → 链完整性
GET /api/sessions/{id}/anchors          → 签名锚点列表
POST /api/sessions/{id}/anchors/verify  → 三层验证（结构+签名+绑定）
```

**方式四：EVID 证据文档（给外部第三方——零门槛）**

```bash
# 导出方：从 journal 导出标准化证据文档（evid_export 为独立命令行工具）
evid_export --journal data/sessions/{id}.jsonl -o evidence.json

# 验证方：单文件验证器（仅需 Python + blake3，无需安装本软件）
python evid_verify.py evidence.json                  # 链完整性（篡改六分类定位）
python evid_verify.py evidence.json --pubkey <hex>   # + Ed25519 验签（长度防移植）
python evid_verify.py --self-test                    # 验证器自检 4/4
```

篡改六分类：`field_modified` / `record_deleted` / `record_inserted` / `tail_truncated` / `signature_invalid` / `chain_substituted`——不止报红，还告诉你动过哪里。

### 部署者配置

审计链开箱即用（哈希链默认启用）。签名锚点需注入密钥：

```bash
# 一次性生成密钥对
evorule anchor-keygen --output /secure/path/anchor.seed
evorule anchor-pubkey /secure/path/anchor.seed  # 分发给验证方

# server 启动时注入（密钥仅进内存，不落审计资产）
export EVORULE_ANCHOR_SEED=<64-hex-seed>
export EVORULE_ANCHOR_INTERVAL=64  # 每 64 条事实自动封一锚
```

---

## 第三层 · 深入（给安全研究者与审计专家）

### 威胁模型

| 攻击向量 | 检出机制 | 层 |
|---|---|---|
| A1 修改记录内容 | content_hash 重算失配 | 哈希链 |
| A2 插入/删除记录 | prev_hash 链接断裂 | 哈希链 |
| A3 前缀截断 | 锚点 fact_range 区间无缝校验失败 | 锚点链 |
| A4 整链重算 | 锚点 chain_head 与重算链头绑定失败 | 锚点链 |
| A5 整链替换 | 伪造链无有效签名 | 锚点链 |
| A6 引擎替换 | engine_id 入锚点载荷 | 锚点链 |
| A7 锚点前缀截断 | 锚点链 prev 链接断裂 + seq 连续性 | 锚点链 |
| A8 格式降级 | WAL 文件头格式戳缺失 | 文件头 |
| A9 尾截断 | 哨兵基线记录回退 / 锚点 fact_range 超出 | 哨兵/锚点 |
| A10 密钥持有者从头记假账 | **不防**——见诚实边界 | — |
| A11 篡改 EVID 证据文档 | evid_hash 重算失配——六分类定位（改字段/删/插/删尾） | EVID 投影 |
| A12 伪造 EVID 投影链 | 投影层无密钥、自洽链可构造——签名绑定检出：chain_substituted / signature_invalid | EVID+锚点 |

### 哈希契约

**输入规范化**：每条事实（Fact）通过显式字段构造序列化为稳定 JSON——
- 键序固定（BTreeMap 字母序，与 Python `json.dumps(sort_keys=True)` 一致）
- `None` 字段省略（Option::None 与 Python None 行为一致）
- UTF-8 编码，无转义差异

**哈希计算**：
```
content_hash = BLAKE3( stable_json_utf8 )
chain_hash   = BLAKE3( prev_chain_hash_hex ++ content_hash_hex )
```

**跨语言保证**：11 组黄金向量（含 domain_attr 两态 / 中文换行 / i64 边界）由 Rust 实算导出，Python 独立实现对拍 11/11 一致。CI 强制运行对拍——不靠注释宣称兼容。

**签名载荷**（Ed25519 确定性 nonce，RFC 8032）：
```
signature_payload = "evorule-anchor/2|{session_id}|{seq}|{lo}|{hi}|{chain_head}|{key_id}|{engine_id}|{logical_time}"
```
字段以 `|` 串接——第三方验证器仅需 Ed25519 库即可验签，零额外依赖。

**EVID 投影哈希**（证据文档逐条记录，7 字段因果元组全部字符串化/显式 null——杜绝数值编码分裂）：
```
evid_hash = BLAKE3( canonical_json([ str(seq), step_id, fact_type,
                                      cause|null, previous_evid,
                                      fact_hash, domain_check|null ]) )
```
5 组向量 + 创世根由 Python **独立推导**、Rust 测试**逐值断言**（双向锁定，任一实现漂移即红）；CI `evid-parity` 常驻运行（验证器自检 + 向量对拍 + 七连注入验收）。

### 形式化语义

审计链哈希完整性的核心性质——**链步 back-link 归纳**——已通过有界模型检测验证（Kani 0.67.0， unwind=10，SUCCESSFUL）：

> 对任意链 `L`，`compute_chain_hash(L)` 等于逐条 `chain_step` 折叠至末尾。

 差分测试（proptest）常驻 CI：进程内追加路径与 WAL 重放路径产生全等四元组（快照/队列/版本/链尾哈希）。

### 诚实边界

| 不防什么 | 为什么 | 补位 |
|---|---|---|
| 密钥持有者从头记假账 | 密钥持有者可以构造自洽链+有效签名——这是所有签名体系的共同边界 | 多签/外部见证/HSM（部署级） |
| 审计链自身的密钥泄露 | 密钥泄露→签名可伪造 | HSM/密钥轮换（已支持 key_id） |
| 哈希碰撞 | BLAKE3 抗碰撞性是密码学假设，非本系统证明 | 密码学社区持续审计 |
| 审计链节点不可用 | 可用性非完整性范畴 | 部署级冗余 |
| 伪造 EVID 证据投影 | 投影层**故意不含密钥**——任何人都能构造一条自洽投影链，这是"公开可验"的代价 | 投影链不可单独作为"执行确实发生过"的证据；须与签名锚点联用（chain_root 绑定 + 长度防移植签名） |

### 架构分层

```
┌─────────────────────────────────────────────┐
│             签名锚点链（防抵赖）               │
│   Ed25519 · 链头绑定 · 区间无缝 · 密钥轮换    │
├─────────────────────────────────────────────┤
│             事实哈希链（防篡改）               │
│   BLAKE3 · 逐条链接 · write-ahead · 崩溃恢复  │
│   ┌─────────────────────────────────────┐   │
│   │       EVID 公开投影（可外验）         │   │
│   │   标准证据文档 · 单文件验证器 · 无密钥 │   │
│   │   chain_root ← 锚点签名绑定（联用）   │   │
│   └─────────────────────────────────────┘   │
├─────────────────────────────────────────────┤
│             运行时哨兵（防懈怠）               │
│   基线比对 · 前缀锚定 · 跨扫描连续性          │
├─────────────────────────────────────────────┤
│             跨语言验证（防单点）               │
│   黄金向量 · 独立实现 · CI 强制对拍           │
└─────────────────────────────────────────────┘
```

---

## 附：快速开始

```bash
# 1. 克隆本仓（本文档所在仓）
git clone https://github.com/evorule/evo-agent.git

# 2. EVID 证据投影：跨语言向量对拍（克隆即可验，无需运行环境）
pip install blake3
python scripts/evid_verify.py --vectors scripts/evid_golden_vectors.json
python scripts/evid_verify.py --self-test
# 期望输出: vectors: 6/6 与 自检 4/4 PASS

# 3. 事实链跨语言黄金向量（内核仓）
git clone https://github.com/evorule/evorule.git
python evorule/scripts/cross_lang_verifier.py evorule/scripts/g5_golden_vectors.json
# 期望输出: PASS: 11 组向量 / 0 失配

# 4. 从运行中的 server 导出审计链并验证
curl http://localhost:18099/api/sessions/1/audit?include_content=true > audit.json
evorule verify-chain audit.json

# 5. 有 journal 文件时：导出证据文档并用单文件验证器离线验证
cargo run --bin evid_export -- --journal data/sessions/{id}.jsonl -o evidence.json
python scripts/evid_verify.py evidence.json        # 验链；--pubkey <hex> 可加验签
```

## 附：文件索引

| 文件 | 说明 |
|---|---|
| `docs/audit-chain-whitepaper.md` | 本文档 |
| `src/agent/journal.rs` | journal 链哈希实现 |
| `src/agent/journal_anchor.rs` | journal 链 → 引擎链锚定 |
| `src/agent/memory_event/store.rs` | 事件防篡改对账（verify_cause_binding） |
| `src/agent/atif.rs` | 审计轨迹导出（携带引擎链尾） |
| `src/bin/journal_verify.rs` | journal 链独立验证工具 |
| `src/agent/evid.rs` | EVID 证据投影（因果元组 + 哈希契约 + journal 投影器） |
| `src/bin/evid_export.rs` | journal → 标准化证据文档导出 |
| `scripts/evid_verify.py` | 单文件零依赖验证器（链/签名/篡改六分类） |
| `scripts/evid_golden_vectors.json` | EVID 跨语言黄金向量 |
| `scripts/evid_injection_test.py` | 七连破坏注入验收（CI 常驻） |

---

<a id="english"></a>

# evo-agent Audit Chain: A Trustworthiness Whitepaper with Independent Verifiability

> **In one sentence**: every step your AI assistant takes is recorded on a tamper-evident cryptographic chain. You don't need to trust us — you can verify it yourself.

---

## Layer 1 · The 30-second read (for every user)

When you complete a task with evo-agent — asking a question, executing an operation, generating a document — what happens behind the scenes?

Every model invocation, every response, every rule hit, and every state change is appended to a cryptographic chain that is **append-only**. Each link carries the hash fingerprint of its predecessor; tamper with any single link and every subsequent fingerprint mismatches — **tampering turns the chain red**.

But "we say it's secure" means nothing. What actually matters: **you can verify it yourself**.

One command — no need to install our software; Python works, and so do twenty lines of code in any language — recompute the hashes of the audit records in your hand and compare against the chain-tail value. Match: trustworthy. Mismatch: it has been tampered with.

Going further: the entire audit chain can be exported as a **standardized evidence document (EVID)** — a JSON file you hand to any third party, who can verify it fully offline with a single-file Python script and one blake3 library. No software from us, no access to our servers.

This is not a promise. It is math.

---

## Layer 2 · The 5-minute read (for engineers and technical decision-makers)

### What it is

evo-agent ships with a three-layer audit system plus a public projection, recording **all facts** of the AI agent's execution:

| Layer | Problem solved | In one sentence |
|---|---|---|
| **Fact hash chain** | What if records get modified | Every fact carries a BLAKE3 hash + a link to its predecessor; modifying any single record breaks the chain |
| **Signature anchor chain** | What if the whole chain gets rewritten | Periodic Ed25519 signatures over the chain tail (seals) — a fully recomputed chain still fails the binding verification |
| **Runtime sentinel** | What if nobody is watching | Scheduled scans + cross-scan baseline comparison — pure appends and rewritten history are precisely distinguishable |
| **EVID public projection** | How can external third parties verify | One-click export of the audit chain into a standardized evidence document; a single-file verifier checks the chain + signatures offline (keyless by design — the projection must be used together with signature anchors) |

### What actually gets recorded

Take a real session as an example — a user asks the AI "what is 1+1?", and the audit chain automatically records **20 facts**:

| Fact on chain | What it records |
|---|---|
| Command | The user's original instruction (verbatim on chain) |
| IoRequest → IoResponse | The exact message set sent to the model + the model's actual response (verbatim on chain) |
| StateTransition | A full snapshot of every state change |
| TransitionTrace | The hit/miss decision of every rule (with domain attribution) |
| Stable | Convergence marker |
| Session summary & preference sedimentation | Automatically captured user preferences (protected-domain write) |
| journal anchor | Chain-tail fingerprint of the local operation log (bound to the engine chain) |

**No sampling, no log-level filtering** — all facts, every single one on chain.

### Tamper detection, measured

We ran four attack tests against this real chain:

| Attack | Difficulty | Result |
|---|---|---|
| Modify one response's content ("2" → "3", without recomputing hashes) | Low | ✅ **Detected**: chain hash mismatch |
| Delete trailing records | Medium | ✅ **Detected**: sentinel baseline "record rollback" |
| Modify content + **recompute all hashes** (making the chain self-consistent) | High | ✅ **Detected**: signature anchor binding failure — the anchor's claimed chain head ≠ the recomputed chain head |
| Forge a signature anchor | Very high | Requires the signing private key; the key never touches audit assets |

**The key point**: the third attack (full-chain recomputation) is a natural blind spot of hash chains — after modifying content, the attacker recomputes every hash and the chain becomes fully self-consistent. The signature anchor chain exists precisely to close this gap: anchors are signed off-chain, and without the private key an attacker cannot forge an anchor that passes binding verification.

**The evidence-document surface is likewise tested**: seven consecutive corruption injections against an EVID document (field modification / middle-record deletion / record insertion / tail truncation / full recomputation + old signature / same key, different chain / verifier self-test) are all detected (8/8), enforced as a permanent CI acceptance gate.

### How to verify

**Option 1: CLI verification (recommended)**

```bash
# Export the audit chain from the server
curl http://your-server:port/api/sessions/{id}/audit?include_content=true > audit.json

# Verify the hash chain + signature anchors
evorule verify-chain audit.json
evorule verify-anchors audit.json --pubkey <your-public-key>
```

**Option 2: any language, twenty lines of code**

The audit chain's hash algorithms are public: `fact_hash = blake3(stable_json_utf8)` and `chain_step = blake3(prev + content)`. You don't need to install evo-agent — recompute them in Python, Go, JavaScript, any language. We publish 11 cross-language golden vectors (including CJK text and boundary values) so you can validate your implementation.

**Option 3: HTTP endpoints**

```
GET /api/sessions/{id}/audit/verify     → chain integrity
GET /api/sessions/{id}/anchors          → signature anchor list
POST /api/sessions/{id}/anchors/verify  → three-layer verification (structure + signature + binding)
```

**Option 4: EVID evidence document (for external third parties — zero barrier)**

```bash
# Exporter: export a standardized evidence document from a journal (evid_export is a standalone CLI tool)
evid_export --journal data/sessions/{id}.jsonl -o evidence.json

# Verifier: single-file verifier (Python + blake3 only, no installation of this software)
python evid_verify.py evidence.json                  # chain integrity (tampering located in six categories)
python evid_verify.py evidence.json --pubkey <hex>   # + Ed25519 signature verification (length anti-transplant)
python evid_verify.py --self-test                    # verifier self-test 4/4
```

The six tamper categories: `field_modified` / `record_deleted` / `record_inserted` / `tail_truncated` / `signature_invalid` / `chain_substituted` — it doesn't just turn red; it tells you what was touched.

### Deployer configuration

The audit chain works out of the box (the hash chain is enabled by default). Signature anchors require key injection:

```bash
# One-time key pair generation
evorule anchor-keygen --output /secure/path/anchor.seed
evorule anchor-pubkey /secure/path/anchor.seed  # distribute to verifiers

# Inject at server startup (the key lives only in memory, never in audit assets)
export EVORULE_ANCHOR_SEED=<64-hex-seed>
export EVORULE_ANCHOR_INTERVAL=64  # auto-seal an anchor every 64 facts
```

---

## Layer 3 · Going deeper (for security researchers and audit experts)

### Threat model

| Attack vector | Detection mechanism | Layer |
|---|---|---|
| A1 Modify record content | content_hash recomputation mismatch | Hash chain |
| A2 Insert/delete records | prev_hash link breakage | Hash chain |
| A3 Prefix truncation | Anchor fact_range gapless-verification failure | Anchor chain |
| A4 Full-chain recomputation | Anchor chain_head binding failure against the recomputed head | Anchor chain |
| A5 Whole-chain substitution | The forged chain has no valid signature | Anchor chain |
| A6 Engine substitution | engine_id included in the anchor payload | Anchor chain |
| A7 Anchor prefix truncation | Anchor-chain prev link breakage + seq continuity | Anchor chain |
| A8 Format downgrade | Missing WAL file-header format stamp | File header |
| A9 Tail truncation | Sentinel baseline record rollback / anchor fact_range overrun | Sentinel/anchors |
| A10 Key holder writes a false ledger from scratch | **Not defended** — see Honest boundaries | — |
| A11 Tamper with an EVID evidence document | evid_hash recomputation mismatch — six-category localization (modify/delete/insert/truncate tail) | EVID projection |
| A12 Forge an EVID projection chain | The projection is keyless and a self-consistent chain can be constructed — signature binding detects it: chain_substituted / signature_invalid | EVID+anchors |

### Hash contract

**Input canonicalization**: every Fact is serialized to stable JSON via explicit field construction —
- Fixed key order (BTreeMap alphabetical order, matching Python `json.dumps(sort_keys=True)`)
- `None` fields omitted (Option::None behaves like Python None)
- UTF-8 encoding, no escaping divergence

**Hash computation**:
```
content_hash = BLAKE3( stable_json_utf8 )
chain_hash   = BLAKE3( prev_chain_hash_hex ++ content_hash_hex )
```

**Cross-language guarantee**: 11 golden vectors (covering both domain_attr states / CJK line breaks / i64 boundaries) are exported from real Rust computations, and an independent Python implementation cross-checks them 11/11 identical. CI runs the cross-check mandatorily — compatibility is not merely claimed in comments.

**Signature payload** (Ed25519 deterministic nonce, RFC 8032):
```
signature_payload = "evorule-anchor/2|{session_id}|{seq}|{lo}|{hi}|{chain_head}|{key_id}|{engine_id}|{logical_time}"
```
Fields are joined with `|` — a third-party verifier needs only an Ed25519 library to verify signatures, zero extra dependencies.

**EVID projection hash** (per record in the evidence document; the 7-field causal tuple is fully stringified / explicitly null — eliminating numeric-encoding splits):
```
evid_hash = BLAKE3( canonical_json([ str(seq), step_id, fact_type,
                                      cause|null, previous_evid,
                                      fact_hash, domain_check|null ]) )
```
The 5 vectors + genesis root are **independently derived** in Python and **asserted value-by-value** in Rust tests (two-way locking; a drift in either implementation turns red); CI runs `evid-parity` permanently (verifier self-test + vector cross-check + seven-injection acceptance).

### Formal semantics

The core property of audit-chain hash integrity — **induction over chain-step back-links** — has been verified by bounded model checking (Kani 0.67.0, unwind=10, SUCCESSFUL):

> For any chain `L`, `compute_chain_hash(L)` equals folding `chain_step` over the records up to the tail.

Differential testing (proptest) runs permanently in CI: the in-process append path and the WAL replay path produce fully equal quadruples (snapshot/queue/version/chain-tail hash).

### Honest boundaries

| What is not defended | Why | Mitigation |
|---|---|---|
| The key holder writes a false ledger from scratch | The key holder can construct a self-consistent chain + valid signatures — the common boundary of all signature systems | Multi-party signatures / external witnessing / HSM (deployment level) |
| Key leakage of the audit chain itself | Key leakage → signatures become forgeable | HSM / key rotation (key_id already supported) |
| Hash collisions | BLAKE3 collision resistance is a cryptographic assumption, not proven by this system | Ongoing cryptographic community audit |
| Audit-chain node unavailability | Availability is out of the integrity scope | Deployment-level redundancy |
| Forged EVID evidence projection | The projection **intentionally contains no keys** — anyone can construct a self-consistent projection chain; this is the price of "publicly verifiable" | A projection chain alone must not serve as evidence that "execution actually happened"; it must be used together with signature anchors (chain_root binding + length anti-transplant signature) |

### Architecture layers

```
┌─────────────────────────────────────────────┐
│ Signature anchor chain (non-repudiation)    │
│ Ed25519 · chain-head binding · gapless      │
│ ranges · key rotation                       │
├─────────────────────────────────────────────┤
│ Fact hash chain (tamper evidence)           │
│ BLAKE3 · per-record links · write-ahead ·   │
│ crash recovery                              │
│ ┌─────────────────────────────────────────┐ │
│ │ EVID public projection (externally      │ │
│ │ verifiable, keyless)                    │ │
│ │ standard evidence document · single-    │ │
│ │ file verifier · chain_root ← anchor     │ │
│ │ binding (combined use)                  │ │
│ └─────────────────────────────────────────┘ │
├─────────────────────────────────────────────┤
│ Runtime sentinel (anti-neglect)             │
│ baseline comparison · prefix anchoring ·    │
│ cross-scan continuity                       │
├─────────────────────────────────────────────┤
│ Cross-language verification (no single      │
│ point of trust)                             │
│ golden vectors · independent impls · CI     │
│ enforced cross-checks                       │
└─────────────────────────────────────────────┘
```

---

## Appendix: quick start

```bash
# 1. Clone this repository (where this document lives)
git clone https://github.com/evorule/evo-agent.git

# 2. EVID evidence projection: cross-language vector cross-check (verifiable right after cloning, no runtime needed)
pip install blake3
python scripts/evid_verify.py --vectors scripts/evid_golden_vectors.json
python scripts/evid_verify.py --self-test
# Expected output: vectors: 6/6 and self-test 4/4 PASS

# 3. Fact-chain cross-language golden vectors (kernel repository)
git clone https://github.com/evorule/evorule.git
python evorule/scripts/cross_lang_verifier.py evorule/scripts/g5_golden_vectors.json
# Expected output: PASS: 11 vectors / 0 mismatches

# 4. Export the audit chain from a running server and verify it
curl http://localhost:18099/api/sessions/1/audit?include_content=true > audit.json
evorule verify-chain audit.json

# 5. With a journal file: export the evidence document and verify it offline with the single-file verifier
cargo run --bin evid_export -- --journal data/sessions/{id}.jsonl -o evidence.json
python scripts/evid_verify.py evidence.json        # verify the chain; add --pubkey <hex> to verify signatures
```

## Appendix: file index

| File | Description |
|---|---|
| `docs/audit-chain-whitepaper.md` | This document |
| `src/agent/journal.rs` | journal chain-hash implementation |
| `src/agent/journal_anchor.rs` | journal chain → engine-chain anchoring |
| `src/agent/memory_event/store.rs` | Event tamper-evidence reconciliation (verify_cause_binding) |
| `src/agent/atif.rs` | Audit-trail export (carrying the engine chain tail) |
| `src/bin/journal_verify.rs` | Standalone journal-chain verification tool |
| `src/agent/evid.rs` | EVID evidence projection (causal tuple + hash contract + journal projector) |
| `src/bin/evid_export.rs` | journal → standardized evidence document export |
| `scripts/evid_verify.py` | Single-file zero-dependency verifier (chain/signature/six tamper categories) |
| `scripts/evid_golden_vectors.json` | EVID cross-language golden vectors |
| `scripts/evid_injection_test.py` | Seven-injection corruption acceptance (permanent in CI) |
