// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 内置 mention provider — 选区/终端/问题(工作台状态类活引用)。
//
// 三类共同点:引用的是「当前工作台状态」而非可查询实体——
//   selection:发送时重取当前活动编辑器选区(主组取 activePath,副组取组内
//             激活路径;空选区/无 model → 降级丢弃)
//   terminal :按 termId 取 xterm buffer 尾部 N 行(直接读缓冲,不引 serialize
//             addon;实例已销毁 → 降级丢弃)
//   problems :发送时重读诊断聚合快照(ref '*' = 全工作区,否则按文件路径)
// 候选面:三者均不产空查询候选(弹层入口由 composer 空查询快捷项承载);
// 非空查询下终端支持 @<termId 子串> 点名,问题支持 @<路径模糊> 按文件点名。
import { get } from 'svelte/store';
import { activePath } from './stores.js';
import { activeGroupId, getGroup } from './editorGroups.js';
import { getModelForPath } from './modelRegistry.js';
import { problems, SEVERITY } from './diagnostics.js';
import { instances, getActiveTerminal } from './pty.js';
import { getSetting } from './settings.js';
import { registerProvider, xmlBlock } from './mentions.js';
import { fuzzyMatchPath } from './fuzzy.js';

const CANDIDATE_LIMIT = 50;
/** 终端尾部行数缺省(mentions.terminalLines 设置键随后续设置批接入) */
const TERMINAL_LINES_DEFAULT = 200;
/** 单次引用序列化的问题条数上限 */
const PROBLEMS_LIMIT = 50;

const SEV_LABEL = {
  [SEVERITY.Error]: 'error',
  [SEVERITY.Warning]: 'warning',
  [SEVERITY.Info]: 'info',
  [SEVERITY.Hint]: 'hint',
};

// ---- 选区 ----

/** 当前活动编辑器的非空选区;不可得(无活动文件/无 model/空选区)返回 null */
export function getActiveSelection() {
  const gid = get(activeGroupId);
  let path = null;
  if (gid === 'primary') path = get(activePath);
  else path = getGroup(gid)?.activePath || null;
  if (!path) return null;
  const model = getModelForPath(path);
  const sel = model?.getSelection?.();
  if (!model || !sel) return null;
  const empty =
    (sel.startLineNumber ?? 0) === (sel.endLineNumber ?? 0) &&
    (sel.startColumn ?? 0) === (sel.endColumn ?? 0);
  if (empty) return null;
  const text = model.getValueInRange(sel);
  if (!text) return null;
  return { path, text, startLine: sel.startLineNumber, endLine: sel.endLineNumber };
}

const selectionProvider = {
  candidates: async () => [],
  collect: async () => getActiveSelection(),
  serialize(m, c) {
    const n = c.endLine - c.startLine + 1;
    return {
      xml: xmlBlock('selection', { path: c.path, lines: `${c.startLine}-${c.endLine}` }, c.text),
      summary: `${c.path}:${c.startLine}-${c.endLine}(选区 ${n} 行)`,
    };
  },
};

// ---- 终端 ----

/** 当前活动终端 id(pty 活动上报;无活动实例返回 null) */
export function activeTerminalRef() {
  return getActiveTerminal()?.termId || null;
}

async function terminalCandidates(query) {
  const q = String(query || '').trim();
  if (!q) return [];
  const out = [];
  for (const inst of instances.values()) {
    if (!String(inst.termId).includes(q)) continue;
    out.push({
      type: 'terminal',
      ref: inst.termId,
      label: inst.termId,
      detail: `${inst.termId}(${inst.state})`,
      positions: [],
    });
  }
  return out.slice(0, CANDIDATE_LIMIT);
}

