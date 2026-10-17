// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// DAP 1.x 消息层(over WebSocket 文本帧)。
//
// serve 端 dap 桥已做 Content-Length 帧 ↔ WS 文本帧的双向翻译,本层只见裸
// JSON 消息:seq 自增的请求-响应配对 / 事件分发 / 超时。与 LSP JSON-RPC 层的
// 形状差异:DAP 响应用 request_seq 回指(而非同 id),事件无 method 只有 event。

/** 请求默认超时(ms);initialize/launch 等慢请求由调用方按需覆盖 */
export const DEFAULT_REQUEST_TIMEOUT = 15_000;

/** 创建一个 DAP 消息层实例。`send(text)` 由上层注入(WebSocket 发送)。 */
export function createDapLayer(send) {
  let nextSeq = 1;
  /** request_seq → { resolve, reject, timer, command } */
  const pending = new Map();
  /** event 名 → Set<handler> */
  const eventHandlers = new Map();

  function handleReply(message) {
    const entry = pending.get(message.request_seq);
    if (!entry) return; // 迟到/未知 request_seq:丢弃
    clearTimeout(entry.timer);
    pending.delete(message.request_seq);
    if (message.success === false) {
      const err = new Error(message.message || `DAP request failed: ${entry.command}`);
      err.body = message.body;
      entry.reject(err);
    } else {
      entry.resolve(message.body);
    }
  }

  function dispatchEvent(message) {
    const handlers = eventHandlers.get(message.event);
    if (handlers) for (const h of handlers) h(message.body ?? {});
    // 通配订阅(透传面:session 层统一裁决时用)
    const wildcards = eventHandlers.get('*');
    if (wildcards) for (const h of wildcards) h(message.body ?? {}, message.event);
  }

  return {
    /** 收到一条 WS 文本帧(DAP JSON);解析失败静默丢弃(防注水) */
    onMessage(text) {
      let message;
      try {
        message = JSON.parse(text);
      } catch {
        return;
      }
      if (!message || typeof message !== 'object' || typeof message.type !== 'string') return;
      if (message.type === 'response') handleReply(message);
      else if (message.type === 'event') dispatchEvent(message);
      // adapter→client 的反向请求(reverse request,v1 无最小应答集)静默忽略
    },

    /** 发起 DAP 请求,返回 Promise<body>;超时按 DEFAULT_REQUEST_TIMEOUT(可覆写) */
    request(command, args, timeoutMs = DEFAULT_REQUEST_TIMEOUT) {
      const seq = nextSeq++;
      const message = { seq, type: 'request', command };
      if (args !== undefined) message.arguments = args;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(seq);
          reject(new Error(`DAP request timeout: ${command}`));
        }, timeoutMs);
        pending.set(seq, { resolve, reject, timer, command });
        send(JSON.stringify(message));
      });
    },

    /** 订阅 adapter→client 事件;返回解绑函数 */
    onEvent(event, handler) {
      if (!eventHandlers.has(event)) eventHandlers.set(event, new Set());
      eventHandlers.get(event).add(handler);
      return () => eventHandlers.get(event)?.delete(handler);
    },

    /** 连接断开:全部在途请求按失败收场 */
    failAllPending(reason) {
      for (const [, entry] of pending) {
        clearTimeout(entry.timer);
        entry.reject(new Error(reason));
      }
      pending.clear();
    },

    /** 测试观察:在途请求数 */
    pendingCount() {
      return pending.size;
    },
  };
}
