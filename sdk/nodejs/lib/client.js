// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

'use strict';

const http = require('http');
const https = require('https');
const { URL } = require('url');
const { EventEmitter } = require('events');
const { SSEParser } = require('./sse-parser');

// =========================================================================
// 类型定义(JSDoc)
// =========================================================================

/**
 * @typedef {Object} AgentRunRequest
 * @property {string} agent_type — agent 类型名
 * @property {string} goal — 任务描述
 * @property {number} [max_steps] — 最大步数(可选,覆盖 agent 定义)
 * @property {number} [temperature] — 温度参数(可选)
 * @property {string} [model] — 模型名(可选)
 */

/**
 * @typedef {Object} AgentRunResponse
 * @property {boolean} success — 是否成功
 * @property {string} content — 输出内容
 * @property {number} steps — 执行步数
 * @property {number} duration_ms — 耗时(毫秒)
 * @property {string|null} error — 错误信息
 */

/**
 * @typedef {Object} AgentInfo
 * @property {string} agent_type — agent 类型名
 * @property {string} version — 版本号
 * @property {string} description — 描述
 * @property {string[]} tools — 工具列表
 */

/**
 * @typedef {Object} AgentListResponse
 * @property {AgentInfo[]} agents — agent 列表
 */

/**
 * @typedef {Object} AgentDefinitionResponse
 * @property {string} agent_type
 * @property {string} version
 * @property {string} description
 * @property {string} system_prompt
 * @property {string} model
 * @property {number} temperature
 * @property {number} max_steps
 * @property {string[]} tools
 * @property {Object|null} memory_config
 */

/**
 * @typedef {Object} AgentResult
 * @property {boolean} success
 * @property {string} content
 * @property {number} steps
 * @property {number} duration_ms
 * @property {string[]} tool_calls
 * @property {string|null} error
 */

/**
 * @typedef {Object} CancelResponse
 * @property {boolean} success
 * @property {string} message
 * @property {string} session_id
 */

/**
 * @typedef {Object} AgentEvent
 * @property {string} type — 事件名(session_created/step/llm_delta/llm_done/tool_call/tool_result/done/error/info)
 * @property {*} data — 事件数据(已解析的 JSON)
 */

/**
 * @typedef {Object} EvoAgentClientOptions
 * @property {number} [timeout=30000] — 默认请求超时(毫秒),仅用于非流式请求
 * @property {number} [streamTimeout=300000] — 流式请求超时(毫秒),0 = 无超时
 */

// =========================================================================
// EvoAgentClient — HTTP 客户端主类
// =========================================================================

/**
 * evo-agent HTTP API 客户端。
 *
 * 零依赖,仅使用 Node.js 内置模块(http/https/events/url)。
 *
 * @example
 * const { EvoAgentClient } = require('evo-agent-node');
 * const client = new EvoAgentClient('http://127.0.0.1:8081');
 *
 * // 同步执行
 * const result = await client.run('general', { goal: 'hello' });
 *
 * // 流式执行
 * const stream = await client.runStream('general', { goal: 'hello' });
 * for await (const event of stream) {
 *   if (event.type === 'done') console.log(event.data);
 * }
 */
class EvoAgentClient {
  /**
   * @param {string} baseUrl — evo-agent server 地址,如 `http://127.0.0.1:8081`
   * @param {EvoAgentClientOptions} [options]
   */
  constructor(baseUrl, options = {}) {
    this.baseUrl = baseUrl.replace(/\/+$/, ''); // 去掉末尾斜杠
    this.parsedUrl = new URL(this.baseUrl);
    this.timeout = options.timeout ?? 30000;
    this.streamTimeout = options.streamTimeout ?? 300000;
  }

  /**
   * 健康检查。
   * @returns {Promise<string>} `"ok"`
   */
  async health() {
    const resp = await this._request('GET', '/health');
    return resp.body;
  }

  /**
   * 列出所有可用 agent。
   * @returns {Promise<AgentListResponse>}
   */
  async listAgents() {
    const resp = await this._request('GET', '/agents');
    return JSON.parse(resp.body);
  }

