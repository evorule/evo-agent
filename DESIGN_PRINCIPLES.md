# EvoRule 设计原则(EvoRule Design Principles)

> 这是**整个 EvoRule 生态**的宪法级原则。
> 任何新功能 / 新工具 / 新 API / 新应用,都必须用这 5 条作为 review checklist。
>
> 起源:**EvoRule 作者 × Mavis**,2026-07-20,review evo-agent P0 #2 的 6 工具 3 层安全模型时确立。

---

## 根本哲学(Philosophy)

> **JSON 是一等公民**(规则 / 状态 / 事件 / 审计)
> **代码只是机制**(不需要透明,只要让 JSON 规则可运行即可)

**含义:**

- **evorule 运行的是 JSON 结构化规则,不是代码**
- **代码是机制层的、是辅助的** —— 它的内部不需要透明(只要 deterministic + 形式化可验证)
- **用户看到 / 操作的,全部是 JSON** —— agent.json / rules/*.json / fact log
- **审计 / 回放的对象,也是 JSON** —— 不是 .rs 源码,不是 .py 字节码

**反例(写代码时容易犯的错):**

- 把 JSON 当代码的语法糖(嵌入表达式 / 函数 / 变量)
- 隐藏 JSON 的内部结构(让用户只能看 UI 不让看 JSON)
- 让代码层的逻辑"穿透"到 JSON 层(用户必须懂代码才能配 JSON)

---

## 5 个核心属性(5 Attributes)

### 1. 透明(Transparent)

> 一切设计 / 选项 / 行为必须能被人看到。

**白话:** 用户不靠猜、不靠"魔法",能看清楚系统在做什么。

**落地方式:**

- 所有 6 个工具的 active/candidate/blocked 分类**全列出来**(`evo-agent tools list`)
- 任何 candidate 工具被调用,**先返回 proposal** 而不是直接执行
- `config` 子命令打印**完整合并后配置**(default + user + project + env)
- `validate` 子命令在跑之前就告诉用户:模型 / 工具 / memory 是什么

**反例:**

- 默默执行用户没看到的事
- 隐藏内部状态(让用户只能 reload 才能看到)
- "运行了 5 分钟没反应" —— 没有进度

### 2. 可选(Selectable)

> 人类能选择。不只是"接受 / 拒绝"二选一,而是**多档位**。

**落地方式(active / candidate / blocked 三层):**

- **active**:白名单,直接执行,无需请示(给 LLM 自由度)
- **candidate**:备选,LLM 想用 → 摊开 proposal(description / risk / alternative)→ 用户批 → 再执行
- **blocked**:永不允许(逃逸出口 / 不可逆破坏)

**为什么是 3 层而不是 2 层?**

2 层(allow / deny)逼用户在**不可控**和**完全无功能**之间二选一。
3 层给用户一个**有缓冲的选择空间** —— 大部分危险操作可以"看 proposal 再放"。

**反例:**

- "二选一"硬切(应该给 active/candidate/blocked 三层)
- 默默把 candidate 升级成 active
- 把 blocked 的"绕过路径"藏在文档角落

### 3. 可控(Controllable)

> 关键操作必须经人类批准。**不可越权。**

**落地方式:**

- `run` 默认拒绝所有 candidate 工具(必须显式 `--auto-approve-candidates`)
- `propose` 协议:`{status: "needs_approval", description, risk, alternative}` + `instructions: "Ask user; on approval, call with approved=true"`
- `blocked` 工具**永远拒绝**,即使有 `approved=true`(防止"全部 approved 一把梭")
- 工具白名单是**显式列出**的(不是黑名单默认拒绝)
- `http_get` 的 SSRF 防护是**硬编码 IP 段黑名单**(127/8, 10/8, 172.16/12, 192.168/16, 169.254/16, IPv6 fe80/fc00/::1)

**反例:**

- "agent 自动跑 10 分钟" 没问
- "一键全部 approved" 的快捷方式
- 把 SSRF 防护做成"可关闭"的开关

### 4. 可回放(Replayable)

> 任何决定**可重放**(replay / diff / rewind)。**可会放** 是笔误,标准名是"可回放"。

**落地方式:**

- evorule 的 **fact log**(append-only) = 决策的"录像带"
- **replay**:重放历史事件,得到完全相同的状态
- **diff**:对比两个版本的 fact,看哪一步改变
- **rewind**:回退到某个 version,从那里继续
- agent 的每次 `run` 输出结构化 JSON(success / steps / tool_calls / duration_ms / error)

**对应杀手锏应用:** [`D:\evorule-application\time-travel-debugger`](../evorule-application/) —— 用 evorule 的 fact log 做出"时间旅行调试器",**唯一能做到**这个能力的产品。

**反例:**

- 只 log "开始 / 结束",不 log 中间每步
- log 不可回放(只 forward)
- rewind 后状态不一致

### 5. 可审计(Auditable)

> 每个决策有 fact log,事后可追溯。

**落地方式:**

- evorule 的 `FactsLog` 是**append-only**的(blake3 哈希链,防篡改)
- 每个 fact 有 `version` / `path` / `value` / `source_session_id` / `type`
- agent 的每次 `run` 输出是 JSON,自带时间戳、步骤数、工具调用列表
- `audit_verify` 端点(在 evorule-server 里)可以**事后校验**日志是否被改过

**对合规用户(医疗 / 律所 / 金融 / 政务):**

- "不可篡改的审计链" = **SOX / HIPAA / 等保 2.0** 的硬要求
- blake3 哈希链 = 给监管的"防篡改证据"
- evorule 没有智能 = **没有 AI 风险** = 合规友好

**反例:**

- log 可以被改 / 被删
- log 不带因果链(只有 flat records,没法追"为什么")
- 错误日志只说"失败",不说"为什么失败"

---

## 5 原则的相互关系

```
         透明(Transparent)
              │
              │   没有透明,其他 4 条
              │   都没意义(用户看不见)
              ▼
可选 → 可控 → 可回放 → 可审计
   │       │        │         │
   │       │        │         └─ 留痕(可事后追)
   │       │        └─────── 重放(可验证)
   │       └─────────────── 批准(关键操作必经)
   └───────────────────── 多档位选择(不 2 选 1)
```

**一句话总结:**

> 让用户看清(透明)、能选(可选)、能批(可控)、能重放(可回放)、能追溯(可审计)。

---

## 应用:生态项目 review checklist

> 今后**所有 EvoRule 生态项目**(`evorule` / `evo-agent` / `evorule-cli` / `evorule-application`)的设计 review,都要用这 5 条作为 checklist。

**任何一个新功能 / 新工具 / 新 API 都要问:**

| # | 问题 | 反例(NO) | 正例(YES) |
|---|---|---|---|
| 1 | 透明吗?选项 / 行为对用户可见? | `tools` 命令默认隐藏,需要 debug flag 才看到 | `evo-agent tools list` 直接打印所有 6 个工具 + 3 层分类 |
| 2 | 可选吗?用户有多种路径(不只接受/拒绝)? | "启用 / 禁用" 二选一 | active / candidate / blocked 三层 |
| 3 | 可控吗?关键操作经用户批准? | candidate 工具默默执行 | candidate 工具返回 proposal,等用户批 |
| 4 | 可回放吗?这个决定能 replay / diff / rewind 吗? | 跑完只输出"成功",没记录每步 | fact log 留痕,能 replay / diff / rewind |
| 5 | 可审计吗?有 fact log / 决策记录? | 错误日志只说"失败",没 trace | append-only blake3 哈希链,可 `audit_verify` |

**反模式清单(出现任一,设计 review 应被打回):**

- 默默执行用户没看到 / 没批准的事
- "二选一"硬切(应该给 active / candidate / blocked 三层)
- 把 JSON 当代码的语法糖(JSON 是数据,代码是机制)
- 隐藏错误或失败原因
- log 不可回放 / 可被改
- 自动化流程没有"暂停点"让用户干预

---

## 实际代码中的体现(Examples)

### 6 个 builtin 工具(0.1.0)

| 工具 | active | candidate | blocked |
|---|---|---|---|
| **file_read** | ./workdir 下任何非隐藏文件 | 任何绝对路径 / `..` / 隐藏文件 | (走 file_read 不应该触达的边界) |
| **file_list** | ./workdir 下任何目录 | (同上) | (同上) |
| **file_write** | 写 ./workdir/workspace/ 下新文件 | 覆盖现有文件(`overwrite=true` 待批) | 写 ./workdir/workspace/ 之外 |
| **search_files** | glob 在 ./workdir 下 | (同上) | (同上) |
| **shell_exec** | 8 active: `cargo` / `git` / `ls` / `cat` / ... | 20 candidate: `rm` / `mv` / `sed` / ... | 28 blocked: `sudo` / `bash` / `python` / `curl` / `kill` / ... |
| **http_get** | 6 active hosts: `docs.rs` / `crates.io` / ... | 任何其他公开 host | `http://` / localhost / 内网 / 169.254.169.254 (cloud metadata) |

**propose 协议**(统一格式,所有 candidate 一致):

```json
{
  "status": "needs_approval",
  "command": "rm -rf /tmp/build",
  "description": "删除 /tmp/build 目录(避免阻塞)",
  "risk": "误删不可逆;rm -rf 没有提示",
  "alternative": "用 file manager GUI;或 mv 到 ~/.local/trash",
  "instructions": "Ask user; on approval, call with approved=true"
}
```

**SSRF 防护**(`http_get`):

```rust
// 硬编码 IP 段黑名单
const BLOCKED_IP_RANGES: &[&str] = &[
    "127.0.0.0/8",      // loopback
    "10.0.0.0/8",       // private
    "172.16.0.0/12",    // private
    "192.168.0.0/16",   // private
    "169.254.0.0/16",   // link-local, **especially cloud metadata**
    "::1",              // IPv6 loopback
    "fe80::/10",        // IPv6 link-local
    "fc00::/7",         // IPv6 ULA
];
```

---

## 演化(Evolution)

> 5 原则不是"一成不变的宪法",而是**会生长的活物**。

**当前(0.1.0):** 5 原则已落地在 builtin_tools 6 工具上,runner 桥接 / CLI 已通。

**下一阶段(0.2.0):**

- runner.run 循环需要处理 candidate 工具返回的 proposal(目前会 error out)
- `run --auto-approve-candidates` 需要把 decision 写进 fact log
- 时间旅行调试器应用层接入,让用户能 replay / diff / rewind agent run

**长期:**

- 5 原则要落到 evorule CLI(`evorule run` 也要有 candidate 工具审批流程)
- 5 原则要落到 evorule-application(每个 panel v1 → v2 加 AI 解释,保留 5 原则)

---

## 参考

- [`README.md`](README.md) — evo-agent 总览
- [`D:\evorule-application\STRATEGIC_DIRECTION.md`](../evorule-application/STRATEGIC_DIRECTION.md) — EvoRule 全生态战略
- `src/builtin_tools/mod.rs` — 6 工具的 3 层分类代码
- `src/builtin_tools/shell_exec.rs` — 8 active + 20 candidate + 28 blocked
- `src/builtin_tools/http_get.rs` — 6 active hosts + SSRF 防护
- 起源对话:**Mavis × EvoRule 作者,2026-07-20**

---

> "人类很乐意让 LLM 帮他们做所有事,但担心**不透明、失控、不可预测**。
> 所以白名单、备选、黑名单,应**全部列出**:让人类可选,说明影响,列表本身就是透明。"
> —— EvoRule 作者,2026-07-20
