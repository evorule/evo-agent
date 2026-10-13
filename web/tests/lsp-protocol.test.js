// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// LSP 协议层单测:请求-响应路由/server→client 请求最小应答集/通知分发/超时。
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { createRpcLayer, SERVER_REQUEST_ANSWERS } from '../src/lib/lsp/protocol.js';

/** 捕获发送帧的假传输层 */
function fakeSend() {
  const sent = [];
  const layer = createRpcLayer((text) => sent.push(JSON.parse(text)));
  return { layer, sent };
}

describe('protocol: request-response routing', () => {
  let t;
  beforeEach(() => {
    t = fakeSend();
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('request 发送带自增 id 的请求,响应按 id 路由 resolve', async () => {
    const p1 = t.layer.request('textDocument/hover', { x: 1 });
    const p2 = t.layer.request('textDocument/completion', {});
    expect(t.sent).toHaveLength(2);
    expect(t.sent[0].id).toBe(1);
    expect(t.sent[1].id).toBe(2);
    // 倒序应答也按 id 路由
    t.layer.onMessage(JSON.stringify({ jsonrpc: '2.0', id: 2, result: ['completion'] }));
    t.layer.onMessage(JSON.stringify({ jsonrpc: '2.0', id: 1, result: { contents: [] } }));
    expect(await p2).toEqual(['completion']);
    expect(await p1).toEqual({ contents: [] });
  });

  it('error 响应 reject 并带 code', async () => {
    const p = t.layer.request('textDocument/hover', {});
    t.layer.onMessage(
      JSON.stringify({ jsonrpc: '2.0', id: 1, error: { code: -32601, message: 'nope' } }),
    );
    await expect(p).rejects.toThrow(/nope.*-32601/s);
  });

  it('超时 reject 并清理在途表', async () => {
    const p = t.layer.request('slow/method', {}, 1000);
    vi.advanceTimersByTime(1001);
    await expect(p).rejects.toThrow(/timeout/);
    expect(t.layer.pendingCount()).toBe(0);
  });

  it('failAllPending 使全部在途请求失败(断连语义)', async () => {
    const p1 = t.layer.request('a', {});
    const p2 = t.layer.request('b', {});
    t.layer.failAllPending('connection closed');
    await expect(p1).rejects.toThrow(/connection closed/);
    await expect(p2).rejects.toThrow(/connection closed/);
  });
});

describe('protocol: server→client requests (minimal answer set)', () => {
  it('registerCapability → 空 OK 应答', async () => {
    const t = fakeSend();
    t.layer.onMessage(
      JSON.stringify({
        jsonrpc: '2.0',
        id: 9,
        method: 'client/registerCapability',
        params: { registrations: [] },
      }),
    );
    expect(t.sent[0]).toMatchObject({ id: 9, result: {} });
  });

  it('workDoneProgress/create → null OK 应答', async () => {
    const t = fakeSend();
    t.layer.onMessage(
      JSON.stringify({
        jsonrpc: '2.0',
        id: 10,
        method: 'window/workDoneProgress/create',
        params: { token: 't' },
      }),
    );
    expect(t.sent[0]).toMatchObject({ id: 10, result: null });
  });

  it('workspace/configuration → METHOD_NOT_FOUND 拒收(server 回落默认)', async () => {
    const t = fakeSend();
    t.layer.onMessage(
      JSON.stringify({
        jsonrpc: '2.0',
        id: 11,
        method: 'workspace/configuration',
        params: { items: [{ section: 'rust' }] },
      }),
    );
    expect(t.sent[0].error).toMatchObject({ code: -32601 });
  });

  it('未知方法 → METHOD_NOT_FOUND', async () => {
    const t = fakeSend();
    t.layer.onMessage(JSON.stringify({ jsonrpc: '2.0', id: 12, method: 'window/showInput' }));
    expect(t.sent[0].error).toMatchObject({ code: -32601 });
  });

  it('onRequest 可扩展应答器', async () => {
    const t = fakeSend();
    const off = t.layer.onRequest('workspace/semanticTokens/refresh', () => ({}));
    t.layer.onMessage(
      JSON.stringify({ jsonrpc: '2.0', id: 13, method: 'workspace/semanticTokens/refresh' }),
    );
    expect(t.sent[0]).toMatchObject({ id: 13, result: {} });
    off();
    t.layer.onMessage(
      JSON.stringify({ jsonrpc: '2.0', id: 14, method: 'workspace/semanticTokens/refresh' }),
    );
    expect(t.sent[1].error).toMatchObject({ code: -32601 });
  });

  it('SERVER_REQUEST_ANSWERS 覆盖最小集三方法', () => {
    expect(Object.keys(SERVER_REQUEST_ANSWERS)).toContain('client/registerCapability');
    expect(Object.keys(SERVER_REQUEST_ANSWERS)).toContain('window/workDoneProgress/create');
  });
});

describe('protocol: notifications', () => {
  it('onNotification 按 method 分发,解绑后不再收', () => {
    const t = fakeSend();
    const seen = [];
    const off = t.layer.onNotification('textDocument/publishDiagnostics', (p) => seen.push(p));
    t.layer.onMessage(
      JSON.stringify({ jsonrpc: '2.0', method: 'textDocument/publishDiagnostics', params: { a: 1 } }),
    );
    off();
    t.layer.onMessage(
      JSON.stringify({ jsonrpc: '2.0', method: 'textDocument/publishDiagnostics', params: { a: 2 } }),
    );
    expect(seen).toEqual([{ a: 1 }]);
  });

  it('非 JSON 帧静默丢弃(防注水)', () => {
    const t = fakeSend();
    expect(() => t.layer.onMessage('not-json')).not.toThrow();
    expect(() => t.layer.onMessage(null)).not.toThrow();
  });

  it('notify 发送无 id 通知', () => {
    const t = fakeSend();
    t.layer.notify('textDocument/didOpen', { x: 1 });
    expect(t.sent[0]).toEqual({ jsonrpc: '2.0', method: 'textDocument/didOpen', params: { x: 1 } });
  });
});