  /**
   * 查看单个 agent 的完整定义。
   * @param {string} agentType — agent 类型名
   * @returns {Promise<AgentDefinitionResponse>}
   */
  async getAgent(agentType) {
    const resp = await this._request('GET', `/agents/${encodeURIComponent(agentType)}`);
    return JSON.parse(resp.body);
  }

  /**
   * 同步执行 agent(阻塞直到完成)。
   *
   * @param {string} agentType — agent 类型名
   * @param {AgentRunRequest|string} request — 请求体,或直接传 goal 字符串(快捷方式)
   * @returns {Promise<AgentRunResponse>}
   *
   * @example
   * // 完整请求
   * const result = await client.run('general', {
   *   agent_type: 'general',
   *   goal: 'summarize the README',
   *   max_steps: 20,
   * });
   *
   * @example
   * // 快捷方式(只传 goal)
   * const result = await client.run('general', 'summarize the README');
   */
  async run(agentType, request) {
    const body = typeof request === 'string'
      ? { agent_type: agentType, goal: request }
      : request;
    const resp = await this._request(
      'POST',
      `/agents/${encodeURIComponent(agentType)}/run`,
      body,
    );
    return JSON.parse(resp.body);
  }

  /**
   * SSE 流式执行 agent。
   *
   * 返回 `AgentEventStream`,可用 `for await...of` 消费,也可用 EventEmitter 模式。
   * **两种模式互斥**,选一种即可。
   *
   * @param {string} agentType — agent 类型名
   * @param {AgentRunRequest|string} request — 请求体,或直接传 goal 字符串
   * @returns {Promise<AgentEventStream>}
   *
   * @example
   * // 方式 1: AsyncIterator(推荐)
   * const stream = await client.runStream('general', { goal: 'hello' });
   * for await (const event of stream) {
   *   if (event.type === 'llm_delta') process.stdout.write(event.data.text);
   *   if (event.type === 'done') break;
   * }
   *
   * @example
   * // 方式 2: EventEmitter
   * const stream = await client.runStream('general', { goal: 'hello' });
   * stream.on('llm_delta', (data) => process.stdout.write(data.text));
   * stream.on('done', (result) => console.log(result));
   * await stream.consume();
   */
  async runStream(agentType, request) {
    const body = typeof request === 'string'
      ? { agent_type: agentType, goal: request }
      : request;
    return new AgentEventStream(
      this.parsedUrl,
      `/agents/${encodeURIComponent(agentType)}/run/stream`,
      body,
      this.streamTimeout,
    );
  }

  /**
   * 取消正在运行的 session。
   *
   * @param {string} agentType — agent 类型名
   * @param {string} sessionId — session ID(从 SSE `session_created` 事件获取)
   * @returns {Promise<CancelResponse>}
   * @throws {Error} HTTP 404 时抛出(session 不存在或已结束)
   */
  async cancel(agentType, sessionId) {
    const resp = await this._request(
      'POST',
      `/agents/${encodeURIComponent(agentType)}/cancel?session_id=${encodeURIComponent(sessionId)}`,
    );
    if (resp.statusCode === 404) {
      throw new Error(`session not found: ${sessionId} (already finished or never existed)`);
    }
    return JSON.parse(resp.body);
  }

  // -------------------------------------------------------------------
  // 内部: 发送 HTTP 请求
  // -------------------------------------------------------------------

