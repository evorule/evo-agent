// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// DAP 装配入口:适配器发现/launch 配置/断点真相源与持久化/会话事件接线/
// 调试控制台缓冲。组件层(DebugView/BreakpointsPanel/…)只读本模块 store。
//
// 断点存储:.evo/debug-breakpoints.json(路径 → 行号数组;跨会话持久,
// 设计档 §三.1)——与运行期会话内断点真相源(session 层 Map)双向同步:
// 启动时读文件灌入,变更(增删/verified 写回)防抖 500ms 写回文件。
//
// 治理:launch 首跑确认沿 B10 先例(一次性信任,localStorage 键),由
// DebugView 在调用 startDebug 前消费 needsDebugConfirm。

import { writable, get } from 'svelte/store';
import { getDapAdapters, getLspStatus, readFile, writeFile } from '../api.js';
import { openFile, pushPanelEvent, sidebarView } from '../stores.js';
import { registerCommand } from '../commands.js';
import { openBottomPanel } from '../panel.js';
import { createDapClient, DAP_STATES } from './client.js';
import { createDebugSession, DBG_STATES } from './session.js';
import { loadLaunchConfigs } from './launch.js';

// ---- 命令注册(B1 命令注册制;模块加载即全局可用——EditorPane 常挂载链上
// import decorations.js → index.js,保证 app 启动即注册) ----
registerCommand({
  id: 'workbench.debug.start',
  title: '调试: 打开运行和调试视图',
  category: '运行',
  run: () => sidebarView.set('debug'),
});
registerCommand({
  id: 'workbench.debug.continue',
  title: '调试: 继续',
  category: '运行',
  run: () => debugContinue().catch(() => {}),
});
registerCommand({
  id: 'workbench.debug.stepOver',
  title: '调试: 单步跳过',
  category: '运行',
  run: () => debugNext().catch(() => {}),
});
registerCommand({
  id: 'workbench.debug.stepInto',
  title: '调试: 单步进入',
  category: '运行',
  run: () => debugStepIn().catch(() => {}),
});
registerCommand({
  id: 'workbench.debug.stepOut',
  title: '调试: 单步跳出',
  category: '运行',
  run: () => debugStepOut().catch(() => {}),
});
registerCommand({
  id: 'workbench.debug.stop',
  title: '调试: 停止',
  category: '运行',
  run: () => debugStop().catch(() => {}),
});
registerCommand({
  id: 'workbench.debug.console',
  title: '调试: 打开调试控制台',
  category: '运行',
  run: () => openBottomPanel('debugconsole'),
});

// ---- stores(组件层消费) ----

/** 会话状态(DBG_STATES 之一)+ detail */
export const debugState = writable({ state: DBG_STATES.IDLE, detail: '' });
/** /api/dap/adapters 白名单清单 [{name,command,langs}] */
export const debugAdapters = writable([]);
/** launch 配置 {configs:[], source|null} */
export const debugLaunch = writable({ configs: [], source: null });
/** 断点真相源镜像:{path: [lines]}(session verified 写回亦落此) */
export const debugBreakpoints = writable({});
/** stopped 命中帧:{path,line,threadId,frameId}|null(编辑器高亮/跳转) */
export const debugStopped = writable(null);
/** 线程/栈帧/作用域变量( stopped 期数据) */
export const debugThreads = writable([]);
export const debugFrames = writable([]);
export const debugVariables = writable([]); // [{name,value,variablesReference}]
/** 调试控制台行缓冲:[{stream:'stdout'|'stderr'|'repl'|'system', text}] */
export const debugConsole = writable([]);
/** launch 首跑确认待决:{config}|null(DebugView 消费) */
export const debugConfirmPending = writable(null);

// ---- 模块级装配态 ----

const CONSOLE_CAP = 1000;
const BP_STORE_PATH = '.evo/debug-breakpoints.json';
const CONFIRM_KEY = 'evo_debug_confirm_v1';
const BP_SAVE_DEBOUNCE_MS = 500;

let inited = false;
let session = null; // createDebugSession 实例(单会话)
let client = null;
let workdirBase = '';
let bpSaveTimer = null;

function logLine(detail, level = 'log') {
  pushPanelEvent('sys', { label: '[dap]', detail, level: level === 'error' ? 'error' : level === 'warning' ? 'warn' : undefined });
}

function pushConsole(stream, text) {
  debugConsole.update((lines) => {
    lines.push({ stream, text, time: Date.now() });
    if (lines.length > CONSOLE_CAP) lines.splice(0, lines.length - CONSOLE_CAP);
    return lines;
  });
}

