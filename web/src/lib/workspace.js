// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 会话级工作区管理(E1 配套纯函数 + 批 I 统一切换入口)。
//
// 语义口径(批 I 重锚定型):工作区 = 连接级 ?workspace= 的能力边界根,
// 改值写 localStorage `evo_workspace`。文件面 REST 8 端点按 X-Evo-Workspace
// 头重锚(见 api.js),文件树/编辑/搜索**即刻**随工作区;agent 能力边界仍
// 对新连接生效(新建会话/换线/重连)。未设工作区 = serve workdir(严格零变化)。

import { get } from 'svelte/store';
import { tabs, closeTab, workspaceEpoch } from './stores.js';

export const RECENT_KEY = 'evo_workspace_recent';
export const RECENT_CAP = 5;

/**
 * 工作区输入校验:空串 = 恢复缺省(serve workdir);非空须为绝对路径
 * (Windows 盘符 / UNC / POSIX 根),与对话侧栏既有校验同口径。
 * @returns {{ok: boolean, value: string, error?: string}}
 */
export function validateWorkspaceInput(raw) {
  const v = String(raw ?? '').trim();
  if (!v) return { ok: true, value: '' };
  const isAbs = /^[a-zA-Z]:[\\/]/.test(v) || v.startsWith('\\\\') || v.startsWith('/');
  if (!isAbs) return { ok: false, value: v, error: '需要绝对路径(如 D:\\projects\\demo)' };
  return { ok: true, value: v };
}

/**
 * 最近工作区列表更新:置顶 + 去重 + 容量截断(缺省 5)。
 * 空值不改动;非数组输入容错为空表。
 */
export function updateWorkspaceRecent(list, value, cap = RECENT_CAP) {
  const base = Array.isArray(list) ? list : [];
  const v = String(value || '').trim();
  if (!v) return base.slice(0, cap);
  return [v, ...base.filter((p) => p !== v)].slice(0, cap);
}

export function loadRecentWorkspaces() {
  try {
    const arr = JSON.parse(localStorage.getItem(RECENT_KEY) || '[]');
    return Array.isArray(arr) ? arr.filter((p) => typeof p === 'string') : [];
  } catch {
    return [];
  }
}

export function saveRecentWorkspaces(list) {
  try {
    localStorage.setItem(RECENT_KEY, JSON.stringify(Array.isArray(list) ? list : []));
  } catch {
    /* 存储不可用:会话内有效 */
  }
}

/**
 * fs_events 事件换算锚(批 I):fs watcher 固定盯 serve workdir,事件路径为
 * workdir 相对;树空间为「有效根」相对(工作区 || workdir)。本函数给出
 * workdir → 树空间的换算前缀:
 * - 未设工作区 / 工作区 = workdir → ''(事件空间 == 树空间,直通);
 * - 工作区在 workdir 之下 → workdir 相对子路径(如 'workspace/verify'),
 *   事件路径剥掉该前缀即树路径;
 * - 工作区在 workdir 之外 → null(watcher 不可见,事件整体忽略,
 *   树以手动刷新/重连兜底)。
 * 分段比较(归一 \ → /),Windows 下可忽略大小写。
 */
export function workspaceAnchor(workdir, workspace, { caseInsensitive = false } = {}) {
  if (!workspace) return '';
  const segs = (p) =>
    String(p || '')
      .replace(/[\\/]+/g, '/')
      .replace(/\/+$/g, '')
      .split('/')
      .filter(Boolean);
  const wd = segs(workdir);
  const ws = segs(workspace);
  const eq = (a, b) => (caseInsensitive ? a.toLowerCase() === b.toLowerCase() : a === b);
  if (ws.length < wd.length) return null;
  for (let i = 0; i < wd.length; i++) {
    if (!eq(ws[i], wd[i])) return null;
  }
  if (ws.length === wd.length) return '';
  return ws.slice(wd.length).join('/');
}

/** 显示用末段路径(长目录名收窄) */
export function workspaceLabel(path) {
  const parts = String(path || '').split(/[\\/]/).filter(Boolean);
  return parts.length ? parts[parts.length - 1] : '';
}

/**
 * 工作区统一切换入口(批 I 段 3;Explorer / 首页工作区卡 / 对话侧栏共用):
 * 1. 输入校验(空串 = 恢复缺省);
 * 2. 已打开文件 tab 含未保存修改时先过确认(confirmFn 注入各自确认 UI,
 *    返回 Promise<boolean>;无 dirty 直接通过);
 * 3. 落库 + 最近列表 + 广播 evo:workspace-changed + 关闭全部文件 tab
 *    (文件面即刻重锚,旧空间路径的 tab 一律失效,防错写同名文件);
 * 4. workspaceEpoch 自增(文件树订阅重建)。
 *
 * 返回 {ok, value?, error?, cancelled?}:校验失败 error 非空;用户取消
 * cancelled = true(未落库零副作用)。
 */
export async function switchWorkspace(raw, confirmFn = () => Promise.resolve(true)) {
  const chk = validateWorkspaceInput(raw);
  if (!chk.ok) return { ok: false, error: chk.error };
  const fileTabs = get(tabs).filter((t) => t && !t.kind);
  if (fileTabs.some((t) => t.dirty)) {
    const go = await confirmFn('切换工作区将关闭已打开的文件,未保存的修改将丢失。继续切换?');
    if (!go) return { ok: false, cancelled: true };
  }
  if (chk.value) {
    localStorage.setItem('evo_workspace', chk.value);
    saveRecentWorkspaces(updateWorkspaceRecent(loadRecentWorkspaces(), chk.value));
  } else {
    localStorage.removeItem('evo_workspace');
  }
  try {
    window.dispatchEvent(new CustomEvent('evo:workspace-changed', { detail: { value: chk.value } }));
  } catch {
    /* 非浏览器环境(测试)静默 */
  }
  for (const t of fileTabs) closeTab(t.path);
  workspaceEpoch.update((n) => n + 1);
  return { ok: true, value: chk.value };
}
