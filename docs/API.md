# evo-agent HTTP API 调用文档

> **版本**: 0.1.0 (2026-07-22)
> **基线**: G1-G6 P0 项全部完成
> **协议**: HTTP/1.1, JSON, SSE

---

## 目录

- [1. 启动 Server](#1-启动-server)
- [2. 端点一览](#2-端点一览)
- [3. 健康检查](#3-健康检查)
- [4. 列出 Agent](#4-列出-agent)
- [5. 查看 Agent 定义](#5-查看-agent-定义)
- [6. 同步执行 Agent](#6-同步执行-agent)
- [7. SSE 流式执行 Agent](#7-sse-流式执行-agent)
  - [7.1 请求](#71-请求)
  - [7.2 SSE 事件类型](#72-sse-事件类型)
  - [7.3 事件序列示意](#73-事件序列示意)
  - [7.4 curl 消费示例](#74-curl-消费示例)
  - [7.5 Python 消费示例](#75-python-消费示例)
  - [7.6 JavaScript 消费示例](#76-javascript-消费示例)
- [8. 取消正在运行的 Session](#8-取消正在运行的-session)
  - [8.1 请求](#81-请求)
  - [8.2 响应](#82-响应)
  - [8.3 取消机制说明](#83-取消机制说明)
  - [8.4 完整取消示例](#84-完整取消示例)
- [9. 端到端完整示例](#9-端到端完整示例)
- [10. 错误处理](#10-错误处理)
- [11. 限制与边界](#11-限制与边界)

---

## 1. 启动 Server

```bash
# 默认监听 127.0.0.1:8081
evo-agent serve

# 指定地址和端口
evo-agent serve --host 0.0.0.0 --port 9000
```

启动后输出：

```text
evo-agent HTTP server listening on http://127.0.0.1:8081
  GET  /health
  GET  /agents
  POST /agents/{type}/run
  POST /agents/{type}/run/stream  (SSE)
  POST /agents/{type}/cancel?session_id=xxx
press Ctrl+C to shut down
```

**优雅关闭**: 按 `Ctrl+C` 或发送 `SIGTERM`(Unix),server 停止接受新连接并等待在途请求完成后退出。

**中间件**:
- CORS: `permissive`(允许所有来源,生产环境应收紧)
- Body limit: 1 MB

---

## 2. 端点一览

| 方法 | 路径 | 说明 | G项 |
|------|------|------|-----|
| GET | `/health` | 健康检查,返回 `"ok"` | G5 |
| GET | `/agents` | 列出所有可用 agent 类型 | — |
| GET | `/agents/{agent_type}` | 查看单个 agent 的完整定义 | — |
| POST | `/agents/{agent_type}/run` | 同步执行 agent,等待完成后返回结果 | — |
| POST | `/agents/{agent_type}/run/stream` | SSE 流式执行,逐 token 返回 LLM 输出 | G4 |
| POST | `/agents/{agent_type}/cancel?session_id=xxx` | 取消正在运行的 session | G6 |

**Base URL**: `http://127.0.0.1:8081`(默认)

---

## 3. 健康检查

### 请求

```http
GET /health HTTP/1.1
```

### 响应

```
HTTP/1.1 200 OK
Content-Type: text/plain

ok
```

### curl

```bash
curl http://127.0.0.1:8081/health
# ok
```

---

## 4. 列出 Agent

### 请求

```http
GET /agents HTTP/1.1
```

### 响应

```json
{
  "agents": [
    {
      "agent_type": "general",
      "version": "0.1.0",
      "description": "General-purpose agent — file ops, shell commands, and web fetch",
      "tools": ["file_read", "file_list", "file_write", "search_files", "shell_exec", "http_get"]
    },
    {
      "agent_type": "researcher",
      "version": "0.1.0",
      "description": "Research agent — summary and search",
      "tools": ["file_read", "search_files", "file_list"]
    }
  ]
}
```

### 字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| `agents` | array | agent 列表 |
| `agents[].agent_type` | string | agent 类型名(用于后续 API 调用的 `{agent_type}` 路径参数) |
| `agents[].version` | string | agent 定义版本 |
| `agents[].description` | string | 描述 |
| `agents[].tools` | string[] | 该 agent 配置的工具列表 |

### curl

```bash
curl http://127.0.0.1:8081/agents | jq .
```

---

## 5. 查看 Agent 定义

### 请求

```http
GET /agents/{agent_type} HTTP/1.1
```

**路径参数**:

| 参数 | 类型 | 说明 |
|------|------|------|
| `agent_type` | string | agent 类型名,如 `general`、`researcher` |

### 响应

```json
{
  "agent_type": "general",
  "version": "0.1.0",
  "description": "General-purpose agent — file ops, shell commands, and web fetch",
  "system_prompt": "You are a helpful assistant...",
  "model": "gpt-4o-mini",
  "temperature": 0.7,
  "max_steps": 10,
  "tools": ["file_read", "file_list", "file_write", "search_files", "shell_exec", "http_get"],
  "memory_config": {
    "type": "persistent",
    "namespace": "general",
    "message_persist": "every_message"
  }
}
```

### 字段说明

| 字段 | 类型 | 说明 |
|------|------|------|
| `system_prompt` | string | 系统提示词 |
| `model` | string | LLM 模型名 |
| `temperature` | float | 温度参数 |
| `max_steps` | int | 最大执行步数 |
| `memory_config` | object\|null | 记忆配置(详见下表) |
| `memory_config.type` | string | 记忆类型:`"none"` 或 `"persistent"` |
| `memory_config.namespace` | string | 记忆命名空间 |
| `memory_config.message_persist` | string | 消息持久化模式:`every_message` / `every_n` / `per_react_round` / `disabled` |
| `memory_config.ttl_secs` | int\|null | 记忆过期时间(秒),`null` 表示永久 |
| `memory_config.summary_model` | string\|null | 摘要模型名(P4 阶段使用) |

### 错误

| 状态码 | 说明 |
|--------|------|
| 404 | agent 类型不存在 |

### curl

```bash
curl http://127.0.0.1:8081/agents/general | jq .
```

---

## 6. 同步执行 Agent

### 请求

```http
POST /agents/{agent_type}/run HTTP/1.1
Content-Type: application/json

{
  "agent_type": "general",
  "goal": "summarize the README",
  "max_steps": 20,
  "temperature": 0.3,
  "model": "gpt-4o"
}
```

**请求体** (`AgentRunRequest`):

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `agent_type` | string | 是 | agent 类型名(应与路径参数一致) |
| `goal` | string | 是 | 任务描述 |
| `max_steps` | int | 否 | 覆盖 agent 定义中的 max_steps |
| `temperature` | float | 否 | 覆盖 agent 定义中的 temperature |
| `model` | string | 否 | 覆盖 agent 定义中的 model |

> **注意**: 同步执行是阻塞的 — agent 跑完(可能多轮 LLM 调用 + 工具调用)后才返回。长任务建议用[流式执行](#7-sse-流式执行-agent)。

### 响应

```json
{
  "success": true,
  "content": "The README describes...",
  "steps": 3,
  "duration_ms": 15234,
  "error": null
}
```

**响应体** (`AgentRunResponse`):

| 字段 | 类型 | 说明 |
|------|------|------|
| `success` | bool | 是否成功完成 |
| `content` | string | agent 最终输出内容 |
| `steps` | int | 执行的步数 |
| `duration_ms` | int | 总耗时(毫秒) |
| `error` | string\|null | 错误信息(失败时) |

### 错误

| 状态码 | 说明 |
|--------|------|
| 404 | agent 类型不存在 |

### curl

```bash
curl -X POST http://127.0.0.1:8081/agents/general/run \
  -H "Content-Type: application/json" \
  -d '{"agent_type": "general", "goal": "summarize the README"}' | jq .
```

### Python

```python
import requests

resp = requests.post(
    "http://127.0.0.1:8081/agents/general/run",
    json={"agent_type": "general", "goal": "summarize the README"},
    timeout=300,
)
result = resp.json()
print(f"success={result['success']}, steps={result['steps']}")
print(result["content"])
```

---

## 7. SSE 流式执行 Agent

这是 G4 的核心端点。与[同步执行](#6-同步执行-agent)不同,流式执行通过 Server-Sent Events (SSE) 逐帧返回 agent 执行过程中的所有事件 — 包括 LLM 的 token 增量、工具调用、步骤进度等。

### 7.1 请求

```http
POST /agents/{agent_type}/run/stream HTTP/1.1
Content-Type: application/json
Accept: text/event-stream

{
  "agent_type": "general",
  "goal": "write a haiku about Rust"
}
```

请求体与[同步执行](#6-同步执行-agent)完全相同(`AgentRunRequest`)。

**响应**:

```
HTTP/1.1 200 OK
Content-Type: text/event-stream
Cache-Control: no-cache
Connection: keep-alive
```

响应体是 SSE 流,每个事件格式为:

```text
event: <event_name>
data: <json>

```

### 7.2 SSE 事件类型

共 9 种事件,按 `event:` 字段区分:

| event 名 | data 结构 | 说明 | 出现次数 |
|-----------|-----------|------|----------|
| `session_created` | `{"session_id": "..."}` | evorule session 已创建 | 1 次(首个事件) |
| `step` | `{"step": 1}` | 进入第 N 步(从 1 开始) | 每步 1 次 |
| `llm_delta` | `{"text": "hello"}` | LLM 输出增量(token 级) | 0~N 次 |
| `llm_done` | `{"content": "...", "finish_reason": "stop"}` | 本轮 LLM 输出完成 | 每轮 LLM 调用 1 次 |
| `tool_call` | `{"name": "search", "args": {...}}` | LLM 发起了工具调用 | 0~N 次 |
| `tool_result` | `{"name": "search", "result": {...}}` | 工具执行完成 | 0~N 次 |
| `done` | `AgentResult` JSON | **整个任务完成(终帧)** | 1 次(终帧) |
| `error` | `{"error": "..."}` | 错误(可恢复或终态) | 0~N 次 |
| `info` | `{"message": "auto_rewind triggered"}` | 中间状态信息 | 0~N 次 |

**`done` 事件的 data 结构** (`AgentResult`):

```json
{
  "success": true,
  "content": "Safe and fast,\nOwnership guides every line,\nBorrow checker smiles.",
  "steps": 1,
  "duration_ms": 3421,
  "tool_calls": [],
  "error": null
}
```

**关键约定**:
- `session_created` 始终是第一个事件,携带 `session_id`(用于后续[/cancel](#8-取消正在运行的-session)调用)
- `done` 始终是最后一个事件(正常结束时),收到后应关闭流
- `error` 事件可能后跟 `done`(终态错误),也可能是中间可恢复错误
- `llm_delta` 的 `text` 字段是增量文本,需要**拼接**才能得到完整内容
- `llm_done` 的 `content` 字段是本轮完整内容(已聚合)

### 7.3 事件序列示意

一个典型的单步、无工具调用的流式执行:

```text
event: session_created          ← session 创建
data: {"session_id":"s-abc123"}

event: step                     ← 第 1 步
data: {"step":1}

event: llm_delta                ← token 增量(重复 N 次)
data: {"text":"Safe"}

event: llm_delta
data: {"text":" and"}

event: llm_delta
data: {"text":" fast,"}

  ... (更多 delta) ...

event: llm_done                 ← 本轮 LLM 完成
data: {"content":"Safe and fast,...","finish_reason":"stop"}

event: done                     ← 任务完成(终帧)
data: {"success":true,"content":"Safe and fast,...","steps":1,"duration_ms":3421,"tool_calls":[],"error":null}
```

一个多步、带工具调用的流式执行:

```text
event: session_created
data: {"session_id":"s-def456"}

event: step
data: {"step":1}

event: llm_delta                ← LLM 决定调用工具
data: {"text":""}

event: llm_done
data: {"content":"","finish_reason":"tool_calls"}

event: tool_call                ← 工具调用
data: {"name":"search_files","args":{"pattern":"*.rs"}}

event: tool_result              ← 工具返回
data: {"name":"search_files","result":{"matches":["main.rs","lib.rs"]}}

event: step                     ← 第 2 步(LLM 拿到工具结果继续)
data: {"step":2}

event: llm_delta
data: {"text":"Found 2 Rust files..."}

event: llm_done
data: {"content":"Found 2 Rust files...","finish_reason":"stop"}

event: done
data: {"success":true,"content":"Found 2 Rust files...","steps":2,...}
```

### 7.4 curl 消费示例

```bash
# -N 禁用缓冲,实时看到 SSE 流
curl -N -X POST http://127.0.0.1:8081/agents/general/run/stream \
  -H "Content-Type: application/json" \
  -d '{"agent_type": "general", "goal": "write a haiku about Rust"}'
```

输出:

```text
event: session_created
data: {"session_id":"s-abc123"}

event: step
data: {"step":1}

event: llm_delta
data: {"text":"Safe"}

event: llm_delta
data: {"text":" and fast,"}

event: llm_done
data: {"content":"Safe and fast,\nOwnership guides every line,\nBorrow checker smiles.","finish_reason":"stop"}

event: done
data: {"success":true,"content":"Safe and fast,\nOwnership guides every line,\nBorrow checker smiles.","steps":1,"duration_ms":3421,"tool_calls":[],"error":null}
```

### 7.5 Python 消费示例

```python
"""
evo-agent SSE 流式执行消费示例
依赖: pip install requests
"""
import json
import requests

url = "http://127.0.0.1:8081/agents/general/run/stream"
payload = {"agent_type": "general", "goal": "write a haiku about Rust"}

session_id = None
full_text = ""

with requests.post(url, json=payload, stream=True, timeout=300) as resp:
    event_name = None

    for line in resp.iter_lines(decode_unicode=True):
        if not line:
            # 空行 = 事件分隔,重置 event_name
            event_name = None
            continue

        if line.startswith("event: "):
            event_name = line[7:]
        elif line.startswith("data: "):
            data = json.loads(line[6:])

            if event_name == "session_created":
                session_id = data["session_id"]
                print(f"[session] {session_id}")

            elif event_name == "step":
                print(f"\n--- step {data['step']} ---")

            elif event_name == "llm_delta":
                # 增量文本实时打印(不换行)
                print(data["text"], end="", flush=True)
                full_text += data["text"]

            elif event_name == "llm_done":
                print(f"\n[llm done: {data.get('finish_reason', '?')}]")

            elif event_name == "tool_call":
                print(f"\n[tool call] {data['name']} {data['args']}")

            elif event_name == "tool_result":
                print(f"[tool result] {data['name']} {data['result']}")

            elif event_name == "info":
                print(f"\n[info] {data['message']}")

            elif event_name == "error":
                print(f"\n[error] {data['error']}")

            elif event_name == "done":
                print(f"\n\n=== Done ===")
                print(f"  success: {data['success']}")
                print(f"  steps: {data['steps']}")
                print(f"  duration: {data['duration_ms']}ms")
                if data.get("error"):
                    print(f"  error: {data['error']}")
                break

print(f"\nsession_id (可用于取消): {session_id}")
```

### 7.6 JavaScript 消费示例

#### 浏览器 (EventSource API)

```javascript
/**
 * 浏览器 SSE 消费示例
 * 注意: EventSource 只支持 GET,这里用 fetch + ReadableStream 消费 POST SSE
 */
async function runAgentStream(goal) {
  const resp = await fetch("http://127.0.0.1:8081/agents/general/run/stream", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ agent_type: "general", goal }),
  });

  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let eventName = null;
  let sessionId = null;

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split("\n");
    buffer = lines.pop(); // 保留最后不完整的行

    for (const line of lines) {
      if (line === "") {
        eventName = null;
        continue;
      }
      if (line.startsWith("event: ")) {
        eventName = line.slice(7);
      } else if (line.startsWith("data: ")) {
        const data = JSON.parse(line.slice(6));

        switch (eventName) {
          case "session_created":
            sessionId = data.session_id;
            console.log(`[session] ${sessionId}`);
            break;
          case "step":
            console.log(`\n--- step ${data.step} ---`);
            break;
          case "llm_delta":
            // 增量文本追加到页面
            process.stdout.write(data.text);
            // 或: document.getElementById("output").textContent += data.text;
            break;
          case "llm_done":
            console.log(`\n[llm done: ${data.finish_reason || "?"}]`);
            break;
          case "tool_call":
            console.log(`[tool call] ${data.name}`, data.args);
            break;
          case "tool_result":
            console.log(`[tool result] ${data.name}`, data.result);
            break;
          case "done":
            console.log(`\n=== Done ===`);
            console.log(`  success: ${data.success}`);
            console.log(`  steps: ${data.steps}`);
            console.log(`  duration: ${data.duration_ms}ms`);
            return { sessionId, result: data };
          case "error":
            console.error(`[error] ${data.error}`);
            break;
          case "info":
            console.log(`[info] ${data.message}`);
            break;
        }
      }
    }
  }
}

// 使用
runAgentStream("write a haiku about Rust").then(({ sessionId }) => {
  console.log(`session_id (可用于取消): ${sessionId}`);
});
```

#### Node.js

```javascript
/**
 * Node.js SSE 消费示例
 * 无额外依赖,使用内置 http 模块
 */
const http = require("http");

function runAgentStream(goal) {
  const body = JSON.stringify({ agent_type: "general", goal });

  const req = http.request(
    {
      hostname: "127.0.0.1",
      port: 8081,
      path: "/agents/general/run/stream",
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "Content-Length": Buffer.byteLength(body),
      },
    },
    (res) => {
      let buffer = "";
      let eventName = null;
      let sessionId = null;

      res.on("data", (chunk) => {
        buffer += chunk.toString();
        const lines = buffer.split("\n");
        buffer = lines.pop();

        for (const line of lines) {
          if (line === "") { eventName = null; continue; }
          if (line.startsWith("event: ")) { eventName = line.slice(7); }
          else if (line.startsWith("data: ")) {
            const data = JSON.parse(line.slice(6));
            switch (eventName) {
              case "session_created":
                sessionId = data.session_id;
                console.log(`[session] ${sessionId}`);
                break;
              case "llm_delta":
                process.stdout.write(data.text);
                break;
              case "llm_done":
                console.log(`\n[llm done: ${data.finish_reason}]`);
                break;
              case "tool_call":
                console.log(`\n[tool call] ${data.name}`, data.args);
                break;
              case "tool_result":
                console.log(`[tool result] ${data.name}`, data.result);
                break;
              case "done":
                console.log(`\n=== Done ===`);
                console.log(`  success: ${data.success}, steps: ${data.steps}`);
                // sessionId 可用于取消
                break;
              case "error":
                console.error(`[error] ${data.error}`);
                break;
            }
          }
        }
      });
    }
  );

  req.write(body);
  req.end();
}

runAgentStream("write a haiku about Rust");
```

---

## 8. 取消正在运行的 Session

### 8.1 请求

```http
POST /agents/{agent_type}/cancel?session_id=s-abc123 HTTP/1.1
```

**路径参数**:

| 参数 | 类型 | 说明 |
|------|------|------|
| `agent_type` | string | agent 类型名 |

**Query 参数**:

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `session_id` | string | 是 | 要取消的 session ID(从 SSE `session_created` 事件获取) |

> 请求体为空。

### 8.2 响应

**成功 (200)**:

```json
{
  "success": true,
  "message": "cancelled",
  "session_id": "s-abc123"
}
```

**session 不存在 (404)**:

session 已结束或不存在时返回 404(无响应体)。

### 8.3 取消机制说明

取消信号在三个层级生效:

```
POST /cancel ──→ SessionStore 查找 token ──→ token.cancel()
                                              │
                    ┌─────────────────────────┼─────────────────────────┐
                    ▼                         ▼                         ▼
             event 边界检查            LLM 调用中 select!         LLM 流式 chunk 边界
             (run / run_streaming)     (handle_io_request)       (llm_stream.next)
                    │                         │                         │
                    ▼                         ▼                         ▼
             优雅清理:                  优雅清理:                  优雅清理:
             flush 消息                 提交 error io_response      提交 error io_response
             返回 cancelled             flush 消息                 flush 消息
                                       返回 cancelled              yield Error + Done
```

**关键行为**:
- 取消后,agent 在**下一个 event 边界**或 **LLM chunk 边界**响应(不是立即杀死)
- 响应前会优雅清理:提交 error io_response(防止 evorule 卡死等待 IoResponse)、flush 缓冲消息
- 流式执行被取消时,SSE 流会发出 `error` 事件 + `done` 事件(终帧),然后关闭流
- 取消后 session 从 SessionStore 移除,重复 cancel 同一 session 返回 404

**取消后的 SSE 流**:

```text
event: error
data: {"error":"Internal error: cancelled by user"}

event: done
data: {"success":false,"content":"","steps":2,"duration_ms":1543,"tool_calls":[],"error":"cancelled by user"}
```

### 8.4 完整取消示例

#### curl

```bash
# 步骤 1: 启动流式执行(后台运行,捕获 session_id)
SESSION_ID=$(curl -N -s -X POST http://127.0.0.1:8081/agents/general/run/stream \
  -H "Content-Type: application/json" \
  -d '{"agent_type": "general", "goal": "write a very long essay about everything"}' \
  | grep -m1 "session_created" | sed 's/.*"session_id":"\([^"]*\)".*/\1/')

echo "session_id: $SESSION_ID"

# 步骤 2: 取消
curl -X POST "http://127.0.0.1:8081/agents/general/cancel?session_id=$SESSION_ID"
# {"success":true,"message":"cancelled","session_id":"s-abc123"}
```

#### Python(异步取消)

```python
"""
流式执行 + 异步取消示例
依赖: pip install requests
"""
import json
import threading
import requests

session_id_holder = {"id": None}
cancel_flag = threading.Event()


def consume_stream():
    """消费 SSE 流"""
    url = "http://127.0.0.1:8081/agents/general/run/stream"
    payload = {"agent_type": "general", "goal": "write a very long essay"}

    with requests.post(url, json=payload, stream=True, timeout=300) as resp:
        event_name = None
        for line in resp.iter_lines(decode_unicode=True):
            if not line:
                event_name = None
                continue
            if line.startswith("event: "):
                event_name = line[7:]
            elif line.startswith("data: "):
                data = json.loads(line[6:])

                if event_name == "session_created":
                    session_id_holder["id"] = data["session_id"]
                    print(f"[session] {data['session_id']}")
                    cancel_flag.set()  # 通知主线程可以取消了

                elif event_name == "llm_delta":
                    print(data["text"], end="", flush=True)

                elif event_name == "done":
                    print(f"\n=== Done: success={data['success']} ===")
                    if data.get("error"):
                        print(f"error: {data['error']}")
                    break

                elif event_name == "error":
                    print(f"\n[error] {data['error']}")


# 启动流式消费线程
stream_thread = threading.Thread(target=consume_stream)
stream_thread.start()

# 等 session_id 就绪
cancel_flag.wait(timeout=10)

# 取消
if session_id_holder["id"]:
    print(f"\n[cancelling session {session_id_holder['id']}]")
    resp = requests.post(
        f"http://127.0.0.1:8081/agents/general/cancel",
        params={"session_id": session_id_holder["id"]},
    )
    print(f"[cancel response] {resp.status_code} {resp.json()}")

stream_thread.join(timeout=30)
print("[done]")
```

#### JavaScript(浏览器 + AbortController)

```javascript
/**
 * 流式执行 + 用户取消示例(浏览器)
 * 同时演示两种取消方式:
 *   1. /cancel 端点(server 端优雅取消)
 *   2. AbortController(客户端断开 SSE 连接)
 */
async function runWithCancel(goal) {
  let sessionId = null;

  // 方式 1: /cancel 端点取消
  const controller = new AbortController();

  const streamPromise = (async () => {
    const resp = await fetch(
      "http://127.0.0.1:8081/agents/general/run/stream",
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ agent_type: "general", goal }),
        signal: controller.signal,
      }
    );

    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    let eventName = null;

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;

      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop();

      for (const line of lines) {
        if (line === "") { eventName = null; continue; }
        if (line.startsWith("event: ")) { eventName = line.slice(7); }
        else if (line.startsWith("data: ")) {
          const data = JSON.parse(line.slice(6));
          if (eventName === "session_created") {
            sessionId = data.session_id;
            console.log(`[session] ${sessionId}`);
          } else if (eventName === "llm_delta") {
            document.getElementById("output").textContent += data.text;
          } else if (eventName === "done") {
            console.log("[done]", data);
            return;
          }
        }
      }
    }
  })();

  // 用户点击"停止"按钮时调用
  window.cancelAgent = async function () {
    if (sessionId) {
      // 方式 1: 调用 /cancel 端点(server 端优雅取消,会发送 error + done 事件)
      await fetch(
        `http://127.0.0.1:8081/agents/general/cancel?session_id=${sessionId}`,
        { method: "POST" }
      );
      console.log("[cancel sent]");
    } else {
      // 方式 2: session_id 还没拿到,直接断开连接
      controller.abort();
      console.log("[aborted connection]");
    }
  };

  await streamPromise;
}

// 使用
runWithCancel("write a haiku about Rust");
// 用户点"停止"按钮 → window.cancelAgent()
```

---

## 9. 端到端完整示例

以下示例展示完整的"启动流式执行 → 实时显示 → 用户取消"流程:

### Python 完整示例

```python
"""
evo-agent 端到端示例: 流式执行 + 取消
用法: python example.py
依赖: pip install requests
"""
import json
import sys
import threading
import requests

SERVER = "http://127.0.0.1:8081"
AGENT_TYPE = "general"


def list_agents():
    """列出可用 agent"""
    resp = requests.get(f"{SERVER}/agents", timeout=10)
    agents = resp.json()["agents"]
    print("可用 agent:")
    for a in agents:
        print(f"  - {a['agent_type']} (v{a['version']}): {a['description']}")
    return agents


def run_streaming(goal, auto_cancel_after=None):
    """
    流式执行 agent
    auto_cancel_after: 自动取消的秒数(None = 不自动取消,等用户按 Enter)
    """
    session_id = [None]
    stop_flag = threading.Event()

    def consume():
        url = f"{SERVER}/agents/{AGENT_TYPE}/run/stream"
        payload = {"agent_type": AGENT_TYPE, "goal": goal}

        try:
            with requests.post(url, json=payload, stream=True, timeout=300) as resp:
                event_name = None
                for line in resp.iter_lines(decode_unicode=True):
                    if not line:
                        event_name = None
                        continue
                    if line.startswith("event: "):
                        event_name = line[7:]
                    elif line.startswith("data: "):
                        data = json.loads(line[6:])
                        handle_event(event_name, data, session_id)
                        if event_name == "done":
                            return
        except requests.exceptions.ConnectionError:
            print("\n[连接断开]")

    def handle_event(name, data, sid):
        if name == "session_created":
            sid[0] = data["session_id"]
            print(f"[session] {data['session_id']}")
            stop_flag.set()  # session_id 就绪
        elif name == "step":
            print(f"\n--- step {data['step']} ---")
        elif name == "llm_delta":
            sys.stdout.write(data["text"])
            sys.stdout.flush()
        elif name == "llm_done":
            reason = data.get("finish_reason", "?")
            print(f"\n  [llm done: {reason}]")
        elif name == "tool_call":
            print(f"  [tool call] {data['name']}({data['args']})")
        elif name == "tool_result":
            print(f"  [tool result] {data['name']} → {data['result']}")
        elif name == "info":
            print(f"  [info] {data['message']}")
        elif name == "error":
            print(f"\n  [error] {data['error']}")
        elif name == "done":
            print(f"\n{'='*40}")
            print(f"  success: {data['success']}")
            print(f"  steps:   {data['steps']}")
            print(f"  time:    {data['duration_ms']}ms")
            if data.get("error"):
                print(f"  error:   {data['error']}")

    # 启动消费线程
    t = threading.Thread(target=consume)
    t.start()

    # 等 session_id
    stop_flag.wait(timeout=15)

    if auto_cancel_after:
        # 自动取消模式
        import time
        time.sleep(auto_cancel_after)
        if session_id[0]:
            print(f"\n[自动取消 session {session_id[0]}]")
            cancel(session_id[0])
    else:
        # 手动取消模式
        input("\n按 Enter 取消...")
        if session_id[0]:
            cancel(session_id[0])

    t.join(timeout=30)


def cancel(session_id):
    """取消正在运行的 session"""
    resp = requests.post(
        f"{SERVER}/agents/{AGENT_TYPE}/cancel",
        params={"session_id": session_id},
        timeout=10,
    )
    if resp.status_code == 200:
        print(f"[取消成功] {resp.json()}")
    else:
        print(f"[取消失败] HTTP {resp.status_code}")


if __name__ == "__main__":
    print("=" * 50)
    print("evo-agent 端到端示例")
    print("=" * 50)

    # 1. 列出 agent
    list_agents()
    print()

    # 2. 流式执行(3 秒后自动取消)
    print("--- 流式执行(3 秒后自动取消)---")
    run_streaming("write a very detailed essay about the history of computing",
                  auto_cancel_after=3)
```

---

## 10. 错误处理

### HTTP 状态码

| 状态码 | 含义 | 出现场景 |
|--------|------|----------|
| 200 | 成功 | 所有 GET 端点、同步 run、SSE 流(流中用 event 传达错误) |
| 404 | 未找到 | agent 类型不存在、cancel 的 session 不在运行中 |
| 500 | 服务器内部错误 | SessionStore 锁中毒等极端情况 |

### SSE 流中的错误

SSE 流不会用 HTTP 错误码传达 agent 执行错误(流一旦建立就是 200)。错误通过 `error` 事件传达:

```text
event: error
data: {"error":"LLM error: rate limit exceeded"}
```

**可恢复错误**(如 LLM 重试中的瞬时错误):
- 发出 `error` 事件后,agent 可能继续执行(G3 重试机制)
- 流不关闭,后续可能有 `step` / `llm_delta` 等事件

**终态错误**(如 max_steps 超限、取消):
- 发出 `error` 事件后,紧跟 `done` 事件(终帧)
- `done` 事件的 `success` 为 `false`,`error` 字段有值

### 流级错误

如果底层 stream 产出 `Err(AgentError)`(非 AgentEvent 级别的错误),也会映射为 `error` 事件:

```text
event: error
data: {"error":"Internal error: ..."}
```

---

## 11. 限制与边界

### 当前限制(0.1.0)

| 限制 | 说明 | 规划 |
|------|------|------|
| **同步 run 无法 HTTP 取消** | `POST /run` 是阻塞的,客户端不知道 session_id,无法调 `/cancel`。同步 run 只能通过 CLI Ctrl+C 取消 | 流式执行是首选方案 |
| **CORS permissive** | 允许所有来源,生产环境需收紧 | 配置化 |
| **无认证** | 当前无 auth 中间件 | 0.2.0 规划 |
| **SessionStore 非持久** | server 重启后 SessionStore 清空,正在运行的 session 无法取消 | 可接受(server 重启 = 所有 session 终止) |
| **单进程** | 不支持多实例共享 SessionStore | 0.3.0 规划(Redis SessionStore) |
| **Body limit 1MB** | 请求体上限 1MB | 够用(goal 文本通常 <10KB) |

### 取消的响应延迟

| 场景 | 响应延迟 |
|------|----------|
| 等待 SSE event | **立即**(select! 在 event_stream.next() 上) |
| LLM 同步调用(run) | **立即**(select! 在 handle_io_request 上) |
| LLM 流式输出(run_streaming) | **下一个 chunk**(select! 在 llm_stream.next() 上,通常 <100ms) |
| 工具执行中 | **工具完成后**(当前不在工具调用中插入 select!) |

> **注意**: 工具执行(如 `shell_exec` 跑长命令)期间不响应取消。这是已知限制,0.2.0 规划在工具执行层加 cancel 检查。

---

## 12. MCP 协议(G12)

evo-agent 可作为 **MCP 客户端**连接外部 MCP server(如 Claude Desktop 的 stdio server、
`@modelcontextprotocol/server-filesystem` 等),把 MCP server 暴露的工具注册到 agent 的
工具集中。

### 配置

在 `evo-agent.toml` 中添加 `[[mcp.servers]]` 段:

```toml
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp.servers]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "ghp_xxx" }
```

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `name` | string | 是 | server 名称,用于工具名前缀 `mcp_{name}_{tool}` |
| `command` | string | 是 | 启动命令(如 `npx` / `node` / `python`) |
| `args` | string[] | 否 | 命令参数 |
| `env` | map | 否 | 注入子进程的环境变量(用于 token 等) |

### 工具命名

MCP 工具注册时加 `mcp_{server}_{tool}` 前缀,避免与内置工具(`file_read` / `http_get` 等)
冲突。例如 `filesystem` server 的 `read_file` 工具 → 注册为 `mcp_filesystem_read_file`。

要让 LLM 使用 MCP 工具,在 `agent.json` 的 `tools` 列表中声明带前缀的全名,并在
`system_prompt` 中说明何时使用。

### 传输层

P1 只实现 **stdio 传输**(子进程 stdin/stdout 收发 JSON-RPC 2.0 消息)。
SSE 传输(远程 MCP server)规划在 P2。

### 启动注册

`evo-agent run` 启动时:
1. 读取 `config.mcp.servers`
2. 对每个 server:spawn 子进程 → `initialize` 握手 → `tools/list` 发现工具
3. 每个工具包装成 `McpToolAdapter`,注册到 `ToolHandler`
4. 某个 server 启动失败 → 跳过该 server(不影响其他 server / agent 启动)

> **serve 模式边界**:P1 阶段 MCP 工具仅在 `evo-agent run` 中生效。`serve` 模式的
> 工具架构(含内置工具 + MCP)改造留待后续迭代。

### 架构

```text
ToolHandler.register_tool("mcp_filesystem_read_file", adapter)
        │
        ▼  ToolFunction::call (async, G13)
  McpToolAdapter  ──►  McpClient.call_tool("read_file", args)
                              │
                              ▼  JSON-RPC 2.0
                        StdioTransport  ──►  MCP server 子进程 (stdin/stdout)
```

---

## 附录: 数据类型汇总

### AgentRunRequest

```json
{
  "agent_type": "string",
  "goal": "string",
  "max_steps": 20,
  "temperature": 0.3,
  "model": "string"
}
```

### AgentRunResponse

```json
{
  "success": true,
  "content": "string",
  "steps": 3,
  "duration_ms": 15234,
  "error": null
}
```

### AgentResult(SSE `done` 事件的 data)

```json
{
  "success": true,
  "content": "string",
  "steps": 3,
  "duration_ms": 15234,
  "tool_calls": ["search_files", "file_read"],
  "error": null
}
```

### CancelResponse

```json
{
  "success": true,
  "message": "cancelled",
  "session_id": "s-abc123"
}
```
