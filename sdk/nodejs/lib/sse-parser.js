// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

'use strict';

/**
 * SSE 帧解析器 — 从字节流解析 Server-Sent Events 帧。
 *
 * SSE 协议(https://html.spec.whatwg.org/multipage/server-sent-events.html):
 *   event: <name>      事件名(可选,默认 "message")
 *   data: <text>       数据行(可多行,用 \n 拼接)
 *   id: <id>           事件 ID(本实现忽略)
 *   retry: <ms>        重连间隔(本实现忽略)
 *   : <comment>        注释(忽略)
 *   <空行>             事件分隔 — 派发当前缓冲的事件
 *
 * 用法:
 *   const parser = new SSEParser();
 *   parser.feed(chunk);  // 喂入字符串,返回事件数组 [{ type, data }]
 *   parser.flush();       // 流结束时调用,返回剩余缓冲(如果有)
 */
class SSEParser {
  constructor() {
    /** @type {string} 未处理的缓冲区(可能是不完整的行) */
    this.buffer = '';
    /** @type {string|null} 当前事件的 event 名 */
    this.eventName = null;
    /** @type {string[]} 当前事件的 data 行 */
    this.dataLines = [];
  }

  /**
   * 喂入一段文本,返回完整的事件数组。
   *
   * @param {string} chunk — 文本片段(通常由 TextDecoder 产出)
   * @returns {Array<{type: string, data: *}>} 完整的事件列表
   */
  feed(chunk) {
    this.buffer += chunk;
    const events = [];

    // 按行分割(SSE 规范: \r\n / \n / \r 都算行结束)
    const lines = this.buffer.split(/\r\n|\n|\r/);
    // 最后一段可能不完整,留到下次
    this.buffer = lines.pop();

    for (const line of lines) {
      if (line === '') {
        // 空行 = 事件分隔,派发当前事件
        if (this.dataLines.length > 0) {
          const dataStr = this.dataLines.join('\n');
          events.push({
            type: this.eventName || 'message',
            data: this._parseData(dataStr),
          });
        }
        this.eventName = null;
        this.dataLines = [];
      } else if (line[0] === ':') {
        // 注释行,忽略(SSE 心跳也用这个)
      } else if (line.startsWith('event:')) {
        this.eventName = line.slice(6).trim();
      } else if (line.startsWith('data:')) {
        // SSE 规范: "data:" 后如果紧跟空格,去掉一个空格
        const value = line.slice(5);
        this.dataLines.push(value.startsWith(' ') ? value.slice(1) : value);
      } else if (line.startsWith('id:')) {
        // 事件 ID,本实现忽略
      } else if (line.startsWith('retry:')) {
        // 重连间隔,本实现忽略
      }
      // 其他未知字段忽略
    }

    return events;
  }

  /**
   * 流结束时调用,返回缓冲区中剩余的事件(如果有)。
   * @returns {Array<{type: string, data: *}>}
   */
  flush() {
    const events = [];
    // 处理缓冲区中最后未以空行结尾的事件
    if (this.buffer !== '') {
      // 处理最后一行
      const line = this.buffer;
      this.buffer = '';
      if (line.startsWith('event:')) {
        this.eventName = line.slice(6).trim();
      } else if (line.startsWith('data:')) {
        const value = line.slice(5);
        this.dataLines.push(value.startsWith(' ') ? value.slice(1) : value);
      }
    }
    if (this.dataLines.length > 0) {
      const dataStr = this.dataLines.join('\n');
      events.push({
        type: this.eventName || 'message',
        data: this._parseData(dataStr),
      });
      this.eventName = null;
      this.dataLines = [];
    }
    return events;
  }

  /**
   * 尝试把 data 字符串解析为 JSON,失败则返回原始字符串。
   * @param {string} raw
   * @returns {*}
   */
  _parseData(raw) {
    try {
      return JSON.parse(raw);
    } catch {
      return raw;
    }
  }
}

module.exports = { SSEParser };
