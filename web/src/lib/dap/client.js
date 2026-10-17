// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// DAP 调试客户端:WS 连接生命周期 + DAP 1.x 请求/事件 API。
//
// 与 LSP client 的同构与差异(34 号 §三):
// - 同构:WS 文本帧进出;serve 桥已做 Content-Length 帧翻译,本层只见 JSON;
// - 差异:DAP initialize 后无 didOpen 文档同步——断点由本层按文件全量同步
//   (setBreakpoints),代码状态由适配器自行读取 debuggee 文件。
//
// 状态推进不在此层裁决(session 层职责);本层只提供连接与请求原语 +
// 事件透传(单一 onMessage 入口,上层注册全部事件处理)。

import { createDapLayer } from './protocol.js';

/** 生命周期状态可读名 */
export const DAP_STATES = {
  IDLE: 'idle',
  CONNECTING: 'connecting',
  INITIALIZING: 'initializing',
  LAUNCHING: 'launching', // launch/configurationDone 已发,等 stopped/terminated
  READY: 'ready', // 会话活动(运行中或停住)
  STOPPED: 'stopped',
  TERMINATED: 'terminated',
  FAILED: 'failed',
  ENDED: 'ended', // WS 关闭收尾
};

/**
 * 创建一个 DAP 调试客户端(单调试会话;适配器生命周期=WS 连接生命周期,
 * serve 桥保证断连即 kill,前端无需孤儿进程兜底)。
 *
 * @param {object} opts
 * @param {string} opts.wsUrl     完整 WS 地址(/api/dap/ws?adapter=..&token=..)
 * @param {Function} opts.onEvent (event) => void:state/log 事件(state 与 LSP 同款)
 * @param {Function} opts.onDapEvent (event, body) => void:adapter→client 事件透传
 */
export function createDapClient({ wsUrl, onEvent, onDapEvent }) {
  let ws = null;
  let dap = null;
  let state = DAP_STATES.IDLE;
  let disposed = false;

  function setState(next, detail = '') {
    state = next;
    onEvent({ kind: 'state', state, detail });
  }

  function onWsMessage(ev) {
    if (typeof ev.data === 'string') dap.onMessage(ev.data);
  }

  function connect() {
    if (disposed || ws) return;
    setState(DAP_STATES.CONNECTING, 'ws');
    dap = createDapLayer((text) => {
      if (ws && ws.readyState === WebSocket.OPEN) ws.send(text);
    });
    // 全事件透传(session 层裁决状态推进;stopped/output/terminated/breakpoint 等)
    dap.onEvent('*', (body, event) => onDapEvent(event, body));
    try {
      ws = new WebSocket(wsUrl);
    } catch (e) {
      setState(DAP_STATES.FAILED, String(e?.message || e));
      return;
    }
    ws.onmessage = onWsMessage;
    ws.onopen = () => onEvent({ kind: 'ws-open' });
    ws.onclose = (ev) => {
      dap?.failAllPending('connection closed');
      ws = null;
      if (disposed) return;
      // 终态(terminated/failed/ended)不被 close 覆盖;运行中断连=会话异常终止
      if ([DAP_STATES.TERMINATED, DAP_STATES.FAILED, DAP_STATES.ENDED].includes(state)) return;
      setState(DAP_STATES.ENDED, `ws closed (${ev.code})`);
    };
    ws.onerror = () => {
      if (state === DAP_STATES.CONNECTING) setState(DAP_STATES.FAILED, 'ws error');
    };
  }

  return {
    get state() {
      return state;
    },
    connect,
    setState,

    async request(command, args, timeoutMs) {
      return dap.request(command, args, timeoutMs);
    },

    /** 主动断开(终止调试会话;serve 桥随之 kill 适配器) */
    disconnect() {
      disposed = true;
      dap?.failAllPending('client disposed');
      if (ws) {
        ws.onclose = null;
        try {
          ws.close(1000);
        } catch {
          /* 已断 */
        }
        ws = null;
      }
      setState(DAP_STATES.ENDED, 'disposed');
    },

    /** 测试注入 */
    __test: {
      injectDap(layer) {
        dap = layer;
      },
      get dap() {
        return dap;
      },
      get ws() {
        return ws;
      },
      emitWsOpen() {
        onEvent({ kind: 'ws-open' });
      },
    },
  };
}
