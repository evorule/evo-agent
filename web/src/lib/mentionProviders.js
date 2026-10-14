// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 内置 mention provider — 文件/文件夹(候选与内容抓取)+ 发送前对账。
//
// file 与 folder 共用同一 provider 实例(候选按 isDir 各自带 type,弹层按 type
// 分组;collect/serialize 按 mention.type 分支),符合注册表「同实例多类型注册
// 只询问一次」的去重约定。
//
// 文件抓取截断链(发送时按当时实况取材):
//   活动编辑器有选区 → 只带选区
//   无选区但已打开   → 光标邻近 ±CURSOR_WINDOW 行窗口
//   未打开,≤上限    → 全文
//   未打开,>上限    → 大纲降级(LSP 符号清单,无符号取文件首段),标已截断
import { get } from 'svelte/store';
import { readFile, listDir } from './api.js';
import { ensureFileIndex } from './stores.js';
import { registerProvider, xmlBlock } from './mentions.js';
import { fuzzyMatchPath } from './fuzzy.js';
import { getModelForPath } from './modelRegistry.js';
import { requestWorkspaceSymbol, symbolStore } from './lsp/index.js';
import { getSetting } from './settings.js';

const CANDIDATE_LIMIT = 50;
// 文件夹摘要:BFS 深度与条目上限(超出标省略行)
const FOLDER_DEPTH = 2;
const FOLDER_ENTRY_LIMIT = 50;
// 文件截断链参数(mentions.maxFileLines 设置键随后续设置批接入)
const CURSOR_WINDOW = 120;
const FILE_FULL_LIMIT = 600;
const OUTLINE_LIMIT = 100;

/** LSP SymbolKind 数字 → 短标签(大纲行用;未列出的省略标签) */
const SYM_KIND = {
  1: 'file',
  2: 'module',
  3: 'namespace',
  5: 'class',
  6: 'method',
  7: 'property',
  8: 'field',
  9: 'ctor',
  10: 'enum',
  11: 'interface',
  12: 'fn',
  13: 'var',
  14: 'const',
};

// 符号候选:上限与上下文窗口(@符号 = 符号行 ±SYMBOL_CONTEXT 行)
const SYMBOL_LIMIT = 30;
const SYMBOL_CONTEXT = 30;

/** 敏感文件名模式(命中仅标 ⚠ 提示,可见可选不拦) */
const SENSITIVE_RE = /(^|[/\\.])(\.env|id_rsa|credential|secret|token)|\.pem$/i;

/** 路径是否命中敏感文件名(候选标 ⚠;判断口径:名段或扩展名关键词) */
export function isSensitivePath(path) {
  return SENSITIVE_RE.test(String(path || ''));
}

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
  const c = {
    type: e.isDir ? 'folder' : 'file',
    ref: e.path,
    label: e.name,
    detail: e.path,
    positions: [],
  };
  if (!e.isDir && isSensitivePath(e.path)) c.warn = true;
  return c;
}

async function candidates(query) {
  return rankFiles(await ensureFileIndex(), query);
}

async function collect(m) {
  if (m.type === 'folder') return collectFolder(m);
  // 已打开:走编辑器实况(选区 > 光标邻近窗口)
  const model = getModelForPath(m.ref);
  if (model) {
    const sel = model.getSelection?.();
    const selEmpty =
      !sel ||
      ((sel.startLineNumber ?? 0) === (sel.endLineNumber ?? 0) &&
        (sel.startColumn ?? 0) === (sel.endColumn ?? 0));
    if (!selEmpty) {
      const text = model.getValueInRange(sel);
      if (text) {
        return {
          path: m.ref,
          content: text,
          mode: 'selection',
          startLine: sel.startLineNumber,
          endLine: sel.endLineNumber,
        };
      }
    }
    const lineCount = model.getLineCount?.() ?? 0;
    const pos = model.getPosition?.()?.lineNumber ?? 1;
    const from = Math.max(1, pos - CURSOR_WINDOW);
    const to = Math.min(lineCount, pos + CURSOR_WINDOW);
    if (lineCount > 0 && to >= from) {
      const lines = [];
      for (let ln = from; ln <= to; ln++) lines.push(model.getLineContent(ln));
      return {
        path: m.ref,
        content: lines.join('\n'),
        mode: 'window',
        startLine: from,
        endLine: to,
        lineCount,
      };
    }
  }
  // 未打开:读盘,超上限降级大纲
  const res = await readFile(m.ref);
  const content = res.content ?? '';
  const lineCount = content ? content.split('\n').length : 0;
  const max =
    Number(getSetting('mentions.maxFileLines') ?? FILE_FULL_LIMIT) || FILE_FULL_LIMIT;
  if (lineCount > max) return outlineCollect(m.ref, content);
  return { path: m.ref, content, mode: 'full', lineCount };
}

