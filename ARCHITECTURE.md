# evo-agent · ARCHITECTURE

> AI Agent 编排层 — 通过 evorule HTTP API 实现 LLM + 工具 + 记忆的完整 ReAct 闭环

## 1. 概述

evo-agent 是在 evorule 反应式执行引擎之上构建的 AI Agent 编排层。它不重新发明状态机,也不内嵌 LLM 客户端 — 所有 LLM 调用、工具调用、记忆读写都转成 evorule 的 `IoRequest` 事件,由 evorule 反应器负责执行、审计、回滚。

**与 evorule-agent 的关系**:两者都是 evorule 引擎上的 Agent 实现,evorule-agent 是**库形态**的大脑主控运行时(同进程、共享类型、零 HTTP),evo-agent 是**服务形态**的 HTTP 解耦编排层(跨进程、JSON 通讯、19 个 evorule 端点)。两者都把 evorule 作为"身体",但绑定深度与适用场景不同。

**项目状态**:`v0.1.0` (2026-07-20),158/158 单元测试通过,完整 Fact 闭环 + 三层记忆 + DAG 工作流 + 3 层安全模型已落地。

---

## 2. 2-Loop 解耦架构

evo-agent 与 evorule-server 通过 HTTP + SSE 通讯,不共享内存、不嵌入进程 — **机制层与应用层彻底解耦**:

```
┌──────────────────────────────┐         ┌──────────────────────────────┐
│  evo-agent (应用层)            │         │  evorule-server (机制层)      │
│                              │         │                              │
│  ┌──────────────────────┐    │         │  ┌──────────────────────┐    │
│  │  Application Loop    │    │         │  │  Reactor Loop        │    │
│  │                      │    │         │  │                      │    │
│  │  for step in 0..N:   │    │         │  │  drain command       │    │
│  │    LLM call          │    │  HTTP   │  │  stable detect       │    │
│  │    parse resp        │    │ ──────> │  │  block on cmd_rx     │    │
│  │    handle io_request │    │ <────── │  │  while pending_io=0: │    │
│  │    POST io_response  │    │   SSE   │  │    execute_transition│    │
│  └──────────────────────┘    │         │  └──────────────────────┘    │
│           ▲                  │         │           │                  │
│  ┌──────────────────────┐    │         │  ┌──────────────────────┐    │
│  │  MemoryManager       │    │         │  │  FactsLog (append)   │    │
│  │  ToolRegistry        │    │         │  │  + causal chain      │    │
│  │  AgentDefinition     │    │         │  │  + WAL               │    │
│  │  ApprovalCallback    │    │         │  │  + audit_verify      │    │
│  │  WorkflowEngine      │    │         │  │                      │    │
│  └──────────────────────┘    │         │  └──────────────────────┘    │
└──────────────────────────────┘         └──────────────────────────────┘
```

**完整 Fact 闭环**:
1. AgentRunner 发 Command → `POST /api/sessions/{id}/command`
2. evorule 反应器产生 IoRequest → SSE 推 `io_request` 事件
3. AgentRunner 执行外部调用(LLM/工具)→ `POST /api/sessions/{id}/io_response`
4. evorule 产生 IoResponse + StateTransition → SSE 推 `stable` 事件
5. AgentRunner 收到 stable,继续下一步或返回 `AgentResult`

---

## 3. 核心模块

