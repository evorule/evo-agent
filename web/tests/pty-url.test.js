// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// PTY WS URL 单测:终端连接须携带会话工作区(localStorage evo_workspace,
// 与 agent WS 同源)——终端 cwd 与写工具族/checkpoint 同根;缺省不带参数=零变化。
// @vitest-environment jsdom
import { describe, it, expect, vi } from 'vitest';

vi.mock('@xterm/xterm', () => ({ Terminal: class {} }));
vi.mock('@xterm/addon-fit', () => ({ FitAddon: class {} }));
vi.mock('@xterm/addon-webgl', () => ({ WebglAddon: class {} }));
vi.mock('@xterm/addon-search', () => ({ SearchAddon: class {} }));
vi.mock('@xterm/addon-unicode11', () => ({ Unicode11Addon: class {} }));

const { ptyUrl } = await import('../src/lib/pty.js');

describe('pty ws URL 会话工作区', () => {
  it('localStorage 有工作区 → URL 携带 workspace 参数', () => {
    localStorage.setItem('evo_workspace', 'D:\\projects\\demo');
    const url = ptyUrl('term-1', 120, 30, '');
    expect(url.startsWith('ws://')).toBe(true);
    expect(url).toContain(`workspace=${encodeURIComponent('D:\\projects\\demo')}`);
  });

  it('缺省无工作区 → 不带 workspace 参数(行为零变化)', () => {
    localStorage.removeItem('evo_workspace');
    const url = ptyUrl('term-1', 120, 30, '');
    expect(url).not.toContain('workspace=');
  });

  it('短票仍拼接在 workspace 之后', () => {
    localStorage.setItem('evo_workspace', 'D:\\projects\\demo');
    const url = ptyUrl('term-1', 120, 30, '&ticket=t-123');
    expect(url.endsWith('&ticket=t-123')).toBe(true);
    expect(url.indexOf('workspace=')).toBeLessThan(url.indexOf('&ticket='));
  });
});
