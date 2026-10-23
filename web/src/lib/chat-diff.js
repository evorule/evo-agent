// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 编辑类工具结果的前端行级 diff(M2):经典 LCS DP,行粒度。
// 收口口径:纯函数 + 调用方提供输入;任一侧行数超帽返回 null,由调用方
// 降级为原文展示(防 O(n·m) 内存膨胀)。后端结构化 diff 另立批次。

/** 行数帽:800×800=64 万格 Int32 表(约 2.5MB),安全上限。 */
const DIFF_LINE_CAP = 800;

/**
 * 从工具卡消息提取 diff 输入(前端解析收口)。
 * args 兼容三种形态:JSON 字符串(ws.js safeJson 存储形态)/对象(历史
 * transcript arguments)/null。仅 edit_file 且 old_string/new_string 均为
 * 字符串时可 diff;其余(file_write 整文件写、历史卡 args 缺失)返回 null。
 * @param {{name?: string, args?: any}} msg 工具卡消息
 * @returns {{oldText: string, newText: string} | null}
 */
export function toolDiffPayload(msg) {
  if (!msg || msg.name !== 'edit_file') return null;
  const raw = msg.args;
  let a = raw;
  if (typeof raw === 'string') {
    try {
      a = JSON.parse(raw);
    } catch {
      return null; // 截断/残缺 JSON → 降级
    }
  }
  if (!a || typeof a !== 'object') return null;
  const o = a.old_string;
  const n = a.new_string;
  if (typeof o !== 'string' || typeof n !== 'string') return null;
  return { oldText: o, newText: n };
}

/**
 * 行级 diff:CRLF 归一 LF 后按行 LCS。输出 del 块在前、add 块在后
 * (标准 diff 观感);任一侧超帽返回 null 交由调用方降级。
 * @param {string} oldText 旧文本
 * @param {string} newText 新文本
 * @returns {Array<{t: 'ctx'|'del'|'add', s: string}> | null}
 */
export function lineDiff(oldText, newText) {
  // 空串视为零行(''.split('\n') 会产生幽灵空行 [''],毁掉「空 new=全删」语义)
  const so = String(oldText ?? '');
  const sn = String(newText ?? '');
  const a = so === '' ? [] : so.replaceAll('\r\n', '\n').split('\n');
  const b = sn === '' ? [] : sn.replaceAll('\r\n', '\n').split('\n');
  if (a.length > DIFF_LINE_CAP || b.length > DIFF_LINE_CAP) return null;
  const n = a.length;
  const m = b.length;
  const w = m + 1;
  // dp[i*w+j] = LCS(a[0..i), b[0..j))
  const dp = new Int32Array((n + 1) * w);
  for (let i = 1; i <= n; i++) {
    for (let j = 1; j <= m; j++) {
      dp[i * w + j] =
        a[i - 1] === b[j - 1]
          ? dp[(i - 1) * w + (j - 1)] + 1
          : Math.max(dp[(i - 1) * w + j], dp[i * w + (j - 1)]);
    }
  }
  // 回溯:相等行取 ctx;否则倾向 add(回溯先 add → 正序即 del 在前)
  const out = [];
  let i = n;
  let j = m;
  while (i > 0 || j > 0) {
    if (i > 0 && j > 0 && a[i - 1] === b[j - 1]) {
      out.push({ t: 'ctx', s: a[i - 1] });
      i -= 1;
      j -= 1;
    } else if (j > 0 && (i === 0 || dp[(i - 1) * w + j] <= dp[i * w + (j - 1)])) {
      out.push({ t: 'add', s: b[j - 1] });
      j -= 1;
    } else {
      out.push({ t: 'del', s: a[i - 1] });
      i -= 1;
    }
  }
  out.reverse();
  return out;
}
