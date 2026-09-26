// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// stores 基建单测:B2 前端契约——sidebarView 侧面板视图切换与
// pendingReveal 一次性定位信号(openFile reveal 参数)。
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';

vi.mock('../src/lib/api.js', () => ({
  listSessions: vi.fn(async () => []),
  readFile: vi.fn(async () => ({ content: '' })),
  writeFile: vi.fn(async () => ({})),
  getAgentDef: vi.fn(async () => ({})),
  getEvolutionSignals: vi.fn(async () => []),
  getWorkbenchSettings: vi.fn(async () => ({})),
  getWorkbenchSettingsSchema: vi.fn(async () => ({ entries: [] })),
  putWorkbenchSetting: vi.fn(async () => ({})),
}));

import { sidebarView, pendingReveal, openFile, tabs, activePath } from '../src/lib/stores.js';
import { readFile } from '../src/lib/api.js';

// stores 为模块单例,跨用例重置本组涉及的状态
beforeEach(() => {
  tabs.set([]);
  activePath.set(null);
  pendingReveal.set(null);
  sidebarView.set('explorer');
});

describe('sidebarView', () => {
  it('默认视图为 explorer', () => {
    expect(get(sidebarView)).toBe('explorer');
  });
});

describe('openFile reveal 信号', () => {
  it('新开文件带 reveal → 设 pendingReveal(坐标透传)', async () => {
    vi.mocked(readFile).mockResolvedValueOnce({ content: 'a\nb' });
    const err = await openFile('src/lib/a.js', { line: 2, col: 0, endCol: 3 });
    expect(err).toBeNull();
    expect(get(activePath)).toBe('src/lib/a.js');
    expect(get(tabs)).toHaveLength(1);
    expect(get(pendingReveal)).toEqual({ path: 'src/lib/a.js', line: 2, col: 0, endCol: 3 });
  });

  it('新开文件不带 reveal → pendingReveal 保持 null', async () => {
    await openFile('src/b.js');
    expect(get(activePath)).toBe('src/b.js');
    expect(get(pendingReveal)).toBeNull();
  });

  it('已开 tab 带 reveal → 激活并设信号(不重复开 tab)', async () => {
    tabs.set([{ path: 'src/c.js', name: 'c.js', content: '', dirty: false, error: null }]);
    const err = await openFile('src/c.js', { line: 5, col: 1, endCol: 2 });
    expect(err).toBeNull();
    expect(get(activePath)).toBe('src/c.js');
    expect(get(tabs)).toHaveLength(1);
    expect(get(pendingReveal)).toEqual({ path: 'src/c.js', line: 5, col: 1, endCol: 2 });
  });

  it('打开失败 → 加错误占位 tab,不设信号,返回错误消息', async () => {
    vi.mocked(readFile).mockRejectedValueOnce(new Error('boom'));
    const err = await openFile('src/d.js', { line: 1, col: 0, endCol: 1 });
    expect(err).toBe('boom');
    expect(get(tabs)).toHaveLength(1);
    expect(get(tabs)[0].error).toBe('boom');
    expect(get(pendingReveal)).toBeNull();
  });
});
