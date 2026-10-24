// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 真流式思考面(ws.js)单测:LlmThinkingDelta 帧接入——thinking 累积/流式
// 收口时序(首个非思考帧/LlmDone/Done 收)/纯思考气泡不被空气泡清理移除
// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';

vi.mock('../src/lib/orchestration.js', () => ({
  refreshOrchestration: vi.fn(),
  orchestrationToolCall: vi.fn(),
  orchestrationToolResult: vi.fn(),
}));
vi.mock('../src/lib/api.js', async (importOriginal) => ({
  // api.js 顶层纯函数定义无副作用,部分模拟保底(Done 帧触发 refreshGovBadges
  // → getAgentDef 等 stores 侧消费)
  ...(await importOriginal()),
  getTranscript: vi.fn(async () => ({ messages: [], source: 'live', count: 0 })),
}));
vi.mock('../src/lib/audit.js', () => ({
  initAuditWatcher: vi.fn(),
}));

// MockWebSocket:onmessage 语义由测试侧 deliver 驱动(不真连)
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

const { connect, sendMessage, newSession } = await import('../src/lib/ws.js');
const { messages } = await import('../src/lib/stores.js');

/** 当前轮流式气泡(最后一条 assistant;ToolCall 后末位是工具卡,按 kind 过滤) */
function streamBubble() {
  const arr = get(messages);
  return arr.filter((x) => x.kind === 'assistant').at(-1);
}

async function startTurn() {
  MockWebSocket.last.open();
  await sendMessage('hi');
}

beforeEach(() => {
  MockWebSocket.instances.length = 0;
  messages.set([]);
  newSession();
});

describe('ws LlmThinkingDelta 思考面', () => {
  it('思考帧累积:首帧起记 startedAt + streaming=true', async () => {
    await startTurn();
    MockWebSocket.last.deliver({ type: 'LlmThinkingDelta', text: '思' });
    MockWebSocket.last.deliver({ type: 'LlmThinkingDelta', text: '考' });
    const m = streamBubble();
    expect(m.thinking).toBe('思考');
    expect(m.thinkingStreaming).toBe(true);
    expect(m.thinkingStartedAt).toBeGreaterThan(0);
    expect(m.thinkingEndedAt).toBeUndefined();
    // 思考不进正文
    expect(m.text).toBe('');
  });

  it('首个正文帧收思考流:streaming=false + endedAt 固化', async () => {
    await startTurn();
    MockWebSocket.last.deliver({ type: 'LlmThinkingDelta', text: '思' });
    MockWebSocket.last.deliver({ type: 'LlmDelta', text: '答' });
    const m = streamBubble();
    expect(m.thinkingStreaming).toBe(false);
    expect(m.thinkingEndedAt).toBeGreaterThanOrEqual(m.thinkingStartedAt);
    expect(m.text).toBe('答');
    expect(m.thinking).toBe('思');
  });

  it('ToolCall 同样收思考流', async () => {
    await startTurn();
    MockWebSocket.last.deliver({ type: 'LlmThinkingDelta', text: '先想' });
    MockWebSocket.last.deliver({ type: 'ToolCall', name: 'file_read', args: {} });
    expect(streamBubble().thinkingStreaming).toBe(false);
    // 工具卡气泡照常创建
    const arr = get(messages);
    expect(arr.some((x) => x.kind === 'tool' && x.name === 'file_read')).toBe(true);
  });

  it('LlmDone 与 Done 均收口;Done 后气泡 streaming=false', async () => {
    await startTurn();
    MockWebSocket.last.deliver({ type: 'LlmThinkingDelta', text: '思' });
    MockWebSocket.last.deliver({ type: 'LlmDone' });
    expect(streamBubble().thinkingStreaming).toBe(false);
    MockWebSocket.last.deliver({ type: 'Done', success: true, steps: 1 });
    const m = streamBubble();
    expect(m.streaming).toBe(false);
    expect(m.thinking).toBe('思');
  });

  it('纯思考轮(零正文):Done 后气泡保留不被空气泡清理移除', async () => {
    await startTurn();
    MockWebSocket.last.deliver({ type: 'LlmThinkingDelta', text: '只有思考' });
    MockWebSocket.last.deliver({ type: 'Done', success: true, steps: 1 });
    const m = streamBubble();
    expect(m.kind).toBe('assistant');
    expect(m.thinking).toBe('只有思考');
    expect(m.text).toBe('');
  });

  it('无思考轮行为零变化:正文照常,thinking 字段不存在', async () => {
    await startTurn();
    MockWebSocket.last.deliver({ type: 'LlmDelta', text: 'plain' });
    MockWebSocket.last.deliver({ type: 'Done', success: true, steps: 1 });
    const arr = get(messages);
    const assistant = arr.filter((x) => x.kind === 'assistant');
    expect(assistant).toHaveLength(1);
    expect(assistant[0].text).toBe('plain');
    expect(assistant[0].thinking).toBeUndefined();
    expect(assistant[0].thinkingStreaming).toBeUndefined();
  });
});
