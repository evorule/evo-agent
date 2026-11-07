// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
/** JSON 结构化视图的纯函数面(解析/摘要/场景契约识别)。
 *
 *  消费方=EditorPane 的结构化视图 toggle(opt-in,不劫持路由——JSON 保持
 *  可编辑)与 StructuredView 组件。数据源=tab 内存 content,零网络。
 *  scene/1.0 契约(视频引擎场景描述)识别出时附场景键卡;识别不出走通用
 *  键摘要。解析失败如实报错(不静默降级为文本)。 */

/** 场景契约标记:任一命中即按 scene/1.0 呈现键卡 */
const SCENE_MARKERS = ['scene/1.0'];

/**
 * 解析 JSON 文本。
 * @returns {{ok: true, value: object} | {ok: false, error: string}}
 */
export function parseJson(text) {
  try {
    return { ok: true, value: JSON.parse(text) };
  } catch (e) {
    return { ok: false, error: String(e?.message || e) };
  }
}

/** 是否场景契约(显式 version/v 标记或 objects+timeline 组合特征) */
export function isSceneContract(value) {
  if (!value || typeof value !== 'object') return false;
  const marker = value.version ?? value.v;
  if (typeof marker === 'string' && SCENE_MARKERS.some((m) => marker.startsWith(m))) return true;
  return Array.isArray(value.objects) && Array.isArray(value.timeline);
}

/**
 * 顶层摘要卡数据:通用键计数;scene 契约提取 fps/duration/camera/objects。
 * @returns {{ kind: 'scene'|'generic', cards: {label: string, value: string}[] }}
 */
export function summarize(value) {
  const cards = [];
  if (isSceneContract(value)) {
    cards.push({ label: '契约', value: String(value.version ?? value.v ?? 'scene/1.0') });
    if (value.fps !== undefined) cards.push({ label: 'fps', value: String(value.fps) });
    if (value.duration !== undefined) cards.push({ label: 'duration', value: `${value.duration}s` });
    if (value.camera) {
      const c = value.camera;
      cards.push({
        label: 'camera',
        value: c.pos ? `[${c.pos.join(', ')}]` : '—',
      });
    }
    cards.push({
      label: 'objects',
      value: `${Array.isArray(value.objects) ? value.objects.length : 0} 项`,
    });
    cards.push({
      label: 'timeline',
      value: `${Array.isArray(value.timeline) ? value.timeline.length : 0} 条`,
    });
    return { kind: 'scene', cards };
  }
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    const keys = Object.keys(value);
    cards.push({ label: '顶层键', value: `${keys.length} 个` });
    for (const k of keys.slice(0, 6)) {
      const v = value[k];
      const shape = Array.isArray(v)
        ? `数组(${v.length})`
        : v && typeof v === 'object'
          ? '对象'
          : typeof v === 'string'
            ? `"${v.length > 24 ? v.slice(0, 24) + '…' : v}"`
            : String(v);
      cards.push({ label: k, value: shape });
    }
    return { kind: 'generic', cards };
  }
  cards.push({ label: '类型', value: Array.isArray(value) ? `数组(${value.length})` : typeof value });
  return { kind: 'generic', cards };
}

/**
 * 折叠树节点(深度受限;超深/超广以占位标记,防大文件撑爆 DOM)。
 * @returns {{ name: string, type: string, preview: string, children: node[]|null, truncated: boolean }}
 */
const MAX_DEPTH = 6;
const MAX_CHILDREN = 24;

export function buildTree(value, name = 'root', depth = 0) {
  const type = Array.isArray(value) ? 'array' : value === null ? 'null' : typeof value;
  if (depth >= MAX_DEPTH || value === null || typeof value !== 'object') {
    const preview =
      typeof value === 'string'
        ? `"${value.length > 60 ? value.slice(0, 60) + '…' : value}"`
        : String(value);
    return { name, type, preview, children: null, truncated: false };
  }
  const entries = Array.isArray(value)
    ? value.map((v, i) => [String(i), v])
    : Object.entries(value);
  const truncated = entries.length > MAX_CHILDREN;
  const children = entries.slice(0, MAX_CHILDREN).map(([k, v]) => buildTree(v, k, depth + 1));
  return { name, type, preview: `${type}(${entries.length})`, children, truncated };
}
