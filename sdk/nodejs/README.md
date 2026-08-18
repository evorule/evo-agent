# evo-agent Node.js SDK

零依赖的 evo-agent HTTP/SSE 客户端。仅使用 Node.js 内置模块，无需 `npm install` 任何第三方包。

## 安装

### 方式 1: 本地 link

```bash
cd sdk/nodejs
npm link
# 在你的项目里:
npm link evo-agent-node
```

### 方式 2: 直接引用路径

```json
{
  "dependencies": {
    "evo-agent-node": "file:../evo-agent/sdk/nodejs"
  }
}
```

### 方式 3: 直接 require

```bash
# 把 sdk/nodejs 目录复制到你的项目里,直接 require
const { EvoAgentClient } = require('./path/to/sdk/nodejs');
```

## 快速开始

```javascript
const { EvoAgentClient } = require('evo-agent-node');

const client = new EvoAgentClient('http://127.0.0.1:8081');

// 同步执行
const result = await client.run('general', 'What files are in the current directory?');
console.log(result.content);

// 流式执行(实时显示 LLM 输出)
const stream = await client.runStream('general', 'Write a haiku about Rust');
for await (const event of stream) {
  if (event.type === 'llm_delta') process.stdout.write(event.data.text);
  if (event.type === 'done') console.log('\n\nFinished:', event.data);
}
```

## API

### `new EvoAgentClient(baseUrl, options?)`

创建客户端实例。

| 参数 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `baseUrl` | string | — | server 地址,如 `http://127.0.0.1:8081` |
| `options.timeout` | number | 30000 | 非流式请求超时(ms) |
| `options.streamTimeout` | number | 300000 | 流式请求超时(ms),0=无超时 |

### 方法

| 方法 | 返回 | 说明 |
|------|------|------|
| `health()` | `Promise<string>` | 健康检查,返回 `"ok"` |
| `listAgents()` | `Promise<AgentListResponse>` | 列出所有 agent |
| `getAgent(type)` | `Promise<AgentDefinitionResponse>` | 查看 agent 定义 |
| `run(type, request)` | `Promise<AgentRunResponse>` | 同步执行(阻塞) |
| `runStream(type, request)` | `Promise<AgentEventStream>` | SSE 流式执行 |
| `cancel(type, sessionId)` | `Promise<CancelResponse>` | 取消正在运行的 session |

`run()` 和 `runStream()` 的第二个参数可以是完整请求体 `{ agent_type, goal, ... }`，也可以直接传 goal 字符串(快捷方式)。

## SSE 流式执行

`runStream()` 返回 `AgentEventStream`，支持**两种消费模式**(互斥):

### 模式 1: AsyncIterator(推荐)

```javascript
const stream = await client.runStream('general', 'hello');

for await (const event of stream) {
  switch (event.type) {
    case 'session_created':
      console.log('session:', stream.sessionId);
      break;
    case 'llm_delta':
      process.stdout.write(event.data.text);  // 实时输出
      break;
    case 'done':
      console.log('\nresult:', event.data);
      break;
  }
}
```

### 模式 2: EventEmitter

```javascript
const stream = await client.runStream('general', 'hello');

stream.on('llm_delta', (data) => process.stdout.write(data.text));
stream.on('done', (result) => console.log('\nresult:', result));
stream.on('error', (data) => console.error('error:', data.error));

await stream.consume();  // 阻塞直到流结束
```

### 便捷方法: collect()

只要最终结果,不要中间事件:

```javascript
const stream = await client.runStream('general', 'hello');
const result = await stream.collect();
console.log(result.content);
```

## 事件类型

| `event.type` | `event.data` | 说明 |
|--------------|--------------|------|
| `session_created` | `{ session_id: string }` | session 已创建(首个事件) |
| `step` | `{ step: number }` | 进入第 N 步 |
| `llm_delta` | `{ text: string }` | LLM token 增量(需拼接) |
| `llm_done` | `{ content: string, finish_reason: string\|null }` | 本轮 LLM 完成 |
| `tool_call` | `{ name: string, args: object }` | 工具调用 |
| `tool_result` | `{ name: string, result: object }` | 工具返回 |
| `done` | `AgentResult` | **任务完成(终帧)** |
| `error` | `{ error: string }` | 错误 |
| `info` | `{ message: string }` | 中间状态信息 |

## 取消

```javascript
const stream = await client.runStream('general', 'very long task');

// 方式 1: 用 client 取消
setTimeout(async () => {
  await client.cancel('general', stream.sessionId);
}, 3000);

// 方式 2: 用 stream 自身取消(不依赖 client 实例)
setTimeout(async () => {
  await stream.cancel();  // 内部直接调 /cancel
}, 3000);

// 消费流(取消后会收到 error + done 事件)
for await (const event of stream) {
  if (event.type === 'error') console.log('cancelled:', event.data.error);
  if (event.type === 'done') break;
}
```

> `stream.sessionId` 在 `session_created` 事件后填充。取消前确保它不为 `null`。

## 示例

```bash
# 确保 server 在运行
evo-agent serve --port 8081

# 基础 API 调用(health + list + get)
node examples/quickstart.js

# SSE 流式 + 取消(按 Enter 取消,或 AUTO_CANCEL_MS=3000)
node examples/stream-cancel.js

# EventEmitter 模式
node examples/event-driven.js
```

## 环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `EVO_AGENT_URL` | `http://127.0.0.1:8081` | server 地址(示例脚本用) |
| `AGENT_TYPE` | `general` | agent 类型(示例脚本用) |
| `GOAL` | — | 任务描述(示例脚本用) |
| `AUTO_CANCEL_MS` | `0` | 自动取消延迟(示例脚本用,0=手动) |
| `DEBUG` | — | 设置后输出所有事件的调试日志 |

## 限制

- Node.js >= 14.0.0(需要 `TextDecoder` 和 `for await...of` 支持)
- 零依赖:不依赖 `eventsource`、`axios` 等第三方库
- 两种消费模式(AsyncIterator / EventEmitter)互斥,不能混用
- `collect()` 内部使用 AsyncIterator,与手动迭代互斥

## License

AGPL-3.0-or-later
