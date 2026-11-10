// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 工具输出尾随帧消费测试:ToolOutputChunk 追加到 running 工具卡流式段
// (环形帽截尾)、ToolResult 到达清段切权威、非 running 卡不消费。
// 同 ws-approval-auto 模式:MockWebSocket deliver 驱动,api.js 局部 mock。
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
  approveProposal: vi.fn(),
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

async function flush() {
  await new Promise((r) => setTimeout(r, 10));
}

beforeEach(async () => {
  MockWebSocket.instances.length = 0;
  messages.set([]);
  await connect('new');
  MockWebSocket.last.open();
  await sendMessage('run a long command');
});

function lastTool() {
  const arr = get(messages);
  return arr[arr.length - 1].kind === 'tool' ? arr[arr.length - 1] : null;
}

describe('ToolOutputChunk 尾随消费', () => {
  it('chunk 追加到 running 卡流式段;超环帽截尾置标', () => {
    const ws = MockWebSocket.last;
    ws.deliver({ type: 'ToolCall', name: 'shell_exec', args: { command: 'ping' } });
    ws.deliver({ type: 'ToolOutputChunk', tool_name: 'shell_exec', chunk: 'line1\n' });
    ws.deliver({ type: 'ToolOutputChunk', tool_name: 'shell_exec', chunk: 'line2\n' });
    const m = lastTool();
    expect(m.running).toBe(true);
    expect(m.streamOut).toBe('line1\nline2\n');
    expect(m.streamTruncated).toBe(false);
    // 环形帽(16KB):一次超帽 chunk 触发截尾置标
    ws.deliver({
      type: 'ToolOutputChunk',
      tool_name: 'shell_exec',
      chunk: 'X'.repeat(20 * 1024),
    });
    const m2 = lastTool();
    expect(m2.streamOut.length).toBe(16 * 1024);
    expect(m2.streamTruncated).toBe(true);
  });

  it('ToolResult 到达:流式段清空,权威结果接管', async () => {
    const ws = MockWebSocket.last;
    ws.deliver({ type: 'ToolCall', name: 'shell_exec', args: { command: 'ping' } });
    ws.deliver({ type: 'ToolOutputChunk', tool_name: 'shell_exec', chunk: 'partial' });
    ws.deliver({
      type: 'ToolResult',
      name: 'shell_exec',
      result: { stdout: 'full output' },
    });
    await flush();
    const m = lastTool();
    expect(m.running).toBe(false);
    expect(m.streamOut).toBe('');
    expect(m.result).toContain('full output');
  });

  it('chunk 在无 running 卡时静默丢弃(跨帧迟到不挂)', () => {
    const ws = MockWebSocket.last;
    expect(() =>
      ws.deliver({ type: 'ToolOutputChunk', tool_name: 'shell_exec', chunk: 'orphan' }),
    ).not.toThrow();
    expect(get(messages).some((m) => m.streamOut === 'orphan')).toBe(false);
  });
});
