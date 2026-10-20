// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect, vi, beforeEach } from 'vitest';

// getJsonRetry 直测:网络级失败(fetch reject)退避重试、HTTP 响应不重试。
// 桩掉全局 fetch,控制每次调用结果;localStorage 桩防 node 环境缺 DOM API。

vi.stubGlobal('localStorage', {
  getItem: () => 'test-token',
  setItem: () => {},
  removeItem: () => {},
});

import { getJsonRetry } from '../src/lib/api.js';

function okResponse(body) {
  return { ok: true, json: () => Promise.resolve(body) };
}

describe('getJsonRetry(连接韧性:停机窗对账拉取)', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it('网络失败后重试到成功(退避节奏,计时器桩零等待)', async () => {
    const fetchMock = vi
      .fn()
      .mockRejectedValueOnce(new TypeError('fetch failed'))
      .mockRejectedValueOnce(new TypeError('fetch failed'))
      .mockResolvedValueOnce(okResponse({ resumable: [] }));
    vi.stubGlobal('fetch', fetchMock);
    const timers = vi.spyOn(globalThis, 'setTimeout').mockImplementation((fn) => {
      fn();
      return 0;
    });
    const out = await getJsonRetry('/api/sessions/resumable', 5);
    expect(out).toEqual({ resumable: [] });
    expect(fetchMock).toHaveBeenCalledTimes(3);
    timers.mockRestore();
  });

  it('HTTP 错误响应不重试(错误语义归调用方)', async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue({ ok: false, status: 500, text: () => Promise.resolve('boom') });
    vi.stubGlobal('fetch', fetchMock);
    // unwrap 以响应体文本为错误消息(实现在 api.js,此处只断不重试)
    await expect(getJsonRetry('/api/sessions', 5)).rejects.toThrow('boom');
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('重试耗尽后如实抛出网络错误', async () => {
    const fetchMock = vi.fn().mockRejectedValue(new TypeError('fetch failed'));
    vi.stubGlobal('fetch', fetchMock);
    const timers = vi.spyOn(globalThis, 'setTimeout').mockImplementation((fn) => {
      fn();
      return 0;
    });
    await expect(getJsonRetry('/x', 3)).rejects.toThrow('fetch failed');
    expect(fetchMock).toHaveBeenCalledTimes(3);
    timers.mockRestore();
  });
});
