// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// G16 换票接线入场券:serve 侧 `?token=` 长期凭据恒 401(auth 启用态),浏览器
// WS 建连唯一合法通道是 Bearer 换一次性短票(POST /api/auth/ticket → ?ticket=)。
// 本测试锁两件事:
//  1) wsAuthQuery 换票语义(有票带票/无票匿名/被拒回落匿名)
//  2) ws.js 主会话流建连 URL 走 ?ticket= 而非已封死的 ?token=
// (dap/lsp/pty/tasks 四处同 helper 共用,语义由 1) 覆盖)
// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('../src/lib/orchestration.js', () => ({
  refreshOrchestration: vi.fn(),
  orchestrationToolCall: vi.fn(),
  orchestrationToolResult: vi.fn(),
  orchestration: { update: vi.fn() },
}));
vi.mock('../src/lib/api.js', async (importOriginal) => ({
  ...(await importOriginal()),
  getTranscript: vi.fn(async () => ({ messages: [], source: 'live', count: 0 })),
}));
vi.mock('../src/lib/audit.js', () => ({
  initAuditWatcher: vi.fn(),
}));

// MockWebSocket:记录构造 URL,不真连
class MockWebSocket {
  static instances = [];
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  constructor(url) {
    this.url = url;
    this.readyState = MockWebSocket.CONNECTING;
    MockWebSocket.instances.push(this);
  }
  send() {}
  close() {
    this.readyState = MockWebSocket.CLOSED;
  }
  static get last() {
    return MockWebSocket.instances[MockWebSocket.instances.length - 1];
  }
}
vi.stubGlobal('WebSocket', MockWebSocket);

const { wsAuthQuery } = await import('../src/lib/api.js');
const { connect } = await import('../src/lib/ws.js');

function lastUrl() {
  return MockWebSocket.last?.url ?? '';
}

beforeEach(() => {
  localStorage.clear();
  MockWebSocket.instances.length = 0;
  vi.unstubAllGlobals();
  vi.stubGlobal('WebSocket', MockWebSocket);
});

describe('wsAuthQuery 换票语义', () => {
  it('token 在位:POST /api/auth/ticket 携 Bearer,返回 &ticket= 查询串', async () => {
    localStorage.setItem('evo_agent_token', 'tok-1');
    const fetchMock = vi.fn(async (path, opts) => ({
      ok: true,
      json: async () => ({ ticket: 'tk-abc', expires_in: 30 }),
    }));
    vi.stubGlobal('fetch', fetchMock);

    const q = await wsAuthQuery();
    expect(q).toBe('&ticket=tk-abc');
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [path, opts] = fetchMock.mock.calls[0];
    expect(path).toBe('/api/auth/ticket');
    expect(opts.method).toBe('POST');
    expect(opts.headers.Authorization).toBe('Bearer tok-1');
  });

  it('无 token(auth 关闭态):返回空串且不发起换票', async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
    expect(await wsAuthQuery()).toBe('');
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('换票被拒(401):回落空串(匿名直连,由服务端裁决)', async () => {
    localStorage.setItem('evo_agent_token', 'bad-tok');
    vi.stubGlobal('fetch', vi.fn(async () => ({ ok: false, status: 401 })));
    expect(await wsAuthQuery()).toBe('');
  });

  it('换票网络异常:回落空串不抛', async () => {
    localStorage.setItem('evo_agent_token', 'tok-1');
    vi.stubGlobal('fetch', vi.fn(async () => { throw new Error('net down'); }));
    expect(await wsAuthQuery()).toBe('');
  });

  it('响应缺 ticket 字段:回落空串', async () => {
    localStorage.setItem('evo_agent_token', 'tok-1');
    vi.stubGlobal('fetch', vi.fn(async () => ({ ok: true, json: async () => ({}) })));
    expect(await wsAuthQuery()).toBe('');
  });
});

describe('ws.js 主会话流建连接线', () => {
  it('token 在位:建连 URL 携 ?ticket= 且不再携带 ?token=', async () => {
    localStorage.setItem('evo_agent_token', 'tok-1');
    vi.stubGlobal('fetch', vi.fn(async () => ({
      ok: true,
      json: async () => ({ ticket: 'tk-xyz', expires_in: 30 }),
    })));
    await connect('new');
    const url = lastUrl();
    expect(url).toContain('/api/sessions/new/ws');
    expect(url).toContain('&ticket=tk-xyz');
    expect(url).not.toContain('token=');
  });

  it('无 token:匿名直连(URL 不含票也不含 token)', async () => {
    vi.stubGlobal('fetch', vi.fn());
    await connect('new');
    const url = lastUrl();
    expect(url).not.toContain('ticket=');
    expect(url).not.toContain('token=');
  });
});
