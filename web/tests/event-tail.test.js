// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect, vi } from 'vitest';
import { EventTail, TAIL_OFFLINE_THRESHOLD } from '../src/lib/event-tail.js';

/** 真 Response 对象(node undici):status/headers.get/text 全语义 */
function resp(status, body, headers = {}) {
  return new Response(body, { status, headers });
}

function cr206(start, end, total) {
  return { 'content-range': `bytes ${start}-${end}/${total}` };
}

function makeTail(fetchImpl, extra = {}) {
  const events = [];
  const states = [];
  const parseErrors = [];
  const tail = new EventTail({
    path: 'video_engine/projects/demo/build.progress.ndjson',
    fetchImpl,
    onEvents: (e) => events.push(...e),
    onState: (s) => states.push(s),
    onParseError: (l) => parseErrors.push(l),
    ...extra,
  });
  return { tail, events, states, parseErrors };
}

describe('EventTail', () => {
  it('full 200 on start: parses all lines and advances cursor', async () => {
    const fetchImpl = vi.fn(async () =>
      resp(200, '{"seq":1}\n{"seq":2}\n', { 'content-length': '20' })
    );
    const { tail, events, states } = makeTail(fetchImpl);
    await tail.start();
    expect(events.map((e) => e.seq)).toEqual([1, 2]);
    expect(tail.cursor).toBe(20);
    expect(states[states.length - 1]).toBe('following');
    expect(fetchImpl.mock.calls[0][1].Range).toBe('bytes=0-');
  });

  it('206 append: incremental events with partial-line buffering', async () => {
    const bodies = [
      resp(200, '{"seq":1}\n{"seq":2}\n'),
      // 第二轮:半行先到(无尾换行)——解析应止于完整行,残段留缓冲
      resp(206, '{"seq":3}\n{"seq":', cr206(20, 37, 100)),
      resp(206, '4}\n', cr206(38, 40, 100)),
    ];
    const fetchImpl = vi.fn(async () => bodies.shift());
    const { tail, events } = makeTail(fetchImpl);
    await tail.start();
    expect(events.map((e) => e.seq)).toEqual([1, 2]);
    await tail.pull();
    expect(events.map((e) => e.seq)).toEqual([1, 2, 3]);
    expect(tail.cursor).toBe(38);
    await tail.pull();
    expect(events.map((e) => e.seq)).toEqual([1, 2, 3, 4]);
    expect(tail.cursor).toBe(41);
    // 游标请求从上一 end+1 继续
    expect(fetchImpl.mock.calls[2][1].Range).toBe('bytes=38-');
  });

  it('416 with total===cursor is a clean no-op', async () => {
    const fetchImpl = vi.fn()
      .mockResolvedValueOnce(resp(200, '{"seq":1}\n'))
      .mockResolvedValueOnce(resp(416, '', { 'content-range': 'bytes */10' }));
    const { tail, events, states } = makeTail(fetchImpl);
    await tail.start();
    await tail.pull();
    expect(events).toHaveLength(1);
    expect(tail.cursor).toBe(10);
    expect(states[states.length - 1]).toBe('following');
  });

  it('416 with total<cursor detects truncation and refetches from zero', async () => {
    const fetchImpl = vi.fn()
      .mockResolvedValueOnce(resp(200, '{"a":1}\n{"b":2}\n{"c":3}\n'))
      // 文件被重写变小:游标 24 落在 total 9 之外 → 416
      .mockResolvedValueOnce(resp(416, '', { 'content-range': 'bytes */9' }))
      .mockResolvedValueOnce(resp(200, '{"x":9}\n'));
    const { tail, events } = makeTail(fetchImpl);
    await tail.start();
    await tail.pull();
    // 重放语义:截断后的新内容全量呈现
    expect(events.map((e) => Object.values(e)[0])).toEqual([1, 2, 3, 9]);
    expect(fetchImpl.mock.calls[2][1].Range).toBe('bytes=0-');
  });

  it('404 enters waiting; recovery after the file reappears', async () => {
    const fetchImpl = vi.fn()
      .mockResolvedValueOnce(resp(404, 'path does not exist'))
      .mockResolvedValueOnce(resp(206, '{"seq":1}\n', cr206(0, 10, 10)));
    const { tail, events, states } = makeTail(fetchImpl);
    await tail.start();
    expect(tail.state).toBe('waiting');
    await tail.noteFsEvent(['video_engine/projects/demo/build.progress.ndjson']);
    expect(events.map((e) => e.seq)).toEqual([1]);
    expect(states).toContain('following');
  });

  it('offline after consecutive failures; retry recovers', async () => {
    let fail = true;
    const fetchImpl = vi.fn(async () => {
      if (fail) throw new Error('conn reset');
      return resp(200, '{"seq":1}\n');
    });
    const { tail, states, events } = makeTail(fetchImpl);
    await tail.start();
    await tail.pull();
    expect(tail.state).toBe('offline');
    expect(states.filter((s) => s === 'offline')).toHaveLength(1);
    fail = false;
    await tail.retry();
    expect(events.map((e) => e.seq)).toEqual([1]);
    expect(tail.state).toBe('following');
  });

  it('threshold constant is 2 (§7 explicit-offline criterion)', () => {
    expect(TAIL_OFFLINE_THRESHOLD).toBe(2);
  });

  it('noteFsEvent matches the watched file and its parent dir only', async () => {
    const fetchImpl = vi.fn(async () => resp(206, '{"seq":1}\n', cr206(0, 10, 10)));
    const { tail, events } = makeTail(fetchImpl);
    await tail.start();
    const before = fetchImpl.mock.calls.length;
    tail.noteFsEvent(['unrelated/path.txt']);
    expect(fetchImpl.mock.calls.length).toBe(before);
    tail.noteFsEvent(['video_engine/projects/demo']);
    expect(fetchImpl.mock.calls.length).toBe(before + 1);
    expect(events).toHaveLength(1);
  });

  it('parse failures are reported and do not break the stream', async () => {
    const fetchImpl = vi.fn()
      .mockResolvedValueOnce(resp(200, '{"seq":1}\nNOT JSON AT ALL\n{"seq":2}\n'))
      .mockResolvedValueOnce(resp(206, '{"seq":3}\n', cr206(37, 46, 46)));
    const { tail, events, parseErrors } = makeTail(fetchImpl);
    await tail.start();
    await tail.pull();
    expect(events.map((e) => e.seq)).toEqual([1, 2, 3]);
    expect(parseErrors).toHaveLength(1);
  });

  it('concurrent pulls merge into one follow-up pull', async () => {
    let release;
    const gate = new Promise((r) => (release = r));
    const fetchImpl = vi.fn()
      .mockImplementationOnce(async () => {
        await gate;
        return resp(206, '{"seq":1}\n', cr206(0, 10, 10));
      })
      // 补拉尊重游标(11-):服务端无新数据 → 416 no-op(不重复交付)
      .mockResolvedValue(resp(416, '', { 'content-range': 'bytes */11' }));
    const { tail, events } = makeTail(fetchImpl);
    const p1 = tail.pull();
    const p2 = tail.pull();
    const p3 = tail.pull();
    release();
    await Promise.all([p1, p2, p3]);
    // 首轮合并为一次执行;合并标记在首轮结束后触发恰好一次补拉
    await new Promise((r) => setTimeout(r, 5));
    expect(fetchImpl).toHaveBeenCalledTimes(2);
    expect(events).toHaveLength(1);
  });

  it('stop() freezes the tail (pull and fs events do nothing)', async () => {
    const fetchImpl = vi.fn(async () => resp(200, '{"seq":1}\n'));
    const { tail } = makeTail(fetchImpl);
    tail.stop();
    await tail.pull();
    tail.noteFsEvent(['video_engine/projects/demo/build.progress.ndjson']);
    await tail.retry();
    expect(fetchImpl).toHaveBeenCalledTimes(0);
    expect(tail.state).toBe('idle');
  });
});
