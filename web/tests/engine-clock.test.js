// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 引擎活性时钟单测:回合起点(sendMessage/resume)必须重置 lastEngineEventAt。
// 缺陷形态:时钟仅帧到达漏斗触碰——上一轮遗留的陈旧时间戳令琥珀停滞提示
// (turn 活跃且距上次引擎事件 ≥10s)在新回合一开始即误报(发送后引擎尚在
// 思考、零帧窗口)。SessionCreated 重建续发路径与帧漏斗同帧相邻(时钟由
// 434 行同步触碰),重置为统一不变式,伴生回归锁住续发行为本体。
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

const { connect, sendMessage, resumeSession } = await import('../src/lib/ws.js');
const { messages, turnActive, lastEngineEventAt } = await import('../src/lib/stores.js');

/** 把时钟拨回 60s 前(模拟上一轮遗留的陈旧触碰),返回新鲜度断言闭包 */
function staleClock() {
  lastEngineEventAt.set(Date.now() - 60_000);
  const stale = get(lastEngineEventAt);
  return () => {
    expect(get(lastEngineEventAt)).toBeGreaterThan(stale + 30_000);
  };
}

beforeEach(async () => {
  messages.set([]);
  turnActive.set(false);
  await connect('new');
  MockWebSocket.last.open();
});

describe('回合起点引擎时钟重置', () => {
  it('sendMessage 起点重置时钟(无帧窗口不误报停滞)', async () => {
    const assertFresh = staleClock();
    await sendMessage('测试输入');
    assertFresh();
    expect(get(turnActive)).toBe(true);
  });

  it('resume 起点重置时钟(恢复会话同理)', async () => {
    const assertFresh = staleClock();
    await resumeSession();
    assertFresh();
    expect(get(turnActive)).toBe(true);
  });

  it('重建续发(SessionCreated autoSend)回合照常起轮:回注重发+turn 活跃', async () => {
    // 先落 sessionId(否则自愈取不到 deadSid,降级裸重发无回注块)
    MockWebSocket.last.deliver({ type: 'SessionCreated', session_id: 'dead-1' });
    await sendMessage('死前消息');
    // 死会话错误帧 → 自愈重建(取历史+新建+pendingRebuild 就位)
    MockWebSocket.last.deliver({
      type: 'Done',
      success: false,
      error: 'Evorule error: API returned error: 404',
    });
    // flush 微任务链:getTranscript → newSession → connect('new') 建新 socket
    await new Promise((r) => setTimeout(r, 20));
    const sock = MockWebSocket.last;
    expect(sock).not.toBe(MockWebSocket.instances[0]);
    sock.open();
    sock.deliver({ type: 'SessionCreated', session_id: 'rebuilt-1' });
    // 续发:最后一条 user 气泡回推 + wire 前缀回注块 + 回合活跃
    const kinds = get(messages).map((m) => m.kind);
    expect(kinds).toContain('user');
    expect(get(turnActive)).toBe(true);
    const sent = JSON.parse(sock.sent.at(-1));
    expect(sent.type).toBe('message');
    expect(sent.content).toContain('死前消息');
    expect(sent.content).toContain('<recovered_history>');
  });
});
