// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 终端会话管理:xterm 实例 + PTY WS 双向泵。
//
// 职责:多实例注册表(termId→instance)/创建(WS binaryType=arraybuffer,尺寸
// 为 fit 实测)/销毁链(dispose 全件)/背压(写队列水位 HIGH 100KB→pause、LOW
// 10KB→resume;16ms 合帧写)/resize 节流(ResizeObserver 由视图层驱动 fit())。
// 后台实例不销毁:WS 与缓冲持续,仅显示切换(切回不丢输出)。

import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebglAddon } from '@xterm/addon-webgl';
import { SearchAddon } from '@xterm/addon-search';
import { Unicode11Addon } from '@xterm/addon-unicode11';
import '@xterm/xterm/css/xterm.css';
import { terminalTheme } from './xterm-theme.js';
import { getSetting } from './settings.js';
import { pushPanelEvent } from './stores.js';
import { wsAuthQuery } from './api.js';

/** 写队列水位(字节):超 HIGH 告诉后端暂停,回落 LOW 恢复 */
export const WATERMARK_HIGH = 100 * 1024;
export const WATERMARK_LOW = 10 * 1024;
/** 合帧窗口(ms):输出风暴时按帧批量 write,降低渲染压力 */
const FLUSH_INTERVAL_MS = 16;
/** resize 控制帧节流(ms) */
const RESIZE_THROTTLE_MS = 200;

/** 实例注册表(导出供视图层存活判定;勿直接改写,走 create/dispose) */
export const instances = new Map();
let counter = 0;
/** 实例清单变更回调(视图层订阅刷新 chips) */
let notify = () => {};

/** 活动终端 id(视图层激活/创建时上报;chat「当前终端」引用定位用) */
let activeTermId = null;

/** 视图层激活某实例时上报(null = 无活动) */
export function setActiveTerminal(termId) {
  activeTermId = termId || null;
}

/** 当前活动实例(无活动或已销毁返回 null;语义从严不回落兜底) */
export function getActiveTerminal() {
  if (activeTermId && instances.has(activeTermId)) return instances.get(activeTermId);
  return null;
}

export function ptyUrl(termId, cols, rows, authq) {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  let url = `${proto}://${location.host}/api/pty/ws?term=${encodeURIComponent(termId)}&cols=${cols}&rows=${rows}`;
  // 会话工作区:与 agent WS 同源(localStorage evo_workspace)——终端 cwd
  // 与写工具族/checkpoint 同根;缺省不携带=serve agent-root 行为零变化。
  const workspace = localStorage.getItem('evo_workspace');
  if (workspace) url += `&workspace=${encodeURIComponent(workspace)}`;
  // G16 换票:authq = `&ticket=...`(Bearer 换一次性短票;?token= 恒 401)
  return url + (authq || '');
}

/** 预取下一个实例 id(视图层先渲染槽位再创建,防 DOM 换键重建) */
export function peekNextTerminalId() {
  return `term-${counter + 1}`;
}