function serialize(m, c) {
  if (m.type === 'folder') {
    return {
      xml: xmlBlock('dir_summary', { path: c.path }, c.tree),
      summary: `${c.path}/(目录摘要)`,
    };
  }
  if (c.mode === 'selection') {
    return {
      xml: xmlBlock(
        'file_content',
        { path: c.path, lines: `${c.startLine}-${c.endLine}`, mode: 'selection' },
        c.content,
      ),
      summary: `${c.path}:${c.startLine}-${c.endLine}(选区)`,
    };
  }
  if (c.mode === 'window') {
    return {
      xml: xmlBlock(
        'file_content',
        { path: c.path, lines: `${c.startLine}-${c.endLine}`, total_lines: c.lineCount, mode: 'cursor_window' },
        c.content,
      ),
      summary: `${c.path} ${c.startLine}-${c.endLine}/${c.lineCount} 行(光标邻近)`,
    };
  }
  if (c.mode === 'outline') {
    return {
      xml: xmlBlock('file_outline', { path: c.path, total_lines: c.lineCount, truncated: 'true' }, c.outline),
      summary: `${c.path}(大纲,已截断,共 ${c.lineCount} 行)`,
    };
  }
  const lines = Number.isFinite(c.lineCount) ? `(${c.lineCount} 行)` : '';
  return { xml: xmlBlock('file_content', { path: c.path }, c.content), summary: `${c.path}${lines}` };
}

/** 大纲降级:优先 LSP documentSymbol 清单,无符号取文件首段;一律标截断 */
function outlineCollect(path, content) {
  const allLines = content ? content.split('\n') : [];
  const syms = get(symbolStore).get(path);
  const lines = [];
  if (Array.isArray(syms) && syms.length) flattenSymbols(syms, 0, lines);
  const outline = lines.length
    ? lines.slice(0, OUTLINE_LIMIT).join('\n')
    : allLines.slice(0, CURSOR_WINDOW).join('\n');
  return { path, mode: 'outline', outline, lineCount: allLines.length };
}

