// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 工作台设置 store:schema 注册表 + 合并设置 + 生效层的单一前端快照。
//
// serve 是 schema 与合并序的单源(见 serve 端 settings 模块);前端零合并逻辑。
// localStorage['evo_settings_cache'] 承担两件事:
//   1) 启动首帧同步回放(编辑器创建参数取缓存值,避免异步到达后的闪变);
//   2) serve 不可达时降级为「缓存快照 + 只读」(degraded 标志,UI 顶部黄条提示)。
// 缓存不含身份类信息,与既有身份键(evo_agent_token 等)互不混用。

import { writable, get } from 'svelte/store';
import {
  getWorkbenchSettings,
  getWorkbenchSettingsSchema,
  putWorkbenchSetting,
  writeFile,
} from './api.js';

/** 缓存镜像键(降级回放;仅 schema/设置/生效层,无凭据) */
export const SETTINGS_CACHE_KEY = 'evo_settings_cache';

/** store 形态:{ entries, settings, sources, degraded, loaded }
 *  - entries: schema 条目数组(serve 下发)
 *  - settings: 合并后扁平键值对象(如 { 'editor.fontSize': 14 })
 *  - sources: 每键生效层 'default' | 'user' | 'workspace'
 *  - degraded: true = serve 不可达,缓存回放只读
 *  - loaded: 是否完成过至少一次在线加载 */
export const settingsState = writable(initialState());

/** 拉取 schema+合并设置(并行);失败降级:保留当前内存态并标记只读。 */
export async function loadSettings() {
  try {
    const [schemaRes, settingsRes] = await Promise.all([
      getWorkbenchSettingsSchema(),
      getWorkbenchSettings(),
    ]);
    const next = {
      entries: Array.isArray(schemaRes?.entries) ? schemaRes.entries : [],
      settings: settingsRes?.settings ?? {},
      sources: settingsRes?.sources ?? {},
      degraded: false,
      loaded: true,
    };
    settingsState.set(next);
    persistCache(next);
    return true;
  } catch {
    settingsState.update((s) => ({ ...s, degraded: true }));
    return false;
  }
}

/**
 * 写单键(PUT;value=null 表示重置)。成功后用 serve 返回的写后合并值与
 * 生效层就地修补 store(PUT 响应即权威,免一次 GET;JSON 全量编辑场景走
 * loadSettings 重拉)。失败抛错由调用方 toast+回滚控件值。
 */
export async function setSetting(key, value) {
  const res = await putWorkbenchSetting(key, value);
  settingsState.update((s) => {
    const next = {
      ...s,
      settings: { ...s.settings, [key]: res.value },
      sources: { ...s.sources, [key]: res.source },
      degraded: false,
      loaded: true,
    };
    persistCache(next);
    return next;
  });
  return res;
}

/** 读某键当前合并值(快照;响应式场景请订阅 store) */
export function getSetting(key) {
  return get(settingsState).settings[key];
}

/** 读某键当前生效层 */
export function getSettingSource(key) {
  return get(settingsState).sources[key];
}

// ---- 缓存回放与持久化 ----

function ls() {
  return typeof localStorage === 'undefined' ? null : localStorage;
}

/** 从存储恢复初始态(模块装载时同步执行一次;无存储/缓存损坏 → 空态)。 */
export function initialState(storage = ls()) {
  if (!storage) return emptyState();
  try {
    const raw = JSON.parse(storage.getItem(SETTINGS_CACHE_KEY) || 'null');
    if (
      raw &&
      typeof raw === 'object' &&
      Array.isArray(raw.entries) &&
      raw.settings &&
      typeof raw.settings === 'object'
    ) {
      return {
        entries: raw.entries,
        settings: raw.settings,
        sources: raw.sources && typeof raw.sources === 'object' ? raw.sources : {},
        degraded: false,
        loaded: false,
      };
    }
  } catch {
    /* 损坏缓存视为无缓存 */
  }
  return emptyState();
}

function emptyState() {
  return { entries: [], settings: {}, sources: {}, degraded: false, loaded: false };
}

