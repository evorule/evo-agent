// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// S2 对话与历史:真实 LLM API E2E 探针(Node 原生 WS)
// 流程:新会话发消息 → 等 Done → 回报 session id(供后续 REST 验证)
const sid = process.argv[2] || 'new';
const text = process.argv[3] || '请只回复四个字:探针就绪';
const proto = process.env.WS_PROTO || 'ws';
const port = process.env.WS_PORT || '8081';
const ws = new WebSocket(`${proto}://127.0.0.1:${port}/api/sessions/${sid}/ws?agent_type=general`);
const t0 = Date.now();
let session = null, deltas = 0, chars = 0, done = false, errored = false;

ws.onopen = () => ws.send(JSON.stringify({ type: 'message', content: text }));
ws.onmessage = (ev) => {
  const f = JSON.parse(ev.data);
  if (f.type === 'SessionCreated') { session = f.session_id; console.error(`[probe] SessionCreated session=${session} @${Date.now() - t0}ms`); }
  else if (f.type === 'LlmDelta') { deltas++; chars += (f.text || '').length; }
  else if (f.type === 'ToolCall') console.error(`[probe] ToolCall ${f.name} args=${JSON.stringify(f.args ?? {}).slice(0, 120)}`);
  else if (f.type === 'ToolResult') console.error(`[probe] ToolResult ${f.name} kind=${typeof f.result}${typeof f.result === 'object' && f.result ? ` path=${f.result.path ?? '?'}` : ''}`);
  else if (f.type === 'Done') { done = true; console.error(`[probe] Done success=${f.success} steps=${f.steps} deltas=${deltas} chars=${chars} @${Date.now() - t0}ms`); setTimeout(() => ws.close(), 200); }
  else if (f.type === 'Error') { errored = true; console.error(`[probe] Error ${f.error}`); }
};
ws.onclose = () => {
  if (!done && !errored) console.error('[probe] closed before Done');
  console.log(JSON.stringify({ session, done, errored, deltas, chars }));
  process.exit(done ? 0 : 1);
};
setTimeout(() => { console.error('[probe] timeout 120s'); ws.close(); process.exit(2); }, 120000);