/** 创建一个终端实例并连接 PTY。container 为 xterm 挂载节点;id 可预指定。 */
export function createTerminalSession(container, forcedId) {
  let termId;
  if (forcedId) {
    termId = forcedId;
    // 计数器与预指定 id 对齐(peek 语义:term-N → counter=N),否则 peek 恒返回
    // 同一 id,第二次创建会同 key 覆盖首实例(真机实证)
    const n = parseInt(String(forcedId).replace('term-', ''), 10);
    if (Number.isFinite(n) && n > counter) counter = n;
  } else {
    termId = `term-${++counter}`;
  }
  const fontSize = Number(getSetting('terminal.fontSize') ?? 14);
  const scrollback = Number(getSetting('terminal.scrollback') ?? 1000);

  const term = new Terminal({
    fontSize,
    scrollback,
    theme: terminalTheme,
    fontFamily: '"JetBrains Mono", Consolas, "Microsoft YaHei", monospace',
    cursorBlink: true,
    allowProposedApi: true,
  });
  const fit = new FitAddon();
  const search = new SearchAddon();
  term.loadAddon(fit);
  term.loadAddon(search);
  term.loadAddon(new Unicode11Addon());
  // xterm 6 改名:activeVersion(v5)→activeProvider(v6);失败回落默认宽度处理
  try {
    term.unicode.activeProvider = 'unicode11';
  } catch {
    /* CJK 宽度修正不可用 */
  }
  term.open(container);
  try {
    fit.fit();
  } catch {
    /* 容器尚不可测:WS 打开后视图层会再 fit */
  }

  const inst = {
    termId,
    term,
    fit,
    search,
    container,
    state: 'connecting', // connecting | ready | exited | error
    exitCode: null,
    hint: '',
    ws: null,
    webgl: null,
    /** 出向写队列(合帧)与水位 */
    queue: [],
    outstanding: 0,
    paused: false,
    flushTimer: null,
    pendingResize: null,
    resizeTimer: null,
    disposers: [],
  };
  instances.set(termId, inst);

  const dims = currentDims(inst);
  // G16 换票:票一次性;v1 无断线恢复,无重连换票面。await 窗口内
  // 键盘/resize 事件由 inst.ws 空值守卫兜住(不发送,不崩溃)。
  void (async () => {
    const authq = await wsAuthQuery();
    if (instances.get(termId) !== inst) return; // 期间被销毁/抢代,弃
    const ws = new WebSocket(ptyUrl(termId, dims.cols, dims.rows, authq));
    ws.binaryType = 'arraybuffer';
    inst.ws = ws;

    ws.onopen = () => {
      setState(inst, 'ready');
    };
    ws.onmessage = (ev) => {
      if (ev.data instanceof ArrayBuffer) {
        enqueueWrite(inst, new Uint8Array(ev.data));
        return;
      }
      // 控制帧(Text JSON)
      let msg;
      try {
        msg = JSON.parse(ev.data);
      } catch {
        return;
      }
      switch (msg.type) {
        case 'gov_event': {
          const e = msg.event || {};
          if (e.action === 'pty.created') {
            pushPanelEvent('gov', {
              label: 'pty.created',
              detail: `${e.termId} · ${e.shell} · ${e.cwd} · approver=${e.approver}`,
            });
          } else if (e.action === 'pty.closed') {
            pushPanelEvent('gov', {
              label: 'pty.closed',
              detail: `${e.termId} · reason=${e.reason}${e.exitCode != null ? ` · exit=${e.exitCode}` : ''}`,
            });
          }
          break;
        }
        case 'exit':
          setState(inst, 'exited');
          inst.exitCode = msg.code;
          notify();
          break;
        case 'error':
          setState(inst, 'error');
          inst.hint = msg.hint || '';
          notify();
          break;
        default:
          break;
      }
    };
    ws.onclose = () => {
      if (inst.state === 'ready' || inst.state === 'connecting') {
        // v1 无断线恢复:会话终止,重开即干净会话(设计档降级矩阵)
        setState(inst, 'exited');
        inst.exitCode = null;
        inst.hint = '连接已断开,进程已回收。重新打开终端即新会话。';
        notify();
      }
    };
    ws.onerror = () => {};
  })();

  // 键盘 → WS(Binary)——G16 换票后建连 async 化,await 窗口内 inst.ws 为
  // null,空值守卫兜住(不发送,不崩溃)
  inst.disposers.push(
    term.onData((data) => {
      const sock = inst.ws;
      if (sock && sock.readyState === WebSocket.OPEN) {
        sock.send(new TextEncoder().encode(data));
      }
    }),
    term.onResize(({ cols, rows }) => {
      const sock = inst.ws;
      if (sock && sock.readyState === WebSocket.OPEN) {
        sock.send(JSON.stringify({ type: 'resize', cols, rows }));
      }
    }),
  );

  // 有选区 Ctrl+C=复制,否则 ^C 透传;Ctrl+F 打开查找(视图层消费)
  term.attachCustomKeyEventHandler((ev) => {
    if (ev.type !== 'keydown') return true;
    if (ev.ctrlKey && !ev.shiftKey && !ev.altKey && (ev.key === 'c' || ev.key === 'C')) {
      if (term.hasSelection()) {
        navigator.clipboard?.writeText(term.getSelection()).catch(() => {});
        return false;
      }
      return true; // ^C 中断前台进程
    }
    if (ev.ctrlKey && ev.key === 'f') {
      inst.onSearchIntent?.();
      return false;
    }
    return true;
  });

  notify();
  return inst;
}

/** 当前实测尺寸(fit 不可测时保底 80×24;ConPTY 零尺寸坑) */
function currentDims(inst) {
  try {
    inst.fit.fit();
    const { cols, rows } = inst.term;
    if (cols > 1 && rows > 1) return { cols, rows };
  } catch {
    /* 不可测 */
  }
  return { cols: 80, rows: 24 };
}

