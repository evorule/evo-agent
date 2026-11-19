// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 回归锁:「复制 API Token」逻辑面三态——①有 token+clipboard 可用=
// 复制成功;②token 未注入=empty 拒绝(auth 关闭态提示);③clipboard 不可用
// 掉 execCommand 回退链仍成功。token 源=壳 init_script 注入的
// localStorage['evo_agent_token'](webview 已持有,按钮零新增暴露面)。
// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';

const { copyApiToken } = await import('../src/lib/tokenCopy.js');

describe('copyApiToken(token 显式复制门)', () => {
  let writeText;

  beforeEach(() => {
    writeText = vi.fn().mockResolvedValue(undefined);
    // jsdom 无 navigator.clipboard:注入可写替身,缺失态由 delete 模拟
    Object.defineProperty(window, 'navigator', {
      value: { clipboard: { writeText } },
      configurable: true,
      writable: true,
    });
    localStorage.setItem('evo_agent_token', 'srv-token-abc123');
  });

  afterEach(() => {
    vi.restoreAllMocks();
    localStorage.removeItem('evo_agent_token');
    document.querySelectorAll('textarea').forEach((t) => t.remove());
  });

  it('有 token+clipboard 可用 → 写入剪贴板成功', async () => {
    const r = await copyApiToken();
    expect(r).toEqual({ ok: true, reason: 'copied' });
    expect(writeText).toHaveBeenCalledWith('srv-token-abc123');
  });

  it('clipboard 抛错 → execCommand 回退链成功', async () => {
    writeText.mockRejectedValue(new Error('denied by permission'));
    const exec = vi.fn().mockReturnValue(true);
    document.execCommand = exec;
    const r = await copyApiToken();
    expect(r).toEqual({ ok: true, reason: 'copied' });
    expect(exec).toHaveBeenCalledWith('copy');
    delete document.execCommand;
  });

  it('token 未注入 → empty 拒绝(不碰剪贴板)', async () => {
    localStorage.removeItem('evo_agent_token');
    const r = await copyApiToken();
    expect(r).toEqual({ ok: false, reason: 'empty' });
    expect(writeText).not.toHaveBeenCalled();
  });

  it('双链皆败 → denied', async () => {
    writeText.mockRejectedValue(new Error('x'));
    document.execCommand = vi.fn().mockReturnValue(false);
    const r = await copyApiToken();
    expect(r).toEqual({ ok: false, reason: 'denied' });
    delete document.execCommand;
  });
});
