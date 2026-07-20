# Evo-Agent

> AI Agent 编排层 —— 在 evorule 反应式执行引擎之上,实现 LLM + 工具 + 记忆的完整 ReAct 闭环。

[![Rust](https://img.shields.io/badge/rust-1.74%2B-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![Version](https://img.shields.io/badge/version-0.1.0-green.svg)](Cargo.toml)

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

## CLI 用法

evo-agent 自带一个独立 CLI,适合**单次跑任务**或**人工审计工具安全模型**。先 build:

```bash
cargo build --release --bin evo-agent
# 或在 D:\evo-agent\ 下:
#   target\release\evo-agent.exe
```

### 5 个子命令

```text
evo-agent run <goal>           # 跑 agent(给一个 goal + 可选 agent 类型)
evo-agent list                  # 列出 agents/ 目录下的所有 agent
evo-agent tools list            # 列出 6 个工具(active/candidate/blocked 3 层)
evo-agent tools show <name>     # 显示单个工具的 active/candidate/blocked 详情
evo-agent validate <agent>      # 校验 agent.json 是否合法
evo-agent config                # 显示合并后的配置(default + user + project + env)
```

### 示例:跑一个 agent

```bash
# 准备 agent.json
mkdir -p agents
cat > agents/researcher.json <<'JSON'
{
  "agent_type": "researcher",
  "version": "0.1.0",
  "description": "Research agent",
  "system_prompt": "You are a careful research assistant.",
  "model": "MiniMax-M2.5",
  "temperature": 0.3,
  "max_steps": 20,
  "step_timeout_secs": 60,
  "tools": ["file_read", "search_files"]
}
JSON

# 跑
MINIMAX_API_KEY=sk-... \
  ./target/release/evo-agent run "总结一下 README" -a researcher
```

> **5 原则落地**(详见 [`DESIGN_PRINCIPLES.md`](DESIGN_PRINCIPLES.md)):
> - **透明**:`config` 输出完整合并后配置;`tools list/show` 把 active/candidate/blocked 全列出来
> - **可选**:用户能选 active(白名单)/candidate(待批)/blocked(永不)三档
> - **可控**:candidate 工具默认拒绝;带 `--auto-approve-candidates` 才放行
> - **可回放**:每次 run 输出结构化 JSON 结果,后续 0.2.0 接 evorule fact log
> - **可审计**:`run` 结果(成功/失败/步骤数/工具调用列表)是 JSON,适合归档

### 示例:看 3 层安全模型

```bash
$ evo-agent tools list
=== 6 Built-in Tools (3-layer security model) ===

[ACTIVE] 直接执行(无需请示):
  - file_read
  - file_list
  - file_write
  - search_files
  - shell_exec
  - http_get

[CANDIDATE] 备选(LLM 想用 → 摊开 proposal 给你看 → 你批 → 再执行):
  - rm — 删除文件或目录 (risk: 误删不可逆;rm -rf 没有提示)
  - mv — 移动/重命名文件 (risk: 覆盖现有文件无提示;...)
  ...

[BLOCKED] 永不批准(逃逸出口 / 不可逆破坏):
  - sudo — 权限提升 — 跨安全边界
  - python — Turing-complete — 任何操作都可做
  - bash — shell 逃逸 — 绕过白名单
  ...
```

### `run` 完整参数

```text
Usage: evo-agent run [OPTIONS] <GOAL>

Arguments:
  <GOAL>    任务描述(给 agent 的指令)

Options:
  -a, --agent <AGENT>              agent 类型名(默认 config.agents.default)
      --auto-approve-candidates    自动批准 candidate 工具(0.1.0 默认拒绝,带此 flag 则放行)
  -v, --verbose                    详细输出(debug logging)
      --workdir <WORKDIR>          工作目录(默认当前目录)
  -h, --help                       Print help
```

### Windows PowerShell 注意

PowerShell 5.1 用 `''` 单引号传 JSON 会吃掉 `"`,所以 CLI 的 payload 参数用 `--payload-file` 而不是 inline:

```powershell
# ❌ 不行(单引号会吃掉 ")
evo-agent run '{\"x\":10}'

# ✅ 用文件
'{"x":10}' | Set-Content -Encoding utf8 payload.json
evo-agent run --payload-file payload.json ...
```

> **0.1.0 状态**:`run` 命令已能跑通 agent 桥接(可跑全栈:`Config::load` → `default_safe_toolkit` → `AgentRunner::from_definition`),但**真实 LLM 调用 + SSE 事件循环**还在等 evorule-server 端到端测试。`list` / `tools list` / `tools show` / `config` / `validate` 5 个命令**完全可用**。

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

### v0.1.0 状态(2026-07-20)

**已完成 ✅:**

- ✅ **LLM Handler 真实集成**(reqwest 调用 OpenAI 兼容 API,支持 minimax / DeepSeek / OpenAI)
- ✅ **6 个内置工具**:`file_read` / `file_list` / `file_write` / `search_files` / `shell_exec` / `http_get`
- ✅ **3 层安全模型**:active(8 shell 命令 + 6 http 主机) / candidate(20 shell + 任意公开 host) / blocked(28 shell + SSRF 黑名单)
- ✅ **propose 协议**:candidate 工具返回 `{status: "needs_approval", description, risk, alternative}`
- ✅ **SSRF 防护**:硬编码 IP 段黑名单(127/8, 10/8, 172.16/12, 192.168/16, 169.254/16)
- ✅ **工作目录沙箱**:file_* / search_files 拒绝绝对路径 + `..` + symlink 逃逸
- ✅ **P0 #1 Bridge**:`AgentRunner::from_definition` 把 agent.json + 6 工具 → 可跑 Runner
- ✅ **P0 #2 CLI**:`run` / `list` / `tools list` / `tools show` / `validate` / `config` 6 个子命令
- ✅ **P0 #3 Config**:4 子结构 + 3 层加载 + `${ENV:VAR}` 占位符
- ✅ **AGPL-3.0 + CC0-1.0** 双协议(代码 + core_eval.json)
- ✅ **158/158 unit tests** pass

**已知限制 ⚠️:**

- ⚠️ **168 warnings**(主要是 pre-existing `missing_docs`,不影响运行,`cargo fix --lib` 可一键补)
- ⚠️ **3 pre-existing integration tests fail**(缺 LLM mock,跟踪到 0.2.0)
- ⚠️ **17 文件中文注释乱码**(PowerShell 5.1 GBK 误读,跟踪到 0.2.0 重写)
- ⚠️ **runner.run 遇到 candidate 工具 proposal 会 error out**(0.1.0:未实现 propose 暂停;0.2.0:加 `auto_approve` 路径 + fact log)
- ⚠️ **CLI `run` 命令需要 evorule-server 在线**(桥接通,需要 server 跑起来才能完整跑通)

### 路线图

| 阶段 | 目标 | 预计 |
|---|---|---|
| v0.2.0 | runner.run 处理 candidate 工具 proposal(暂停 → user 批 → 再执行) | 1 周 |
| v0.2.0 | 168 warnings 清零(补 `///` doc) | 2 天 |
| v0.2.0 | 17 文件中文注释重写(走 [System.IO.File]::WriteAllText,UTF-8 no BOM) | 1 天 |
| v0.2.0 | 3 integration test mock LLM(用 wiremock-rs 替换真实 HTTP) | 1 周 |
| v0.2.0 | Gitee push(0.1.0 → 0.2.0 后) | TBD |
| v0.3.0 | time-travel-debugger 应用层接入(用 evorule fact log 做 replay/diff/rewind) | 3 周 |
| v0.4.0 | audit-inspector(blake3 哈希链验证 UI) | 2 周 |
| v0.5.0 | live-monitor(实时 fact 流) | 2 周 |

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
