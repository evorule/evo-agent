// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 工作台布局尺寸管理(E2 divider 配套):侧栏宽/底部面板高/对话栏宽的钳制
// 纯函数 + 持久化 store。分组宽百分比换算同理。钳制口径:侧栏 180–480px,
// 面板 120–50vh,对话栏 300–560px,副组 10–80%(与批 E 设计档一致)。
// localStorage 不可用(测试/隐私模式)时降级为会话内布局。

import { writable } from 'svelte/store';

export const SIDEBAR_MIN = 180;
export const SIDEBAR_MAX = 480;
export const SIDEBAR_DEFAULT = 240;
export const SIDEBAR_KEY = 'evo_sidebar_w';

export const PANEL_MIN = 120;
export const PANEL_MAX_VH = 0.5;
export const PANEL_DEFAULT = 180;
export const PANEL_KEY = 'evo_bottom_h';

export const CHAT_MIN = 300;
export const CHAT_MAX = 560;
export const CHAT_DEFAULT = 380;
export const CHAT_KEY = 'evo_chat_w';

export const GROUP_MIN_PCT = 10;
export const GROUP_MAX_PCT = 80;

/** 取整 + 区间钳制(非数值回落 min,保证恒返回有限数) */
export function clampInt(v, min, max) {
  const n = Math.round(Number(v));
  if (!Number.isFinite(n)) return min;
  return Math.min(max, Math.max(min, n));
}

export function clampSidebarWidth(px) {
  return clampInt(px, SIDEBAR_MIN, SIDEBAR_MAX);
}

export function clampChatWidth(px) {
  return clampInt(px, CHAT_MIN, CHAT_MAX);
}

function defaultViewportHeight() {
  try {
    return window.innerHeight;
  } catch {
    return 800;
  }
}

/** 面板高钳制:上限 = max(50% 视口高, 下限)——视口过矮时下限兜底 */
export function clampPanelHeight(px, viewportHeight = defaultViewportHeight()) {
  const max = Math.max(PANEL_MIN, Math.floor(viewportHeight * PANEL_MAX_VH));
  return clampInt(px, PANEL_MIN, max);
}

/** 副组宽换算:起点百分比 + 像素增量/行宽 → 钳制取整百分比
 *  (符号约定:dx>0 = 目标组变宽;调用方按分隔条方位自行取反) */
export function groupPercentFromDrag(startPercent, dx, rowWidth) {
  const fallback = clampInt(startPercent, GROUP_MIN_PCT, GROUP_MAX_PCT);
  const w = Number(rowWidth);
  if (!Number.isFinite(w) || w <= 0) return fallback;
  const next = Number(startPercent) + (Number(dx) / w) * 100;
  if (!Number.isFinite(next)) return fallback;
  return clampInt(next, GROUP_MIN_PCT, GROUP_MAX_PCT);
}

// ---- localStorage 读写(全部容错:测试环境/隐私模式静默降级) ----

function readStoredInt(key, fallback) {
  try {
    const raw = localStorage.getItem(key);
    if (raw == null) return fallback;
    const n = parseInt(raw, 10);
    return Number.isFinite(n) ? n : fallback;
  } catch {
    return fallback;
  }
}

function writeStored(key, v) {
  try {
    localStorage.setItem(key, String(v));
  } catch {
    /* 存储不可用:会话内布局 */
  }
}

/** 侧栏宽度 store(初始化读持久值;钳制后落库) */
export const sidebarWidth = writable(clampSidebarWidth(readStoredInt(SIDEBAR_KEY, SIDEBAR_DEFAULT)));

export function setSidebarWidth(px) {
  const v = clampSidebarWidth(px);
  sidebarWidth.set(v);
  writeStored(SIDEBAR_KEY, v);
  return v;
}

export function hasStoredSidebarWidth() {
  try {
    return localStorage.getItem(SIDEBAR_KEY) != null;
  } catch {
    return false;
  }
}

/** 侧栏宽度 → 根 CSS 变量(各侧栏视图 width: var(--sidebar-w, 各自缺省)) */
export function applySidebarWidth(px) {
  try {
    document.documentElement.style.setProperty('--sidebar-w', `${px}px`);
  } catch {
    /* 非 DOM 环境(测试)静默 */
  }
}

/** 底部面板高度 store(初始化读持久值;钳制后落库) */
export const panelHeight = writable(clampPanelHeight(readStoredInt(PANEL_KEY, PANEL_DEFAULT)));

export function setPanelHeight(px, viewportHeight) {
  const v = clampPanelHeight(px, viewportHeight);
  panelHeight.set(v);
  writeStored(PANEL_KEY, v);
  return v;
}

/** 对话栏宽度 store(初始化读持久值;钳制后落库) */
export const chatWidth = writable(clampChatWidth(readStoredInt(CHAT_KEY, CHAT_DEFAULT)));

export function setChatWidth(px) {
  const v = clampChatWidth(px);
  chatWidth.set(v);
  writeStored(CHAT_KEY, v);
  return v;
}