| 模块 | 文件 | 职责 |
|---|---|---|
| **AgentRunner** | `src/agent/runner.rs` | ReAct 主循环,事件驱动架构,完整 Fact 闭环 |
| **MemoryManager** | `src/agent/memory.rs` | 三层记忆管理(shared / session / messages) |
| **AgentDefinitionManager** | `src/agent/definition.rs` | 从 `agent.json` 加载 Agent 定义 |
| **ToolRegistry / ToolHandler** | `src/agent/tool_registry.rs` / `src/io_handlers/tool_handler.rs` | 工具注册中心 + 动态调度 |
| **DelegateContext** | `src/agent/delegate.rs` | Agent 嵌套(深度 + 并发限流) |
| **WorkflowEngine** | `src/agent/workflow.rs` | DAG 拓扑编排多 agent |
| **ContextWindowManager** | `src/agent/context_window.rs` | Token 计数 + 消息裁剪 |
| **OutputValidator** | `src/agent/output_validator.rs` | LLM 输出 JSON Schema 校验 |
| **MemoryEventStore** | `src/agent/memory_event/store.rs` | 结构化记忆事件 + 因果链 |
| **EvoruleApiClient** | `src/api/evorule_client.rs` | 19 个 evorule 端点透传 |
| **AgentApi** | `src/api/agent_api.rs` | evo-agent 自有 HTTP 服务(3 个端点) |
| **builtin_tools** | `src/builtin_tools/*` | 6 个内置工具 + 3 层安全模型 |
| **MCP 集成** | `src/mcp/*` | Model Context Protocol 客户端(stdio) |

---

## 4. 三层记忆(P1 设计)

`src/agent/memory.rs:7-11` 定义命名空间约定:

| 命名空间 | 用途 | 生命周期 |
|---|---|---|
| `__memory__.agent_{type}.shared.{key}` | 跨会话共享知识 | 永久 |
| `__memory__.agent_{type}.session_{id}.{key}` | 单会话私有状态 | 会话期间 |
| `__memory__.agent_{type}.session_{id}.messages.{idx}` | 短期消息历史 | 会话期间 |
| `__memory__.agent_{type}.session_{id}.summary` | 会话摘要 | 会话期间 |
| `__memory__.agent_{type}.session_{id}.meta` | 会话元数据 | 会话期间 |

**所有记忆通过 `POST /api/sessions/{id}/payload` 写入 evorule**,所以:
- 记忆本身就是 Fact,可回放、可审计
- `used_at_startup` 记录启动时引用了哪些 shared fact
- `facts_by_prefix` 支持路径前缀查询

**结构化记忆事件**(P2,`src/agent/memory_event/`):
- `MemoryEvent` + `Entity` + `Causal Chain` — 人生事件 / 对话里程碑 / 情感时刻的结构化记录
- `EventExtractor` 从对话/工具结果中自动提取
- `ReplayEngine` 因果链遍历 + 确定性回放

---

## 5. 3 层安全模型(`active` / `candidate` / `blocked`)

`DESIGN_PRINCIPLES.md` 定义的 5 原则中"可选"与"可控"的具体落地:

| 类别 | 行为 | 示例(以 `shell_exec` 为例) |
|---|---|---|
| **active** | 白名单,直接执行,无需请示 | `cargo` / `git` / `ls` / `cat` 等 8 个 |
| **candidate** | 备选,LLM 想用 → 返回 proposal → 用户批 → 再执行 | `rm` / `mv` / `sed` / `chmod` 等 20 个 |
| **blocked** | 永不允许(逃逸出口 / 不可逆破坏) | `sudo` / `bash` / `python` / `curl` / `kill` 等 28 个 |

**`propose` 协议**(统一格式,所有 candidate 一致):
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

**通用安全机制**(`src/builtin_tools/mod.rs:19-37`):
1. **白名单 + 默认 deny**:`shell_exec` 只允许列出的命令;`http_get` 只允许列出的 host
2. **工作目录沙箱**:`file_*` / `search_files` 拒绝绝对路径 + `..` + symlink 逃逸
3. **资源限制**:size limit / max_results / timeout
4. **No shell**:`shell_exec` 走 `std::process::Command` 直接 exec,不经任何 shell 解析
5. **SSRF 防护**:`http_get` 硬编码黑名单(127/8, 10/8, 172.16/12, 192.168/16, 169.254/16, IPv6 fe80/fc00/::1)

**6 个内置工具**:
- `file_read` / `file_list` / `file_write` / `search_files` — 文件 I/O(沙箱)
- `shell_exec` — 8 active + 20 candidate + 28 blocked
- `http_get` — 6 active hosts + SSRF 防护

---

## 6. Agent 嵌套与工作流

### Delegate(G9,`src/agent/delegate.rs`)

Agent 可调用其他 Agent,3 种模式:

| 方法 | 行为 | 适用 |
|---|---|---|
| `delegate()` | 串行单子 agent | 强依赖 |
| `delegate_parallel()` | 并行多子 agent(`join_all`) | 独立子任务 |
| `delegate_race()` | 竞速(任一完成即返回,取消其余) | 多源探查 |

**安全强制**:
- 深度强制:`delegate()` 检查 `current_depth >= max_depth`,超限直接 `Err`(防无限递归)
- 默认深度 `DEFAULT_MAX_DELEGATE_DEPTH = 3`
- 并发限流:`max_concurrent` 用 `tokio::sync::Semaphore` 限制并行子 agent 数,防止 evorule session 数暴增

### Workflow(`src/agent/workflow.rs`)

DAG(有向无环图)拓扑编排多 agent,用 JSON DSL 定义:

```json
{
  "workflow_id": "research_and_write",
  "nodes": [
    { "id": "research_rust",   "agent_type": "researcher", "task": "...", "depends_on": [] },
    { "id": "research_python", "agent_type": "researcher", "task": "...", "depends_on": [] },
    { "id": "write_report",    "agent_type": "writer",
      "task_template": "基于: {research_rust} 与 {research_python}",
      "depends_on": ["research_rust", "research_python"] }
  ],
  "output_node": "write_report"
}
```

**执行算法**:
1. 拓扑排序(Kahn 分层):按 `depends_on` 把节点分成若干层,同层无互相依赖
2. 逐层执行:同层节点并行(`delegate_parallel`)
3. 模板渲染:下一层的 `task_template` 中 `{node_id}` 被上游结果替换
4. 任一节点失败 → 整个工作流终止,返回 `Err`
5. 返回 `output_node` 的结果

---

## 7. Agent 定义与桥接

`agents/{type}.json` 描述 Agent:

```json
{
  "agent_type": "researcher",
  "version": "0.1.0",
  "description": "Research agent — summary and search",
  "system_prompt": "You are a careful research assistant.",
  "model": "MiniMax-M2.5",
  "temperature": 0.3,
  "max_steps": 20,
  "step_timeout_secs": 60,
  "tools": ["file_read", "search_files", "file_list"]
}
```

**桥接流程**(`AgentRunner::from_definition`,`src/agent/runner.rs:427-`):
1. `AgentDefinitionManager::load("researcher")` 加载 JSON
2. 构造 `EvoruleApiClient`(默认 `EVORULE_AUTH_TOKEN` 环境变量)
3. `default_safe_toolkit(workdir)` 装 6 个工具
4. `AgentRunner::from_definition(def, client, tool_handler)` 一步组装
5. 早失败校验:`def.tools` 全部已在 `tool_handler` 注册(否则 `Err`)

**配置文件**(`evo-agent.toml` + `examples/config.toml`):
- 4 子结构 + 3 层加载(default + user + project + env)
- `${ENV:VAR}` 占位符

---

## 8. 5 原则宪法(`DESIGN_PRINCIPLES.md`)

所有新功能/新工具/新 API 的 review checklist:

| # | 原则 | 落地方式 |
|---|---|---|
| 1 | **透明** | `tools list` 列全部 6 工具 + 3 层分类;`config` 打印完整合并后配置;`validate` 跑前告知 |
| 2 | **可选** | active / candidate / blocked 三层(不是 2 选 1) |
| 3 | **可控** | `propose` 协议 + 候选工具默认拒绝 + `--auto-approve-candidates` 显式 flag |
| 4 | **可回放** | evorule fact log 录像带 + replay / diff / rewind 端点 |
| 5 | **可审计** | append-only blake3 哈希链 + `audit_verify` 端点 + 每 run 输出 JSON |

**反模式清单**(出现任一,设计 review 应被打回):
- 默默执行用户没看到 / 没批准的事
- "二选一"硬切(应给 active / candidate / blocked)
- 把 JSON 当代码的语法糖
- 隐藏错误或失败原因
- log 不可回放 / 可被改
- 自动化流程没有"暂停点"让用户干预

---

## 9. HTTP API 概览