/** xterm 缓冲尾部 maxLines 行(尾随空行剔除;全空返回 null) */
export function readTerminalTail(inst, maxLines = TERMINAL_LINES_DEFAULT) {
  const buf = inst?.term?.buffer?.active;
  if (!buf || !Number.isFinite(buf.length)) return null;
  const total = buf.length;
  const lines = [];
  for (let y = total - 1; y >= 0 && lines.length < maxLines; y--) {
    const ln = buf.getLine(y);
    if (!ln) break;
    lines.push(ln.translateToString(true));
  }
  lines.reverse();
  while (lines.length && !lines[lines.length - 1].trim()) lines.pop();
  if (!lines.length) return null;
  return { content: lines.join('\n'), count: lines.length, truncated: total > maxLines };
}

async function collectTerminal(m) {
  const inst = instances.get(m.ref);
  if (!inst) return null;
  const max =
    Number(getSetting('mentions.terminalLines') ?? TERMINAL_LINES_DEFAULT) || TERMINAL_LINES_DEFAULT;
  const tail = readTerminalTail(inst, max);
  if (!tail) return null;
  return { termId: inst.termId, ...tail };
}

function serializeTerminal(m, c) {
  return {
    xml: xmlBlock(
      'terminal_output',
      { term: c.termId, lines: c.count, truncated: c.truncated ? 'true' : undefined },
      c.content,
    ),
    summary: `${c.termId}(终端输出 ${c.count} 行${c.truncated ? ',已截断' : ''})`,
  };
}

const terminalProvider = {
  candidates: terminalCandidates,
  collect: collectTerminal,
  serialize: serializeTerminal,
};

// ---- 问题 ----

/** 工作区诊断总条数(快捷项可用性判定) */
export function problemsTotal() {
  const snap = get(problems);
  return snap.groups.reduce((n, g) => n + g.markers.length, 0);
}

async function problemsCandidates(query) {
  const q = String(query || '').trim();
  if (!q) return [];
  const snap = get(problems);
  const scored = [];
  for (const g of snap.groups) {
    const hit = fuzzyMatchPath(q, g.path);
    if (hit) scored.push({ g, hit });
  }
  scored.sort((a, b) => b.hit.score - a.hit.score || (a.g.path < b.g.path ? -1 : 1));
  return scored.slice(0, CANDIDATE_LIMIT).map(({ g, hit }) => ({
    type: 'problems',
    ref: g.path,
    label: g.path.split('/').pop() || g.path,
    detail: `${g.path}(共 ${g.markers.length} 条)`,
    positions: hit.positions,
  }));
}

/** 诊断条目 → `path:line:col severity message` 行(快照现值;上限 PROBLEMS_LIMIT) */
function problemEntries(ref) {
  const snap = get(problems);
  const groups = ref === '*' ? snap.groups : snap.groups.filter((g) => g.path === ref);
  const out = [];
  for (const g of groups) {
    for (const mk of g.markers) {
      out.push(
        `${g.path}:${mk.startLineNumber || 1}:${mk.startColumn || 1} ${SEV_LABEL[mk.severity] || 'info'} ${mk.message}`,
      );
    }
  }
  return out;
}

async function collectProblems(m) {
  const entries = problemEntries(m.ref);
  if (!entries.length) return null;
  return {
    scope: m.ref,
    text: entries.slice(0, PROBLEMS_LIMIT).join('\n'),
    count: Math.min(entries.length, PROBLEMS_LIMIT),
    truncated: entries.length > PROBLEMS_LIMIT,
  };
}

function serializeProblems(m, c) {
  return {
    xml: xmlBlock(
      'problems',
      {
        scope: c.scope === '*' ? 'workspace' : c.scope,
        count: c.count,
        truncated: c.truncated ? 'true' : undefined,
      },
      c.text,
    ),
    summary: c.scope === '*' ? `工作区问题 ×${c.count}` : `${c.scope}(问题 ×${c.count})`,
  };
}

const problemsProvider = {
  candidates: problemsCandidates,
  collect: collectProblems,
  serialize: serializeProblems,
};

/** 注册状态类 provider(ChatSidebar 挂载时调用;重复调用按注册表语义幂等替换) */
export function registerMentionSourceProviders() {
  registerProvider('selection', selectionProvider);
  registerProvider('terminal', terminalProvider);
  registerProvider('problems', problemsProvider);
}
