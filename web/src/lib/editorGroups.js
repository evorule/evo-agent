// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 编辑器组分屏状态(v1 横向组列):主组(EditorPane,全能力)+ 至多 2 个副组
// (GroupPane,普通文件 tab)——合计 3 组上限(设计档 §一)。
//
// 副组只登记「组成员路径 + 组内激活路径 + 宽度」;文件内容/dirty 全局态仍在
// stores.js 的 tabs(同路径多组引用共享同一 model 与内容)。关闭语义:组内关
// tab 只摘本组成员;全局最后一个引用才真正关 tab(防孤儿 model)。

import { writable, get } from 'svelte/store';
import { tabs, openFile, closeTab } from './stores.js';

/** 副组列表:[{id, paths:[], activePath, width}](width 为百分比字符串) */
export const secondaryGroups = writable([]);

/** 焦点组 id('primary' | 副组 id) */
export const activeGroupId = writable('primary');

/** 焦点切换信号(GroupPane 订阅聚焦自己的编辑器):n 每次自增 */
export const groupFocusSignal = writable({ id: '', n: 0 });

/** 最大化组 id(null = 正常布局;toggle 语义) */
export const maximizedGroupId = writable(null);

export function toggleMaximize(id) {
  maximizedGroupId.update((cur) => (cur === id ? null : id || null));
}

/** 副组上限(主组 + 2 副组 = 3 组) */
export const MAX_SECONDARY = 2;

const PERSIST_KEY = 'evo_editor_groups_v2';
let seq = 0;

/** 副组清单变更触发渲染(内容本体在各组路径映射里) */
export const groupListVersion = writable(0);

function bump() {
  groupListVersion.update((n) => n + 1);
}

function setGroups(groups) {
  secondaryGroups.set(groups);
  persistGroups();
  bump();
  refreshSplitOpen();
}

/** 新建副组;已达上限返回 null(调用方提示「已达最大组数」) */
export function addSecondaryGroup(path) {
  const groups = get(secondaryGroups);
  if (groups.length >= MAX_SECONDARY) return null;
  seq += 1;
  const id = `split-${seq}`;
  const width = `${Math.round(100 / (groups.length + 2))}%`;
  setGroups([...groups, { id, paths: path ? [path] : [], activePath: path || null, width }]);
  activeGroupId.set(id);
  return id;
}

export function getGroup(id) {
  return get(secondaryGroups).find((g) => g.id === id) || null;
}

/** 组内打开路径:入组成员+置激活 */
export function groupOpenPath(id, path) {
  setGroups(
    get(secondaryGroups).map((g) =>
      g.id === id
        ? { ...g, paths: g.paths.includes(path) ? g.paths : [...g.paths, path], activePath: path }
        : g,
    ),
  );
}

/** 组内关闭路径:摘成员+激活回落;返回 {groupDied, wasLastRef}
 *  (wasLastRef=无其他组引用该路径,调用方需走全局 closeTab 真正关 tab) */
export function groupClosePath(id, path) {
  const groups = get(secondaryGroups);
  const target = groups.find((g) => g.id === id);
  if (!target) return { groupDied: false, wasLastRef: true };
  const otherRef = groups.some((g) => g.id !== id && g.paths.includes(path));
  let rest = groups
    .map((g) =>
      g.id === id
        ? {
            ...g,
            paths: g.paths.filter((p) => p !== path),
            activePath: g.activePath === path ? g.paths.filter((p) => p !== path).slice(-1)[0] || null : g.activePath,
          }
        : g,
    )
    .map((g) => ({ ...g, activePath: g.activePath || g.paths[g.paths.length - 1] || null }));
  const died = rest.find((g) => g.id === id).paths.length === 0;
  if (died) rest = rest.filter((g) => g.id !== id);
  setGroups(rest);
  return { groupDied: died, wasLastRef: !otherRef };
}

/** 关闭整个副组(组内路径交调用方处置) */
export function closeSecondaryGroup(id) {
  setGroups(get(secondaryGroups).filter((g) => g.id !== id));
  if (get(activeGroupId) === id) activeGroupId.set('primary');
}

/** tab 移动目标计算:主组激活 tab → 第一个副组(无则建,超限 null);
 *  副组激活 tab → 主组。返回 {path, fromId, toId} 或 null。 */
export function moveActiveTabTarget(primaryActivePath) {
  const groups = get(secondaryGroups);
  const active = get(activeGroupId);
  if (active === 'primary') {
    if (!primaryActivePath) return null;
    let target = groups[0];
    if (!target) {
      const id = addSecondaryGroup(null);
      if (!id) return null;
      target = get(secondaryGroups).find((g) => g.id === id);
    }
    return { path: primaryActivePath, fromId: 'primary', toId: target.id };
  }
  const g = groups.find((x) => x.id === active);
  if (!g || !g.activePath) return null;
  return { path: g.activePath, fromId: active, toId: 'primary' };
}

/** 组内摘路径(移动的源侧;激活回落) */
export function groupRemovePath(id, path) {
  setGroups(
    get(secondaryGroups).map((g) =>
      g.id === id
        ? {
            ...g,
            paths: g.paths.filter((p) => p !== path),
            activePath: g.activePath === path ? g.paths.filter((p) => p !== path).slice(-1)[0] || null : g.activePath,
          }
        : g,
    ),
  );
}