function persistCache(state) {
  const storage = ls();
  if (!storage) return;
  try {
    storage.setItem(
      SETTINGS_CACHE_KEY,
      JSON.stringify({ entries: state.entries, settings: state.settings, sources: state.sources }),
    );
  } catch {
    /* 写失败静默(缓存是尽力而为的加速/降级层) */
  }
}

// ---- JSON 双模式(设置 JSON 文档编辑) ----
//
// user 模式:文档 = user 层键值集(生效层标注为 user 的键),保存 = 与打开时快照
// 做 diff → 逐键 PUT(删除键 = PUT null 重置),不新增端点。
// workspace 模式:文档 = <workdir>/.evo/settings.json 直读直写(files 通道)。

/** 工作区层设置文件路径(files 通道读写) */
export const WORKSPACE_SETTINGS_FILE = '.evo/settings.json';

/** 从设置快照派生 user 层 JSON 文档对象(仅生效层为 user 的键值) */
export function userLayerDoc(state) {
  const out = {};
  const sources = state?.sources || {};
  for (const [k, v] of Object.entries(state?.settings || {})) {
    if (sources[k] === 'user') out[k] = v;
  }
  return out;
}

/** schema 条目 → 标准 JSON Schema(Monaco jsonDefaults 挂载用;仅 properties 面) */
export function buildSettingsSchema(entries) {
  const properties = {};
  for (const e of entries || []) {
    const prop = { description: e.description || e.key };
    if (e.type === 'number') {
      prop.type = 'number';
      if (Array.isArray(e.range) && e.range.length === 2) {
        prop.minimum = e.range[0];
        prop.maximum = e.range[1];
      }
    } else if (e.type === 'enum') {
      prop.type = 'string';
      prop.enum = Array.isArray(e.enum_values) ? e.enum_values : [];
    } else if (e.type === 'boolean') {
      prop.type = 'boolean';
    } else if (e.type === 'array') {
      prop.type = 'array';
    } else {
      prop.type = 'string';
    }
    properties[e.key] = prop;
  }
  return { type: 'object', properties, additionalProperties: false };
}

function parseObject(text, fallbackErrorPrefix) {
  let doc;
  try {
    doc = JSON.parse(text);
  } catch (e) {
    return { error: `${fallbackErrorPrefix}:${String(e?.message || e)}` };
  }
  if (!doc || typeof doc !== 'object' || Array.isArray(doc)) {
    return { error: `${fallbackErrorPrefix}:设置文档必须是 JSON 对象` };
  }
  return { doc };
}

/**
 * 保存 user 层 JSON 文档(text)相对打开时快照(baselineText)的变更:
 * 删除键 → PUT null 重置;新增/变更键 → PUT 值。全部尝试后返回失败清单,
 * 全部成功则重拉设置快照保证 sources 一致。返回错误消息或 null。
 */
export async function saveUserSettingsDoc(text, baselineText) {
  const { doc, error } = parseObject(text, 'JSON 解析失败');
  if (error) return error;
  const { doc: base, error: baseError } = parseObject(baselineText || '{}', '基线解析失败');
  if (baseError) return baseError;

  const failures = [];
  for (const k of Object.keys(base)) {
    if (k in doc) continue;
    try {
      await setSetting(k, null);
    } catch (e) {
      failures.push(`${k}:${String(e?.message || e)}`);
    }
  }
  for (const [k, v] of Object.entries(doc)) {
    if (k in base && JSON.stringify(base[k]) === JSON.stringify(v)) continue;
    try {
      await setSetting(k, v);
    } catch (e) {
      failures.push(`${k}:${String(e?.message || e)}`);
    }
  }
  if (failures.length > 0) return `${failures.length} 项保存失败 — ${failures.join('; ')}`;
  await loadSettings();
  return null;
}

/** 保存 workspace 层 JSON 文档(整体直写工作区设置文件)。返回错误消息或 null */
export async function saveWorkspaceSettingsDoc(text) {
  const { error } = parseObject(text, 'JSON 解析失败');
  if (error) return error;
  try {
    await writeFile(WORKSPACE_SETTINGS_FILE, text);
  } catch (e) {
    return String(e?.message || e);
  }
  await loadSettings();
  return null;
}
