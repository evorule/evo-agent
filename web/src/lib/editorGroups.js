// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 分屏副组状态(轻量实现):副组只承载普通文件 tab(与主组共享同一 model,
// 内容/undo 实时同步);diff/产物/设置页留在主组。完整组模型随后续批次。

import { writable, get } from 'svelte/store';

/** 分屏开启态 */
export const splitOpen = writable(false);

/** 副组当前显示的文件路径(null=空态) */
export const splitPath = writable(null);

/** 副组宽度比例(0-1;拖 sash 调整) */
export const splitWidth = writable(0.5);

const SPLIT_KEY = 'evo_split_path';

/** 切换分屏:开=把给定路径(缺省当前激活文件)放进副组;关=收起 */
export function toggleSplit(path) {
  if (get(splitOpen)) {
    splitOpen.set(false);
    return;
  }
  if (path) splitPath.set(path);
  splitOpen.set(true);
}

export function setSplitPath(path) {
  splitPath.set(path);
}

export function closeSplit() {
  splitOpen.set(false);
}

/** 持久化副组路径(会话恢复用;仅路径元数据,零内容) */
export function persistSplit() {
  try {
    if (get(splitOpen)) localStorage.setItem(SPLIT_KEY, get(splitPath) || '');
    else localStorage.removeItem(SPLIT_KEY);
  } catch {
    /* 存储不可用:会话内布局 */
  }
}

export function restoreSplit() {
  try {
    const p = localStorage.getItem(SPLIT_KEY);
    if (p != null) {
      splitOpen.set(true);
      splitPath.set(p || null);
    }
  } catch {
    /* 不可用 */
  }
}
