// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 轮内时序分段(ws.js)单测:ToolCall 帧固化当前流式气泡,后续 LlmDelta 新开
// 气泡——叙述段与工具卡按引擎事件时序交错,不再跨 step 合并单气泡。
// 附:工具后思考增量归新段;纯工具轮无空气泡残留。
// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';

vi.mock('../src/lib/orchestration.js', () => ({
  refreshOrchestration: vi.fn(),
  orchestrationToolCall: vi.fn(),
  orchestrationToolResult: vi.fn(),
}));
vi.mock('../src/lib/api.js', async (importOriginal) => ({
  ...(await importOriginal()),
  getTranscript: vi.fn(async () => ({ messages: [], source: 'live', count: 0 })),
}));
vi.mock('../src/lib/audit.js', () => ({
  initAuditWatcher: vi.fn(),
}));

class MockWebSocket {
  static instances = [];
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  constructor(url) {
    this.url = url;
    this.readyState = MockWebSocket.CONNECTING;
    this.sent = [];
    MockWebSocket.instances.push(this);
  }
  send(data) {
    this.sent.push(data);
  }
  close() {
    this.readyState = MockWebSocket.CLOSED;
  }
  open() {
    this.readyState = MockWebSocket.OPEN;
    this.onopen?.();
  }
  deliver(frame) {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }
  static get last() {
    return MockWebSocket.instances[MockWebSocket.instances.length - 1];
  }
}
vi.stubGlobal('WebSocket', MockWebSocket);

const { connect, sendMessage } = await import('../src/lib/ws.js');
const { messages } = await import('../src/lib/stores.js');

beforeEach(() => {
  messages.set([]);
  connect('new');
  MockWebSocket.last.open();
});

async function sendAndDeliver(frames) {
  await sendMessage('测试输入');
  const sock = MockWebSocket.last;
  for (const f of frames) sock.deliver(f);
  return get(messages);
}

describe('轮内时序分段', () => {
  it('Delta→ToolCall→Delta→Done 三消息交错(文本段不合并)', async () => {
    const arr = await sendAndDeliver([
      { type: 'LlmDelta', text: '第一段叙述。' },
      { type: 'ToolCall', name: 'file_read', args: { path: 'a.md' } },
      { type: 'ToolResult', name: 'file_read', result: { content: '正文' } },
      { type: 'LlmDelta', text: '第二段叙述。' },
      { type: 'Done', success: true, steps: 2 },
    ]);
    const kinds = arr.map((m) => m.kind);
    expect(kinds).toEqual(['user', 'assistant', 'tool', 'assistant']);
    expect(arr[1].text).toBe('第一段叙述。');
    expect(arr[1].streaming).toBe(false);
    expect(arr[2].result).toContain('正文');
    expect(arr[3].text).toBe('第二段叙述。');
    expect(arr[3].streaming).toBe(false);
  });

  it('工具调用后的思考增量归新段气泡(不丢)', async () => {
    const arr = await sendAndDeliver([
      { type: 'LlmDelta', text: '先做一步。' },
      { type: 'ToolCall', name: 'shell_exec', args: { command: 'ls' } },
      { type: 'ToolResult', name: 'shell_exec', result: { content: 'ok' } },
      { type: 'LlmThinkingDelta', text: '下一步要想…' },
      { type: 'LlmDelta', text: '再做一步。' },
      { type: 'Done', success: true, steps: 2 },
    ]);
    const segments = arr.filter((m) => m.kind === 'assistant');
    expect(segments).toHaveLength(2);
    expect(segments[1].thinking).toBe('下一步要想…');
    expect(segments[1].text).toBe('再做一步。');
  });

  it('纯工具轮(无文本增量)无空气泡残留', async () => {
    const arr = await sendAndDeliver([
      { type: 'ToolCall', name: 'file_read', args: { path: 'a.md' } },
      { type: 'ToolResult', name: 'file_read', result: { content: 'x' } },
      { type: 'Done', success: true, steps: 1 },
    ]);
    expect(arr.filter((m) => m.kind === 'assistant')).toHaveLength(0);
    expect(arr.filter((m) => m.kind === 'tool')).toHaveLength(1);
  });
});