  /**
   * 发送 HTTP 请求(非流式)。
   * @param {string} method — HTTP 方法
   * @param {string} path — 路径(含 query string)
   * @param {Object} [body] — 请求体(POST 时自动 JSON 序列化)
   * @returns {Promise<{statusCode: number, body: string}>}
   * @private
   */
  _request(method, path, body) {
    return new Promise((resolve, reject) => {
      const isHTTPS = this.parsedUrl.protocol === 'https:';
      const transport = isHTTPS ? https : http;

      const bodyStr = body ? JSON.stringify(body) : null;
      const headers = {
        'Accept': 'application/json',
      };
      if (bodyStr) {
        headers['Content-Type'] = 'application/json';
        headers['Content-Length'] = Buffer.byteLength(bodyStr);
      }

      const req = transport.request(
        {
          hostname: this.parsedUrl.hostname,
          port: this.parsedUrl.port || (isHTTPS ? 443 : 80),
          path,
          method,
          headers,
        },
        (res) => {
          let data = '';
          res.setEncoding('utf8');
          res.on('data', (chunk) => { data += chunk; });
          res.on('end', () => {
            resolve({ statusCode: res.statusCode, body: data });
          });
        },
      );

      req.on('error', reject);

      if (this.timeout > 0) {
        req.setTimeout(this.timeout, () => {
          req.destroy(new Error(`request timeout after ${this.timeout}ms`));
        });
      }

      if (bodyStr) {
        req.write(bodyStr);
      }
      req.end();
    });
  }
}

// =========================================================================
// AgentEventStream — SSE 事件流(AsyncIterator + EventEmitter 双模式)
// =========================================================================

/**
 * agent 流式执行的事件流。
 *
 * 继承 `EventEmitter`,支持两种消费模式(**互斥,选一种**):
 *
 * **模式 1: AsyncIterator(推荐)**
 * ```js
 * for await (const event of stream) {
 *   console.log(event.type, event.data);
 *   if (event.type === 'done') break;
 * }
 * ```
 *
 * **模式 2: EventEmitter**
 * ```js
 * stream.on('llm_delta', (data) => process.stdout.write(data.text));
 * stream.on('done', (result) => console.log(result));
 * await stream.consume();
 * ```
 *
 * 事件类型:
 * - `session_created` — `{ session_id: string }`
 * - `step` — `{ step: number }`
 * - `llm_delta` — `{ text: string }`
 * - `llm_done` — `{ content: string, finish_reason: string|null }`
 * - `tool_call` — `{ name: string, args: object }`
 * - `tool_result` — `{ name: string, result: object }`
 * - `done` — `AgentResult`(终帧,收到后流自动结束)
 * - `error` — `{ error: string }`
 * - `info` — `{ message: string }`
 */
class AgentEventStream extends EventEmitter {
  /** @type {string|null} session ID(session_created 事件后填充) */
  sessionId = null;

  /** @type {boolean} 流是否已结束 */
  ended = false;

  /** @type {Error|null} 流错误 */
  streamError = null;

  /** @type {boolean} 是否已被消费(防止重复消费) */
  #consumed = false;

  /** @type {Array<{type: string, data: *}>} 缓冲的事件队列 */
  #queue = [];

  /** @type {Function|null} 等待下一个事件的 resolve */
  #pendingResolve = null;

  /** @type {Function|null} 等待下一个事件的 reject */
  #pendingReject = null;

  /** @type {SSEParser} SSE 解析器 */
  #parser = new SSEParser();

  /** @type {http.ClientRequest|null} 底层 HTTP 请求 */
  #req = null;

  /** @type {number} 流超时(毫秒) */
  #timeout;

  /** @type {Object} server 连接信息 */
  #serverInfo;

  /** @type {string} 请求路径 */
  #path;

  /** @type {Object} 请求体 */
  #requestBody;

  /**
   * @param {URL} parsedUrl — server URL
   * @param {string} path — 请求路径
   * @param {Object} requestBody — 请求体
   * @param {number} timeout — 流超时(毫秒,0 = 无超时)
   */
  constructor(parsedUrl, path, requestBody, timeout) {
    super();
    this.#serverInfo = parsedUrl;
    this.#path = path;
    this.#requestBody = requestBody;
    this.#timeout = timeout;
  }

