// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 快速打开(B1 PR#4):工作区文件索引 + 最近打开文件。
//
// collectFiles 递归走 listDir 目录树,护栏(00-立项方案 裁决 7):
//   - skip 名单:依赖/产物/隐藏目录不入索引
//   - 条目上限 500:大仓库防面板卡顿
//   - 失败静默降级:单层列表失败跳过该层,整体返回已收集部分(面板仍可用 tabs+recent)

import { listDir } from './api.js';

/** 目录名单(目录名匹配,整树跳过) */
export const SKIP_DIRS = new Set([
  'node_modules',
  '.git',
  'dist',
  'build',
  'target',
  'vendor',
  '.next',
  '.svelte-kit',
  'coverage',
  '__pycache__',
  '.venv',
  'venv',
  '.idea',
  '.vscode',
]);

export const MAX_FILES = 500;

export function shouldSkip(name) {
  return SKIP_DIRS.has(name);
}

/**
 * 递归收集工作区文件(相对路径)。
 * @param {(dir?: string) => Promise<{entries: {name: string, kind: string}[]}>} lister 目录列表(依赖注入,单测传 fake)
 * @param {string} [root] 起始目录('' = 根)
 * @returns {Promise<{files: {path: string, name: string}[], truncated: boolean}>}
 */
export async function collectFiles(lister = listDir, root = '') {
  const files = [];
  let truncated = false;
  const walk = async (dir) => {
    if (truncated) return;
    let entries;
    try {
      entries = await lister(dir || undefined);
    } catch {
      return; // 单层失败静默跳过(降级:已收集部分仍可用)
    }
    for (const e of entries.entries || []) {
      if (truncated) return;
      const path = dir ? `${dir}/${e.name}` : e.name;
      if (e.kind === 'dir') {
        if (!shouldSkip(e.name)) await walk(path);
      } else {
        if (files.length >= MAX_FILES) {
          truncated = true;
          return;
        }
        files.push({ path, name: e.name });
      }
    }
  };
  await walk(root);
  return { files, truncated };
}

// ---- 最近打开文件(localStorage 展示层留痕,同 recent commands 口径) ----

export const RECENT_FILES_KEY = 'evo_recent_files';
export const RECENT_FILES_LIMIT = 10;

export function loadRecentFiles() {
  try {
    const raw = JSON.parse(localStorage.getItem(RECENT_FILES_KEY) || '[]');
    return Array.isArray(raw) ? raw.filter((p) => typeof p === 'string' && p).slice(0, RECENT_FILES_LIMIT) : [];
  } catch {
    return [];
  }
}

export function recordRecentFile(path) {
  try {
    const next = [path, ...loadRecentFiles().filter((p) => p !== path)].slice(0, RECENT_FILES_LIMIT);
    localStorage.setItem(RECENT_FILES_KEY, JSON.stringify(next));
  } catch {
    /* 存储不可用时留痕只存活于内存会话 */
  }
}

/** 从文件名提取 path 的末段(根文件与子目录文件统一口径) */
export function nameOf(path) {
  const segs = path.split('/');
  return segs[segs.length - 1];
}
