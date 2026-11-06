// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 批② web 兼容面测试(ws.js):①自动批准 POST 撞 409 no_pending_item(后端
// T0 allowlist 静默批准先决)→ 改判「后端已决」静默收卡,不报错误回执,
// 随后 ApprovalResult 帧按既有去重并入;②approver=workspace-allowlist
// 收 pending 卡转「已批准(工作区清单)」回执(不留僵尸卡)。同 ws-turn-error
// 模式:MockWebSocket deliver 驱动,api.js 局部 mock。
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
const { workspaceAllowlist, clearSessionMemory } = await import('../src/lib/approvalMemory.js');
const { approveProposal } = await import('../src/lib/api.js');

async function flush() {
  await new Promise((r) => setTimeout(r, 10));
}

beforeEach(async () => {
  MockWebSocket.instances.length = 0;
  approveProposal.mockReset();
  messages.set([]);
  workspaceAllowlist.set([]);
  clearSessionMemory();
  newSession();
  // G16 换票接线后 connect 已 async 化(建连前换票),须 await 落座
  await connect('new');
  MockWebSocket.last.open();
  await sendMessage('hi');
});

function msgOf(kind) {
  return get(messages).filter((m) => m.kind === kind);
}

describe('自动批准 POST 409 no_pending_item 静默收卡(后端 T0 已决)', () => {
  it('409:回执定稿 sent,不回退错误卡', async () => {
    workspaceAllowlist.set([{ tool: 'shell_exec', prefix: 'find' }]);
    approveProposal.mockRejectedValueOnce(
      Object.assign(new Error('no_pending_item'), { status: 409 }),
    );
    MockWebSocket.last.deliver({
      type: 'ApprovalRequired',
      tool_name: 'shell_exec',
      command: 'find src -name x',
      risk: 'low',
      proposal_id: 'p1',
    });
    await flush();
    const receipts = msgOf('approval_receipt');
    expect(receipts).toHaveLength(1);
    expect(receipts[0].decision).toBe('sent');
    expect(receipts[0].scope).toBe('auto-workspace');
    expect(msgOf('approval')).toHaveLength(0); // 无错误回执/待批卡
  });

  it('409 后 ApprovalResult 帧:自动批准登记保留,不重复信息条', async () => {
    workspaceAllowlist.set([{ tool: 'shell_exec', prefix: 'find' }]);
    approveProposal.mockRejectedValueOnce(
      Object.assign(new Error('no_pending_item'), { status: 409 }),
    );
    MockWebSocket.last.deliver({
      type: 'ApprovalRequired',
      tool_name: 'shell_exec',
      command: 'find src',
      risk: 'low',
      proposal_id: 'p1',
    });
    await flush();
    MockWebSocket.last.deliver({
      type: 'ApprovalResult',
      tool_name: 'shell_exec',
      approved: true,
      approver: 'workspace-allowlist',
      auto_rejected: false,
      proposal_id: 'p1',
    });
    await flush();
    expect(msgOf('info').filter((m) => String(m.text).startsWith('审批结果'))).toHaveLength(0);
  });

  it('非 409 通道错误:维持回退待批卡语义', async () => {
    workspaceAllowlist.set([{ tool: 'shell_exec', prefix: 'find' }]);
    approveProposal.mockRejectedValueOnce(new Error('HTTP 500'));
    MockWebSocket.last.deliver({
      type: 'ApprovalRequired',
      tool_name: 'shell_exec',
      command: 'find src',
      risk: 'low',
      proposal_id: 'p1',
    });
    await flush();
    const cards = msgOf('approval');
    expect(cards).toHaveLength(1);
    expect(cards[0].decision).toBe('error');
    expect(String(cards[0].errText)).toContain('自动批准失败');
  });
});

describe('approver=workspace-allowlist 回执映射(后端 T0 静默批准)', () => {
  it('pending 卡收场转「已批准(工作区清单)」回执+映射信息条', async () => {
    // 清单镜像为空(未加载/加载失败)→ 前端判 ask 推待批卡,后端 T0 已决
    MockWebSocket.last.deliver({
      type: 'ApprovalRequired',
      tool_name: 'shell_exec',
      command: 'find src',
      risk: 'low',
      proposal_id: 'p2',
    });
    MockWebSocket.last.deliver({
      type: 'ApprovalResult',
      tool_name: 'shell_exec',
      approved: true,
      approver: 'workspace-allowlist',
      auto_rejected: false,
      proposal_id: 'p2',
    });
    await flush();
    expect(msgOf('approval')).toHaveLength(0); // 卡已收,不留僵尸
    const receipts = msgOf('approval_receipt');
    expect(receipts).toHaveLength(1);
    expect(receipts[0].scope).toBe('workspace-allowlist');
    expect(receipts[0].decision).toBe('sent');
    expect(receipts[0].sentApproved).toBe(true);
    const infos = msgOf('info').filter((m) => String(m.text).startsWith('审批结果'));
    expect(infos).toHaveLength(1);
    expect(infos[0].text).toBe('审批结果:已批准(工作区清单)(工具 shell_exec)');
  });

  it('普通人工批准(approver=user)文案不变', async () => {
    MockWebSocket.last.deliver({
      type: 'ApprovalResult',
      tool_name: 'shell_exec',
      approved: true,
      approver: 'user',
      auto_rejected: false,
      proposal_id: 'p3',
    });
    await flush();
    const infos = msgOf('info').filter((m) => String(m.text).startsWith('审批结果'));
    expect(infos[0].text).toBe('审批结果:已批准(工具 shell_exec)');
  });
});
