// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// DAP 调试会话层:状态机 + launch 时序 + 断点全量同步语义(设计档 §三.3)。
//
// 核心时序(launch):
//   connect → initialize → (response) → launch(配置参数) →
//   [initialized 事件] → setBreakpoints(逐文件全量) → configurationDone → 运行
//
// 状态机: idle→connecting→initializing→launching→running ⇄ stopped→terminated
//   任何态的请求失败 → failed;WS 断连 → ended(适配器由 serve 桥 kill)。
//
// 断点语义:本层是断点唯一真相源(内存 Map);setBreakpoints 按文件**全量**发送
//   (DAP 语义:每次调用替换该文件全部断点),verified 行号写回(适配器可能
//   移动/合并断点);会话建立时全量重放(适配器进程是无状态的新进程)。

import { pathToFileUri } from '../lsp/client.js';

/** 会话状态(镜像设计档 §三.2;与 client 层状态解耦——client 只管连接) */
export const DBG_STATES = {
  IDLE: 'idle',
  CONNECTING: 'connecting',
  INITIALIZING: 'initializing',
  LAUNCHING: 'launching',
  RUNNING: 'running',
  STOPPED: 'stopped',
  TERMINATED: 'terminated',
  FAILED: 'failed',
  ENDED: 'ended',
};

/** 断点单文件行数上限(防误点导致的异常量;UI 同步约束) */
const MAX_BREAKPOINTS_PER_FILE = 200;

/**
 * launch 配置解析:.evo/debug.json 自有形态(数组或 {configurations:[]}),
 * 兼容读取 .vscode/launch.json 同名字段(type/request/name/program/args/cwd/env)。
 * 返回规范形态数组;request 非 launch 的配置过滤掉(v1 仅 launch,设计档 §一)。
 */
export function normalizeLaunchConfigs(raw) {
  if (!raw) return [];
  let list = raw;
  if (!Array.isArray(raw) && Array.isArray(raw.configurations)) list = raw.configurations;
  if (!Array.isArray(list)) return [];
  return list.filter(
    (c) =>
      c &&
      typeof c === 'object' &&
      typeof c.type === 'string' &&
      c.request !== 'attach' && // v1 仅 launch;attach 登记 v1.x
      typeof c.name === 'string',
  );
}

/**
 * 创建调试会话(单会话;新会话=新实例)。
 *
 * @param {object} client createDapClient 实例(连接与请求原语)
 * @param {object} opts
 * @param {string} opts.workdirBase   工作区根(POSIX 风格;file URI 构造用)
 * @param {Function} opts.onState     (state, detail) => void
 * @param {Function} opts.onOutput    (body) => void:output 事件(console 面)
 * @param {Function} opts.onStopped   (body) => void:stopped 事件(视图拉栈/变量)
 * @param {Function} opts.onContinued (body) => void
 * @param {Function} opts.onBreakpoints (path, verified[]) => void:verified 行写回
 */
