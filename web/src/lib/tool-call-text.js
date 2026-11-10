// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 工具卡单行摘要纯函数:从调用参数提炼一行关键信息,供回合过程框内工具卡
// 头部展示(答案为主角、过程为安静脚手架的密度分层)。只读入参,零请求
// 零状态;args 兼容 JSON 字符串(实时卡)与对象(历史卡)两种载体。

/** 首行截断:空白折叠 + 超长省略 */
function head(text, cap) {
  const t = String(text).replace(/\s+/g, ' ').trim();
  return t.length > cap ? `${t.slice(0, cap - 1)}…` : t;
}

function firstString(obj, keys) {
  for (const k of keys) {
    const v = obj?.[k];
    if (typeof v === 'string' && v.trim()) return v.trim();
  }
  return null;
}

/**
 * 工具调用摘要(纯函数):命令族取命令首段;路径族取目标路径(编辑类补
 * 增删行数);查询族取 url/query/pattern;兜底取首个标量键值对。取不到
 * 返回空串(调用方不渲染摘要段)。
 */
export function toolCallSummary(name, args, cap = 64) {
  void name;
  let a = args;
  if (typeof a === 'string') {
    try {
      a = JSON.parse(a);
    } catch {
      return head(a, cap);
    }
  }
  if (a == null || typeof a !== 'object' || Array.isArray(a)) return '';
  const cmd = firstString(a, ['command', 'code', 'script', 'cmd']);
  if (cmd) return head(cmd, cap);
  const path = firstString(a, ['path', 'file_path', 'file', 'target', 'to']);
  if (path) {
    if (typeof a.old_string === 'string' && typeof a.new_string === 'string') {
      const add = a.new_string ? a.new_string.split('\n').length : 0;
      const del = a.old_string ? a.old_string.split('\n').length : 0;
      return head(`${path} (+${add}/-${del} 行)`, cap);
    }
    return head(path, cap);
  }
  const q = firstString(a, ['url', 'query', 'pattern', 'q', 'keyword']);
  if (q) return head(q, cap);
  for (const [k, v] of Object.entries(a)) {
    if (typeof v === 'string' && v.trim()) return head(`${k}=${v}`, cap);
    if (typeof v === 'number' || typeof v === 'boolean') return head(`${k}=${v}`, cap);
  }
  return '';
}

// ---- 审批卡结构化预览分派(纯函数):审批请求帧只携带 command 单文本描述,
// 内嵌内容级 diff 不可达;文件族按操作动词+目标路径预览(配「在编辑器查看
// 现状」),其余工具沿用命令文本块。 ----

/** 文件族工具 → 操作动词(中文名) */
const FILE_TOOL_VERBS = {
  file_write: '覆写',
  file_create: '新建',
  file_move: '移动',
  file_delete: '删除',
  search_replace: '编辑',
  edit_file: '编辑',
};

/**
 * 审批卡预览分派(纯函数):文件族返回 {kind:'file', verb, path}(path=command
 * 文本,即目标路径描述);其余返回 {kind:'text'}(沿用命令文本块)。
 */
export function approvalPreview(toolName, command) {
  const verb = FILE_TOOL_VERBS[toolName];
  if (verb) {
    return { kind: 'file', verb, path: String(command ?? '').trim() };
  }
  return { kind: 'text' };
}
