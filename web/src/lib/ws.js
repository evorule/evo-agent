// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 工作台 WS 客户端 — 复用 evo-agent G16 双向流协议
// (服务端实现见 src/api/ws_handler.rs:client→server snake_case,
//  server→client PascalCase AgentEvent 帧)
import { get } from 'svelte/store';
import {
  connStatus,
  sessionId,
  turnActive,
  stepCount,
  messages,
  pushMessage,
  updateMessage,
  refreshSessions,
  loadTranscriptInto,
  pushPanelEvent,
  refreshGovBadges,
  registerArtifact,
  loadArtifacts,
  clearArtifacts,
  openFile,
  fsEvents,
} from './stores.js';
import { getTranscript } from './api.js';

let ws = null;
let streamMsgId = null; // 当前轮流式 assistant 气泡
let lastToolMsgId = null; // 最近一个运行中工具气泡(按 name 匹配结果)
let pendingWrite = null; // S4:进行中的 file_write {path, content}(ToolResult 成功即登记产物)

function wsUrl(sid) {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  let url = `${proto}://${location.host}/api/sessions/${sid || 'new'}/ws?agent_type=general`;
  // auth 启用时走 ?token= fallback(与 SSE 同口径);token 由用户侧配置注入
  const token = localStorage.getItem('evo_agent_token');
  if (token) url += `&token=${encodeURIComponent(token)}`;
  return url;
}

export function connect(sid) {
  disconnect();
  connStatus.set('connecting');
  ws = new WebSocket(wsUrl(sid));

  ws.onopen = () => connStatus.set('online');
  ws.onclose = () => {
    connStatus.set('offline');
    turnActive.set(false);
    ws = null;
  };
  ws.onerror = () => connStatus.set('offline');
  ws.onmessage = (ev) => {
    let frame;
    try {
      frame = JSON.parse(ev.data);
    } catch {
      return;
    }
    handleFrame(frame);
  };
}

export function disconnect() {
  if (ws) {
    const sock = ws;
    ws = null;
    try {
      sock.onclose = null;
      sock.close();
    } catch {
      /* already closing */
    }
  }
}

export function sendMessage(content) {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  pushMessage({ kind: 'user', text: content });
  streamMsgId = pushMessage({ kind: 'assistant', text: '', streaming: true });
  stepCount.set(0);
  turnActive.set(true);
  ws.send(JSON.stringify({ type: 'message', content }));
}

export function interrupt() {
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify({ type: 'interrupt' }));
  }
}

/** 新建会话:断开当前连接并以 "new" 重连 */
export function newSession() {
  sessionId.set(null);
  localStorage.removeItem('evo_session_id');
  messages.set([]);
  clearArtifacts();
  connect('new');
}

/** 打开历史会话:断开 → 回灌消息 → 以该会话 id 重连(继续对话) */
export async function openSession(sid) {
  if (!sid || sid === get(sessionId)) return;
  sessionId.set(sid);
  localStorage.setItem('evo_session_id', sid);
  loadArtifacts(sid); // S4:恢复该会话的产物留痕(草稿基线/定稿状态)
  disconnect();
  messages.set([]);
  connStatus.set('connecting');
  try {
    const res = await getTranscript(sid);
    loadTranscriptInto(res.messages || []);
    if (res.source === 'snapshot') {
      // 引擎会话已被 30min 闲置 TTL 回收,历史来自本地快照(非权威副本)
      pushMessage({
        kind: 'info',
        text: `已恢复历史会话(${res.count ?? 0} 条消息,本地快照副本——引擎侧会话已过期;如需继续对话请新建会话)`,
      });
    } else {
      pushMessage({ kind: 'info', text: `已恢复历史会话(${res.count ?? 0} 条消息)` });
    }
  } catch (e) {
    pushMessage({
      kind: 'error',
      text: `历史恢复失败:${String(e?.message || e)}(已重连,可继续发消息)`,
    });
  }
  connect(sid);
}

