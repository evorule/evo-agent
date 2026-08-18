<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: CC0-1.0

  Threat model documents are public artifacts; we release them under CC0 to
  maximize circulation among security-conscious users (compliance, regulated
  industries).
-->

# Threat Model — evo-agent 仓

> **Status**: v0.2.0-draft(从 evorule 生态全栈 `THREAT_MODEL.md` 拆出,走神 9)
> **Author**: EvoRule maintainers
> **Date**: 2026-07-30
> **Methodology**: STRIDE + Attack Trees + Data Flow Diagrams
> **Scope**: **evo-agent 仓**(`D:\evo-agent\`)= AI agent 应用层(LLM 集成 + builtin_tools + 工具权限 + workdir sandbox)
> **Target readers**: 内部工程师 / 独立 security reviewer
> **License**: CC0-1.0
> **配套文档**:
>
> - evorule 机制层威胁 → `D:\evorule\docs\security\THREAT_MODEL_v0.2.0.md`
> - evorule-application 应用层威胁 → `D:\evorule-application\docs\security\THREAT_MODEL.md`
> - 生态全栈旧版(已废弃)→ `D:\evorule\docs\security\THREAT_MODEL.md`(2026-07-20)

---

## 0. 怎么读这份文档

| 你是谁                        | 读哪些章节                                      |
| ----------------------------- | ----------------------------------------------- |
| **工程师**(改代码前)          | §3 资产 + §4 信任边界 + §6 STRIDE per component |
| **安全 reviewer**(独立 review)| §3 + §4 + §6 + §7 attack trees + §8 mitigations |

---

## 1. 一句话定位

> evo-agent 是 EvoRule 的**应用层**:用 evorule 机制层编排 AI agent,负责 LLM 集成、
> 工具调用权限(3-layer model)、workdir 沙箱。本威胁模型识别**所有可能通过 LLM / 工具
> 破坏用户数据或越权的攻击路径**。
>
> **关键洞察:** evo-agent 是**应用层**(application),evorule 是**机制层**(mechanism)。
> LLM 响应是 UNTRUSTED input,但机制层 100% 独立、可形式化验证。应用层破坏不会污染机制层。

---

## 2. 关键洞察(Principles)

### 2.1 5 设计原则

源自 [`D:\evo-agent\DESIGN_PRINCIPLES.md`](DESIGN_PRINCIPLES.md):

| 原则       | 在威胁模型中怎么体现                         |
| ---------- | -------------------------------------------- |
| **透明**   | 所有威胁 + mitigations 都公开,无 hidden 控制 |
| **可选**   | 用户能选 active/candidate/blocked 三档       |
| **可控**   | 关键操作必须经用户批准,blocked 永不允许      |
| **可回放** | 任何决策可以 replay / diff / rewind          |
| **可审计** | 每个决策留 blake3 哈希链 fact log            |

### 2.2 LLM 响应是 UNTRUSTED input

evo-agent 的核心攻击面:**LLM 响应可能被 prompt injection,触发工具调用泄露数据或越权**。
mitigation 是 3-layer 模型(active/candidate/blocked)+ 工具白名单 + workdir 沙箱。

---

## 3. 资产(Assets)

| #       | 资产                                            | 重要性                               | 位置                                   | 备份策略                            |
| ------- | ----------------------------------------------- | ------------------------------------ | -------------------------------------- | ----------------------------------- |
| **A3**  | **agent.json / rules/\*.json**                  | 🟡 High(用户业务)                    | evo-agent 加载                         | 备份靠用户                          |
| **A4**  | **LLM API key**                                 | 🟡 High(泄露 = 经济损失)             | 环境变量                               | 用户管理 + 不入 log                 |
| **A8**  | **工具输出**(file_read / shell_exec / http_get) | 🟢 Medium                            | 各 tool 的返回                         | 短期(过后不再用)                    |

> Fact log / blake3 链 / core_eval.json 在 evorule 仓(见 evorule 威胁模型 A1/A2)。

---

## 4. 信任边界(Trust Boundaries)

### 4.1 边界图

```text
                              UNTRUSTED
                                  │
   ┌──────────────────────────────┼──────────────────────────────┐
   │                              │                              │
   │                              ▼                              │
   │                  ┌───────────────────────┐                  │
   │                  │  LLM Provider         │                  │
   │                  │  (minimax / DeepSeek  │                  │
   │                  │   / OpenAI / 等)      │                  │
   │                  │  [SEMI-TRUSTED]       │                  │
   │                  └───────────┬───────────┘                  │
   │                              │ HTTPS + Bearer                │
   │                              ▼                              │
   │   ┌──────────────────┐  ┌────────────┐  ┌──────────────┐   │
   │   │ External HTTP    │  │            │  │ File system  │   │
   │   │ (docs.rs /       │  │ evo-agent  │  │ (workdir +   │   │
   │   │  crates.io /     │  │ (application│  │  workspace)  │   │
   │   │  github)         │  │  layer)    │  │              │   │
   │   │ [SEMI-TRUSTED]   │  │[UNTRUSTED  │  │[SEMI-TRUSTED]│   │
   │   └────────┬─────────┘  │ input from │  └──────┬───────┘   │
   │            │ HTTPS       │  LLM/user] │         │           │
   │            └─────────────►│            │◄────────┘           │
   │                           └─────┬──────┘                     │
   │                                 │ lib call / HTTP            │
   │                                 ▼                            │
   │                  ┌───────────────────────┐                    │
   │                  │  evorule 机制层        │ → 见 evorule 威胁模型│
   │                  │  [TRUSTED]            │                    │
   │                  └───────────────────────┘                    │
   └─────────────────────────────────────────────────────────────┘
                              TRUSTED
```

### 4.2 信任边界清单

| #       | 边界                                        | 方向 | 当前认证                                                                                                                           | 威胁等级                                                       | 详见      |
| ------- | ------------------------------------------- | ---- | ---------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- | --------- |
| **B1**  | User → evo-agent CLI                        | 入   | 无(local process)                                                                                                                  | 🟢 LOW                                                         | §6.1      |
| **B3**  | evo-agent → LLM provider (HTTPS)            | 出   | Bearer token                                                                                                                       | 🟢 LOW(env)                                                    | §6.1,§7.2 |
| **B4**  | evo-agent → External HTTP (HTTPS)           | 出   | 无,但 evo-agent `http_get` 工具有 SSRF 防护(blocklist 127.0.0.0/8、169.254/16 等)                                                | 🟢 LOW(evo-agent)                                              | §6.1,§7.2 |
| **B9**  | LLM Provider → evo-agent                    | 入   | HTTPS + cert                                                                                                                       | 🟢 LOW                                                         | §6.1,§7.2 |

> 注:evo-agent → evorule-server 的 HTTP 边界(B2)在 evorule-application 威胁模型。

---

## 5. 数据流图(DFD Level 1)— AI 工具调用路径

```text
LLM response (B9, UNTRUSTED input)
    │
    │ "I want to call tool X with args Y"
    ▼
┌─────────────────┐
│ LLM Handler     │ (B3, evo-agent)
│ (parse JSON)    │
└──────┬──────────┘
       │
       │ (2) call_service (tool_name, args)
       ▼
┌─────────────────┐
│ Tool Handler    │ (3-layer + propose)
└──────┬──────────┘
       │
       │ (3a) active  → 直接执行
       │ (3b) candidate → 返回 proposal,user 批
       │ (3c) blocked → 拒绝
       ▼
┌─────────────────┐
│ Builtin Tool    │
│ (file_read /    │
│  shell_exec /   │
│  http_get /     │
│  file_write /   │
│  file_list /    │
│  search_files / │
│  delegate_tool) │
└──────┬──────────┘
       │
       │ (4) result → Fact::IoResponse(写入 evorule fact log)
       ▼
LLM (next iteration) + evorule fact log (审计)
```

**关键 attack surface:** LLM 响应是 UNTRUSTED input,但 ToolHandler 把它当 trusted instruction 处理。
**mitigation:** 3-layer 模型(active/candidate/blocked)+ 工具自己的白名单 + workdir 沙箱。

---

## 6. STRIDE per component

### 6.1 evo-agent(应用层,UNTRUSTED input from LLM)

| STRIDE | 威胁                                 | 当前 mitigation                       | 残留风险       |
| ------ | ------------------------------------ | ------------------------------------- | -------------- |
| **S**  | LLM 假冒 evo-agent 调 LLM            | Bearer token in env                   | 🟢 LOW         |
| **S**  | LLM 假冒 user 调 blocked tool        | blocked 永不允许,即使 `approved=true` | 🟢 LOW         |
| **T**  | LLM 改 core_eval.json                | TCB 不读 LLM 改的东西;只读编译时门禁  | 🟢 LOW         |
| **T**  | LLM 改 audit log                     | log path 不在 workdir;server-side 写  | 🟢 LOW         |
| **R**  | LLM 说"我没调过 rm"                  | ✅ log 留痕(M3 closed,tool calls 已入 fact log) | 🟢 LOW  |
| **I**  | LLM 泄露 user 私有数据给 external    | SSRF blocklist + workdir sandbox      | 🟢 LOW         |
| **D**  | LLM 触发 shell_exec 死循环           | 60s timeout                           | 🟢 LOW         |
| **D**  | LLM 触发 file_read 10GB 文件         | 10MB size limit                       | 🟢 LOW         |
| **E**  | LLM 用 `sudo` / `bash` / `python`    | blocked list                          | 🟢 LOW         |
| **E**  | LLM 通过 symlink 逃逸 workdir        | canonicalize() 检查                   | 🟢 LOW         |
| **E**  | LLM 改 agent.json.tools 注入未知工具 | ✅ from_definition 早失败(M4 closed)  | 🟢 LOW         |

### 6.2 builtin_tools(7 个工具)

| 工具            | 权限层    | 关键 mitigation                              | 残留风险 |
| --------------- | --------- | -------------------------------------------- | -------- |
| `file_read`     | active    | workdir sandbox + 10MB limit + canonicalize  | 🟢 LOW   |
| `file_list`     | active    | workdir sandbox                              | 🟢 LOW   |
| `file_write`    | candidate | workdir sandbox + canonicalize + 用户批准    | 🟢 LOW   |
| `search_files`  | active    | workdir sandbox                              | 🟢 LOW   |
| `shell_exec`    | candidate | blocked list(sudo/bash/python/curl 等)+ 60s timeout + 用户批准 | 🟢 LOW |
| `http_get`      | active    | SSRF blocklist(127.0.0.0/8、169.254/16)+ scheme 白名单 | 🟡 MEDIUM(L3/L4 DNS rebinding / TOCTOU 未实现) |
| `delegate_tool` | candidate | 用户批准                                     | 🟢 LOW   |

---

## 7. 攻击树(Attack Trees)

### 7.1 攻击 1:恶意 LLM 响应导致数据泄露

```text
根目标: LLM 响应让 evo-agent 把 user 私有数据发给 attacker
│
├── 路径 A: 通过 http_get 发内网
│   │
│   ├── A1: SSRF — 调 http://127.0.0.1/
│   │   └── 防御: SSRF blocklist (127.0.0.0/8)
│   │   → 残留风险:🟢 LOW
│   │
│   ├── A2: SSRF — 调 169.254.169.254(cloud metadata)
│   │   └── 防御: SSRF blocklist (169.254/16)
│   │   → 残留风险:🟢 LOW
│   │
│   ├── A3: DNS rebinding(攻击者控制 DNS)
│   │   └── 防御: L3 未实现 — re-validate IP after resolve
│   │   → 残留风险:🟡 MEDIUM (L3)
│   │
│   └── A4: TOCTOU(parse 合法 / resolve 非法)
│       └── 防御: L4 未实现
│       → 残留风险:🟡 MEDIUM (L4)
│
├── 路径 B: 通过 file_read 读敏感文件
│   │
│   ├── B1: 绝对路径
│   │   └── 防御: 拒绝
│   │   → 残留风险:🟢 LOW
│   │
│   ├── B2: 相对路径 + `..`
│   │   └── 防御: canonicalize + reject
│   │   → 残留风险:🟢 LOW
│   │
│   ├── B3: symlink 逃逸
│   │   └── 防御: canonicalize 之后必须仍在 workdir
│   │   → 残留风险:🟢 LOW
│   │
│   └── B4: path 长 1MB 触发 buffer 攻击
│       └── 防御: body limit
│       → 残留风险:🟢 LOW
│
├── 路径 C: 通过 shell_exec 调 curl / nc
│   │
│   ├── C1: 直接调 `curl`
│   │   └── 防御: blocked list
│   │   → 残留风险:🟢 LOW
│   │
│   ├── C2: 通过 `xargs` 构造 `sudo curl`
│   │   └── 防御: sudo blocked
│   │   → 残留风险:🟢 LOW
│   │
│   └── C3: shell metacharacter(`;` `|` 等)
│       └── 防御: 拒绝所有 metacharacter
│       → 残留风险:🟢 LOW
│
└── 路径 D: 通过 file_write 写 ../../.bashrc
    │
    ├── D1: 同 B1-B3(absolute / `..` / symlink)
    │   └── 防御: 同上
    │   → 残留风险:🟢 LOW
    │
    └── D2: 写到 ./workspace/ 之后被 attacker 偷
        └── 防御: N/A(workdir 是用户责任)
        → 残留风险:🟢 OUT OF SCOPE
```

### 7.2 攻击 2:Prompt Injection 触发不可逆操作

```text
根目标: LLM 响应被注入,触发 candidate tool(rm -rf 等)
│
├── 路径 A: candidate 工具自动批准
│   │
│   ├── A1: evo-agent.run 默认不传 --auto-approve
│   │   └── 防御: 默认拒绝
│   │   → 残留风险:🟢 LOW
│   │
│   ├── A2: 用户被 social engineering 骗打开 auto-approve
│   │   └── 防御: 不可防御(社会工程学)
│   │   → 残留风险:🟡 MEDIUM
│   │
│   └── A3: LLM 撒谎说"user 已批"
│       └── 防御: user 真批之后系统走 approve=true(M3 fact 留痕)
│       → 残留风险:🟡 MEDIUM (M3 + social)
│
├── 路径 B: 攻击者改 core_eval.json
│   │
│   ├── B1: 通过 file_write 写到 ./workspace/
│   │   └── 防御: 路径白名单 + build.rs 编译时门禁
│   │   → 残留风险:🟢 LOW
│   │
│   ├── B2: 通过 evo-agent 加载 hot-update
│   │   └── 防御: TCB 不支持 hot update(只能编译时)
│   │   → 残留风险:🟢 LOW
│   │
│   └── B3: 通过 SHARED_FACTS 注入伪 core_eval
│       └── 防御: TCB 不从 shared_facts 读 core_eval
│       → 残留风险:🟢 LOW
│
└── 路径 C: LLM 调 blocked 工具
    │
    ├── C1: 直接调 `sudo`
    │   └── 防御: blocked 永不允许
    │   → 残留风险:🟢 LOW
    │
    ├── C2: 调 `bash -c 'sudo ...'`
    │   └── 防御: bash blocked
    │   → 残留风险:🟢 LOW
    │
    └── C3: 调 `python -c "import os; os.system('sudo ...')"`
        └── 防御: python blocked
        → 残留风险:🟢 LOW
```

---

## 8. Mitigation 映射表(Threat → Control → Test)

| 威胁                        | Mitigation                                                                                                                              | 已实现?                                                               | 验证方法             | 漏洞编号   |
| --------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- | -------------------- | ---------- |
| M3 tool call 不写 fact      | ✅ tool calls 已作为 `Fact::IoRequest{io_type:"call_service"}` 写入 fact log                                                              | ✅ 已实现                                                             | integration test     | M3 closed  |
| M4 tools 字段未校验         | from_definition 早失败                                                                                                                  | ✅ 已实现                                                             | unit test            | M4 closed  |
| L1 zip-slip                 | tar 显式 reject `..`                                                                                                                    | ❌ 待办                                                               | unit test            | L1         |
| L2 xargs chain              | 文档化                                                                                                                                  | ❌ 待办                                                               | docs                 | L2         |
| L3 DNS rebinding            | resolve 后再 validate IP                                                                                                                | ❌ 待办                                                               | unit test            | L3         |
| L4 TOCTOU                   | 同 L3                                                                                                                                   | ❌ 待办                                                               | unit test            | L4         |
| SSRF blocklist(evo-agent)   | 硬编码 IP 段(evo-agent `http_get` 工具)                                                                                                 | ✅ done                                                               | unit test            | n/a        |
| 3-layer model               | 7 工具统一(active/candidate/blocked)                                                                                                    | ✅ done                                                               | unit test            | n/a        |
| workdir sandbox             | canonicalize(evo-agent)                                                                                                                 | ✅ done                                                               | unit test            | n/a        |
| `#![forbid(unsafe_code)]`   | evo-agent 全栈                                                                                                                          | ✅ done                                                               | `cargo build`        | n/a        |

---

## 9. 残留风险(Residual Risks)

### 9.1 MEDIUM(后续版本关注)

| #            | 残留风险                                   | 用户影响                                                 | 缓解(短期)                                                         |
| ------------ | ------------------------------------------ | -------------------------------------------------------- | ------------------------------------------------------------------ |
| L3           | DNS rebinding(http_get)                   | 攻击者控制 DNS 绕过 SSRF blocklist                       | resolve 后再 validate IP                                            |
| L4           | TOCTOU(http_get)                          | parse 合法 / resolve 非法                                | 同 L3                                                               |
| social       | 用户被 social engineering 骗打开 auto-approve | candidate 工具自动执行                                   | 不可完全防御;提高透明度                                            |

### 9.2 范围外(Out of Scope)

- ❌ evorule 机制层威胁(Fact log / WAL / TCB)→ evorule 威胁模型
- ❌ HTTP API 认证 / CORS / evorule-server → evorule-application 威胁模型
- ❌ http_handler SSRF / db_handler SQL(io_handlers)→ evorule-application 威胁模型
- ❌ 物理访问 / OS / 内核 / 硬件攻击
- ❌ 第三方 LLM provider 的 SLA / 内部漏洞

---

## 10. 参考(References)

### 10.1 内部

- [`D:\evo-agent\DESIGN_PRINCIPLES.md`](DESIGN_PRINCIPLES.md) — 5 设计原则
- evorule 机制层威胁 → `D:\evorule\docs\security\THREAT_MODEL_v0.2.0.md`
- evorule-application 应用层威胁 → `D:\evorule-application\docs\security\THREAT_MODEL.md`

### 10.2 外部方法学

- **STRIDE** — Microsoft threat modeling
- **Attack Trees** — Schneier, 1999
- **OWASP Top 10 for LLM Applications** — <https://owasp.org/www-project-top-10-for-large-language-model-applications/>

---

## 11. Change Log

| Version | Date       | Change                                                                                                                                                             |
| ------- | ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 0.2.0   | 2026-07-30 | **走神 9 拆分**:从 evorule 生态全栈 `THREAT_MODEL.md`(2026-07-20)拆出 evo-agent 独立范围。保留 LLM/tools/workdir/3-layer 相关章节;M3/M4 状态确认 closed。           |
