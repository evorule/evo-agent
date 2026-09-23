// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { writable } from 'svelte/store';
import { listSessions, readFile, writeFile, getAgentDef, getEvolutionSignals } from './api.js';

/** 连接状态: connecting | online | offline */
export const connStatus = writable('offline');

/** 当前会话 id(null = 尚未创建) */
export const sessionId = writable(null);

/** 当前轮次执行中(控制发送/中断按钮与流式光标) */
export const turnActive = writable(false);

/** 当前轮次步数(Step 事件累计) */
export const stepCount = writable(0);

/** 消息列表。元素: {id, kind, ...}
 *  kind: user | assistant | tool | info | error | approval */
export const messages = writable([]);

let seq = 0;
export function pushMessage(msg) {
  messages.update((arr) => [...arr, { id: ++seq, ...msg }]);
  return seq;
}
export function updateMessage(id, patch) {
  messages.update((arr) => arr.map((m) => (m.id === id ? { ...m, ...patch } : m)));
}

// ---- 会话列表(对话与历史阶段) ----

/** 会话列表(serve 本地索引,按最近活跃降序) */
export const sessions = writable([]);

/** 拉取会话列表;失败静默(列表是辅助面) */
export async function refreshSessions() {
  try {
    const res = await listSessions();
    sessions.set(res.sessions || []);
  } catch {
    /* 列表拉取失败不阻断 */
  }
}

/** MessageRecord(evorule 投影)→ UI 消息 */
export function recordToMessage(r) {
  if (r.role === 'user') {
    return { kind: 'user', text: r.content };
  }
  if (r.role === 'assistant') {
    if (r.tool_calls) {
      // 有工具调用的 assistant 消息:每个调用一张完成态工具卡
      let calls = [];
      try {
        calls = Array.isArray(r.tool_calls) ? r.tool_calls : [r.tool_calls];
      } catch {
        calls = [];
      }
      return calls.map((c) => ({
        kind: 'tool',
        name: c?.name || c?.function?.name || 'tool',
        args: c?.arguments ?? c?.function?.arguments ?? null,
        result: null,
        running: false,
      }));
    }
    return { kind: 'assistant', text: r.content };
  }
  if (r.role === 'tool') {
    return {
      kind: 'tool',
      name: r.tool_name || 'tool',
      args: null,
      result: r.content,
      running: false,
    };
  }
  return null; // system 等不渲染
}

/** 历史消息回灌(清空当前消息,按投影顺序渲染) */
export function loadTranscriptInto(records) {
  let id = 0;
  const out = [];
  for (const r of records) {
    const m = recordToMessage(r);
    if (Array.isArray(m)) {
      for (const item of m) out.push({ id: ++id, ...item });
    } else if (m) {
      out.push({ id: ++id, ...m });
    }
  }
  // seq 接到全局序号之后,避免与实时消息 id 冲突
  messages.set(out);
  if (out.length > 0) seq = Math.max(seq, id);
}

// ---- 编辑器 tabs(标准 IDE 基础阶段) ----

/** 打开的编辑器 tab 列表。元素:{path, name, content, dirty, error} */
export const tabs = writable([]);

/** 当前激活 tab 的文件路径(null = 无激活 tab,显示欢迎页) */
export const activePath = writable(null);

/** 按路径查找已打开的 tab(tabs 快照上) */
export function findTab(list, path) {
  return list.find((t) => t.path === path);
}

/** 打开文件:已开则激活;否则请求内容后开新 tab。返回错误消息或 null */
export async function openFile(path) {
  const name = path.split('/').pop() || path;
  let existing = null;
  tabs.update((list) => {
    existing = findTab(list, path) || null;
    return list;
  });
  if (existing) {
    activePath.set(path);
    return null;
  }
  try {
    const res = await readFile(path);
    const content = res.content ?? '';
    tabs.update((list) => [...list, { path, name, content, dirty: false, error: null }]);
    activePath.set(path);
    return null;
  } catch (e) {
    const msg = String(e?.message || e);
    tabs.update((list) => [...list, { path, name, content: '', dirty: false, error: msg }]);
    activePath.set(path);
    return msg;
  }
}

/** 关闭 tab:返回相邻 tab 路径(若关的是激活 tab) */
export function closeTab(path) {
  let next = null;
  tabs.update((list) => {
    const idx = list.findIndex((t) => t.path === path);
    const rest = list.filter((t) => t.path !== path);
    if (rest.length > 0) {
      next = rest[Math.min(idx, rest.length - 1)].path;
    }
    return rest;
  });
  activePath.update((cur) => (cur === path ? next : cur));
}

/** 标记 tab 内容已变(dirty) */
export function markDirty(path, dirty) {
  tabs.update((list) => list.map((t) => (t.path === path ? { ...t, dirty } : t)));
}

/** 保存完成后回写基准内容与 dirty=false */
export function markSaved(path, content) {
  tabs.update((list) =>
    list.map((t) => (t.path === path ? { ...t, content, dirty: false, error: null } : t)),
  );
}

/** 保存文件(content 由调用方从编辑器 model 取)。返回错误消息或 null */
export async function saveTab(path, content) {
  try {
    await writeFile(path, content);
    markSaved(path, content);
    return null;
  } catch (e) {
    return String(e?.message || e);
  }
}

// ---- 治理叠加阶段:事件流水(底部面板)+ 治理徽标 ----

const EVENT_CAP = 500; // 环形缓冲上限,防长会话内存膨胀

function pushCapped(arr, entry) {
  const next = [...arr, entry];
  return next.length > EVENT_CAP ? next.slice(next.length - EVENT_CAP) : next;
}

/** 系统事件流水(输出 tab):SessionCreated/Step/Info/Error/Done 等非内容帧 */
export const sysEvents = writable([]);

/** 治理事件流(审计 tab):工具调用/审批/错误等治理可见事件 */
export const govEvents = writable([]);

let evSeq = 0;
/** 底部面板事件入口(kind: sys | gov;label/level 决定渲染样式) */
export function pushPanelEvent(kind, entry) {
  const e = { id: ++evSeq, time: Date.now(), ...entry };
  if (kind === 'gov') {
    govEvents.update((arr) => pushCapped(arr, e));
  } else {
    sysEvents.update((arr) => pushCapped(arr, e));
  }
}

/** agent 工具白名单(白名单徽标;null = 未加载) */
export const toolWhitelist = writable(null);

/** 当前会话违规信号数(信号徽标;null = 未加载/不可用) */
export const signalCount = writable(null);

/** 加载治理徽标数据(白名单 + 信号;均 fail-soft,失败置 null 显示「—」) */
export function refreshGovBadges(sid) {
  getAgentDef('general')
    .then((def) => toolWhitelist.set(Array.isArray(def.tools) ? def.tools : []))
    .catch(() => toolWhitelist.set(null));
  if (sid) {
    getEvolutionSignals(sid)
      .then((s) => signalCount.set(typeof s.total_violations === 'number' ? s.total_violations : null))
      .catch(() => signalCount.set(null));
  } else {
    signalCount.set(null);
  }
}