  /**
   * 启动 HTTP 连接(惰性启动:第一次迭代或 consume() 时触发)。
   * @private
   */
  #start() {
    if (this.#req) return; // 已启动

    const isHTTPS = this.#serverInfo.protocol === 'https:';
    const transport = isHTTPS ? https : http;
    const bodyStr = JSON.stringify(this.#requestBody);

    const req = transport.request(
      {
        hostname: this.#serverInfo.hostname,
        port: this.#serverInfo.port || (isHTTPS ? 443 : 80),
        path: this.#path,
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'Accept': 'text/event-stream',
          'Content-Length': Buffer.byteLength(bodyStr),
        },
      },
      (res) => {
        // 非 200 → 读取错误体,reject
        if (res.statusCode !== 200) {
          let errBody = '';
          res.setEncoding('utf8');
          res.on('data', (chunk) => { errBody += chunk; });
          res.on('end', () => {
            const err = new Error(
              `HTTP ${res.statusCode}: ${errBody || 'stream request failed'}`,
            );
            err.statusCode = res.statusCode;
            this.#fail(err);
          });
          return;
        }

        // 200 → 用 TextDecoder 安全解码 UTF-8(处理多字节字符)
        const decoder = new TextDecoder('utf-8');
        res.setEncoding('utf8');

        res.on('data', (chunk) => {
          const events = this.#parser.feed(chunk);
          for (const event of events) {
            this.#pushEvent(event);
          }
        });

        res.on('end', () => {
          // 流结束,flush 解析器中剩余的事件
          const remaining = this.#parser.flush();
          for (const event of remaining) {
            this.#pushEvent(event);
          }
          this.#finish();
        });

        res.on('error', (err) => {
          this.#fail(err);
        });
      },
    );

    req.on('error', (err) => {
      this.#fail(err);
    });

    if (this.#timeout > 0) {
      req.setTimeout(this.#timeout, () => {
        req.destroy(new Error(`stream timeout after ${this.#timeout}ms`));
      });
    }

