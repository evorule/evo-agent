// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { writable } from 'svelte/store';

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
