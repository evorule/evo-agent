// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { get } from 'svelte/store';

vi.mock('../src/lib/api.js', () => ({
  getWorkbenchSettings: vi.fn(),
  getWorkbenchSettingsSchema: vi.fn(),
  putWorkbenchSetting: vi.fn(),
}));

import { settingsState, loadSettings, setSetting, initialState, SETTINGS_CACHE_KEY } from '../src/lib/settings.js';
import { getWorkbenchSettings, getWorkbenchSettingsSchema, putWorkbenchSetting } from '../src/lib/api.js';

const SCHEMA_RES = {
  entries: [
    { key: 'editor.fontSize', type: 'number', default: 14, range: [9, 28], category: '编辑器', description: '字体大小', scope: 'window' },
    { key: 'editor.minimap', type: 'boolean', default: false, category: '编辑器', description: '小地图', scope: 'window' },
  ],
};
const SETTINGS_RES = {
  settings: { 'editor.fontSize': 18, 'editor.minimap': false },
  sources: { 'editor.fontSize': 'user', 'editor.minimap': 'default' },
};

function memoryStorage() {
  const m = new Map();
  return {
    getItem: (k) => (m.has(k) ? m.get(k) : null),
    setItem: (k, v) => m.set(k, String(v)),
    removeItem: (k) => m.delete(k),
  };
}

function resetStore() {
  settingsState.set({ entries: [], settings: {}, sources: {}, degraded: false, loaded: false });
}

beforeEach(() => {
  resetStore();
  vi.clearAllMocks();
  globalThis.localStorage = memoryStorage();
});

afterEach(() => {
  delete globalThis.localStorage;
});

describe('initialState(缓存回放)', () => {
  it('无存储 → 空态', () => {
    expect(initialState(null)).toEqual({ entries: [], settings: {}, sources: {}, degraded: false, loaded: false });
  });

  it('有效缓存 → 回放 entries/settings/sources,loaded=false', () => {
    const storage = memoryStorage();
    storage.setItem(
      SETTINGS_CACHE_KEY,
      JSON.stringify({ entries: SCHEMA_RES.entries, settings: SETTINGS_RES.settings, sources: SETTINGS_RES.sources }),
    );
    const s = initialState(storage);
    expect(s.entries).toEqual(SCHEMA_RES.entries);
    expect(s.settings).toEqual(SETTINGS_RES.settings);
    expect(s.sources).toEqual(SETTINGS_RES.sources);
    expect(s.loaded).toBe(false);
    expect(s.degraded).toBe(false);
  });

  it('缓存损坏(JSON 解析失败) → 空态', () => {
    const storage = memoryStorage();
    storage.setItem(SETTINGS_CACHE_KEY, '{ not json !!!');
    expect(initialState(storage).entries).toEqual([]);
  });

  it('缓存缺 sources 字段 → 回放但 sources 为空对象', () => {
    const storage = memoryStorage();
    storage.setItem(
      SETTINGS_CACHE_KEY,
      JSON.stringify({ entries: SCHEMA_RES.entries, settings: { 'editor.fontSize': 18 } }),
    );
    const s = initialState(storage);
    expect(s.settings['editor.fontSize']).toBe(18);
    expect(s.sources).toEqual({});
  });
});

describe('loadSettings', () => {
  it('成功 → store 填充+loaded=true+缓存落盘', async () => {
    getWorkbenchSettingsSchema.mockResolvedValue(SCHEMA_RES);
    getWorkbenchSettings.mockResolvedValue(SETTINGS_RES);
    const ok = await loadSettings();
    expect(ok).toBe(true);
    const s = get(settingsState);
    expect(s.entries).toEqual(SCHEMA_RES.entries);
    expect(s.settings).toEqual(SETTINGS_RES.settings);
    expect(s.sources).toEqual(SETTINGS_RES.sources);
    expect(s.loaded).toBe(true);
    expect(s.degraded).toBe(false);
    const cached = JSON.parse(globalThis.localStorage.getItem(SETTINGS_CACHE_KEY));
    expect(cached.settings).toEqual(SETTINGS_RES.settings);
  });

  it('失败 → degraded=true,保留当前内存态', async () => {
    settingsState.set({
      entries: SCHEMA_RES.entries,
      settings: { 'editor.fontSize': 18 },
      sources: { 'editor.fontSize': 'user' },
      degraded: false,
      loaded: true,
    });
    getWorkbenchSettingsSchema.mockRejectedValue(new Error('down'));
    getWorkbenchSettings.mockRejectedValue(new Error('down'));
    const ok = await loadSettings();
    expect(ok).toBe(false);
    const s = get(settingsState);
    expect(s.degraded).toBe(true);
    expect(s.settings['editor.fontSize']).toBe(18); // 内存态保留(缓存回放场景)
    expect(s.loaded).toBe(true);
  });

  it('响应形态异常(缺字段) → 安全兜底为空值', async () => {
    getWorkbenchSettingsSchema.mockResolvedValue({});
    getWorkbenchSettings.mockResolvedValue({});
    const ok = await loadSettings();
    expect(ok).toBe(true);
    const s = get(settingsState);
    expect(s.entries).toEqual([]);
    expect(s.settings).toEqual({});
  });
});

describe('setSetting', () => {
  beforeEach(() => {
    settingsState.set({
      entries: SCHEMA_RES.entries,
      settings: { ...SETTINGS_RES.settings },
      sources: { ...SETTINGS_RES.sources },
      degraded: false,
      loaded: true,
    });
  });

  it('成功 → 单键就地修补(值+生效层)+缓存同步', async () => {
    putWorkbenchSetting.mockResolvedValue({ key: 'editor.fontSize', value: 21, source: 'user' });
    const res = await setSetting('editor.fontSize', 21);
    expect(res.value).toBe(21);
    const s = get(settingsState);
    expect(s.settings['editor.fontSize']).toBe(21);
    expect(s.sources['editor.fontSize']).toBe('user');
    // 其余键不动
    expect(s.settings['editor.minimap']).toBe(false);
    const cached = JSON.parse(globalThis.localStorage.getItem(SETTINGS_CACHE_KEY));
    expect(cached.settings['editor.fontSize']).toBe(21);
  });

  it('重置(value=null) → 修补为 serve 返回的回落值与层', async () => {
    putWorkbenchSetting.mockResolvedValue({ key: 'editor.fontSize', value: 14, source: 'default' });
    await setSetting('editor.fontSize', null);
    const s = get(settingsState);
    expect(s.settings['editor.fontSize']).toBe(14);
    expect(s.sources['editor.fontSize']).toBe('default');
  });

  it('失败 → 抛错且 store 不变', async () => {
    putWorkbenchSetting.mockRejectedValue(Object.assign(new Error('HTTP 400'), { status: 400 }));
    await expect(setSetting('editor.fontSize', 999)).rejects.toThrow('HTTP 400');
    const s = get(settingsState);
    expect(s.settings['editor.fontSize']).toBe(18); // 回滚语义:调用方回滚控件值
  });
});
