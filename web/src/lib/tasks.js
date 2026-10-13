// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 任务状态管理:任务清单(.evo/tasks.json)/运行中 map/任务输出 channel 缓冲/
// 诊断桥(setDiagnostics owner=task.<label>)/首跑确认(一次性信任)。
//
// 治理口径:任务只由人的显式动作发起(永不自动运行);命令原文进治理流水。

import { writable, get } from 'svelte/store';
import { outputChannel } from './panel.js';
import { setDiagnostics, clearDiagnostics } from './diagnostics.js';
import { pushPanelEvent } from './stores.js';

/** 工作区任务清单(GET /api/tasks;含 error/hint) */
export const taskList = writable({ tasks: [], error: null, hint: null, loaded: false });

/** 运行中/最近 run(runId → {label,state,exitCode,startedAt}) */
export const taskRuns = writable({});

/** 最近一次运行的 label(rerunLast/默认构建消费) */
export const lastTaskLabel = writable(null);

/** 任务输出 channel 清单(['task:<label>',…];注册顺序即面板下拉序) */
export const taskChannelIds = writable([]);

/** 缓冲版本号(视图层订阅触发重渲染;内容本体在本模块 Map 内) */
export const taskBufferVersion = writable(0);

/** 当前激活任务 channel 的行快照(渲染直接消费;模板零函数调用) */
export const taskLines = writable([]);

/** 诊断自动开面板开关(设置键快照;默认开) */
export const autoRevealProblems = writable(true);

/** 行缓冲上限(头部截断+提示,内存护栏) */
const CHANNEL_LINE_CAP = 5000;

/** 当前激活 channel 键镜像(pushLine 判定用) */
let outputChannelKey = 'syslog';
outputChannel.subscribe((v) => {
  outputChannelKey = v;
  const label = typeof v === 'string' && v.startsWith('task:') ? v.slice(5) : null;
  if (label) {
    const buf = channelBuffers.get(label);
    taskLines.set(buf ? [...buf.lines] : []);
  } else {
    taskLines.set([]);
  }
});
/** 重连回放上限外的本地行缓冲一致口径 */
const CONFIRM_KEY = 'evo_task_confirmed';

/** label → { lines: [{time,stream,text}], truncated: number } */
const channelBuffers = new Map();
/** runId → { label, ws } */
const liveSockets = new Map();

function token() {
  return localStorage.getItem('evo_agent_token') || '';
}

function headers(json = true) {
  const t = token();
  const h = t ? { Authorization: `Bearer ${t}` } : {};
  return json ? { ...h, 'Content-Type': 'application/json' } : h;
}

async function unwrap(resp) {
  if (!resp.ok) {
    const raw = await resp.text().catch(() => '');
    throw new Error(raw || `HTTP ${resp.status}`);
  }
  return resp.json();
}

/** 拉取任务清单 */
export async function refreshTaskList() {
  try {
    const res = await fetch('/api/tasks', { headers: headers(false) }).then(unwrap);
    taskList.set({ ...res, loaded: true });
    return res.tasks || [];
  } catch (e) {
    taskList.set({ tasks: [], error: String(e?.message || e), loaded: true });
    return [];
  }
}

/** 命令原文指纹(首跑确认一次性信任键) */
function commandFingerprint(commandDisplay) {
  let hash = 0;
  for (let i = 0; i < commandDisplay.length; i++) {
    hash = (hash * 31 + commandDisplay.charCodeAt(i)) | 0;
  }
  return String(hash);
}

function confirmedSet() {
  try {
    return JSON.parse(localStorage.getItem(CONFIRM_KEY) || '{}');
  } catch {
    return {};
  }
}

/** 是否需要首跑确认(命令原文未被信任过) */
export function needsConfirm(commandDisplay) {
  return !confirmedSet()[commandFingerprint(commandDisplay)];
}

/** 记住信任(确认框「运行并记住」) */
function rememberConfirm(commandDisplay) {
  const set = confirmedSet();
  set[commandFingerprint(commandDisplay)] = true;
  localStorage.setItem(CONFIRM_KEY, JSON.stringify(set));
}

/** channel 键与缓冲访问 */
function ensureBuffer(label) {
  const key = `task:${label}`;
  if (!channelBuffers.has(label)) {
    channelBuffers.set(label, { lines: [], truncated: 0 });
    taskChannelIds.update((ids) => (ids.includes(key) ? ids : [...ids, key]));
  }
  return channelBuffers.get(label);
}

export function getTaskChannelBuffer(label) {
  return channelBuffers.get(label) || { lines: [], truncated: 0 };
}

export function clearTaskChannel(label) {
  channelBuffers.set(label, { lines: [], truncated: 0 });
  taskBufferVersion.update((n) => n + 1);
}

/** file:line(:col)? 链接预解析(存储层一次;渲染零正则) */
function computeLink(text) {
  const m = /^([^\s:][^:]*?:\d+(?::\d+)?)/.exec(text);
  if (!m) return null;
  const parts = m[0].split(':');
  const col = parts.length >= 3 ? parseInt(parts[parts.length - 1], 10) : 1;
  const lineNo = parseInt(parts[parts.length - 2], 10);
  const path = parts.slice(0, parts.length - 2).join(':');
  if (!path || !Number.isFinite(lineNo) || lineNo <= 0) return null;
  return { linkText: m[0], path, line: lineNo, col: Math.max(0, col - 1), endCol: col };
}

function pushLine(label, stream, text) {
  const buf = ensureBuffer(label);
  buf.lines.push({ time: Date.now(), stream, text });
  if (buf.lines.length > CHANNEL_LINE_CAP) {
    buf.lines.splice(0, buf.lines.length - CHANNEL_LINE_CAP);
    buf.truncated += 1;
  }
  taskBufferVersion.update((n) => n + 1);
  // 激活 channel 的行快照(渲染层直读)
  if (outputChannelKey === `task:${label}`) {
    taskLines.set(buf.lines.map((r) => ({ ...r, link: computeLink(r.text) })));
  }
}