// ---- 断点持久化 ----

function scheduleBpPersist() {
  clearTimeout(bpSaveTimer);
  bpSaveTimer = setTimeout(() => {
    const snapshot = get(debugBreakpoints);
    writeFile(BP_STORE_PATH, JSON.stringify(snapshot, null, 2)).catch(() => {
      /* 持久化失败不阻断运行(下次变更重试) */
    });
  }, BP_SAVE_DEBOUNCE_MS);
}

async function loadPersistedBreakpoints() {
  try {
    const d = await readFile(BP_STORE_PATH);
    const parsed = JSON.parse(d?.content || '{}');
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      debugBreakpoints.set(parsed);
      return parsed;
    }
  } catch {
    /* 无存档:空表 */
  }
  return {};
}

// ---- 会话事件接线 ----

function handleSessionState(state, detail) {
  debugState.set({ state, detail });
  if (state === DBG_STATES.RUNNING) {
    debugStopped.set(null);
    debugThreads.set([]);
    debugFrames.set([]);
    debugVariables.set([]);
  } else if (state === DBG_STATES.TERMINATED || state === DBG_STATES.ENDED || state === DBG_STATES.FAILED) {
    logLine(`会话 ${state}${detail ? ` — ${detail}` : ''}`, state === DBG_STATES.FAILED ? 'error' : undefined);
  }
}

async function handleStopped(body) {
  try {
    const t = await session.threads();
    const threads = t?.threads || [];
    debugThreads.set(threads);
    const threadId = body?.threadId ?? threads[0]?.id;
    if (threadId == null) return;
    const st = await session.stackTrace(threadId);
    const frames = st?.stackFrames || [];
    debugFrames.set(frames);
    const frame = frames[0];
    if (frame?.source?.path) {
      const path = uriToWorkRel(frame.source.path);
      const line = frame.line;
      debugStopped.set({ path, line, threadId, frameId: frame.id });
      if (path) openFile(path, { line }); // 编辑器跳转命中帧(行高亮由 EditorPane 装饰)
    }
    // 顶层作用域变量(懒展开:仅首层;树展开由面板按 variablesReference 请求)
    if (frame?.id != null) {
      const sc = await session.scopes(frame.id);
      const scopes = sc?.scopes || [];
      const vars = [];
      for (const s of scopes.slice(0, 8)) {
        try {
          const v = await session.variables(s.variablesReference);
          vars.push({ scope: s.name, expensive: s.expensive === true, variables: v?.variables || [], variablesReference: s.variablesReference });
        } catch {
          vars.push({ scope: s.name, expensive: true, variables: [] });
        }
      }
      debugVariables.set(vars);
    }
  } catch (e) {
    logLine(`stopped 数据拉取失败:${e?.message || e}`, 'warning');
  }
}

/** DAP source.path(真实路径)→ 工作区相对路径(尽力;失配返回原值) */
function uriToWorkRel(p) {
  if (!p) return '';
  const norm = String(p).replace(/\\/g, '/');
  const base = workdirBase.replace(/\/+$/, '');
  if (base && norm.toLowerCase().startsWith(base.toLowerCase() + '/')) return norm.slice(base.length + 1);
  return norm;
}

function handleOutput(body) {
  const text = typeof body?.output === 'string' ? body.output : '';
  if (!text) return;
  const stream = body?.category === 'stderr' ? 'stderr' : body?.category === 'console' ? 'repl' : 'stdout';
  for (const line of text.split(/\r?\n/)) {
    if (line !== '') pushConsole(stream, line);
  }
}

// ---- 对外动作 ----

/** 首跑确认(命令指纹=adapter+type+program;一次性信任,同 tasks.js 先例) */
export function needsDebugConfirm(config) {
  let set = {};
  try {
    set = JSON.parse(localStorage.getItem(CONFIRM_KEY) || '{}');
  } catch {
    /* 空表 */
  }
  const fp = `${config?.type}|${config?.program || ''}`;
  return !set[fp];
}

export function markDebugConfirmed(config) {
  let set = {};
  try {
    set = JSON.parse(localStorage.getItem(CONFIRM_KEY) || '{}');
  } catch {
    /* 空表 */
  }
  set[`${config?.type}|${config?.program || ''}`] = true;
  localStorage.setItem(CONFIRM_KEY, JSON.stringify(set));
}

/**
 * 发起调试会话。首跑确认由 DebugView 先行(ConfirmDialog);此处直接发起。
 * @param {string} adapter 白名单适配器名
 * @param {object} config  launch 配置(type/request/name/program/args/cwd/env)
 */