function handleFrame(f) {
  switch (f.type) {
    case 'SessionCreated': {
      sessionId.set(f.session_id);
      localStorage.setItem('evo_session_id', f.session_id);
      pushMessage({
        kind: 'info',
        text: `会话已建立${f.memory_enabled ? '(记忆已启用)' : ''}`,
      });
      refreshSessions();
      pushPanelEvent('sys', { label: 'SessionCreated', detail: `会话 ${f.session_id} 已建立` });
      refreshGovBadges(f.session_id);
      break;
    }
    case 'Step':
      stepCount.set(f.step);
      break;
    case 'LlmDelta':
      if (streamMsgId !== null) {
        appendToStream(f.text);
      }
      break;
    case 'ToolCall': {
      lastToolMsgId = pushMessage({
        kind: 'tool',
        name: f.name,
        args: safeJson(f.args),
        running: true,
      });
      // S4:记下 file_write 的目标与内容(草稿基线),待成功结果确认
      if (f.name === 'file_write' && f.args && typeof f.args.path === 'string') {
        pendingWrite = {
          path: f.args.path,
          content: typeof f.args.content === 'string' ? f.args.content : '',
        };
      }
      pushPanelEvent('gov', { label: 'ToolCall', detail: f.name, level: 'info' });
      break;
    }
    case 'ToolResult': {
      if (lastToolMsgId !== null) {
        updateMessage(lastToolMsgId, { running: false, result: safeJson(f.result) });
        lastToolMsgId = null;
      }
      // S4:file_write 成功(结果为对象含 path)→ 登记产物 + 编辑器自动打开 + 产物卡
      if (f.name === 'file_write' && pendingWrite && f.result && typeof f.result === 'object') {
        const sid = get(sessionId);
        registerArtifact(pendingWrite.path, pendingWrite.content, sid);
        openFile(pendingWrite.path);
        pushMessage({
          kind: 'artifact',
          path: pendingWrite.path,
          bytes: pendingWrite.content.length,
        });
        pendingWrite = null;
      }
      pushPanelEvent('gov', { label: 'ToolResult', detail: f.name, level: 'ok' });
      break;
    }
    case 'LlmDone':
      // 流式文本即最终内容;LlmDone 仅标记收尾
      break;
    case 'Done': {
      finishStream();
      if (f.success === false && f.error) {
        pushMessage({ kind: 'error', text: String(f.error) });
        pushPanelEvent('gov', { label: 'TurnFailed', detail: String(f.error), level: 'error' });
      } else {
        pushPanelEvent('sys', { label: 'Done', detail: `本轮完成(step ${f.steps ?? '?'})` });
      }
      refreshGovBadges(get(sessionId));
      turnActive.set(false);
      break;
    }
    case 'Error':
      finishStream();
      pushMessage({ kind: 'error', text: String(f.error) });
      pushPanelEvent('gov', { label: 'Error', detail: String(f.error), level: 'error' });
      turnActive.set(false);
      break;
    case 'Info':
      pushMessage({ kind: 'info', text: String(f.message) });
      pushPanelEvent('sys', { label: 'Info', detail: String(f.message) });
      break;
    case 'fs_events':
      // 文件系统事件批量(相对 workdir);Explorer 订阅消费,一次性信号
      fsEvents.set(f.events || { added: [], updated: [], removed: [], moved: [] });
      break;
    case 'ApprovalRequired':
      // 治理叠加:审批卡按钮走既有 G8 /approve 通道
      pushMessage({
        kind: 'approval',
        toolName: f.tool_name,
        command: f.command,
        risk: f.risk,
        alternative: f.alternative,
        proposalId: f.proposal_id,
      });
      pushPanelEvent('gov', {
        label: 'ApprovalRequired',
        detail: `${f.tool_name}(risk ${f.risk ?? '?'}) 待审批`,
        level: 'warn',
      });
      break;
    case 'ApprovalResult':
      pushMessage({
        kind: 'info',
        text: `审批结果:${f.approved ? '已批准' : '已拒绝'}(工具 ${f.tool_name})`,
      });
      pushPanelEvent('gov', {
        label: 'ApprovalResult',
        detail: `${f.tool_name} ${f.approved ? '已批准' : `已拒绝${f.auto_rejected ? '(超时自动)' : ''}`}`,
        level: f.approved ? 'ok' : 'warn',
      });
      break;
    default:
      break;
  }
}

function appendToStream(text) {
  messages.update((arr) =>
    arr.map((m) => (m.id === streamMsgId ? { ...m, text: (m.text || '') + text } : m)),
  );
}

function finishStream() {
  if (streamMsgId !== null) {
    updateMessage(streamMsgId, { streaming: false });
    // 空流式气泡(如纯工具轮)移除占位
    messages.update((arr) => arr.filter((m) => !(m.id === streamMsgId && !m.text)));
    streamMsgId = null;
  }
}

function safeJson(v) {
  try {
    const s = JSON.stringify(v);
    return s && s.length > 400 ? s.slice(0, 400) + '…' : s;
  } catch {
    return String(v);
  }
}

// 页面加载即恢复上次会话(重连续用;历史回显在对话与历史阶段接入)
export function reconnectFromStorage() {
  const sid = localStorage.getItem('evo_session_id');
  connect(sid || 'new');
}
