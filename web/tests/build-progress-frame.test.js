// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// v2 typed 进度帧通路单测:build_progress 帧接入——store 投影(一次性信号
// 契约同 fsEvents)与合并函数(seq 去重/新到置顶/容量裁剪/无 seq 如实保留)
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
    MockWebSocket.instances.push(this);
  }
  send() {}
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

const { connect } = await import('../src/lib/ws.js');
const { buildProgress } = await import('../src/lib/stores.js');
const { mergeProgressEvents } = await import('../src/lib/progress-events.js');

describe('build_progress frame wiring', () => {
  beforeEach(() => {
    buildProgress.set(null);
  });

  it('delivers parsed typed frames into the one-shot store', async () => {
    await connect('s1');
    MockWebSocket.last.open();
    MockWebSocket.last.deliver({
      type: 'build_progress',
      path: 'video_engine/projects/demo/build.progress.ndjson',
      total_events: 2,
      events: [
        { seq: 1, phase: 'plan' },
        { seq: 2, phase: 'render_shot', progress: 0.5 },
      ],
    });
    const frame = get(buildProgress);
    expect(frame.path).toBe('video_engine/projects/demo/build.progress.ndjson');
    expect(frame.total).toBe(2);
    expect(frame.events.map((e) => e.seq)).toEqual([1, 2]);
    // 消费后置 null(一次性契约)
    buildProgress.set(null);
    expect(get(buildProgress)).toBeNull();
  });
});

describe('mergeProgressEvents', () => {
  it('dedups by seq across channels and puts new events on top', () => {
    const existing = [
      { seq: 3, phase: 'render_shot' },
      { seq: 2, phase: 'plan' },
    ];
    // 尾随通道与 typed 帧重复投递 seq3(双通道冗余,去重无害)
    const merged = mergeProgressEvents(existing, [
      { seq: 3, phase: 'render_shot' },
      { seq: 4, phase: 'concat' },
    ]);
    expect(merged.map((e) => e.seq)).toEqual([4, 3, 2]);
  });

  it('keeps seq-less events as delivered (honest presentation)', () => {
    const merged = mergeProgressEvents([{ seq: 1 }], [{ note: 'anon' }]);
    expect(merged.map((e) => e.note || e.seq)).toEqual(['anon', 1]);
    const again = mergeProgressEvents(merged, [{ note: 'anon' }]);
    expect(again.filter((e) => e.note === 'anon')).toHaveLength(2);
  });

  it('caps at the requested size, keeping the newest', () => {
    const existing = [{ seq: 48 }, { seq: 49 }, { seq: 50 }];
    const merged = mergeProgressEvents(existing, [{ seq: 51 }, { seq: 52 }], 4);
    // 裁剪截尾保新:最旧的 50 出局
    expect(merged.map((e) => e.seq)).toEqual([52, 51, 48, 49]);
    expect(merged).toHaveLength(4);
  });

  it('ignores malformed entries', () => {
    expect(mergeProgressEvents([{ seq: 1 }], [null, 'str', { seq: 2 }]).map((e) => e.seq)).toEqual([
      2, 1,
    ]);
  });
});
