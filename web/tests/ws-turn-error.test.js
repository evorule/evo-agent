// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 失败轮错误卡(ws.js)单测:同轮 Error 帧与 Done(success=false) 常携带同
// 文案双发——同轮同文案只渲染一条错误卡;不同文案各自上报;跨轮不误吞。
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

function errorCards() {
  return get(messages).filter((x) => x.kind === 'error');
}

async function startTurn() {
  MockWebSocket.last.open();
  await sendMessage('hi');
}

beforeEach(async () => {
  MockWebSocket.instances.length = 0;
  messages.set([]);
  newSession();
  // G16 换票接线后 connect 已 async 化(建连前换票),须 await 落座
  await connect('new');
});

describe('失败轮错误卡同轮去重', () => {
  it('Error 帧后 Done(success=false) 同文案:只渲染一条错误卡', async () => {
    await startTurn();
    MockWebSocket.last.deliver({ type: 'Error', error: 'LLM error: boom' });
    MockWebSocket.last.deliver({ type: 'Done', success: false, error: 'LLM error: boom' });
    expect(errorCards()).toHaveLength(1);
  });

  it('仅 Done(success=false):错误卡照常渲染', async () => {
    await startTurn();
    MockWebSocket.last.deliver({ type: 'Done', success: false, error: 'boom' });
    expect(errorCards()).toHaveLength(1);
  });

  it('同轮两条不同错误文案:两条都渲染', async () => {
    await startTurn();
    MockWebSocket.last.deliver({ type: 'Error', error: 'boom A' });
    MockWebSocket.last.deliver({ type: 'Done', success: false, error: 'boom B' });
    expect(errorCards()).toHaveLength(2);
  });

  it('跨轮重置:上一轮已报文案不吞下一轮同文案错误', async () => {
    await startTurn();
    MockWebSocket.last.deliver({ type: 'Error', error: 'boom' });
    MockWebSocket.last.deliver({ type: 'Done', success: false, error: 'boom' });
    expect(errorCards()).toHaveLength(1);
    await sendMessage('hi again');
    MockWebSocket.last.deliver({ type: 'Error', error: 'boom' });
    MockWebSocket.last.deliver({ type: 'Done', success: false, error: 'boom' });
    expect(errorCards()).toHaveLength(2);
  });
});