### evo-agent 自有 API(3 个端点)

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/agent/list` | 列出已注册 Agent 类型 |
| `GET` | `/api/agent/{type}` | 查看 Agent 详细定义 |
| `POST` | `/api/agent/run` | 启动 Agent 运行 |

### 通过 `EvoruleApiClient` 透传到 evorule-server(19 个端点)

- **会话管理**:`create_session` / `fork_session` / `list_sessions`
- **命令**:`command` / `update_payload` / `state`
- **时间机器**:`replay` / `rewind` / `diff`
- **审计**:`audit` / `audit_verify` / `get_causal_chain`
- **共享 Fact**:`shared_facts` / `shared_fact_source` / `shared_fact_used_by`
- **集群**:`join` / `leave` / `cluster_status`
- **I/O 提交**:`submit_io_response` / `record_used_at_startup` / `get_used_at_startup`

### WebSocket 流(`src/api/ws_handler.rs`)

`run_streaming()` 产出的 `AgentEvent` 流(SessionCreated / Step / Delta / ToolCall / Done)经 WebSocket 双向推给前端,支持 LLM 流式输出与外部取消信号。

---

## 10. 关键抽象

| 抽象 | Trait | 用途 | 0.1.0 状态 |
|---|---|---|---|
| `LlmHandler` | `async_trait` | LLM 调用抽象 | **stub**(返回模拟响应,用户需自行实现) |
| `ToolHandler` | `async_trait` | 工具注册与调度 | 6 个内置工具已注册 |
| `ApprovalCallback` | `async_trait` | 候选工具审批 | `CliApproval` / `HttpApproval` / `DenyAll` / `AutoApprove` |
| `EventCallback` | `async_trait` | 结构化事件回调链 | `LoggingCallback` / `MetricsCallback` / `CallbackChain` |
| `TokenCounter` | `async_trait` | 上下文窗口计数 | `ApproxTokenCounter`(无外部依赖,CJK-aware) |

---

## 11. 依赖关系

```toml
# Cargo.toml 关键依赖
evorule-tcb = { path = "../evorule/evorule-tcb" }        # 反应式执行内核
evorule-reactor = { path = "../evorule/evorule-reactor" } # 反应器 + FactsLog

reqwest = { version = "0.12", features = ["json", "stream"] }  # HTTP 客户端
axum = "0.8"                                            # 自有 HTTP 服务
tokio = { version = "1", features = ["full"] }          # 异步运行时
serde / serde_json = "1"                                # JSON 序列化
prometheus = "0.13"                                     # 指标
tracing = "0.1"                                         # 结构化日志
blake3 = "1"                                            # 审计链哈希
clap = "4"                                              # CLI
jsonschema = "0.18"                                     # LLM 输出 JSON Schema 校验(G11)
tokio-util = "0.7"                                      # CancellationToken(G6)
subtle = "2"                                            # 恒定时间比较(防时序攻击,G7)
rustyline = "14"                                        # REPL 行编辑(G15)
```

**`#![forbid(unsafe_code)]` 全栈适用**。

---

## 12. CLI 子命令(`src/bin/evo-agent.rs`)

| 子命令 | 用途 |
|---|---|
| `run <goal>` | 跑 agent(给一个 goal + 可选 agent 类型) |
| `list` | 列出 `agents/` 目录下的所有 agent |
| `tools list` | 列出 6 个工具(active/candidate/blocked 3 层) |
| `tools show <name>` | 显示单个工具的 3 层详情 |
| `validate <agent>` | 校验 agent.json 是否合法 |
| `config` | 显示合并后的配置(default + user + project + env) |

---

## 13. 测试

- **单元测试**:158/158 通过
- **集成测试**:`tests/integration_test.rs`(mockito mock evorule-server,260 行)
  - `auto_recall`(启动时拉取 shared facts)
  - ReAct 主循环的 SSE 事件驱动
  - 工具调用闭环
  - 错误处理路径
- **真实 LLM 烟测**:`tests/llm_real_smoke.py`(4 场景真实 MiniMax 端到端)

---

## 14. 许可

AGPL-3.0-or-later(与 evorule 主项目同步);`core_eval.json` 宪法部分采用 CC0-1.0 公共领域。
