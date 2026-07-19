# Evo-Agent

> AI Agent 编排层 —— 在 evorule 反应式执行引擎之上,实现 LLM + 工具 + 记忆的完整 ReAct 闭环。

[![Rust](https://img.shields.io/badge/rust-1.74%2B-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![Version](https://img.shields.io/badge/version-1.0.0-green.svg)](Cargo.toml)

---

## 一句话定位

**Evo-Agent = LLM 大脑 + 工具手脚 + 持久记忆,执行过程通过 evorule 引擎留下可审计的 Fact 链。**

它不重新发明状态机,也不内嵌 LLM 客户端,而是把 LLM 调用 / 工具调用 / 记忆读写都转成 evorule 的 `IoRequest` 事件,
由 evorule 反应器负责执行、回滚、审计 —— Agent 层只关心"下一步该干什么"。

---

## 核心特性

| 特性 | 说明 |
|---|---|
| 🔁 **完整 Fact 闭环** | 每次 LLM 调用、工具调用、记忆读写都生成可审计 Fact,支持 rewind / replay / diff |
| 🧠 **三层记忆** | 共享记忆 / 会话记忆 / 短期消息,通过 evorule payload API 持久化,跨会话可追溯 |
| 🛠 **工具注册中心** | `ToolRegistry` + `ToolFunction` trait,任何 `async fn(JsonValue) -> Result<JsonValue, String>` 都能注册 |
| 🔌 **可插拔 LLM/工具** | `LlmHandler` / `ToolHandler` 接口,实现后即可接入 OpenAI / Anthropic / DeepSeek / 自定义工具 |
| 📡 **HTTP + SSE 通信** | 与 evorule-server 通过标准 REST + SSE 通信,不共享内存、不嵌入进程 —— **机制层与应用层彻底解耦** |
| 🌐 **自带 HTTP API** | `POST /api/agent/run` 启动一个 Agent 运行,`GET /api/agent/list` 列出已注册 Agent 类型 |
| 🪆 **Agent 嵌套 (Delegate)** | Agent 可调用其他 Agent,最大嵌套深度通过 `DEFAULT_MAX_DELEGATE_DEPTH = 3` 限制 |
| 🔒 **零 unsafe** | `#![forbid(unsafe_code)]` 全栈适用 |

---

## 架构:2-Loop 解耦

Evo-Agent 不是 evorule 的"插件",而是一个**独立的应用层**,通过 HTTP API 与 evorule 引擎对话:

```
┌──────────────────────────────┐         ┌──────────────────────────────┐
│  Evo-Agent (应用层)            │         │  evorule-server (机制层)      │
│                              │         │                              │
│  ┌──────────────────────┐    │         │  ┌──────────────────────┐    │
│  │  Application Loop    │    │         │  │  Reactor Loop        │    │
│  │                      │    │         │  │                      │    │
│  │  for step in 0..N:   │    │         │  │  drain command       │    │
│  │    LLM call  ───┐    │    │  HTTP   │    │  stable detect       │    │
│  │    parse resp   │    │    │ ──────> │    │  block on cmd_rx     │    │
│  │    handle io_   │    │    │ <────── │    │  while pending_io=0: │    │
│  │      request    │    │    │   SSE   │    │    execute_transition│    │
│  │    POST io_resp │    │    │         │    │                      │    │
│  └──────────────────────┘    │         │  └──────────────────────┘    │
│           ▲                  │         │           │                  │
│           │                  │         │           ▼                  │
│  ┌──────────────────────┐    │         │  ┌──────────────────────┐    │
│  │  MemoryManager       │    │         │  │  FactsLog (append)   │    │
│  │  ToolRegistry        │    │         │  │  + causal chain      │    │
│  │  AgentDefinition     │    │         │  │  + WAL               │    │
│  └──────────────────────┘    │         │  └──────────────────────┘    │
└──────────────────────────────┘         └──────────────────────────────┘
```

**为什么分开?** 因为 v4 教训过我们:把"机制"和"应用"塞进同一个进程,会导致确定性 / 可审计性 / 可形式化验证的边界污染。
EvoRule 的 TCB (tier0-tcb) 永远只做加减与因果链,EvoAgent 的所有"业务逻辑"都在它之外。
HTTP 是它们的**唯一契约**。

---

## 快速开始

### 前置条件

- Rust 1.74+
- 一个跑起来的 evorule-server(默认 `http://127.0.0.1:18080`)
- 一个 LLM provider 的 API key(下面会讲怎么接入)

### 启动 evorule-server

```bash
cd ../evorule
cargo build --bin evorule-server
./target/debug/evorule_server --addr 127.0.0.1:18080
```

### 启动 Evo-Agent HTTP API

```bash
cargo run --release
# 默认监听 127.0.0.1:18081
```

### 跑一个 Agent

```bash
curl -X POST http://127.0.0.1:18081/api/agent/run \
  -H "Content-Type: application/json" \
  -d '{
    "agent_type": "researcher",
    "goal": "总结 EvoRule 的 4 个核心特性"
  }'
```

返回:

```json
{
  "success": true,
  "content": "...",
  "steps": 3,
  "duration_ms": 4521,
  "error": null
}
```

### 列出所有可用 Agent

```bash
curl http://127.0.0.1:18081/api/agent/list
```

---

## 核心组件

### `AgentRunner` —— ReAct 循环

文件:`src/agent/runner.rs` (~1040 行)

事件驱动的 ReAct 主循环:

```rust
for step in 0..max_steps {
    // 1. 读 session state
    // 2. 调用 LLM(io_request: call_external)
    // 3. 解析 LLM 响应(content 或 tool_calls)
    // 4. 如果是 tool_call: 执行工具(io_request: call_service)
    // 5. 写回消息历史到 session payload
    // 6. 等待 stable 事件
    // 7. 返回 AgentResult
}
```

**关键设计:**
- **不内嵌 LLM SDK** —— 通过 `LlmHandler` trait 注入
- **不直接调工具** —— 通过 `ToolHandler` trait 注入
- **不本地存记忆** —— 全部走 evorule `payload` API

### `MemoryManager` —— 三层记忆

文件:`src/agent/memory.rs` (~460 行)

| 命名空间 | 用途 | 生命周期 |
|---|---|---|
| `__memory__.agent_{type}.shared.{key}` | 跨会话共享知识 | 永久 |
| `__memory__.agent_{type}.session_{id}.{key}` | 单会话私有状态 | 会话期间 |
| `__memory__.agent_{type}.session_{id}.messages.{idx}` | 短期消息历史 | 会话期间 |

**所有记忆通过 `POST /api/sessions/{id}/payload` 写入**,所以:
- 记忆本身就是 Fact,可回放、可审计
- `used_at_startup` 记录启动时引用了哪些 shared fact
- `facts_by_prefix` 支持路径前缀查询

### `ToolRegistry` —— 工具注册

文件:`src/agent/tool_registry.rs` (~280 行)

```rust
use async_trait::async_trait;
use evo_agent::agent::{ToolSpec, ParameterSpec, ToolRegistry};
use tier0_tcb::JsonValue;

struct WebSearchTool;

#[async_trait]
impl ToolFunction for WebSearchTool {
    async fn call(&self, args: &JsonValue) -> Result<JsonValue, String> {
        let query = args["query"].as_str().ok_or("missing query")?;
        // ... 实际搜索逻辑
        Ok(json!({"results": [...]}))
    }
}

// 注册
let mut registry = ToolRegistry::new();
registry.register(
    ToolSpec {
        name: "web_search".into(),
        description: "在 Web 上搜索关键词".into(),
        parameters: vec![ParameterSpec {
            name: "query".into(),
            r#type: "string".into(),
            description: "搜索关键词".into(),
            required: true,
        }],
        required: vec!["query".into()],
    },
    Arc::new(WebSearchTool),
);
```

### `AgentDefinitionManager` —— Agent 配置加载

文件:`src/agent/definition.rs` (~365 行)

从 `agents/{type}/agent.json` 加载 Agent 定义:

```json
{
  "agent_type": "researcher",
  "version": "1.0.0",
  "description": "研究型 Agent,擅长信息搜集与综合",
  "system_prompt": "你是一个严谨的研究助手...",
  "model": "gpt-4o-mini",
  "temperature": 0.3,
  "max_steps": 15,
  "tools": ["web_search", "fetch_url"],
  "memory": {
    "shared_keys": ["user.profile", "research.preferences"]
  }
}
```

---

## API 概览

### Evo-Agent 自有 API

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/agent/list` | 列出所有已注册 Agent 类型 |
| `GET` | `/api/agent/{type}` | 查看指定 Agent 详细定义 |
| `POST` | `/api/agent/run` | 启动一个 Agent 运行 |

### 通过 `EvoruleApiClient` 透传到 evorule-server

19 个端点(2026-07-19 实测),包括:
- `create_session` / `fork_session` / `list_sessions`
- `command` / `update_payload` / `state`
- `replay` / `rewind` / `diff` / `history`
- `audit` / `audit_verify`
- `shared_facts` / `shared_fact_source` / `shared_fact_used_by`
- `join` / `leave` / `cluster_status`
- `submit_io_response` / `record_used_at_startup`

详细方法签名见 [`src/api/evorule_client.rs`](src/api/evorule_client.rs)。

---

## 接入真实 LLM

**当前状态:** `LlmHandler::call()` 返回模拟响应,`ToolHandler::call()` 同理 —— 这是 v1.0 的占位实现。

要接入真实 LLM,实现 `LlmHandler` trait 即可:

```rust
use evo_agent::io_handlers::LlmHandler;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct OpenAIHandler {
    api_key: String,
    base_url: String,
}

#[async_trait]
impl LlmHandler for OpenAIHandler {
    async fn call(
        &self,
        model: &str,
        system_prompt: &str,
        messages: Vec<Value>,
        tools: Vec<Value>,
    ) -> Result<Value, String> {
        let client = reqwest::Client::new();
        let body = json!({
            "model": model,
            "messages": [
                { "role": "system", "content": system_prompt },
                ...messages,
            ],
            "tools": tools,
        });
        // ... POST 到 OpenAI API
        todo!("实现真实调用")
    }
}
```

然后在 `main.rs` 里替换默认 handler。

> ⚠️ **本仓库不会替你写 OpenAI/Anthropic/DeepSeek 的具体调用** —— 这是用户自行扩展的部分。
> 我们提供的只是抽象,目的是把"机制"(evorule)和"应用"(具体 LLM)彻底分开。

---

## 测试

```bash
# 单元 + 集成测试
cargo test --workspace

# 只跑集成(mock evorule-server)
cargo test --test integration_test

# 跑 evorule-server 真实端到端
# 1. 启动 evorule-server
# 2. cargo test -- --ignored --test-threads=1
```

集成测试用 `mockito` mock evorule-server,覆盖:
- `auto_recall`(启动时拉取 shared facts)
- ReAct 主循环的 SSE 事件驱动
- 工具调用闭环
- 错误处理路径

---

## 已知限制 & 路线图

### v1.0 限制

- ⚠️ **LLM Handler 是 stub**(返回 `"Simulated LLM response"`)
- ⚠️ **Tool Handler 是 stub**(返回 `"Tool execution result: ..."`)
- ⚠️ **0 errors / 124 warnings**(主要是 `missing_docs`,`cargo fix --lib` 可一键补)
- ⚠️ **LICENSE 文件待写**(战略:AGPL-3.0)

### 路线图

| 阶段 | 目标 | 预计 |
|---|---|---|
| v1.0.1 | LLM 真实集成(OpenAI 兼容协议) | 1-2 天 |
| v1.0.1 | Tool 真实集成(至少 1 个示例) | 1 天 |
| v1.0.1 | 124 warnings 清零 | 30 分钟 |
| v1.0.1 | LICENSE / README / 1 个 example | 1 天 |
| v1.1 | Agent 流式输出(SSE) | 1 周 |
| v1.1 | Tool 调用错误重试 / 退避 | 3 天 |
| v1.2 | 嵌套 Agent 之间的 Fact 链可视化 | 2 周 |

---

## 目录结构

```
evo-agent/
├── Cargo.toml                        # 依赖 + path references
├── README.md                         # 本文件
├── src/
│   ├── lib.rs                        # 入口 + 公共导出
│   ├── agent/
│   │   ├── runner.rs                 # ⭐ ReAct 主循环 (~1040 行)
│   │   ├── memory.rs                 # ⭐ 三层记忆管理 (~460 行)
│   │   ├── definition.rs             # Agent 配置加载 (~365 行)
│   │   ├── tool_registry.rs          # 工具注册中心 (~280 行)
│   │   ├── translator.rs             # LLM 响应解析 (~175 行)
│   │   ├── delegate.rs               # Agent 嵌套上下文 (~135 行)
│   │   └── mod.rs
│   ├── api/
│   │   ├── evorule_client.rs         # ⭐ 19 个 evorule 端点 (~530 行)
│   │   ├── agent_api.rs              # ⭐ /api/agent/* 路由 (~240 行)
│   │   └── mod.rs
│   ├── io_handlers/                  # LLM/Tool 抽象层(stub)
│   │   ├── llm_handler.rs            #   LlmHandler trait
│   │   ├── tool_handler.rs           #   ToolHandler trait
│   │   └── mod.rs
│   ├── io_dispatcher.rs              # I/O 分发器(预留扩展)
│   ├── io_handler.rs                 # I/O handler 基类 trait
│   └── json_convert.rs               # serde ↔ tcb::JsonValue 转换
└── tests/
    └── integration_test.rs           # mockito 端到端测试
```

---

## 依赖关系

```toml
# Cargo.toml 关键依赖
tier0-tcb = { path = "../evorule/tier0-tcb" }        # 反应式执行内核
tier1-reactor = { path = "../evorule/tier1-reactor" } # 反应器 + FactsLog

reqwest = { version = "0.12", features = ["json", "stream"] }  # HTTP 客户端
axum = "0.8"                                            # 自有 HTTP 服务
tokio = { version = "1", features = ["full"] }          # 异步运行时
serde / serde_json = "1"                                # JSON 序列化
prometheus = "0.13"                                     # 指标
tracing = "0.1"                                         # 结构化日志
```

**零 unsafe**:`#![forbid(unsafe_code)]` 在所有 module 强制。

---

## 相关项目

- [evorule](../evorule) — 反应式执行引擎(tier0/tier1/tier2)
- [evorule/sdk/typescript](../evorule/sdk/typescript) — TypeScript SDK
- [evorule/sdk/python](../evorule/sdk/python) — Python SDK(规划中)

---

## License

[AGPL-3.0](LICENSE) —— 详见 `oss_strategy.md` 中的协议决策记录。

> 这是**整个 EvoRule 生态**的协议,不只是 evo-agent 单独的协议。
> 我们的立场是"不白送":大厂 fork 之后想"卖闭源 SaaS"也得开源他们的服务。
> 内部用 AGPL 管不到(也没必要),但 fork 这个行为本身 = 我们的胜利。
