# Evo-Agent API 接口文档

> 版本 0.1.0 | 最后更新 2026-08-15

---

## 目录

1. [概述](#1-概述)
2. [鉴权](#2-鉴权)
3. [Agent 管理 API](#3-agent-管理-api)
4. [Agent 执行 API](#4-agent-执行-api)
5. [记忆系统 API](#5-记忆系统-api)
6. [WebSocket 双向流](#6-websocket-双向流)
7. [SSE 流式事件](#7-sse-流式事件)
8. [Metrics API](#8-metrics-api)
9. [透传 evorule-server API](#9-透传-evorule-server-api)

---

## 1. 概述

Evo-Agent 对外提供三种 API 协议：

| 协议 | 用途 | 端点前缀 |
|------|------|----------|
| **HTTP REST** | 同步请求/响应，适合查询和一次性操作 | `/api/*`、`/agents/*` |
| **Server-Sent Events (SSE)** | 服务端推送流，用于流式 Agent 执行 | `/agents/{type}/run/stream` |
| **WebSocket** | 双向实时通信，用于 REPL 交互模式 | `/api/sessions/{id}/ws` |

### 基础 URL

```
默认: http://127.0.0.1:8081
生产: https://your-agent-server.com
```

### 通用响应格式

大多数端点返回业务特定的 JSON 结构。SSE 和 WebSocket 端点使用不同的帧格式。错误响应使用标准 HTTP 状态码。

---

## 2. 鉴权

### 认证方式

evo-agent 支持三种认证方式，按优先级递减：

| 方式 | 格式 | 适用场景 |
|------|------|----------|
| **Bearer Token** | `Authorization: Bearer <token>` | 所有 HTTP 端点（首选） |
| **Query Parameter** | `?token=<token>` | SSE 端点、WebSocket（浏览器 EventSource 不支持自定义 Header） |
| **环境变量** | `EVORULE_AUTH_TOKEN` | evorule-server 内部调用 |

客户端侧（evo-agent 作为 evorule-server 调用方）的 token 解析：`EVORULE_SERVICE_TOKEN`（service 身份，可写受保护域 `stable.llm`/`stable.system`）优先，缺省回退 `EVORULE_AUTH_TOKEN`（user 身份，受保护域写入将被 server 以 403 拒绝并走 best-effort warn 链路）。

### Token 轮换

支持无缝 token 轮换：

```text
旧 token → previous_tokens（过渡期仍有效）
新 token → current_tokens（当前有效）
再次轮换时，最旧的 previous_tokens 被丢弃
```

### 豁免路径

以下路径无需鉴权：

- `GET /health` — 健康检查
- `GET /metrics` — Prometheus 指标抓取

### 配置示例

**evo-agent.toml:**
```toml
[auth]
enabled = true
tokens = ["your-secret-token"]
```

**环境变量:**
```bash
EVO_AGENT_AUTH__ENABLED=true
EVO_AGENT_AUTH__TOKENS=token1,token2
```

**CLI:**
```bash
evo-agent serve --auth-token your-secret-token
```

---

## 3. Agent 管理 API

### 3.1 列出所有 Agent

```
GET /agents
```

**响应:**
```json
{
  "agents": [
    {
      "agent_type": "general",
      "version": "0.1.0",
      "description": "通用 Agent — 文件操作 + Shell + Web",
      "tools": ["file_read", "file_list", "file_write", "search_files", "shell_exec", "http_get"]
    },
    {
      "agent_type": "rule-copilot",
      "version": "0.1.0",
      "description": "规则协作 Agent",
      "tools": ["ws_list", "rule_list", "rule_create", ...]
    }
  ]
}
```

### 3.2 获取 Agent 详情

```
GET /agents/{agent_type}
```

**路径参数:**

| 参数 | 类型 | 说明 |
|------|------|------|
| `agent_type` | `string` | Agent 类型标识符（如 `general`、`researcher`） |

**响应:**
```json
{
  "agent_type": "general",
  "version": "0.1.0",
  "description": "通用 Agent",
  "system_prompt": "You are a helpful general-purpose assistant...",
  "model": "MiniMax-M2.5",
  "temperature": 0.3,
  "max_steps": 20,
  "tools": ["file_read", "file_list", "file_write", "search_files", "shell_exec", "http_get"],
  "memory_config": null
}
```

**错误:**

| 状态码 | 说明 |
|--------|------|
| `404 Not Found` | Agent 类型不存在 |

---

## 4. Agent 执行 API

### 4.1 执行 Agent（同步）

```
POST /agents/{agent_type}/run
```

**路径参数:**

| 参数 | 类型 | 说明 |
|------|------|------|
| `agent_type` | `string` | Agent 类型标识符 |

**请求体:**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `goal` | `string` | ✅ | 任务描述 |
| `agent_type` | `string` | ✅ | Agent 类型（冗余校验） |
| `max_steps` | `number` | ❌ | 覆盖 Agent 默认最大步数 |
| `temperature` | `number` | ❌ | 覆盖 LLM 温度参数（0.0 - 2.0） |
| `model` | `string` | ❌ | 覆盖 LLM 模型名 |

**请求示例:**
```json
{
  "agent_type": "general",
  "goal": "分析当前目录下的所有 Rust 文件结构",
  "max_steps": 15,
  "temperature": 0.3
}
```

**成功响应:**
```json
{
  "success": true,
  "content": "当前目录包含以下 Rust 文件:\n- main.rs\n- lib.rs\n...",
  "steps": 8,
  "duration_ms": 3245,
  "error": null
}
```

**失败响应:**
```json
{
  "success": false,
  "content": "",
  "steps": 0,
  "duration_ms": 0,
  "error": "LLM 调用失败: connection timeout"
}
```

**错误:**

| 状态码 | 说明 |
|--------|------|
| `404 Not Found` | Agent 类型不存在 |
| `500 Internal Server Error` | LLM 处理器未初始化 |

### 4.2 执行 Agent（SSE 流式）

```
POST /agents/{agent_type}/run/stream
```

请求体同同步端点。响应为 `text/event-stream`。

**SSE 事件类型:**

| 事件名 | 数据字段 | 说明 |
|--------|----------|------|
| `session_created` | `{"session_id": "42"}` | evorule session 已创建 |
| `step` | `{"step": 7}` | 当前 ReAct 轮次 |
| `llm_delta` | `{"text": "好的"}` | LLM 流式输出片段 |
| `llm_done` | `{"content": "...", "finish_reason": "stop"}` | LLM 输出完成 |
| `tool_call` | `{"name": "file_read", "args": {...}}` | 工具调用开始 |
| `tool_result` | `{"name": "file_read", "result": {...}}` | 工具调用完成 |
| `approval_required` | `{"tool_name": "shell_exec", "command": "rm ...", "risk": "high", "alternative": "..."}` | 需用户审批（candidate 工具） |
| `approval_result` | `{"tool_name": "shell_exec", "approved": true}` | 审批结果 |
| `done` | `{...AgentResult...}` | Agent 执行完成（终帧） |
| `error` | `{"error": "..."}` | 发生错误（终帧） |
| `info` | `{"message": "..."}` | 提示信息 |

**SSE 响应示例:**
```
event: session_created
data: {"session_id": "42"}

event: step
data: {"step": 1}

event: llm_delta
data: {"text": "根据"}

event: llm_delta
data: {"text": "分析"}

event: llm_done
data: {"content": "根据分析结果...", "finish_reason": "stop"}

event: tool_call
data: {"name": "file_read", "args": {"path": "./Cargo.toml"}}

event: tool_result
data: {"name": "file_read", "result": {"content": "[package]\nname = evo-agent..."}}

event: done
data: {"success": true, "content": "...", "steps": 3, "duration_ms": 1523}
```

### 4.3 取消 Agent 执行

```
POST /agents/{agent_type}/cancel?session_id={session_id}
```

**查询参数:**

| 参数 | 类型 | 说明 |
|------|------|------|
| `session_id` | `string` | 要取消的 session ID（由 `session_created` 事件返回） |

**成功响应:**
```json
{
  "success": true,
  "message": "cancelled",
  "session_id": "42"
}
```

**错误:**

| 状态码 | 说明 |
|--------|------|
| `404 Not Found` | Session 未在运行中（已结束或不存在） |

### 4.4 审批 Candidate 工具

```
POST /agents/{agent_type}/approve
```

**请求体:**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `session_id` | `string` | ✅ | 等待审批的 session ID |
| `approved` | `boolean` | ✅ | `true` = 批准执行，`false` = 拒绝 |

**请求示例:**
```json
{
  "session_id": "42",
  "approved": true
}
```

**成功响应:**
```json
{
  "success": true,
  "message": "approval delivered",
  "session_id": "42",
  "approved": true
}
```

**错误:**

| 状态码 | 说明 |
|--------|------|
| `404 Not Found` | Session 不在等待审批（已超时/不存在/未触发审批） |

> **超时:** 审批请求在 60 秒内未响应将自动拒绝。

---

## 5. 记忆系统 API

### 5.1 查询 Session 记忆事件

```
GET /api/sessions/{id}/events
```

**路径参数:**

| 参数 | 类型 | 说明 |
|------|------|------|
| `id` | `string` | evorule session ID |

**查询参数:**

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `entity` | `string` | ❌ | 按实体 ID 过滤（如 `?entity=pet_doudou`） |

**成功响应:**
```json
{
  "session_id": "42",
  "count": 15,
  "events": [
    {
      "event_id": "E001",
      "event_type": "interaction",
      "entity_id": "pet_doudou",
      "content": "狗狗不吃饭",
      "timestamp": 1713135000,
      "cause_fact_id": 100,
      // ... 其他 MemoryEvent 字段
    }
  ]
}
```

### 5.2 回放 Session 事件链

```
GET /api/sessions/{id}/replay
```

**路径参数:**

| 参数 | 类型 | 说明 |
|------|------|------|
| `id` | `string` | evorule session ID |

**查询参数:**

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `event` | `string` | ❌ | 从指定事件 ID 出发沿因果链回溯/前进 |
| `entity` | `string` | ❌ | 按实体回放 |
| `direction` | `string` | ❌ | `backward`（默认）或 `forward` |
| `narrate` | `boolean` | ❌ | `true` = LLM 自然语言叙述（temperature=0） |

**成功响应（无叙述）:**
```json
{
  "session_id": "42",
  "count": 8,
  "events": [/* ... */]
}
```

**成功响应（有叙述）:**
```json
{
  "session_id": "42",
  "count": 8,
  "events": [/* ... */],
  "narrative": "狗狗从不吃不喝到最后康复的完整故事...",
  "cited_events": ["E001", "E003", "E005"],
  "cited_facts": [100, 105, 110]
}
```

> **降级行为:** 叙述失败时自动降级为结构化输出，不中断请求。

### 5.3 查询记忆证据

```
GET /agents/{type}/memory/evidence
```

**路径参数:**

| 参数 | 类型 | 说明 |
|------|------|------|
| `type` | `string` | Agent 类型标识符 |

**查询参数:**

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `session_id` | `string` | ✅ | 目标 session ID |
| `scope` | `string` | ✅ | `shared` 或 `session` |
| `key` | `string` | ✅ | KV 键（如 `user.profile`） |

**成功响应:**
```json
{
  "source_fact_id": 100,
  "chain_verified": true,
  "causal_chain": [/* ... */]
}
```

**错误:**

| 状态码 | 说明 |
|--------|------|
| `400 Bad Request` | 无效的 scope 参数 |
| `404 Not Found` | 记忆条目不存在 |
| `502 Bad Gateway` | evorule-server 通信失败 |

### 5.4 记忆召回

```
GET /agents/{type}/memory/recall
```

**路径参数:**

| 参数 | 类型 | 说明 |
|------|------|------|
| `type` | `string` | Agent 类型标识符 |

**查询参数:**

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `goal` | `string` | ✅ | 召回目标（语义匹配用） |
| `max_summaries` | `number` | ❌ | L1 摘要上限（默认 Agent 配置） |
| `max_events` | `number` | ❌ | L2 事件上限（默认 Agent 配置） |
| `with_evidence` | `boolean` | ❌ | `true` = 附带证据（B4 模式） |

**成功响应:**
```json
{
  "summaries": [/* ... */],
  "events": [/* ... */],
  "evidences": [/* ... */]  // 仅 with_evidence=true 时返回
}
```

---

## 6. WebSocket 双向流

### 6.1 连接

```
GET /api/sessions/{id}/ws?agent_type={type}&token={token}
```

**路径参数:**

| 参数 | 类型 | 说明 |
|------|------|------|
| `id` | `string` | Session ID。`"new"` 或空字符串表示首次连接（服务端创建新 session）；传具体 ID 则复用已有 session（continuation 模式） |

**查询参数:**

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `agent_type` | `string` | ✅ | Agent 类型（构造 AgentRunner） |
| `token` | `string` | ✅（启用鉴权时） | 鉴权 token |

### 6.2 Client → Server 消息

客户端发送 JSON 帧，`type` 字段使用 snake_case：

#### 发送消息

```json
{
  "type": "message",
  "content": "帮我查看当前目录结构"
}
```

#### 中断当前轮次

```json
{
  "type": "interrupt"
}
```

#### 回滚到指定版本

```json
{
  "type": "rewind",
  "version": 5
}
```

### 6.3 Server → Client 消息

服务端推送 JSON 帧，`type` 字段使用 PascalCase：

#### Session 创建

```json
{ "type": "SessionCreated", "session_id": "42" }
```

#### LLM 增量输出

```json
{ "type": "LlmDelta", "text": "好的" }
```

#### 工具调用

```json
{ "type": "ToolCall", "name": "file_read", "args": {...} }
```

#### 工具结果

```json
{ "type": "ToolResult", "name": "file_read", "result": {...} }
```

#### 完成

```json
{
  "type": "Done",
  "success": true,
  "content": "当前目录包含...",
  "steps": 3,
  "duration_ms": 1523
}
```

#### 错误

```json
{ "type": "Error", "error": "..." }
```

#### 审批请求

```json
{
  "type": "ApprovalRequired",
  "tool_name": "shell_exec",
  "command": "rm -rf /tmp/test",
  "risk": "high",
  "alternative": "use trash instead"
}
```

> 收到此帧后，客户端应弹审批对话框，调用 `POST /agents/{type}/approve` 送达审批结果。

#### 审批结果

```json
{ "type": "ApprovalResult", "tool_name": "shell_exec", "approved": true }
```

#### 提示信息

```json
{ "type": "Info", "message": "interrupt sent" }
```

### 6.4 生命周期

1. 客户端连接 → 服务端在升级前校验 `?token=`，失败返回 `401`
2. 客户端发 `message` → 服务端构造 fresh AgentRunner（每轮新建），首轮调 `run_streaming`（创建 session），后续调 `run_continuation`（复用 session）
3. 服务端把 AgentEvent 流逐个序列化为 JSON 帧推给客户端
4. 客户端可在任意时刻发 `interrupt` → 服务端 cancel 当前 runner 的 CancellationToken
5. 客户端发 `rewind` → 服务端调 evorule `rewind` API（需在无活跃轮次时）
6. 任一方关闭 WebSocket → 连接结束（session 保留在 evorule，可重连续用）

> **并发限制:** 同一 session 同时只能有一个活跃轮次。在轮次进行中发送新的 `message` 会收到 `Error: a turn is already active`。

---

## 7. SSE 流式事件

SSE 端点的事件格式在 [4.2 执行 Agent（SSE 流式）](#42-执行-agentsse-流式) 中已详细说明。

### 连接示例（JavaScript）

```javascript
const eventSource = new EventSource(
  'http://127.0.0.1:8081/agents/general/run/stream?token=your-token',
  {
    method: 'POST',
    body: JSON.stringify({
      goal: '查看当前目录',
      agent_type: 'general'
    }),
    headers: {
      'Content-Type': 'application/json'
    }
  }
);

eventSource.addEventListener('session_created', (e) => {
  const data = JSON.parse(e.data);
  console.log('Session ID:', data.session_id);
});

eventSource.addEventListener('llm_delta', (e) => {
  const data = JSON.parse(e.data);
  console.log(data.text);  // 流式输出
});

eventSource.addEventListener('done', (e) => {
  const result = JSON.parse(e.data);
  console.log('完成:', result.content);
  eventSource.close();
});

eventSource.addEventListener('error', (e) => {
  console.error('错误:', JSON.parse(e.data).error);
});
```

---

## 8. Metrics API

### 8.1 获取 Prometheus 指标

```
GET /metrics
```

**响应:** `text/plain`（Prometheus 文本格式）

**指标列表:**

| 指标名 | 类型 | 说明 |
|--------|------|------|
| `evo_agent_sessions_total` | counter | 总会话数 |
| `evo_agent_steps_total` | counter | 总 ReAct 步数 |
| `evo_agent_llm_calls_total` | counter | LLM 调用次数（按 model + success 标签） |
| `evo_agent_tool_calls_total` | counter | 工具调用次数（按 name + success 标签） |
| `evo_agent_llm_call_duration_seconds` | histogram | LLM 调用耗时 |
| `evo_agent_tool_call_duration_seconds` | histogram | 工具调用耗时 |
| `evo_agent_sse_connections_active` | gauge | 活跃 SSE 连接数 |

**响应示例:**
```
# HELP evo_agent_sessions_total Total number of agent sessions
# TYPE evo_agent_sessions_total counter
evo_agent_sessions_total 42

# HELP evo_agent_llm_calls_total Total LLM calls
# TYPE evo_agent_llm_calls_total counter
evo_agent_llm_calls_total{model="MiniMax-M2.5",success="true"} 126
```

> **注意:** `/metrics` 端点豁免鉴权（公开路径），供 Prometheus 抓取器无认证访问。

---

## 9. 透传 evorule-server API

evo-agent 通过 `EvoruleApiClient` 和 `WorkspaceApiClient` 透传调用 evorule-server 的 API。这些端点不是 evo-agent 直接暴露的 HTTP 路由，而是内部使用的 Rust crate API。

### 9.1 Session 管理

| 方法 | evorule-server 端点 | 说明 |
|------|---------------------|------|
| `create_session(initial_content?)` | `POST /api/sessions` | 创建新 session |
| `create_session_fork(parent_id, version?)` | `POST /api/sessions/fork/{id}` | Fork 会话（可选指定版本） |
| `get_state(session_id)` | `GET /api/sessions/{id}/state` | 获取 Session 状态 |

### 9.2 命令与载荷

| 方法 | evorule-server 端点 | 说明 |
|------|---------------------|------|
| `submit_command(session_id, command)` | `POST /api/sessions/{id}/command` | 提交指令 |
| `submit_io_response(session_id, request_id, result, error?)` | `POST /api/sessions/{id}/io_response` | 提交 I/O 响应 |
| `update_payload(session_id, path, value)` | `POST /api/sessions/{id}/payload` | 更新 Payload（路径写 `shared.*` 时触发跨会话广播） |

### 9.3 Fact 查询

| 方法 | evorule-server 端点 | 说明 |
|------|---------------------|------|
| `get_facts(session_id, prefix?)` | `GET /api/sessions/{id}/facts?prefix=` | 按路径前缀查询 Session Facts |
| `get_shared_facts(prefix?)` | `GET /api/shared/facts?prefix=` | 查询跨会话共享事实 |
| `get_shared_fact_source(fact_id)` | `GET /api/shared/facts/{id}/source` | 按 ID 查询共享事实（rollup 后仍可访问） |
| `get_sessions_using_fact(fact_id)` | `GET /api/shared/facts/{id}/used_by` | 查询引用某事实的所有 Session |

### 9.4 审计链

| 方法 | evorule-server 端点 | 说明 |
|------|---------------------|------|
| `get_audit_report(session_id)` | `GET /api/sessions/{id}/audit` | 获取审计报告 |
| `verify_audit_typed(session_id)` | `GET /api/sessions/{id}/audit/verify` | 类型化整链验证 → `AuditVerify` |
| `get_causal_chain_typed(session_id, fact_id)` | `GET /api/sessions/{id}/audit/causal/{fact_id}` | 类型化因果链 → `CausalChain` |

### 9.5 时间旅行

| 方法 | evorule-server 端点 | 说明 |
|------|---------------------|------|
| `rewind(session_id, version)` | `GET /api/sessions/{id}/rewind?version=` | 回滚到指定版本 |
| `replay(session_id)` | `GET /api/sessions/{id}/replay` | 全量重放 |
| `replay_range(session_id, from?, to?)` | `GET /api/sessions/{id}/replay?from=&to=` | 版本区间重放 |
| `diff(session_id, a, b)` | `GET /api/sessions/{id}/diff?a=&b=` | 版本差异对比 |

### 9.6 集群协作

| 方法 | evorule-server 端点 | 说明 |
|------|---------------------|------|
| `join_cluster(session_id, cluster_id?)` | `POST /api/sessions/{id}/join` | 加入/创建集群 |
| `leave_cluster(session_id)` | `POST /api/sessions/{id}/leave` | 离开集群 |
| `get_cluster_status(session_id)` | `GET /api/sessions/{id}/cluster` | 查询集群状态 |

### 9.7 其他

| 方法 | evorule-server 端点 | 说明 |
|------|---------------------|------|
| `record_used_at_startup(session_id, fact_ids)` | `POST /api/sessions/{id}/used_at_startup` | 记录启动时引用的事实 |
| `get_used_at_startup(session_id)` | `GET /api/sessions/{id}/used_at_startup` | 查询启动时引用的事实 |
| `subscribe_events(session_id)` | `GET /api/sessions/{id}/events` | 订阅事件流 |
| `get_debug_phase(session_id)` | `GET /api/sessions/{id}/debug/phase` | 获取调试阶段 |

### 9.8 evorule-server 新增端点（v0.2.4）

以下端点由 evorule-server v0.2.4 新增，配套 evo-agent L-3/L-1 修复：

#### Rollup 共享事实

```
POST /api/shared/facts/rollup
```

**请求体:**
```json
{
  "fact_ids": [1, 2, 3]
}
```

**成功响应:**
```json
{
  "success": true,
  "message": "3 facts marked as rolled up",
  "fact_id": null
}
```

**说明:** 标记一批共享事实为已 rollup。被标记的事实在 `facts_by_path_prefix` 查询中被过滤，但 `fact_by_id` 仍可访问（审计可追溯性保留）。

#### Session Payload 广播

`POST /api/sessions/{id}/payload` 的行为变更：

- 当 `path` 以 `shared.` 开头时，evorule-server 自动将 PayloadUpdate 同步广播到 `SharedFactsLog`
- 广播为 best-effort：失败仅 `tracing::warn!`，不影响主流程
- 这使得跨会话共享事实立即可见于其他 session

---

## 附录：数据模型

### AgentRunRequest

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `agent_type` | `string` | ✅ | Agent 类型 |
| `goal` | `string` | ✅ | 任务描述 |
| `max_steps` | `number` | ❌ | 覆盖最大步数 |
| `temperature` | `number` | ❌ | 覆盖温度 |
| `model` | `string` | ❌ | 覆盖模型名 |

### AgentRunResponse

| 字段 | 类型 | 说明 |
|------|------|------|
| `success` | `boolean` | 是否成功 |
| `content` | `string` | 结果内容 |
| `steps` | `number` | 执行步数 |
| `duration_ms` | `number` | 执行耗时（毫秒） |
| `error` | `string \| null` | 错误信息 |

### AgentInfo

| 字段 | 类型 | 说明 |
|------|------|------|
| `agent_type` | `string` | Agent 类型 |
| `version` | `string` | 版本号 |
| `description` | `string` | 描述 |
| `tools` | `string[]` | 工具列表 |

### AgentDefinitionResponse

| 字段 | 类型 | 说明 |
|------|------|------|
| `agent_type` | `string` | Agent 类型 |
| `version` | `string` | 版本号 |
| `description` | `string` | 描述 |
| `system_prompt` | `string` | 系统提示词 |
| `model` | `string` | 模型名 |
| `temperature` | `number` | 温度 |
| `max_steps` | `number` | 最大步数 |
| `tools` | `string[]` | 工具列表 |
| `memory_config` | `MemoryConfig \| null` | 记忆配置 |

### MemoryConfig

| 字段 | 类型 | 说明 |
|------|------|------|
| `type` | `string` | `"none"` 或 `"persistent"` |
| `namespace` | `string` | 命名空间 |
| `message_persist` | `MessagePersistConfig` | 消息持久化配置 |
| `max_session_summaries` | `number` | 会话摘要上限（默认 3） |
| `max_injected_events` | `number` | 事件注入上限（默认 5） |
| `summary_rollup_threshold` | `number` | 摘要 rollup 阈值（默认 10） |
| `enable_event_extraction` | `boolean` | 是否启用事件提取（默认 true） |

---

## 错误码

### HTTP 状态码

| 状态码 | 说明 |
|--------|------|
| `200 OK` | 请求成功 |
| `400 Bad Request` | 请求参数无效 |
| `401 Unauthorized` | 鉴权失败或缺失 |
| `404 Not Found` | 资源不存在 |
| `426 Upgrade Required` | WebSocket 升级请求不完整（缺少必要参数） |
| `500 Internal Server Error` | 服务器内部错误（LLM/工具调用失败） |
| `502 Bad Gateway` | 上游服务（evorule-server）通信失败 |

### SSE/WS 错误事件

```json
{ "type": "Error", "error": "..." }
```

常见错误消息：
- `"agent '{type}' not found"` — Agent 类型不存在
- `"a turn is already active; send interrupt first"` — 轮次并发冲突
- `"no active turn to interrupt"` — 无活跃轮次可中断
- `"cannot rewind during active turn; send interrupt first"` — 轮次中不能回滚
- `"LLM 调用失败: connection timeout"` — LLM 网络超时

---

## 版本变更日志

### v0.1.0 (当前)

- 初始 API 发布
- HTTP REST / SSE / WebSocket 三种协议
- 鉴权系统（Bearer Token + 轮换）
- Metrics 指标端点
- 记忆系统 API（事件查询、回放、证据、召回）
- 审批系统 API（candidate 工具审批）
- 跨会话共享事实广播（L-1）
- Rollup 端点（L-3）
