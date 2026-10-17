// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// DAP 协议层单测:seq 请求-响应配对/事件分发(含通配)/超时/坏帧丢弃。
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { createDapLayer } from '../src/lib/dap/protocol.js';

/** 捕获发送帧的假传输层 */
function fakeSend() {
  const sent = [];
  const layer = createDapLayer((text) => sent.push(JSON.parse(text)));
  return { layer, sent };
}

describe('dap protocol: request-response pairing', () => {
  let t;
  beforeEach(() => {
    t = fakeSend();
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('request 发送带自增 seq 的请求,响应按 request_seq 配对 resolve', async () => {
    const p1 = t.layer.request('initialize', { adapterID: 'rust' });
    const p2 = t.layer.request('threads');
    expect(t.sent).toHaveLength(2);
    expect(t.sent[0]).toMatchObject({ seq: 1, type: 'request', command: 'initialize' });
    expect(t.sent[1]).toMatchObject({ seq: 2, type: 'request', command: 'threads' });
    // 倒序应答也按 request_seq 路由
    t.layer.onMessage(JSON.stringify({ seq: 10, type: 'response', request_seq: 2, success: true, command: 'threads', body: { threads: [] } }));
    t.layer.onMessage(JSON.stringify({ seq: 11, type: 'response', request_seq: 1, success: true, command: 'initialize', body: {} }));
    expect(await p2).toEqual({ threads: [] });
    expect(await p1).toEqual({});
  });

  it('success=false 响应 reject 并携带 message', async () => {
    const p = t.layer.request('launch', {});
    t.layer.onMessage(JSON.stringify({ seq: 2, type: 'response', request_seq: 1, success: false, command: 'launch', message: 'invalid program' }));
    await expect(p).rejects.toThrow('invalid program');
  });

  it('超时 reject 并清理 pending', async () => {
    const p = t.layer.request('pause', undefined, 1000);
    const pErr = expect(p).rejects.toThrow('DAP request timeout: pause');
    vi.advanceTimersByTime(1500);
    await pErr;
    expect(t.layer.pendingCount()).toBe(0);
  });

  it('迟到/未知 request_seq 丢弃;非 JSON 帧静默丢弃', () => {
    expect(() => t.layer.onMessage('not-json')).not.toThrow();
    expect(() =>
      t.layer.onMessage(JSON.stringify({ seq: 9, type: 'response', request_seq: 999, success: true })),
    ).not.toThrow();
    expect(t.layer.pendingCount()).toBe(0);
  });
});

describe('dap protocol: events', () => {
  it('事件按名分发;通配订阅收到 (body, event)', () => {
    const t = fakeSend();
    const seen = [];
    const wild = [];
    t.layer.onEvent('stopped', (b) => seen.push(b));
    t.layer.onEvent('*', (b, ev) => wild.push([ev, b]));
    t.layer.onMessage(JSON.stringify({ seq: 1, type: 'event', event: 'stopped', body: { reason: 'breakpoint' } }));
    t.layer.onMessage(JSON.stringify({ seq: 2, type: 'event', event: 'output', body: { output: 'hi' } }));
    expect(seen).toEqual([{ reason: 'breakpoint' }]);
    expect(wild.map(([ev]) => ev)).toEqual(['stopped', 'output']);
  });

  it('onEvent 返回解绑函数', () => {
    const t = fakeSend();
    const seen = [];
    const off = t.layer.onEvent('output', (b) => seen.push(b));
    t.layer.onMessage(JSON.stringify({ seq: 1, type: 'event', event: 'output', body: { output: 'a' } }));
    off();
    t.layer.onMessage(JSON.stringify({ seq: 2, type: 'event', event: 'output', body: { output: 'b' } }));
    expect(seen).toEqual([{ output: 'a' }]);
  });

  it('failAllPending 断连时全部 reject', async () => {
    const t = fakeSend();
    const p = t.layer.request('threads');
    t.layer.failAllPending('connection closed');
    await expect(p).rejects.toThrow('connection closed');
  });
});