export async function startDebug(adapter, config) {
  if (session && ![DBG_STATES.IDLE, DBG_STATES.ENDED, DBG_STATES.TERMINATED, DBG_STATES.FAILED].includes(get(debugState).state)) {
    throw new Error('调试会话已在进行中');
  }
  client = createDapClient({
    wsUrl: `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/api/dap/ws?adapter=${encodeURIComponent(adapter)}${(localStorage.getItem('evo_agent_token') || '') ? `&token=${encodeURIComponent(localStorage.getItem('evo_agent_token'))}` : ''}`,
    onEvent: (ev) => session?.handleClientEvent(ev),
    onDapEvent: (event, body) => session?.handleDapEvent(event, body),
  });
  session = createDebugSession(client, {
    workdirBase,
    onState: handleSessionState,
    onOutput: handleOutput,
    onStopped: handleStopped,
    onContinued: () => debugStopped.set(null),
    onBreakpoints: (path, linesOrBps) => {
      // verified 写回(数组=行号)与 UI 增删共用入口;存档键用工作区相对路径
      const key = uriToWorkRel(path);
      debugBreakpoints.update((bp) => ({ ...bp, [key]: Array.isArray(linesOrBps) ? linesOrBps : bp[key] || [] }));
      scheduleBpPersist();
    },
  });
  // 灌入存档断点(工作区相对路径;新适配器进程无状态,启动后全量重放)
  session.primeBreakpoints(get(debugBreakpoints));
  debugConsole.set([]);
  pushConsole('system', `启动调试:${adapter} → ${config.name || config.program || ''}`);
  try {
    await session.start(adapter, config);
  } catch (e) {
    pushConsole('stderr', `启动失败:${e?.message || e}`);
    handleSessionState(DBG_STATES.FAILED, e?.message || String(e));
    throw e;
  }
}

/** 增删断点(编辑器 gutter/断点面板共用;会话活动时全量同步该文件) */
export async function toggleBreakpoint(path, line) {
  debugBreakpoints.update((bp) => {
    const lines = bp[path] || [];
    const next = lines.includes(line) ? lines.filter((l) => l !== line) : [...lines, line].sort((a, b) => a - b);
    const out = { ...bp, [path]: next };
    if (!next.length) delete out[path];
    return out;
  });
  scheduleBpPersist();
  if (session) {
    const lines = get(debugBreakpoints)[path] || [];
    try {
      await session.applyBreakpoints(path, lines);
    } catch {
      /* 会话不活动:下次会话启动全量重放 */
    }
  }
}

export async function debugContinue() {
  await session?.continue_();
}
export async function debugNext() {
  await session?.next();
}
export async function debugStepIn() {
  await session?.stepIn();
}
export async function debugStepOut() {
  await session?.stepOut();
}
export async function debugStop() {
  await session?.stop();
}

/** 调试控制台 evaluate(repl) */
export async function debugEvaluate(expression) {
  if (!session) throw new Error('无活动调试会话');
  pushConsole('repl', `> ${expression}`);
  try {
    const r = await session.evaluate(expression, get(debugStopped)?.frameId);
    const result = r?.result ?? '';
    if (result) for (const line of result.split(/\r?\n/)) pushConsole('repl', line);
    return r;
  } catch (e) {
    pushConsole('stderr', String(e?.message || e));
    throw e;
  }
}

/**
 * 装配(幂等):适配器/launch 配置/断点存档装载。
 * workdirBase 借道 /api/lsp/status(同 serve 下发的权威工作区根)。
 */
export async function initDebug() {
  if (inited) return;
  inited = true;
  try {
    const st = await getLspStatus();
    workdirBase = String(st.workdir || '').replace(/\/+$/, '');
  } catch {
    workdirBase = '';
  }
  try {
    const a = await getDapAdapters();
    debugAdapters.set(a?.adapters || []);
    if (!a?.adapters?.length) {
      logLine('dap.adapters 白名单为空(fail-closed)——调试不可用,请在设置中配置适配器', 'warning');
    }
  } catch (e) {
    logLine(`适配器清单获取失败:${e?.message || e}`, 'warning');
  }
  const launch = await loadLaunchConfigs();
  debugLaunch.set(launch);
  if (!launch.configs.length && launch.source) {
    logLine(`${launch.source} 存在但无可用 launch 配置`, 'warning');
  }
  await loadPersistedBreakpoints();
}

export { DBG_STATES, DAP_STATES };
