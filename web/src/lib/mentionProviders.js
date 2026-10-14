// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 内置 mention provider — 文件/文件夹(@上下文候选与内容抓取)。
// file 与 folder 共用同一 provider 实例(候选按 isDir 各自带 type,弹层按 type
// 分组;collect/serialize 按 mention.type 分支),符合注册表「同实例多类型注册
// 只询问一次」的去重约定。
import { readFile, listDir } from './api.js';
import { ensureFileIndex } from './stores.js';
import { registerProvider, xmlBlock } from './mentions.js';
import { fuzzyMatchPath } from './fuzzy.js';

const CANDIDATE_LIMIT = 50;
// 文件夹摘要:BFS 深度与条目上限(超出标省略行)
const FOLDER_DEPTH = 2;
const FOLDER_ENTRY_LIMIT = 50;

/** 纯排序:对索引做模糊匹配打分(basename 加权,路径短 tiebreak),截取前 limit。
 *  空查询按路径字典序取前 limit(让位给弹层快捷项) */
export function rankFiles(index, query, limit = CANDIDATE_LIMIT) {
  const q = String(query || '').trim();
  if (!q) {
    return index.slice(0, limit).map(toCandidate);
  }
  const scored = [];
  for (const e of index) {
    const hit = fuzzyMatchPath(q, e.path);
    if (hit) scored.push({ e, hit });
  }
  scored.sort((a, b) => b.hit.score - a.hit.score || (a.e.path < b.e.path ? -1 : 1));
  return scored.slice(0, limit).map(({ e, hit }) => ({ ...toCandidate(e), positions: hit.positions }));
}

function toCandidate(e) {
  return {
    type: e.isDir ? 'folder' : 'file',
    ref: e.path,
    label: e.name,
    detail: e.path,
    positions: [],
  };
}

async function candidates(query) {
  return rankFiles(await ensureFileIndex(), query);
}

async function collect(m) {
  if (m.type === 'folder') return collectFolder(m);
  const res = await readFile(m.ref);
  return { path: m.ref, content: res.content ?? '' };
}

function serialize(m, c) {
  return m.type === 'folder'
    ? {
        xml: xmlBlock('dir_summary', { path: c.path }, c.tree),
        summary: `${c.path}/(目录摘要)`,
      }
    : { xml: xmlBlock('file_content', { path: c.path }, c.content), summary: c.path };
}

/** 文件夹摘要:目标目录 + 一层子目录(FOLDER_DEPTH),条目 FOLDER_ENTRY_LIMIT 上限。
 *  根目录读取失败返回 null(整体降级);子目录失败记占位行不致命 */
async function collectFolder(m) {
  const lines = [];
  let count = 0;
  let rootOk = true;
  const walk = async (dir, depth, prefix) => {
    if (depth > FOLDER_DEPTH) return;
    let res;
    try {
      res = await listDir(dir);
    } catch {
      if (depth === 1) rootOk = false;
      else lines.push(`${prefix}(子目录读取失败)`);
      return;
    }
    const entries = [...(res.entries || [])].sort((a, b) =>
      a.kind === b.kind
        ? a.name.localeCompare(b.name)
        : a.kind === 'dir'
          ? -1
          : 1,
    );
    for (let i = 0; i < entries.length; i++) {
      if (count >= FOLDER_ENTRY_LIMIT) {
        lines.push(`${prefix}…(已截断)`);
        return;
      }
      const e = entries[i];
      const last = i === entries.length - 1;
      lines.push(`${prefix}${last ? '└─ ' : '├─ '}${e.name}${e.kind === 'dir' ? '/' : ''}`);
      count++;
      if (e.kind === 'dir') {
        await walk(dir ? `${dir}/${e.name}` : e.name, depth + 1, `${prefix}${last ? '   ' : '│  '}`);
      }
    }
  };
  await walk(m.ref, 1, '');
  if (!rootOk) return null;
  return { path: m.ref, tree: lines.join('\n') || '(空目录)' };
}

const provider = { candidates, collect, serialize };

/** 注册内置 provider(ChatSidebar 挂载时调用;重复调用按注册表语义幂等替换) */
export function registerMentionProviders() {
  registerProvider('file', provider);
  registerProvider('folder', provider);
}