/** 拖拽调宽(E2 divider):按 id 设定副组宽度(百分比字符串);持久化随 setGroups */
export function setGroupWidth(id, width) {
  setGroups(get(secondaryGroups).map((g) => (g.id === id ? { ...g, width } : g)));
}

/** 均分副组宽度(主组 flex:1 占余;N 副+主 → 每副 1/(N+1)) */
export function evenWidths() {
  const groups = get(secondaryGroups);
  if (!groups.length) return;
  const w = `${Math.round(100 / (groups.length + 1))}%`;
  setGroups(groups.map((g) => ({ ...g, width: w })));
}

// ---- H1 tab 拖拽:组内重排 + 落点分发 ----

/** 纯重排:摘 fromIndex 元素插到 toIndex(越界收拢到有效区间;同位返回副本) */
export function reorderedPaths(list, fromIndex, toIndex) {
  if (!Array.isArray(list)) return list;
  const out = list.slice();
  if (out.length === 0) return out;
  const from = Math.max(0, Math.min(fromIndex, out.length - 1));
  const to = Math.max(0, Math.min(toIndex, out.length - 1));
  if (from === to) return out;
  const [item] = out.splice(from, 1);
  out.splice(to, 0, item);
  return out;
}

/** 主组 tab 面重排(主组无 paths 成员概念,顺序 = 全局 tabs 顺序;不动激活) */
export function reorderTab(path, toIndex) {
  if (!path) return;
  tabs.update((list) => {
    const from = list.findIndex((t) => t.path === path);
    if (from === -1) return list;
    const to = Number.isInteger(toIndex) ? toIndex : list.length - 1;
    return reorderedPaths(list, from, to);
  });
}

/** 副组组内 tab 重排(不改激活态;path 不在组内为无操作) */
export function moveTabWithinSecondary(id, path, toIndex) {
  setGroups(
    get(secondaryGroups).map((g) => {
      if (g.id !== id) return g;
      const from = g.paths.indexOf(path);
      if (from === -1) return g;
      const to = Math.max(0, Math.min(Number.isInteger(toIndex) ? toIndex : g.paths.length - 1, g.paths.length - 1));
      if (to === from) return g;
      return { ...g, paths: reorderedPaths(g.paths, from, to) };
    }),
  );
}

/** tab 拖拽落点分发(主组/副组统一入口):同组重排;跨组移动语义与
 *  view.moveTabToNextGroup 命令一致——主组→副组 = 副组登记 + 主组关 tab;
 *  副组→主组 = 摘成员 + 全局激活;副组→副组 = 摘源 + 目标组打开+落位。 */
export function dispatchTabDrop(fromId, path, toId, toIndex) {
  if (!path || !toId) return;
  if (fromId === toId) {
    if (toId === 'primary') reorderTab(path, toIndex);
    else moveTabWithinSecondary(toId, path, toIndex);
    return;
  }
  if (fromId === 'primary') {
    groupOpenPath(toId, path);
    closeTab(path);
    return;
  }
  groupRemovePath(fromId, path);
  if (toId === 'primary') {
    openFile(path);
  } else {
    groupOpenPath(toId, path);
    moveTabWithinSecondary(toId, path, toIndex);
  }
}

// ---- 持久化(组结构+成员路径+激活 id;仅路径元数据,零内容/零敏感) ----

export function persistGroups() {
  try {
    localStorage.setItem(
      PERSIST_KEY,
      JSON.stringify({ groups: get(secondaryGroups), active: get(activeGroupId), seq }),
    );
  } catch {
    /* 存储不可用:会话内布局 */
  }
}

/** 恢复(挂载时调用一次);返回副组数(0 = 无恢复)。路径内容由视图层懒取 */
export function restoreGroups() {
  try {
    const raw = JSON.parse(localStorage.getItem(PERSIST_KEY) || 'null');
    if (raw && Array.isArray(raw.groups)) {
      const groups = raw.groups.filter((g) => g && Array.isArray(g.paths));
      for (const g of groups) {
        const n = parseInt(String(g.id).replace('split-', ''), 10);
        if (Number.isFinite(n) && n > seq) seq = n;
      }
      secondaryGroups.set(groups);
      activeGroupId.set(raw.active || 'primary');
      bump();
      refreshSplitOpen();
      return groups.length;
    }
  } catch {
    /* 损坏视为无 */
  }
  return 0;
}

// ---- 兼容面:分屏开关语义(按钮/旧命令消费) ----

/** 任一副组存在即视为「分屏开」 */
export const splitOpen = writable(false);

function refreshSplitOpen() {
  splitOpen.set(get(secondaryGroups).length > 0);
}

/** 分屏开关:无副组→以给定路径新建副组;有副组→关最后一个 */
export function toggleSplit(path) {
  const groups = get(secondaryGroups);
  if (groups.length > 0) {
    closeSecondaryGroup(groups[groups.length - 1].id);
    return null;
  }
  return addSecondaryGroup(path);
}

/** 关闭全部分屏副组 */
export function closeSplit() {
  for (const g of get(secondaryGroups)) closeSecondaryGroup(g.id);
}