    req.write(bodyStr);
    req.end();
    this.#req = req;
  }

  /**
   * 将一个事件推入流(EventEmitter emit + 队列/迭代器)。
   * @param {{type: string, data: *}} event
   * @private
   */
  #pushEvent(event) {
    // 捕获 session_id
    if (event.type === 'session_created' && event.data?.session_id) {
      this.sessionId = event.data.session_id;
    }

    // EventEmitter 模式: 发射事件
    this.emit(event.type, event.data);
    this.emit('event', event);

    // AsyncIterator 模式: 推入队列或直接 resolve
    if (this.#pendingResolve) {
      const resolve = this.#pendingResolve;
      this.#pendingResolve = null;
      this.#pendingReject = null;
      resolve({ value: event, done: false });
    } else {
      this.#queue.push(event);
    }

    // done 事件 = 终帧
    if (event.type === 'done') {
      this.#finish();
    }
  }

  /**
   * 正常结束流。
   * @private
   */
  #finish() {
    if (this.ended) return;
    this.ended = true;
    this.emit('end');

    // 如果有等待的迭代器,resolve 为 done
    if (this.#pendingResolve) {
      const resolve = this.#pendingResolve;
      this.#pendingResolve = null;
      this.#pendingReject = null;
      resolve({ value: undefined, done: true });
    }
  }

  /**
   * 异常结束流。
   * @param {Error} err
   * @private
   */
  #fail(err) {
    if (this.ended) return;
    this.streamError = err;
    this.ended = true;
    this.emit('error', err);
    this.emit('end');

    if (this.#pendingReject) {
      const reject = this.#pendingReject;
      this.#pendingResolve = null;
      this.#pendingReject = null;
      reject(err);
    } else if (!this.#consumed) {
      // 还没人消费过,把错误放进队列(下次 next() 会拿到)
      this.#queue.push({ type: '__error__', data: { error: err.message }, __error: err });
    }
  }

  // -------------------------------------------------------------------
  // 公开 API
  // -------------------------------------------------------------------

  /**
   * AsyncIterator 实现 — 支持 `for await...of`。
   *
   * @returns {AsyncIterator<AgentEvent>}
   */
  [Symbol.asyncIterator]() {
    if (this.#consumed) {
      throw new Error('AgentEventStream already consumed (use one mode only: for-await or consume())');
    }
    this.#consumed = true;
    this.#start();

    const self = this;
    return {
      next() {
        // 队列中有事件 → 直接返回
        if (self.#queue.length > 0) {
          const event = self.#queue.shift();
          if (event.__error) {
            return Promise.reject(event.__error);
          }
          return Promise.resolve({ value: event, done: false });
        }
        // 流已结束
        if (self.ended) {
          return Promise.resolve({ value: undefined, done: true });
        }
        // 等待下一个事件
        return new Promise((resolve, reject) => {
          self.#pendingResolve = resolve;
          self.#pendingReject = reject;
        });
      },

      return() {
        // 提前退出(for-await break / return)→ 销毁底层连接
        if (self.#req) {
          self.#req.destroy();
        }
        return Promise.resolve({ value: undefined, done: true });
      },
    };
  }

  /**
   * EventEmitter 模式: 开始消费流,通过 `emit` 分发事件。
   *
   * 与 `for await...of` **互斥** — 调用此方法后不能再迭代。
   *
   * @returns {Promise<void>} 流结束后 resolve(正常或异常)
   */
  consume() {
    if (this.#consumed) {
      throw new Error('AgentEventStream already consumed (use one mode only: for-await or consume())');
    }
    this.#consumed = true;
    this.#start();

    return new Promise((resolve, reject) => {
      this.on('end', resolve);
      this.on('error', reject);
    });
  }

  /**
   * 取消正在运行的 session(调用 /cancel 端点)。
   *
   * 需要在 `session_created` 事件后调用(此时 `this.sessionId` 已填充)。
   * 取消后,server 会发出 `error` + `done` 事件,流自动结束。
   *
   * @param {EvoAgentClient} [client] — 客户端实例(如果不传,用内部 HTTP 请求)
   * @returns {Promise<void>}
   */
  async cancel(client) {
    if (!this.sessionId) {
      throw new Error('sessionId not available yet (wait for session_created event)');
    }

    const agentType = this.#requestBody.agent_type;
    if (client) {
      await client.cancel(agentType, this.sessionId);
    } else {
      // 用内部 HTTP 请求直接调 /cancel
      await this.#cancelDirect(agentType, this.sessionId);
    }
  }

  /**
   * 内部直接调 /cancel(不依赖 EvoAgentClient 实例)。
   * @private
   */
  #cancelDirect(agentType, sessionId) {
    return new Promise((resolve, reject) => {
      const isHTTPS = this.#serverInfo.protocol === 'https:';
      const transport = isHTTPS ? https : http;
      const path = `/agents/${encodeURIComponent(agentType)}/cancel?session_id=${encodeURIComponent(sessionId)}`;

      const req = transport.request(
        {
          hostname: this.#serverInfo.hostname,
          port: this.#serverInfo.port || (isHTTPS ? 443 : 80),
          path,
          method: 'POST',
        },
        (res) => {
          let data = '';
          res.setEncoding('utf8');
          res.on('data', (chunk) => { data += chunk; });
          res.on('end', () => {
            if (res.statusCode === 200) {
              resolve();
            } else {
              reject(new Error(`cancel failed: HTTP ${res.statusCode}`));
            }
          });
        },
      );
      req.on('error', reject);
      req.end();
    });
  }

  /**
   * 便捷方法: 收集所有事件直到 `done`,返回最终结果。
   *
   * 内部使用 AsyncIterator 消费,与 `for await...of` / `consume()` **互斥**。
   *
   * @returns {Promise<AgentResult>} 最终的 AgentResult
   */
  async collect() {
    let result = null;
    for await (const event of this) {
      if (event.type === 'done') {
        result = event.data;
        break;
      }
      if (event.type === 'error' && !this.sessionId) {
        // 流级错误(没拿到 session_id 就挂了)
        throw new Error(event.data?.error || 'stream error');
      }
    }
    if (!result) {
      throw new Error('stream ended without done event');
    }
    return result;
  }
}

module.exports = { EvoAgentClient, AgentEventStream };