function flattenSymbols(syms, depth, out) {
  if (depth > 3) return;
  for (const s of syms) {
    if (!s || typeof s.name !== 'string') continue;
    const range = s.range || s.location?.range;
    const line = range?.start?.line != null ? `${range.start.line + 1}:` : '';
    const kind = SYM_KIND[s.kind] ? `${SYM_KIND[s.kind]} ` : '';
    out.push(`${line}${'  '.repeat(depth)}${kind}${s.name}`.trim());
    if (Array.isArray(s.children)) flattenSymbols(s.children, depth + 1, out);
  }
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

// ---- 符号 provider(@上下文符号引用) ----

/** symbolStore 存档(DocumentSymbol 树)→ 扁平 {name, kind, path, line} 列表 */
function collectLocalSymbols(syms, path, depth, out) {
  if (depth > 3 || out.length > 500) return;
  for (const s of Array.isArray(syms) ? syms : []) {
    if (!s || typeof s.name !== 'string') continue;
    const range = s.range || s.location?.range;
    const line = range?.start?.line != null ? range.start.line + 1 : 1;
    out.push({ name: s.name, kind: s.kind, path, line });
    if (Array.isArray(s.children)) collectLocalSymbols(s.children, path, depth + 1, out);
  }
}

async function symbolCandidates(query) {
  const q = String(query || '').trim();
  if (!q) return [];
  // 首选:语言门 workspace/symbol 活查询;无可用门 → symbolStore 本地兜底
  let hits = await requestWorkspaceSymbol(q);
  if (!hits) {
    hits = [];
    for (const [path, syms] of get(symbolStore)) collectLocalSymbols(syms, path, 0, hits);
  }
  const scored = [];
  for (const s of hits) {
    const hit = fuzzyMatchPath(q, s.name);
    if (hit) scored.push({ s, hit });
  }
  scored.sort((a, b) => b.hit.score - a.hit.score || (a.s.path < b.s.path ? -1 : 1));
  return scored.slice(0, SYMBOL_LIMIT).map(({ s, hit }) => ({
    type: 'symbol',
    ref: `${s.path}:${s.line}`,
    label: s.name,
    detail: `${s.path}:${s.line}`,
    positions: hit.positions,
  }));
}

/** 符号上下文抓取:ref = `${path}:${line}`,取符号行 ±SYMBOL_CONTEXT 行 */
async function collectSymbol(m) {
  const at = String(m.ref || '').lastIndexOf(':');
  if (at <= 0) return null;
  const path = m.ref.slice(0, at);
  const line = parseInt(m.ref.slice(at + 1), 10);
  if (!Number.isFinite(line) || line < 1) return null;
  // 已打开走编辑器实况(未保存内容可见);否则读盘
  let allLines = null;
  const model = getModelForPath(path);
  if (model?.getLineCount) {
    const n = model.getLineCount();
    allLines = [];
    for (let ln = 1; ln <= n; ln++) allLines.push(model.getLineContent(ln));
  } else {
    const res = await readFile(path);
    allLines = (res.content ?? '') ? res.content.split('\n') : [];
  }
  if (!allLines.length) return null;
  const from = Math.max(1, line - SYMBOL_CONTEXT);
  const to = Math.min(allLines.length, line + SYMBOL_CONTEXT);
  return {
    path,
    content: allLines.slice(from - 1, to).join('\n'),
    startLine: from,
    endLine: to,
    lineCount: allLines.length,
  };
}

function serializeSymbol(m, c) {
  return {
    xml: xmlBlock(
      'symbol_context',
      { path: c.path, lines: `${c.startLine}-${c.endLine}`, total_lines: c.lineCount },
      c.content,
    ),
    summary: `${c.path}:${c.startLine}-${c.endLine}/${c.lineCount}(符号上下文)`,
  };
}

const symbolProvider = {
  candidates: symbolCandidates,
  collect: collectSymbol,
  serialize: serializeSymbol,
};

/** 注册内置 provider(ChatSidebar 挂载时调用;重复调用按注册表语义幂等替换) */
export function registerMentionProviders() {
  registerProvider('file', provider);
  registerProvider('folder', provider);
  registerProvider('symbol', symbolProvider);
}

// ---- 发送前对账(文本 × mentions 元数据) ----

/** 手输 token 按文件 basename 解析:索引内唯一命中才登记(歧义/未命中不猜) */
async function resolveTokenToEntry(name) {
  const index = await ensureFileIndex();
  const hits = index.filter((e) => !e.isDir && e.name === name);
  return hits.length === 1 ? hits[0] : null;
}

/**
 * 发送前对账:composer 文本与 mentions 元数据求交——
 *  1) 元数据里的 token 已被用户从文本删除 → 移除元数据(removed);
 *  2) 文本里未登记的手输 @token → 按文件 basename 解析,唯一命中登记(added);
 *  3) 解析失败保持纯文本随正文发送(不静默丢弃)。
 * @returns {Promise<{kept: Array, removed: Array, added: string[]}>}
 */
export async function reconcileMentions(draft, mentions) {
  const kept = [];
  const removed = [];
  for (const m of Array.isArray(mentions) ? mentions : []) {
    if (m.token && !draft.includes(m.token)) {
      removed.push(m);
      continue;
    }
    kept.push(m);
  }
  const seen = new Set(kept.filter((m) => m.token).map((m) => m.token));
  const added = [];
  const re = /(^|\s)@([^\s@]+)/g;
  let match;
  while ((match = re.exec(draft)) !== null) {
    const token = `@${match[2]}`;
    if (seen.has(token)) continue;
    seen.add(token);
    const entry = await resolveTokenToEntry(match[2]);
    if (entry) {
      kept.push({
        type: entry.isDir ? 'folder' : 'file',
        ref: entry.path,
        insertAt: match.index + match[1].length,
        token,
      });
      added.push(token);
    }
  }
  return { kept, removed, added };
}
