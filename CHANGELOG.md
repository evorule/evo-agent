<!--
  Copyright 2026 EvoRule Project

  This program is free software: you can redistribute it and/or modify
  it under the terms of the GNU Affero General Public License as published by
  the Free Software Foundation, either version 3 of the License, or
  (at your option) any later version.

  This program is distributed in the hope that it will be useful,
  but WITHOUT ANY WARRANTY; without even the implied warranty of
  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
  GNU Affero General Public License for more details.

  You should have received a copy of the GNU Affero General Public License
  along with this program.  If not, see <https://www.gnu.org/licenses/>.

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# Evo-Agent 更新日志

`evo-agent`(EvoRule 生态的 Agent 编排层)的所有重要变更都记录在此。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/) v1.0,
本项目遵循 [语义化版本控制](https://semver.org/lang/zh-CN/) v2.0。

徽章说明:
- 🆕 新增
- 🔄 变更
- 🐛 修复
- 🗑 弃用
- ⚠️ Breaking Change
- 🔒 安全

---

## [Unreleased]

### 🆕 新增

#### 生态公共设施化（公共契约面 + 契约测试 + 治理文档）
- **公共契约面显式登记** — crate 级文档新增"公共契约面"章节：`LlmHandler`/`StreamChunk`/`ToolHandler` +
  `AuditedLlm` + `EvoruleApiClient`/`ApiError` + `IoHandler` + `config` 受 semver 约束
  （0.x 内 breaking 必须升 minor）；内部模块（builtin_tools/mcp/rule_tools/io_dispatcher/json_convert/metrics）
  标注 `#[doc(hidden)]`，真收窄留待 0.2.0
- **审计桥契约测试 +3** — 协议常量锁定（90s 超时/建链重试 1 次，防无声变更）、
  `is_transient_setup_error` 全分支（真实错误实例）、
  LLM 失败 → 错误回写 io_response 不留悬空 IoRequest（回归锁定）
- **`LlmHandler::with_max_retries`** — builder 风格重试次数覆盖（测试用 0 关闭退避延迟）
- **`NOTICE.md`** — AGPL-3.0 + 商业双许可声明 + 生态/第三方依赖清单（对齐生态范式）
- **`docs/RELEASE_PROCESS.md`** — 发布操作手册（git tag 形态 + crates.io 前置条件如实声明）
- **`verify.ps1`** — 一键验证：build → test → 防泄漏扫描 → 依赖契约断言
- **README"依赖契约"章节** — 引擎 crate 走 crates.io 版本依赖，仓外直接构建；path 依赖回流由 verify.ps1 断言拦截

### 🔄 变更

- **移除 `blake3` 直接依赖** — 零代码调用（仅文档注释提及概念），死依赖删除；
  evorule-reactor 自身对 blake3 的依赖不受影响

### 🐛 修复

- **lib.rs crate 级文档 mojibake** — 门面文档乱码修复为正常中文
- **工具循环在部分 LLM 供应商下静默不执行工具** — 工具描述(schema)此前未随 LLM 请求注入,
  模型只能凭训练先验盲猜工具名或输出供应商私有格式,循环一步即止且表现为假成功;
  现请求携带 OpenAI function 形状的工具描述(指令面与实际 LLM 请求面双接线),
  模型返回标准工具调用,多轮工具循环真实执行(新增集成测试锁定两条请求面)
- **demo 成功判据补业务落盘校验** — 原"退出码 + 审计链验证"双判据可被
  "什么都没做但审计如实记录"的假成功绕过;现要求业务文件真实写入(含金额记录)
  才宣告完成,失败时审计报告照常输出供排查(demo.ps1 / demo.sh 同步)

### 2026-09-07 变更

- **`verify.ps1` 布局断言 → 依赖契约断言** — 引擎 crate 已 crates.io 化，原"并排检出主仓"断言对外部用户必失败；
  现断言 Cargo.toml 中不存在 path 依赖（防回流），仓外 clone → verify 全 PASS
- **README"源码布局契约"章节改写为"依赖契约"** — 移除过时的并排检出要求
- **CHANGELOG 历史条目勘误** — `[1.0.0]` 为内部首发代号（未发布公开 tag），补注记澄清；修正失实链接与"首个公开发布"表述

## [0.1.0] - 2026-07-20

`evo-agent` 与 EvoRule 主线同步从 0.1.0 开始(原内部版本 1.0.0 退役)。

### 🆕 新增

- **AgentRunner 真实 LLM 调用** — `execute_llm_request` 通过 `LlmHandler` 走真实 HTTP API(替换原 stub)
- **tests/llm_real_smoke.py** — 4 场景真实 MiniMax 端到端测试

### 🔄 变更

- **协议统一为 AGPL-3.0-or-later**
- **.gitee/PULL_REQUEST_TEMPLATE.md** — 复用 evorule 的人类审查 checklist
- **65 个 .rs 文件 SPDX header** — 含 evo-agent 全部 17 个 + evorule-reactor/src/ffi.rs

### 🐛 修复

- **版本号 1.0.0 → 0.1.0** — 与 evorule 主线对齐
- **PR 模板 + Gitee CI** — D:\evo-agent\.gitee\ 复制

### ⚠️ Breaking Changes

- **API 不承诺后向兼容**(SemVer 0.x 阶段)
- **execute_tool_request 仍是 stub** — 0.2.0 修

---

## [1.0.0] - 2026-07-19（内部首发代号）

> **注记（2026-09-07 勘误）**：1.0.0 为内部开发期原始版本号，**未发布为公开 git tag**，已被 0.1.0（与 EvoRule 主线对齐）退役——首个公开版本是 `[0.1.0]`。下文为内部时期快照，历史原貌保留；其中"首个公开发布"及引擎版本号等表述以本注记为准。

内部时期功能基线。

### ⚠️ 已知限制(v1.0)

- ⚠️ **LLM Handler 是 stub**(`io_handlers/llm_handler.rs::execute_llm_request` 返回 `"Simulated LLM response"`)
- ⚠️ **Tool Handler 是 stub**(`io_handlers/tool_handler.rs::execute_tool_request` 返回 `"Tool execution result: ..."`)
- ⚠️ 124 warnings(主要是 `missing_docs`,可通过 `cargo fix --lib -p evo-agent` 一键补)
- ⚠️ 1 unused import(`serde_json::Value` at `src/agent/memory.rs:15`)

> **生产环境使用前,请实现真实的 LLM / Tool Handler**。详见 README 的"接入真实 LLM"章节。

### 🆕 新增

#### 核心模块(`src/agent/`)

- **`AgentRunner`**(~1042 行) — ReAct 主循环
  - 事件驱动架构
  - 完整 Fact 闭环:Command → IoRequest → 外部调用 → IoResponse → Stable
  - `AgentConfig` 配置(agent_type / system_prompt / model / temperature / max_steps / step_timeout / tool_names / llm_retry_count)
  - `AgentResult` 结果(success / content / steps / duration_ms / tool_calls / error)
  - 8 种错误类型:`LlmError` / `ToolError` / `Timeout` / `MaxStepsExceeded` / `DelegateError` / `MemoryError` / `Internal` / `EvoruleError`

- **`MemoryManager`**(~460 行) — 三层记忆管理
  - 命名空间约定:
    - 共享:`__memory__.agent_{type}.shared.{key}`
    - 会话:`__memory__.agent_{type}.session_{id}.{key}`
    - 短期:`__memory__.agent_{type}.session_{id}.messages.{idx}`
  - 通过 evorule `POST /api/sessions/{id}/payload` API 写入
  - 6 种错误:`Io` / `Json` / `EmptyKey` / `KeyTooLong` / `EvoruleError` / `SessionNotSet`
  - 自动重试 + 跨调用 ID 跟踪

- **`AgentDefinitionManager`**(~365 行) — 从 `agent.json` 加载 Agent 配置
  - `MemoryConfig` 字段(shared_keys 等)
  - `OutputFormat` 枚举
  - 3 种错误:`Io` / `Json` / `NotFound`

- **`ToolRegistry`**(~280 行) — 工具注册中心
  - `ToolFunction` async trait
  - `ToolSpec` / `ParameterSpec` 工具规格
  - 参数验证(必填检查)
  - 动态注册 / 注销

- **`Translator`**(~175 行) — LLM 响应解析
  - `Message` 枚举(System / User / Assistant / Tool)
  - `LlmResponse` 包装
  - `ToolCall` 解析

- **`Delegate`**(~135 行) — Agent 嵌套
  - `DelegateContext` 跨调用状态
  - 默认最大嵌套深度:`DEFAULT_MAX_DELEGATE_DEPTH = 3`

#### HTTP API(`src/api/`)

- **`EvoruleApiClient`**(~530 行) — 透传 19 个 evorule 端点
  - 会话管理:`create_session` / `fork_session` / `list_sessions`
  - 命令:`command` / `update_payload` / `state`
  - 时间机器:`replay` / `rewind` / `diff`
  - 审计:`audit` / `audit_verify`
  - 共享 Fact:`shared_facts` / `shared_fact_source` / `shared_fact_used_by`
  - 集群:`join` / `leave` / `cluster_status`
  - I/O 提交:`submit_io_response` / `record_used_at_startup`

- **`AgentApi`**(~240 行) — Evo-Agent 自有 HTTP 服务
  - `POST /api/agent/run` — 启动 Agent 运行
  - `GET /api/agent/list` — 列出已注册 Agent
  - `GET /api/agent/{type}` — 查看 Agent 详细定义
  - `AgentRunRequest` / `AgentRunResponse` / `AgentInfo` / `AgentDefinitionResponse` / `AgentListResponse`
  - axum 路由 + State 注入

#### I/O 抽象(`src/io_handlers/`)

- **`LlmHandler`** trait — LLM 调用抽象
  - `execute_llm_request(model, system_prompt, messages, tools) -> Result<JsonValue, String>`
  - **v1.0 状态**:`src/io_handlers/llm_handler.rs::execute_llm_request` 返回 `"Simulated LLM response"`
  - **接入真实 LLM** — 实现 trait,替换默认 handler

- **`ToolHandler`** trait — 工具调用抽象
  - `execute_tool_request(tool_name, args) -> Result<JsonValue, String>`
  - **v1.0 状态**:`src/io_handlers/tool_handler.rs::execute_tool_request` 返回 `"Tool execution result: ..."`

#### 工具模块

- **`io_dispatcher`**(~80 行) — I/O 派发
- **`io_handler`**(~15 行) — `IoHandler` trait + `IoResult`
- **`json_convert`**(~110 行) — `serde_json::Value` ↔ `evorule_tcb::JsonValue` 转换

### 🔄 变更

- **依赖**:
  - `evorule-tcb` (path: `../evorule/evorule-tcb`)
  - `evorule-reactor` (path: `../evorule/evorule-reactor`)
  - `tokio` (full features)
  - `reqwest` 0.12(异步 HTTP 客户端)
  - `axum` 0.8(HTTP 服务)
  - `serde` / `serde_json`
  - `prometheus` 0.13
  - `tracing` 0.1
  - `blake3`(审计链哈希)
  - `clap` 4(CLI)
- **dev-deps**:`tempfile` / `mockito` / `tokio-test`
- **Lints**:`#![forbid(unsafe_code)]` / `#![warn(unused_imports)]` / `#![warn(unused_variables)]` / `#![warn(missing_docs)]`

### 🔧 工程

- **Rust >= 1.74**
- **Cargo workspace**:独立 crate(`evo-agent` 在自己 `Cargo.toml` 里)
- **2 层架构**:
  - **机制层** = evorule (Rust,tier0/tier1/tier2)
  - **应用层** = evo-agent (本项目)
  - **通信**:HTTP + JSON(不是直接 import)
- **测试**:`tests/integration_test.rs`(260 行,mockito mock evorule-server)

### 🔒 安全

- **Bearer 认证**:通过 `EvoruleApiClient` 转发
- **输入验证**:JSON 指令 schema 校验
- **错误处理**:`AgentError` 8 种,显式 `From` 转换
- **限流**:通过 evorule-server `tower_governor` 实现

### 📜 协议

- **evo-agent 代码**:AGPL-3.0-or-later(与 evorule 主项目同步)
- **依赖的 evorule-server 协议**:HTTP + JSON
- **`core_eval.json` 宪法**:CC0 1.0 公共领域(由 evorule 维护)

---

## [未发布] - 1.1.0 计划

### 计划(短期)

- 🆕 真实 LLM Handler 实现(OpenAI / Anthropic / DeepSeek 至少一个)
- 🆕 真实 Tool Handler 实现(至少 1 个示例工具)
- 🆕 `cargo fix --lib` 补 124 warnings
- 🆕 移除 unused import

### 计划(中期)

- 🆕 流式输出(SSE 转 WebSocket 或 HTTP chunked)
- 🆕 Tool 调用错误重试 / 退避
- 🆕 Agent run timeout 保护(防止 Agent 死循环)
- 🆕 Memory 压缩 / 摘要(超长 session memory 摘要)

### 计划(长期)

- 🆕 嵌套 Agent 的 Fact 链可视化
- 🆕 Agent 行为追踪 / 调试 UI
- 🆕 多 Agent 协作(团队 Agent)

---

## 兼容性矩阵

| evo-agent | evorule-server | 状态 |
|---|---|---|
| 1.0.x | >= 6.0.0 | ✅ 当前 |
| 0.x | (未发布) | ❌ 计划从 1.0 起步 |

---

## 升级指南

### 0.x → 1.0.0(无 0.x 版本,跳过)

### 1.0.x → 1.0.y(patch)

无 breaking change,直接升级:
```bash
cargo update evo-agent
```

### 1.x → 2.x(未来 major)

待定。2.0 计划见上方"未发布"。

---

## 与 EvoRule 主项目的关系

```
┌────────────────────────────────────────┐
│  应用层:evo-agent (本项目)              │
│  - LLM 编排                            │
│  - 工具注册                            │
│  - 记忆管理                            │
│  - HTTP API                            │
└────────────────┬───────────────────────┘
                 │ HTTP + JSON
                 │ (不直接 import)
┌────────────────┴───────────────────────┐
│  机制层:evorule (独立仓库)              │
│  https://gitee.com/evorulelab/evorule  │
│  - evorule-tcb (核心)                    │
│  - evorule-reactor (反应器)              │
│  - evorule-governance (HTTP/SSE)         │
└────────────────────────────────────────┘
```

**关键设计**:**机制 vs 应用分离**。evo-agent 是**应用层**,通过 HTTP API 与 evorule(机制层)通信。**不**共享内存,**不**嵌入 evorule 进程。

这个分层是 EvoRule 的核心原则 — 让**机制可独立验证**(无业务污染),让**应用可独立演化**(LLM 升级不需改 evorule)。

---

## 与 SDK 的关系

| 项目 | 语言 | 角色 |
|---|---|---|
| **EvoRule 主项目** | Rust | 反应式执行引擎 |
| **evo-agent** | Rust | Agent 编排层(本项目) |
| **TypeScript SDK** | TypeScript | JS/TS 应用集成 evorule |
| **Python SDK** | Python | Python 应用集成 evorule |
| **Go SDK** | Go | (计划中) |
| **Java SDK** | Java | (计划中) |
| **Web SDK** | Web | (计划中) |

**evo-agent 和 SDK 的区别**:
- **evo-agent** — 内置 LLM 编排,直接跑 Agent
- **SDK** — 仅 HTTP API 客户端,需要你自己写 Agent 逻辑

---

## 历史背景

`evo-agent` 早期是 evorule 项目内部的一部分(在 `evorule/evorule-governance/` 中),后因 v4 教训("机制 vs 应用分离")被拆出到独立仓库。

**为什么拆出**:
- 机制层(确定性 / 可审计 / 可形式化验证)应独立
- 应用层(LLM / 业务 / 工作流)应可独立演化
- 边界清晰 = 各自可独立测试

**拆出后**:
- `evorule-governance` 只剩纯机制(无 agent 逻辑)
- `evo-agent` 自成项目,通过 HTTP API 与 evorule 通信
- 各自的测试 / CI / 发布独立

---

**作者**: EvoRule Project
**邮箱**: evorulelab@gmail.com
**Gitee**: https://gitee.com/evorulelab/evo-agent
**主项目**: https://gitee.com/evorulelab/evorule

---

**本变更日志遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/) v1.0 格式。**
