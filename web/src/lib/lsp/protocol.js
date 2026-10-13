// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// LSP JSON-RPC 2.0 消息层(over WebSocket 文本帧)。
//
// 职责:id 自增的请求-响应路由 / 通知分发 / server→client 请求的最小应答集
// (轻量 client 哲学:能力协商静态完成,动态面按规范给最小合法应答)。
// 传输层只认字符串进出,连接生命周期由上层 client 管理。

const RPC_ERRORS = {
  PARSE_ERROR: -32700,
  INVALID_REQUEST: -32600,
  METHOD_NOT_FOUND: -32601,
  INVALID_PARAMS: -32602,
  INTERNAL_ERROR: -32603,
};

/** 请求默认超时(ms);补全/符号等慢查询由调用方按需覆盖 */
export const DEFAULT_REQUEST_TIMEOUT = 15_000;

/**
 * 最小应答集:server→client 请求的处理表(方法 → 应答 result 或拒收)。
 * - workspace/configuration:无配置面 → -32601(METHOD_NOT_FOUND),server 按规范回落默认值;
 * - client/registerCapability:静态注册 → 空 OK(注册内容忽略);
 * - window/workDoneProgress/create:进度 UI v1 不做 → 空 OK(后续 progress 通知被忽略);
 * - 其余未列方法 → -32601。
 */
export const SERVER_REQUEST_ANSWERS = {
  'client/registerCapability': () => ({}),
  'window/workDoneProgress/create': () => null,
};

/** 创建一个消息层实例。`send(text)` 由上层注入(WebSocket 发送)。 */
export function createRpcLayer(send) {
  let nextId = 1;
  /** id → { resolve, reject, timer } */
  const pending = new Map();
  /** method → Set<handler>(server→client 通知) */
  const notificationHandlers = new Map();
  /** server→client 请求额外应答器(测试注入用) */
  const extraRequestHandlers = new Map();

  function handleReply(message) {
    const entry = pending.get(message.id);
    if (!entry) return; // 迟到/未知 id:丢弃
    clearTimeout(entry.timer);
    pending.delete(message.id);
    if ('error' in message && message.error != null) {
      entry.reject(new Error(`${message.error.message || 'rpc error'} (code ${message.error.code})`));
    } else {
      entry.resolve(message.result);
    }
  }

  async function handleServerRequest(message) {
    const method = message.method;
    let result;
    if (Object.prototype.hasOwnProperty.call(SERVER_REQUEST_ANSWERS, method)) {
      result = SERVER_REQUEST_ANSWERS[method](message.params);
    } else if (extraRequestHandlers.has(method)) {
      result = extraRequestHandlers.get(method)(message.params);
    } else {
      send(JSON.stringify({ jsonrpc: '2.0', id: message.id, error: { code: RPC_ERRORS.METHOD_NOT_FOUND, message: `method not handled by client: ${method}` } }));
      return;
    }
    send(JSON.stringify({ jsonrpc: '2.0', id: message.id, result: result ?? null }));
  }

  function dispatchNotification(message) {
    const handlers = notificationHandlers.get(message.method);
    if (handlers) for (const h of handlers) h(message.params);
  }

  return {
    /** 收到一条 WS 文本帧(JSON-RPC);解析失败静默丢弃(防注水) */
    onMessage(text) {
      let message;
      try {
        message = JSON.parse(text);
      } catch {
        return;
      }
      if (!message || typeof message !== 'object') return;
      if (message.id != null && !message.method) handleReply(message);
      else if (message.id != null && message.method) handleServerRequest(message);
      else if (message.method) dispatchNotification(message);
    },

    /** 发起请求,返回 Promise;超时按 DEFAULT_REQUEST_TIMEOUT(可覆写) */
    request(method, params, timeoutMs = DEFAULT_REQUEST_TIMEOUT) {
      const id = nextId++;
      const message = { jsonrpc: '2.0', id, method };
      if (params !== undefined) message.params = params;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(new Error(`LSP request timeout: ${method}`));
        }, timeoutMs);
        pending.set(id, { resolve, reject, timer });
        send(JSON.stringify(message));
      });
    },

    /** 发送通知(无 id,无应答) */
    notify(method, params) {
      const message = { jsonrpc: '2.0', method };
      if (params !== undefined) message.params = params;
      send(JSON.stringify(message));
    },

    /** 订阅 server→client 通知;返回解绑函数 */
    onNotification(method, handler) {
      if (!notificationHandlers.has(method)) notificationHandlers.set(method, new Set());
      notificationHandlers.get(method).add(handler);
      return () => notificationHandlers.get(method)?.delete(handler);
    },

    /** 注册额外的 server→client 请求应答器(扩展面;v1 最小应答集之外) */
    onRequest(method, handler) {
      extraRequestHandlers.set(method, handler);
      return () => extraRequestHandlers.delete(method);
    },

    /** 连接断开:全部在途请求按失败收场(上层重连后重发) */
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

export { RPC_ERRORS };