function setState(inst, state) {
  inst.state = state;
  notify();
}

/** 出向写:入队+16ms 合帧;水位驱动 pause/resume */
function enqueueWrite(inst, data) {
  inst.queue.push(data);
  if (inst.flushTimer == null) {
    inst.flushTimer = setTimeout(() => flushQueue(inst), FLUSH_INTERVAL_MS);
  }
}

function flushQueue(inst) {
  inst.flushTimer = null;
  if (!inst.queue.length) return;
  const total = inst.queue.reduce((n, c) => n + c.length, 0);
  const merged = new Uint8Array(total);
  let off = 0;
  for (const c of inst.queue) {
    merged.set(c, off);
    off += c.length;
  }
  inst.queue = [];
  inst.outstanding += total;
  inst.term.write(merged, () => {
    inst.outstanding = Math.max(0, inst.outstanding - total);
    maybeWatermark(inst);
  });
  maybeWatermark(inst);
}

function maybeWatermark(inst) {
  if (inst.ws?.readyState !== WebSocket.OPEN) return;
  if (!inst.paused && inst.outstanding > WATERMARK_HIGH) {
    inst.paused = true;
    inst.ws.send(JSON.stringify({ type: 'pause' }));
  } else if (inst.paused && inst.outstanding < WATERMARK_LOW) {
    inst.paused = false;
    inst.ws.send(JSON.stringify({ type: 'resume' }));
  }
}

/** 视图层尺寸变化:fit + resize 帧节流 */
export function refitTerminal(inst) {
  if (inst.state !== 'ready' && inst.state !== 'exited') return;
  const before = { cols: inst.term.cols, rows: inst.term.rows };
  try {
    inst.fit.fit();
  } catch {
    return; // 极小尺寸 fit 抛错:保持现值(防零尺寸)
  }
  const after = { cols: inst.term.cols, rows: inst.term.rows };
  if (before.cols === after.cols && before.rows === after.rows) return;
  clearTimeout(inst.resizeTimer);
  inst.resizeTimer = setTimeout(() => {
    inst.ws?.readyState === WebSocket.OPEN &&
      inst.ws.send(JSON.stringify({ type: 'resize', cols: after.cols, rows: after.rows }));
  }, RESIZE_THROTTLE_MS);
}

/** WebGL 渲染(仅 active 实例启用;上下文耗尽降级 DOM) */
export function attachWebgl(inst) {
  if (inst.webgl || inst.state === 'exited') return;
  try {
    const webgl = new WebglAddon();
    webgl.onContextLoss(() => {
      try {
        webgl.dispose();
      } catch {
        /* 已失联 */
      }
      if (inst.webgl === webgl) inst.webgl = null; // 降级 DOM
    });
    inst.term.loadAddon(webgl);
    inst.webgl = webgl;
  } catch {
    inst.webgl = null; // WebGL 不可用:DOM 渲染
  }
}

export function detachWebgl(inst) {
  if (inst.webgl) {
    try {
      inst.webgl.dispose();
    } catch {
      /* 已失联 */
    }
    inst.webgl = null;
  }
}

/** 手动 kill(× 按钮/命令):关闭 WS——后端断连语义=树级 kill(防孤儿) */
export function killTerminalSession(termId) {
  const inst = instances.get(termId);
  if (!inst) return;
  try {
    inst.ws?.close(1000);
  } catch {
    /* 已断 */
  }
  disposeTerminalSession(termId);
}

/** 销毁实例(dispose 全件;进程回收由后端断连语义保证) */
export function disposeTerminalSession(termId) {
  const inst = instances.get(termId);
  if (!inst) return;
  instances.delete(termId);
  clearTimeout(inst.flushTimer);
  clearTimeout(inst.resizeTimer);
  for (const d of inst.disposers) {
    try {
      d.dispose();
    } catch {
      /* 已清 */
    }
  }
  detachWebgl(inst);
  try {
    inst.ws?.close(1000);
  } catch {
    /* 已断 */
  }
  inst.term.dispose();
  notify();
}

export function listTerminalSessions() {
  return [...instances.values()].map((i) => ({
    termId: i.termId,
    state: i.state,
    exitCode: i.exitCode,
    hint: i.hint,
  }));
}

export function getTerminalSession(termId) {
  return instances.get(termId) || null;
}

/** 视图层挂载时注入清单变更通知与查找意图回调 */
export function setTerminalNotify(fn) {
  notify = fn;
}
