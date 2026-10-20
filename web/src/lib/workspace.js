// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 会话级工作区管理(E1 配套纯函数):输入校验 / 最近列表 / 文件树锚点换算。
//
// 语义口径(与 C 批定型一致):工作区 = 连接级 ?workspace= 的能力边界根,
// 改值写 localStorage `evo_workspace`,对下次连接(新建会话/换线/重连)生效。
// REST 文件面以 serve workdir 为根(绝对路径拒绝,安全红线不松),因此文件树
// 仅当工作区位于 workdir 之下时锚定其相对子目录(workspaceAnchor),否则保持
// workdir 根(agent 边界仍随新连接生效,提示条明示)。

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
 * 文件树锚点换算:工作区在 workdir 之下 → 返回 workdir 相对子路径
 * (如 'workspace/verify');工作区 = workdir 本身或未设置 → ''(根);
 * 工作区在 workdir 之外 → null(树保持 workdir 根)。
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
