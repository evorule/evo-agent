// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// S3 治理叠加:审批触发路径真实 LLM E2E 探针(Node 原生 WS)
// 流程:新会话发消息(触发 file_write 候选审批)→ ApprovalRequired →
//       POST /agents/general/approve 交付决定 → 等 Done → 回报结果
// 用法:node tests/ws_approval_probe.mjs approve|reject [自定义任务文本]
import { readFileSync } from 'node:fs';

const mode = process.argv[2] || 'approve';
const text =
  process.argv[3] ||
  '请把文本 S3审批探针 写入文件 workspace/_s3_probe.txt(目录不存在则自动创建)';
const proto = process.env.WS_PROTO || 'ws';
const base = process.env.WS_BASE || '127.0.0.1:8081';

// .env 注入 token(E2E 与 serve 同源配置;无 token 时留空)
function bearer() {
  try {
    const env = readFileSync(new URL('../.env', import.meta.url), 'utf8');
    const m = env.match(/^EVO_AGENT_AUTH_TOKEN=(.+)$/m) || env.match(/^AUTH_TOKEN=(.+)$/m);
    return m ? m[1].trim() : '';
  } catch {
    return '';
  }
}

const ws = new WebSocket(`${proto}://${base}/api/sessions/new/ws?agent_type=general`);
const t0 = Date.now();
let session = null, approvalSeen = false, delivered = false, done = false;
let toolResults = [], errors = [];

async function deliver(proposalId) {
  const res = await fetch(`http://${base}/agents/general/approve`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...(bearer() ? { Authorization: `Bearer ${bearer()}` } : {}) },
    body: JSON.stringify({
      session_id: String(session),
      approved: mode === 'approve',
      proposal_id: proposalId,
      reason: 'E2E 探针审批',
    }),
  });
  const body = await res.text();
  console.error(`[probe] /approve HTTP ${res.status} ${body.slice(0, 120)}`);
  delivered = res.ok;
}

ws.onopen = () => ws.send(JSON.stringify({ type: 'message', content: text }));
ws.onmessage = (ev) => {
  const f = JSON.parse(ev.data);
  if (f.type === 'SessionCreated') {
    session = f.session_id;
    console.error(`[probe] SessionCreated session=${session} @${Date.now() - t0}ms`);
  } else if (f.type === 'ApprovalRequired') {
    approvalSeen = true;
    console.error(`[probe] ApprovalRequired tool=${f.tool_name} risk=${f.risk} proposal=${f.proposal_id} @${Date.now() - t0}ms`);
    deliver(f.proposal_id);
  } else if (f.type === 'ToolCall') {
    console.error(`[probe] ToolCall ${f.name}`);
  } else if (f.type === 'ToolResult') {
    toolResults.push(f.name);
    console.error(`[probe] ToolResult ${f.name} result=${JSON.stringify(f.result).slice(0, 160)}`);
  } else if (f.type === 'ApprovalResult') {
    console.error(`[probe] ApprovalResult approved=${f.approved} auto_rejected=${f.auto_rejected}`);
  } else if (f.type === 'Done') {
    done = true;
    console.error(`[probe] Done success=${f.success} steps=${f.steps} @${Date.now() - t0}ms`);
    setTimeout(() => ws.close(), 200);
  } else if (f.type === 'Error') {
    errors.push(f.error);
    console.error(`[probe] Error ${f.error}`);
  }
};
ws.onclose = () => {
  const ok = approvalSeen && delivered && done && (mode === 'reject' || toolResults.length > 0);
  console.log(JSON.stringify({ session, mode, approvalSeen, delivered, done, toolResults, errors, ok }));
  process.exit(ok ? 0 : 1);
};
setTimeout(() => { console.error('[probe] timeout 180s'); ws.close(); process.exit(2); }, 180000);
