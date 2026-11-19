// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// N-009 回归锁:回合已被服务端放弃(serve 重启/abort-at-birth)时,stop 回执
// 「no active turn to interrupt」是终态等价物——ws.js Info 分支必须复位
// turnActive+finishStream,否则前端永久卡「执行中」(stop 按钮死锁,
// 2026-11-19 实证三连击无响应)。同 ws-approval-auto 模式:MockWebSocket
// deliver 驱动,api.js/audit.js 局部 mock。
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

const { connect, sendMessage, newSession } = await import('../src/lib/ws.js');
const { turnActive, messages } = await import('../src/lib/stores.js');

async function flush() {
  await new Promise((r) => setTimeout(r, 10));
}

beforeEach(async () => {
  MockWebSocket.instances.length = 0;
  messages.set([]);
  newSession();
  await connect('new');
  MockWebSocket.last.open();
  await sendMessage('恢复试点');
});

describe('Info(no active turn)复位回合态(N-009)', () => {
  it('no active turn to interrupt → turnActive 复位 false', async () => {
    expect(get(turnActive)).toBe(true);
    MockWebSocket.last.deliver({
      type: 'Info',
      message: 'no active turn to interrupt',
    });
    await flush();
    expect(get(turnActive)).toBe(false);
  });

  it('其他 Info 回执不复位(回归守卫:免误伤良性系统提示)', async () => {
    expect(get(turnActive)).toBe(true);
    MockWebSocket.last.deliver({
      type: 'Info',
      message: 'interrupt sent',
    });
    await flush();
    expect(get(turnActive)).toBe(true);
  });
});