export function createDebugSession(client, { workdirBase, onState, onOutput, onStopped, onContinued, onBreakpoints }) {
  let state = DBG_STATES.IDLE;
  let adapterName = null;
  let launchArgs = null;
  /** path(工作区相对) → [{line,verified,id}] — 断点唯一真相源 */
  const breakpoints = new Map();
  /** start() 期间等待 ws-open 的回调(装配层经 handleClientEvent 转发) */
  let wsOpenWaiter = null;
  /** start() 期间等待 initialized 事件的回调(DAP 时序:launch 后配断点) */
  let initializedWaiter = null;
  /** 最近停住线程(执行控制 next/continue/stepIn/stepOut 的必填 threadId) */
  let lastThreadId = null;

  function setState(next, detail = '') {
    state = next;
    onState?.(state, detail);
  }

  /** adapter→client 事件裁决 */
    function handleDapEvent(event, body) {
      switch (event) {
        case 'initialized':
          if (initializedWaiter) {
            const w = initializedWaiter;
            initializedWaiter = null;
            w();
          }
          break;
        case 'output':
        onOutput?.(body || {});
        break;
      case 'stopped':
        if (state === DBG_STATES.RUNNING || state === DBG_STATES.LAUNCHING) setState(DBG_STATES.STOPPED, body?.reason || 'stopped');
        if (body?.threadId != null) lastThreadId = body.threadId; // 执行控制(next/continue)必填
        onStopped?.(body || {});
        break;
      case 'continued':
        if (state === DBG_STATES.STOPPED) setState(DBG_STATES.RUNNING, 'continued');
        onContinued?.(body || {});
        break;
      case 'terminated':
        setState(DBG_STATES.TERMINATED, 'terminated');
        break;
      case 'breakpoint':
        // 适配器侧断点变更(verified 行变化)——写回真相源
        if (body?.breakpoint && body.breakpoint.source?.path != null && onBreakpoints) {
          onBreakpoints(body.breakpoint.source.path, body.breakpoint);
        }
        break;
      default:
        break;
    }
  }

  /** 某文件 → DAP source 形态。initialize 声明 pathFormat='path'——source.path
   * 须为**文件系统路径**（非 file:// URI；URI 形态 debugpy 解析失败断点不绑定,
   * 2026-10-17 真机实测教训）。 */
  function toSource(path) {
    const base = String(workdirBase || '').replace(/\/+$/, '');
    const abs = base ? `${base}/${String(path).replace(/^\//, '')}` : String(path);
    return { name: String(path).split('/').pop(), path: abs };
  }

  /** 单文件断点全量同步(verified 行经 onBreakpoints 写回真相源) */
  async function syncFileBreakpoints(path) {
    const lines = (breakpoints.get(path) || []).map((b) => b.line);
    const body = await client.request('setBreakpoints', {
      source: toSource(path),
      breakpoints: lines.map((line) => ({ line })),
    }, 20_000);
    const verified = (body?.breakpoints || []).filter((b) => b.verified !== false && Number.isFinite(b.line));
    breakpoints.set(path, verified.map((b) => ({ line: b.line, verified: true, id: b.id })));
    onBreakpoints?.(path, verified.map((b) => b.line));
    return verified;
  }

  /** 会话建立后全量重放(新适配器进程无状态) */
  async function syncAllBreakpoints() {
    for (const path of [...breakpoints.keys()]) {
      try {
        await syncFileBreakpoints(path);
      } catch {
        /* 单文件失败不阻断其余文件与 configurationDone */
      }
    }
  }

  return {
    get state() {
      return state;
    },
    get adapter() {
      return adapterName;
    },
    /** path → 行号数组(真相源快照;UI 渲染用) */
    breakpointLines() {
      const out = {};
      for (const [path, bps] of breakpoints) out[path] = bps.map((b) => b.line).sort((a, b) => a - b);
      return out;
    },

    /**
     * UI 层断点变更入口(全量替换某文件断点;store 为单一入口,本方法同步
     * 真相源并在会话活动时向适配器全量同步该文件)。
     */
    async applyBreakpoints(path, lines) {
      if (path.startsWith('evo://')) return; // 虚拟 URI 不可断点
      breakpoints.set(path, (lines || []).map((line) => ({ line, verified: false })));
      if (state === DBG_STATES.RUNNING || state === DBG_STATES.STOPPED || state === DBG_STATES.LAUNCHING) {
        try {
          await syncFileBreakpoints(path);
        } catch (e) {
          onState?.(state, `setBreakpoints 失败:${e?.message || e}`);
        }
      }
    },

    /** 会话建立前灌入存档断点(真相源初始化;UI 键=工作区相对路径) */
    primeBreakpoints(map) {
      breakpoints.clear();
      for (const [path, lines] of Object.entries(map || {})) {
        if (Array.isArray(lines) && lines.length) {
          breakpoints.set(path, lines.map((line) => ({ line, verified: false })));
        }
      }
    },

    /** 发起调试会话:connect → initialize → launch → [initialized] → 断点重放 → configurationDone */
    async start(adapter, config) {
      if (state !== DBG_STATES.IDLE && state !== DBG_STATES.ENDED && state !== DBG_STATES.TERMINATED && state !== DBG_STATES.FAILED) {
        throw new Error('调试会话已在进行中');
      }
      adapterName = adapter;
      launchArgs = config;
      setState(DBG_STATES.CONNECTING, `${adapter}`);
      client.connect();

      // 等 ws open(装配层把 client onEvent 转发进 handleClientEvent)
      await new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          wsOpenWaiter = null;
          reject(new Error('ws 连接超时'));
        }, 15_000);
        wsOpenWaiter = (ev) => {
          if (ev.kind === 'ws-open') {
            clearTimeout(timer);
            wsOpenWaiter = null;
            resolve();
          } else if (ev.kind === 'state' && ev.state === DBG_STATES.FAILED) {
            clearTimeout(timer);
            wsOpenWaiter = null;
            reject(new Error(ev.detail || '连接失败'));
          }
        };
      });

      setState(DBG_STATES.INITIALIZING, 'initialize');
      try {
        await client.request('initialize', {
          clientID: 'evo-agent-workbench',
          clientName: 'evo-agent IDE',
          adapterID: adapter,
          locale: 'zh-cn',
          linesStartAt1: true,
          columnsStartAt1: true,
          pathFormat: 'path',
        }, 30_000);

        setState(DBG_STATES.LAUNCHING, config.name || 'launch');
        // launch 不阻塞等待——debugpy 等适配器普遍在 configurationDone 之后才回
        // launch 响应（DAP 时序惯例：initialize → launch 请求 → initialized 事件 →
        // setBreakpoints* → configurationDone → launch 响应）。先发请求,等
        // initialized 事件(超时 10s 兜底:个别适配器不吐 initialized)再配断点。
        const launchP = client.request('launch', launchArgs, 60_000);
        await new Promise((resolve) => {
          initializedWaiter = resolve;
          setTimeout(() => {
            if (initializedWaiter === resolve) initializedWaiter = null;
            resolve();
          }, 10_000);
        });
        await syncAllBreakpoints();
        await client.request('configurationDone', undefined, 15_000);
        await launchP;
      } catch (e) {
        // 启动链任一环失败:落 FAILED 终态(会话可重开),异常上抛给装配层呈现
        client.disconnect();
        setState(DBG_STATES.FAILED, e?.message || String(e));
        throw e;
      }
      // 仅从 LAUNCHING 推进;断点命中(stopped)可能先于 launch 响应到达,勿覆写
      if (state === DBG_STATES.LAUNCHING) setState(DBG_STATES.RUNNING, 'configurationDone');
    },

    /** 装配层转发 client onEvent 到此(ws-open 等待 + 失败短路) */
    handleClientEvent(ev) {
      if (wsOpenWaiter) wsOpenWaiter(ev);
      if (ev.kind === 'state' && ev.state === DBG_STATES.ENDED && state !== DBG_STATES.TERMINATED && state !== DBG_STATES.FAILED) {
        setState(DBG_STATES.ENDED, ev.detail || 'connection ended');
      }
    },

    /** 装配层转发 client onDapEvent 到此 */
    handleDapEvent(event, body) {
      handleDapEvent(event, body);
    },

    /** 断点全量同步语义自检(测试钩子) */
    __testSyncAll: syncAllBreakpoints,
    __testSyncFile: syncFileBreakpoints,

    /** 断点真相源直改(测试用;生产经 toggleBreakpoint) */
    __testSetBreakpoints(map) {
      breakpoints.clear();
      for (const [k, v] of Object.entries(map)) breakpoints.set(k, v.map((line) => ({ line, verified: false })));
    },
    __testSetState(s) {
      state = s;
    },

    // ---- 执行控制(DAP 直通;会话未活动时静默拒绝) ----
    async _ctl(command) {
      if (state !== DBG_STATES.RUNNING && state !== DBG_STATES.STOPPED) return;
      // DAP next/stepIn/stepOut/continue 均必填 threadId(缺省=适配器拒绝,静默无效)
      const args = {};
      if (lastThreadId != null) args.threadId = lastThreadId;
      return client.request(command, args, 30_000);
    },
    continue_() {
      return this._ctl('continue');
    },
    next() {
      return this._ctl('next');
    },
    stepIn() {
      return this._ctl('stepIn');
    },
    stepOut() {
      return this._ctl('stepOut');
    },

    async evaluate(expression, frameId) {
      if (state !== DBG_STATES.STOPPED && state !== DBG_STATES.RUNNING) throw new Error('会话未活动');
      return client.request('evaluate', { expression, frameId, context: 'repl' }, 15_000);
    },
    async threads() {
      return client.request('threads', undefined, 15_000);
    },
    async stackTrace(threadId) {
      return client.request('stackTrace', { threadId, startFrame: 0, levels: 20 }, 15_000);
    },
    async scopes(frameId) {
      return client.request('scopes', { frameId }, 15_000);
    },
    async variables(variablesReference) {
      return client.request('variables', { variablesReference }, 15_000);
    },

    /** 停止:terminate 请求(适配器不支持则直接断 WS——桥 kill 适配器兜底) */
    async stop() {
      try {
        if (state !== DBG_STATES.ENDED && state !== DBG_STATES.TERMINATED) {
          await client.request('terminate', undefined, 5_000);
        }
      } catch {
        /* 适配器不支持 terminate/已退出:直接断 */
      }
      client.disconnect();
      setState(DBG_STATES.ENDED, 'stopped by user');
    },

    dispose() {
      client.disconnect();
      setState(DBG_STATES.ENDED, 'disposed');
    },
  };
}
