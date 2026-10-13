// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// LSP 轻量 client:连接生命周期 + 文档同步(Full) + monaco model 挂接。
//
// 生命周期状态机(前端镜像 serve 侧语言服务器进程态):
//   idle → connecting → initializing → ready
//        → restarting(serve 退避重启期;新进程 starting 后自动重握手+重 didOpen)
//        → failed(重启预算耗尽;等手动重启)/ unavailable(PATH 无服务器)
//        → stopped(连接关闭)
//
// 文档 URI:基于 status 端点下发的工作区根构造**真实 file:// URI**——语言服务器
// 按真实路径建 VFS 与发布诊断,serve 桥再把 file:// URI 换算回工作区相对路径
// (evoPath)供诊断单源消费。
//
// 文档同步 v1 取 Full(SyncKind=1):model 编辑去抖 300ms 后全量 didChange;
// version 每次发送时取 model.getVersionId() 实时值,过期推送由诊断桥丢弃。

import { createRpcLayer } from './protocol.js';

/** 编辑去抖窗口(ms):连续击键合并为一次 didChange */
const CHANGE_DEBOUNCE_MS = 300;

/** 生命周期状态可读名 */
export const LSP_STATES = {
  IDLE: 'idle',
  CONNECTING: 'connecting',
  INITIALIZING: 'initializing',
  READY: 'ready',
  RESTARTING: 'restarting',
  FAILED: 'failed',
  UNAVAILABLE: 'unavailable',
  STOPPED: 'stopped',
};

/** 工作区相对路径 → 真实 file:// URI(逐段编码,空格/中文等安全;盘符冒号保留) */
export function pathToFileUri(workdirBase, path) {
  const joined = `${String(workdirBase).replace(/\/+$/, '')}/${String(path).replace(/^\//, '')}`;
  return `file:///${joined
    .split('/')
    .map((seg, i) => (i === 0 && /^[A-Za-z]:$/.test(seg) ? seg : encodeURIComponent(seg)))
    .join('/')}`;
}

/**
 * 创建一个语言门的 LSP client。
 *
 * @param {object} opts
 * @param {string} opts.lang          语言门名(rust | typescript)
 * @param {string} opts.serverName    语言服务器名(诊断 owner 后缀)
 * @param {string} opts.wsUrl         完整 WS 地址(含 ?token=)
 * @param {string} opts.workdirBase   工作区根(POSIX 风格,来自 /api/lsp/status)
 * @param {Function} opts.onEvent     (event) => void:state/log 事件
 * @param {Function} opts.onDiagnostics (params) => void:publishDiagnostics 与
 *   {evoOwnerReset: path} 清理信号
 * @param {Function} opts.pathFromModel (model) => string|null:model → 工作区相对路径
 */