function setRunState(runId, patch) {
  taskRuns.update((m) => ({
    ...m,
    [runId]: { ...(m[runId] || {}), ...patch },
  }));
}

/** 诊断条目→单源 markers(后端已按 Monaco severity 值/1-based 行列下发;按条目 path 分组) */
function applyDiagnostics(owner, items) {
  const byPath = new Map();
  for (const d of items || []) {
    const p = d.path || owner;
    if (!byPath.has(p)) byPath.set(p, []);
    byPath.get(p).push({
      message: d.msg ?? '',
      severity: d.severity ?? 8,
      startLineNumber: d.line ?? 1,
      startColumn: d.col ?? 1,
      endLineNumber: d.line ?? 1,
      endColumn: (d.col ?? 1) + 40,
      source: 'task',
      code: d.code || undefined,
    });
  }
  setDiagnostics(owner, owner, []);
  for (const [p, ms] of byPath) {
    setDiagnostics(p, owner, ms);
  }
}

/** 连接一个 run 的事件流(回放+活播) */
function connectRun(runId, label) {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(
    `${proto}://${location.host}/api/tasks/ws?run_id=${encodeURIComponent(runId)}${token() ? `&token=${encodeURIComponent(token())}` : ''}`,
  );
  liveSockets.set(runId, ws);
  const owner = `task.${label}`;
  ws.onmessage = (ev) => {
    let msg;
    try {
      msg = JSON.parse(ev.data);
    } catch {
      return;
    }
    switch (msg.type) {
      case 'started':
        // 运行开始:清同 owner 旧诊断(防叠加),回显命令行
        clearDiagnostics(owner);
        pushLine(label, 'sys', `$ ${msg.command}`);
        break;
      case 'output':
        pushLine(label, msg.stream, msg.line);
        break;
      case 'diagnostics':
        applyDiagnostics(msg.owner, msg.items);
        break;
      case 'ended': {
        pushLine(label, 'sys', `进程退出(code ${msg.exitCode}${msg.cancelled ? ',已取消' : ''},${Math.round(msg.durationMs / 100) / 10}s)`);
        setRunState(runId, { state: 'exited', exitCode: msg.exitCode, cancelled: msg.cancelled });
        if (get(lastTaskLabel) === label) {
          // 最近任务保持
        }
        // autoRevealProblems:非零退出/扫出问题→自动开问题面板(调用方接线)
        if (get(autoRevealProblems)) {
          window.dispatchEvent(new CustomEvent('evo:task-problems', { detail: { label, exitCode: msg.exitCode } }));
        }
        break;
      }
      case 'gov_event': {
        const e = msg.event || {};
        pushPanelEvent('gov', {
          label: e.action,
          detail: `${e.label} · ${e.command}${e.exitCode != null ? ` · exit=${e.exitCode}` : ''}`,
        });
        break;
      }
      case 'error':
        pushLine(label, 'sys', msg.hint || '任务错误');
        break;
      default:
        break;
    }
  };
  ws.onclose = () => {
    liveSockets.delete(runId);
    const runs = get(taskRuns);
    const run = runs[runId];
    if (run && run.state === 'running') {
      setRunState(runId, { state: 'disconnected' });
    }
  };
}

/**
 * 运行任务(首跑确认由调用方先行;此函数直接发起)。
 * 同 label 运行中/并发上限→抛错(调用方 toast)。
 */
export async function startTask(label) {
  const tasks = get(taskList).tasks || [];
  const task = tasks.find((t) => t.label === label);
  if (!task) throw new Error(`任务不存在: ${label}`);
  const res = await fetch('/api/tasks/run', {
    method: 'POST',
    headers: headers(),
    body: JSON.stringify({ label }),
  });
  if (!res.ok) {
    throw new Error((await res.text().catch(() => '')) || `HTTP ${res.status}`);
  }
  const { runId } = await res.json();
  ensureBuffer(label);
  clearTaskChannel(label);
  lastTaskLabel.set(label);
  setRunState(runId, { label, state: 'running', startedAt: Date.now() });
  connectRun(runId, label);
  taskBufferVersion.update((n) => n + 1);
  return runId;
}

/** 取消运行(两段式:第一次优雅,5s 窗口后强杀;再次点击立即强杀) */
export async function cancelTaskRun(runId) {
  const res = await fetch(`/api/tasks/${encodeURIComponent(runId)}/cancel`, {
    method: 'POST',
    headers: headers(false),
  });
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new Error(text || `HTTP ${res.status}`);
  }
  setRunState(runId, { cancelling: true });
}

/** 最近活动 run(terminate 命令消费) */
export function newestActiveRun() {
  const runs = get(taskRuns);
  const active = Object.entries(runs).filter(([, r]) => r.state === 'running');
  if (!active.length) return null;
  active.sort((a, b) => (b[1].startedAt || 0) - (a[1].startedAt || 0));
  return active[0][0];
}

/** 运行并处理首跑确认(视图层提供确认回调;确认框挂起期间 Promise 挂起) */
export async function runTaskWithConfirm(task, { onConfirm } = {}) {
  if (needsConfirm(task.commandDisplay) && onConfirm) {
    const ok = await onConfirm(task);
    if (!ok) return null;
    rememberConfirm(task.commandDisplay);
  } else if (needsConfirm(task.commandDisplay)) {
    rememberConfirm(task.commandDisplay); // 无确认 UI 的调用方(不应存在)默认记住
  }
  return startTask(task.label);
}
