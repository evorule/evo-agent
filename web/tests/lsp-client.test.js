// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// LSP client 单测:文档同步 Full 去抖/版本维护/生命周期控制帧/清幽灵语义。
// 零真实 WebSocket/monaco:消息层与传输注入假件。
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { createLspClient, pathToFileUri } from '../src/lib/lsp/client.js';
import { createRpcLayer } from '../src/lib/lsp/protocol.js';

/** 组装被测 client:假消息层捕获发帧,不经 WebSocket */
function makeClient(overrides = {}) {
  const sent = [];
  const events = [];
  const diagnostics = [];
  const client = createLspClient({
    lang: 'rust',
    serverName: 'rust-analyzer',
    wsUrl: 'ws://test/api/lsp/ws?lang=rust',
    workdirBase: 'D:/work/proj',
    onEvent: (e) => events.push(e),
    onDiagnostics: (p) => diagnostics.push(p),
    pathFromModel: (model) => model.__path ?? null,
    ...overrides,
  });
  const rpc = createRpcLayer((text) => sent.push(JSON.parse(text)));
  client.__test.injectRpc(rpc);
  return { client, sent, events, diagnostics, rpc };
}

/** 假 monaco model(仅 getVersionId/getValue/languageId/路径/订阅桩) */
function fakeModel(text, version, path) {
  return {
    __path: path,
    getLanguageId: () => 'rust',
    getVersionId: () => version,
    getValue: () => text,
    onDidChangeContent: () => ({ dispose: () => {} }),
  };
}

describe('client: document sync (Full + debounce + version)', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('didOpen 带真实 file:// URI 与实时版本', () => {
    const { client, sent } = makeClient();
    client.__test.markReady();
    client.trackModel(fakeModel('fn main() {}', 3, 'src/main.rs'));
    const didOpen = sent.find((m) => m.method === 'textDocument/didOpen');
    expect(didOpen.params.textDocument).toMatchObject({
      uri: pathToFileUri('D:/work/proj', 'src/main.rs'),
      languageId: 'rust',
      version: 3,
      text: 'fn main() {}',
    });
  });

  it('300ms 去抖合并为一次 didChange,version 取实时值', async () => {
    const { client, sent } = makeClient();
    client.__test.markReady();
    const model = fakeModel('v2', 2, 'src/a.rs');
    client.trackModel(model); // didOpen 先行
    sent.length = 0;
    // 连续两次调度(模拟连续击键)只应合并为一帧
    client.__test.scheduleDidChange('src/a.rs', model);
    client.__test.scheduleDidChange('src/a.rs', model);
    vi.advanceTimersByTime(299);
    expect(sent.filter((m) => m.method === 'textDocument/didChange')).toHaveLength(0);
    vi.advanceTimersByTime(2);
    const changes = sent.filter((m) => m.method === 'textDocument/didChange');
    expect(changes).toHaveLength(1);
    expect(changes[0].params.contentChanges).toEqual([{ text: 'v2' }]);
    expect(changes[0].params.textDocument.version).toBe(2);
  });

  it('未就绪期编辑不发生 didChange(内容随 didOpen 全量补)', () => {
    const { client, sent } = makeClient();
    client.trackModel(fakeModel('x', 1, 'src/b.rs')); // state=idle,仅登记
    sent.length = 0;
    client.__test.scheduleDidChange('src/b.rs', fakeModel('y', 2, 'src/b.rs'));
    vi.advanceTimersByTime(500);
    expect(sent.filter((m) => m.method === 'textDocument/didChange')).toHaveLength(0);
  });

  it('didClose 清跟踪并推 evoOwnerReset(清诊断防幽灵)', () => {
    const { client, sent, diagnostics } = makeClient();
    client.__test.markReady();
    client.trackModel(fakeModel('x', 1, 'src/c.rs'));
    client.untrackModel(fakeModel('x', 1, 'src/c.rs'));
    expect(sent.some((m) => m.method === 'textDocument/didClose')).toBe(true);
    expect(diagnostics).toContainEqual({ evoOwnerReset: 'src/c.rs' });
  });
});

describe('client: lifecycle control frames', () => {
  it('lsp_state restarting → 清全部跟踪文档诊断 + 状态迁移', () => {
    const { client, diagnostics, events } = makeClient();
    client.__test.markReady();
    client.trackModel(fakeModel('x', 1, 'src/d.rs'));
    client.onControlFrame({ type: 'lsp_state', state: 'restarting', detail: 'backoff' });
    expect(diagnostics).toContainEqual({ evoOwnerReset: 'src/d.rs' });
    expect(events.some((e) => e.kind === 'state' && e.state === 'restarting')).toBe(true);
  });

  it('lsp_state failed → failed 终态', () => {
    const { client, events } = makeClient();
    client.onControlFrame({ type: 'lsp_state', state: 'failed', detail: 'budget' });
    expect(events.some((e) => e.state === 'failed')).toBe(true);
  });

  it('lsp_log → log 事件转发', () => {
    const { client, events } = makeClient();
    client.onControlFrame({ type: 'lsp_log', level: 'info', message: 'cache warm' });
    expect(events).toContainEqual({ kind: 'log', level: 'info', message: 'cache warm' });
  });

  it('lsp_unavailable → unavailable 状态', () => {
    const { client, events } = makeClient();
    client.onControlFrame({ type: 'lsp_unavailable', hint: 'not installed' });
    expect(events.some((e) => e.state === 'unavailable')).toBe(true);
  });
});

describe('client: feature query conversion', () => {
  it('textDocumentPosition:行列为 1-based→0-based', async () => {
    const { client, sent, rpc } = makeClient();
    const model = { __path: 'src/e.rs', getLanguageId: () => 'rust', getValue: () => '', getVersionId: () => 1 };
    const p = client.completion(model, { lineNumber: 3, column: 7 });
    const req = sent.find((m) => m.method === 'textDocument/completion');
    expect(req.params.position).toEqual({ line: 2, character: 6 });
    expect(req.params.textDocument.uri).toContain('src/e.rs');
    rpc.onMessage(JSON.stringify({ jsonrpc: '2.0', id: req.id, result: [] }));
    expect(await p).toEqual([]);
  });
});
