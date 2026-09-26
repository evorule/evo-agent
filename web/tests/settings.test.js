// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { get } from 'svelte/store';

vi.mock('../src/lib/api.js', () => ({
  getWorkbenchSettings: vi.fn(),
  getWorkbenchSettingsSchema: vi.fn(),
  putWorkbenchSetting: vi.fn(),
  writeFile: vi.fn(),
}));

import {
  settingsState,
  loadSettings,
  setSetting,
  initialState,
  SETTINGS_CACHE_KEY,
  userLayerDoc,
  buildSettingsSchema,
  saveUserSettingsDoc,
  saveWorkspaceSettingsDoc,
  WORKSPACE_SETTINGS_FILE,
} from '../src/lib/settings.js';
import {
  getWorkbenchSettings,
  getWorkbenchSettingsSchema,
  putWorkbenchSetting,
  writeFile,
} from '../src/lib/api.js';

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

describe('userLayerDoc(用户层文档派生)', () => {
  it('仅保留生效层为 user 的键值', () => {
    const doc = userLayerDoc({
      settings: { 'editor.fontSize': 18, 'editor.minimap': true, 'workbench.theme': 'evorule-dark' },
      sources: { 'editor.fontSize': 'user', 'editor.minimap': 'default', 'workbench.theme': 'workspace' },
    });
    expect(doc).toEqual({ 'editor.fontSize': 18 });
  });

  it('缺 sources/空 settings → 空文档', () => {
    expect(userLayerDoc({ settings: {} })).toEqual({});
    expect(userLayerDoc({})).toEqual({});
  });
});

describe('buildSettingsSchema(schema 条目 → JSON Schema)', () => {
  it('四类型映射 + range/enum + 拒绝未知键', () => {
    const schema = buildSettingsSchema([
      { key: 'editor.fontSize', type: 'number', range: [9, 28], description: '字体大小' },
      { key: 'editor.minimap', type: 'boolean', description: '小地图' },
      { key: 'editor.wordWrap', type: 'enum', enum_values: ['on', 'off'], description: '换行' },
      { key: 'keybindings.overrides', type: 'array', description: '键位覆盖' },
    ]);
    expect(schema.type).toBe('object');
    expect(schema.additionalProperties).toBe(false);
    expect(schema.properties['editor.fontSize']).toEqual({ description: '字体大小', type: 'number', minimum: 9, maximum: 28 });
    expect(schema.properties['editor.minimap'].type).toBe('boolean');
    expect(schema.properties['editor.wordWrap'].enum).toEqual(['on', 'off']);
    expect(schema.properties['keybindings.overrides'].type).toBe('array');
  });

  it('空条目 → 空属性 schema', () => {
    const schema = buildSettingsSchema([]);
    expect(schema.properties).toEqual({});
  });
});

describe('saveUserSettingsDoc(diff 逐键保存)', () => {
  const BASE = JSON.stringify({ 'editor.fontSize': 18, 'editor.minimap': false });

  it('新增/修改/删除/未变 → PUT 序列正确(未变跳过,删除 PUT null)', async () => {
    putWorkbenchSetting.mockResolvedValue({ key: 'x', value: null, source: 'default' });
    const next = JSON.stringify({ 'editor.fontSize': 21, 'editor.tabSize': 4 }); // fontSize 改,tabSize 增,minimap 删
    const err = await saveUserSettingsDoc(next, BASE);
    expect(err).toBeNull();
    const calls = putWorkbenchSetting.mock.calls.map(([k, v]) => [k, v]);
    expect(calls).toEqual([
      ['editor.minimap', null], // 删除键 → 重置
      ['editor.fontSize', 21], // 修改键
      ['editor.tabSize', 4], // 新增键
    ]);
  });

  it('无变更 → 零 PUT', async () => {
    const err = await saveUserSettingsDoc(BASE, BASE);
    expect(err).toBeNull();
    expect(putWorkbenchSetting).not.toHaveBeenCalled();
  });

  it('非法 JSON / 非对象 → 返回错误且零 PUT', async () => {
    expect(await saveUserSettingsDoc('{ broken', BASE)).toMatch(/JSON 解析失败/);
    expect(await saveUserSettingsDoc('[1,2]', BASE)).toMatch(/必须是 JSON 对象/);
    expect(putWorkbenchSetting).not.toHaveBeenCalled();
  });

  it('部分键失败 → 全部尝试+失败清单返回', async () => {
    putWorkbenchSetting.mockImplementation(async (key) => {
      if (key === 'editor.fontSize') throw new Error('HTTP 400');
      return { key, value: null, source: 'default' };
    });
    const next = JSON.stringify({ 'editor.fontSize': 999, 'editor.minimap': true });
    const err = await saveUserSettingsDoc(next, BASE);
    expect(err).toMatch(/1 项保存失败/);
    expect(err).toMatch(/editor\.fontSize/);
    expect(putWorkbenchSetting).toHaveBeenCalledTimes(2); // minimap 成功未中断
  });

  it('成功 → 重拉设置快照', async () => {
    putWorkbenchSetting.mockResolvedValue({ key: 'x', value: null, source: 'default' });
    getWorkbenchSettingsSchema.mockResolvedValue(SCHEMA_RES);
    getWorkbenchSettings.mockResolvedValue(SETTINGS_RES);
    await saveUserSettingsDoc(BASE, BASE);
    expect(getWorkbenchSettings).toHaveBeenCalledTimes(1);
  });
});

describe('saveWorkspaceSettingsDoc(工作区文件直写)', () => {
  it('合法 JSON → writeFile 到工作区设置路径+重拉快照', async () => {
    writeFile.mockResolvedValue({ ok: true });
    getWorkbenchSettingsSchema.mockResolvedValue(SCHEMA_RES);
    getWorkbenchSettings.mockResolvedValue(SETTINGS_RES);
    const err = await saveWorkspaceSettingsDoc('{ "editor.fontSize": 16 }');
    expect(err).toBeNull();
    expect(writeFile).toHaveBeenCalledWith(WORKSPACE_SETTINGS_FILE, '{ "editor.fontSize": 16 }');
    expect(getWorkbenchSettings).toHaveBeenCalledTimes(1);
  });

  it('非法 JSON → 返回错误且不写文件', async () => {
    const err = await saveWorkspaceSettingsDoc('nope');
    expect(err).toMatch(/JSON 解析失败/);
    expect(writeFile).not.toHaveBeenCalled();
  });

  it('writeFile 失败 → 返回错误消息', async () => {
    writeFile.mockRejectedValue(new Error('HTTP 500'));
    const err = await saveWorkspaceSettingsDoc('{}');
    expect(err).toBe('HTTP 500');
  });
});