export function createLspClient({ lang, serverName, wsUrl, workdirBase, onEvent, onDiagnostics, pathFromModel }) {
  let ws = null;
  let rpc = null;
  let state = LSP_STATES.IDLE;
  let disposed = false;
  /** path → monaco model(本 client 跟踪的正式文档) */
  const tracked = new Map();
  /** path → [disposable](编辑/保存监听;幂等挂接) */
  const listeners = new Map();
  /** path → 去抖 timer */
  const pendingChanges = new Map();
  /** server capabilities(initialize 响应;特性层读触发字符等) */
  let serverCapabilities = null;

  function setState(next, detail = '') {
    state = next;
    onEvent({ kind: 'state', state, detail });
  }

  /** 工作区相对路径 → 真实 file:// URI */
  function docUri(path) {
    return pathToFileUri(workdirBase, path);
  }

  /** 初始化握手(新建连接与重启复用同一入口;完成后统一补 didOpen) */
  async function handshake() {
    setState(LSP_STATES.INITIALIZING, 'initialize');
    const rootUri = pathToFileUri(workdirBase, '');
    const result = await rpc.request(
      'initialize',
      {
        processId: null,
        rootUri,
        capabilities: {
          textDocument: {
            publishDiagnostics: { versionSupport: true },
            synchronization: { didSave: true },
          },
        },
      },
      60_000, // 设计裁定:initialize 60s 超时
    );
    serverCapabilities = result?.capabilities ?? {};
    rpc.notify('initialized', {});
    setState(LSP_STATES.READY, `${serverName} ready`);
    // (重)握手场景:已跟踪文档统一补 didOpen 触发服务器(重)分析
    for (const [path, model] of tracked) {
      sendDidOpen(path, model);
    }
  }

  function sendDidOpen(path, model) {
    rpc.notify('textDocument/didOpen', {
      textDocument: {
        uri: docUri(path),
        languageId: model.getLanguageId(),
        version: model.getVersionId(),
        text: model.getValue(),
      },
    });
  }

  function scheduleDidChange(path, model) {
    if (state !== LSP_STATES.READY) return; // 未就绪期内容随 didOpen 全量补发
    clearTimeout(pendingChanges.get(path));
    pendingChanges.set(
      path,
      setTimeout(() => {
        pendingChanges.delete(path);
        if (!tracked.has(path)) return; // 已关闭:丢弃过期同步
        rpc.notify('textDocument/didChange', {
          textDocument: { uri: docUri(path), version: model.getVersionId() },
          contentChanges: [{ text: model.getValue() }], // Full 同步
        });
      }, CHANGE_DEBOUNCE_MS),
    );
  }

  function sendDidClose(path) {
    rpc.notify('textDocument/didClose', {
      textDocument: { uri: docUri(path) },
    });
  }

  /** 保存完成钩子(装配层从编辑器保存流程转发;flycheck 类服务器依赖 didSave) */
  function didSave(path) {
    const model = tracked.get(path);
    if (!model || state !== LSP_STATES.READY) return;
    rpc.notify('textDocument/didSave', {
      textDocument: { uri: docUri(path) },
      text: model.getValue(),
    });
  }

  /** 幂等挂接 model 监听(首次 track 时;dispose 随 untrack) */
  function ensureListeners(path, model) {
    if (listeners.has(path)) return;
    const d1 = model.onDidChangeContent(() => scheduleDidChange(path, model));
    listeners.set(path, [d1]);
  }

  function dropListeners(path) {
    for (const d of listeners.get(path) || []) {
      try {
        d.dispose();
      } catch {
        /* 订阅清理失败不阻断 */
      }
    }
    listeners.delete(path);
    clearTimeout(pendingChanges.get(path));
    pendingChanges.delete(path);
  }

  /** WS 文本帧 → 消息层 */
  function onWsMessage(ev) {
    if (typeof ev.data === 'string') rpc.onMessage(ev.data);
  }

  function connect() {
    if (disposed || ws) return;
    setState(LSP_STATES.CONNECTING, 'ws');
    rpc = createRpcLayer((text) => {
      if (ws && ws.readyState === WebSocket.OPEN) ws.send(text);
    });
    // server→client 通知:诊断流
    rpc.onNotification('textDocument/publishDiagnostics', (params) => {
      onDiagnostics(params);
    });

    try {
      ws = new WebSocket(wsUrl);
    } catch (e) {
      setState(LSP_STATES.UNAVAILABLE, String(e?.message || e));
      return;
    }
    ws.onmessage = onWsMessage;
    ws.onopen = () => {
      handshake().catch((e) => {
        setState(LSP_STATES.FAILED, `initialize 失败:${e?.message || e}`);
      });
    };
    ws.onclose = (ev) => {
      rpc?.failAllPending('connection closed');
      ws = null;
      if (disposed) return;
      if (state === LSP_STATES.FAILED || state === LSP_STATES.UNAVAILABLE) return; // 终态不被 close 覆盖
      setState(LSP_STATES.STOPPED, `ws closed (${ev.code})`);
    };
  }

  /** serve 侧控制帧(lsp_state/lsp_log/lsp_unavailable)入口 */
  function onControlFrame(message) {
    switch (message.type) {
      case 'lsp_state':
        if (message.state === 'restarting') {
          // 清本门全部已跟踪文档诊断,新推送覆盖——防幽灵
          for (const path of tracked.keys()) {
            onDiagnostics({ evoOwnerReset: path });
          }
          setState(LSP_STATES.RESTARTING, message.detail || 'server restarting');
        } else if (message.state === 'failed') {
          setState(LSP_STATES.FAILED, message.detail || 'restart budget exhausted');
        } else if (message.state === 'starting') {
          // 新进程已起:同一 WS 上立即重握手(补 didOpen 顺带重分析)
          setState(LSP_STATES.RESTARTING, message.detail || 'server starting');
          handshake().catch((e) => setState(LSP_STATES.FAILED, `re-initialize 失败:${e?.message || e}`));
        } else if (message.state === 'stopped') {
          setState(LSP_STATES.STOPPED, message.detail || '');
        }
        break;
      case 'lsp_log':
        onEvent({ kind: 'log', level: message.level, message: message.message });
        break;
      case 'lsp_unavailable':
        setState(LSP_STATES.UNAVAILABLE, message.hint || 'language server unavailable');
        break;
      default:
        break;
    }
  }

  /** monaco model 生命周期(装配层统一接线后转发) */
  function trackModel(model) {
    const path = pathFromModel(model);
    if (!path || tracked.has(path)) return;
    tracked.set(path, model);
    ensureListeners(path, model);
    if (state === LSP_STATES.READY) sendDidOpen(path, model);
    // 未就绪:仅登记,握手完成后统一补 didOpen(handshake 内遍历 tracked)
  }

  function untrackModel(model) {
    const path = pathFromModel(model);
    if (!path || !tracked.has(path)) return;
    if (state === LSP_STATES.READY) sendDidClose(path);
    tracked.delete(path);
    dropListeners(path);
    // 诊断随关闭清理(服务器侧亦会推空,双保险)
    onDiagnostics({ evoOwnerReset: path });
  }

  // ---- 特性查询(features 层消费;position/uri 统一在此换算) ----

  function textDocumentPosition(model, position) {
    return {
      textDocument: { uri: docUri(pathFromModel(model) || '') },
      position: { line: position.lineNumber - 1, character: position.column - 1 },
    };
  }

  return {
    get lang() {
      return lang;
    },
    get serverName() {
      return serverName;
    },
    get state() {
      return state;
    },
    get capabilities() {
      return serverCapabilities;
    },
    get trackedPaths() {
      return [...tracked.keys()];
    },
    connect,
    onControlFrame,
    trackModel,
    untrackModel,
    didSave,
    textDocumentPosition,

    async completion(model, position) {
      return rpc.request('textDocument/completion', textDocumentPosition(model, position));
    },
    async hover(model, position) {
      return rpc.request('textDocument/hover', textDocumentPosition(model, position), 8_000);
    },
    async definition(model, position) {
      return rpc.request('textDocument/definition', textDocumentPosition(model, position), 8_000);
    },
    async documentSymbol(model) {
      return rpc.request('textDocument/documentSymbol', {
        textDocument: { uri: docUri(pathFromModel(model) || '') },
      });
    },

    /** 主动关闭(设置开关关/装配层销毁) */
    dispose() {
      disposed = true;
      for (const path of [...tracked.keys()]) dropListeners(path);
      tracked.clear();
      rpc?.failAllPending('client disposed');
      if (ws) {
        ws.onclose = null;
        try {
          ws.close(1000);
        } catch {
          /* 已断 */
        }
        ws = null;
      }
      setState(LSP_STATES.STOPPED, 'disposed');
    },

    /** 供测试注入:替换消息层/伪造连接/直改状态 */
    __test: {
      injectRpc(layer) {
        rpc = layer;
      },
      get rpc() {
        return rpc;
      },
      markReady() {
        state = LSP_STATES.READY;
      },
      tracked() {
        return tracked;
      },
      raw: {
        get pendingChanges() {
          return pendingChanges;
        },
      },
      docUri,
      scheduleDidChange,
      sendDidOpen,
      sendDidClose,
      handshakeHook: () => handshake,
    },
  };
}
