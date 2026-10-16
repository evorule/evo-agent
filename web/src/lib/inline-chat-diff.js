// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 行级 LCS diff(Inline Chat 预览用简版):O(NM) 动态规划,函数级片段量级
// 可接受(设计档风险 3:预览仅语义,Accept 以 applyEdits 全文替换为准)。

/** 行级 diff:[{type:'same'|'del'|'add', text, oldLine?, newLine?}] */
export function diffLines(oldText, newText) {
  const a = oldText.split('\n');
  const b = newText.split('\n');
  const n = a.length;
  const m = b.length;
  // DP 表(函数级片段 <500 行,内存可控;>2000 行直接全量替换语义)
  if (n * m > 4_000_000) {
    return [
      { type: 'del', text: '' },
      { type: 'add', text: newText },
    ];
  }
  const dp = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const out = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push({ type: 'same', text: a[i], oldLine: i + 1, newLine: j + 1 });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      out.push({ type: 'del', text: a[i], oldLine: i + 1 });
      i++;
    } else {
      out.push({ type: 'add', text: b[j], newLine: j + 1 });
      j++;
    }
  }
  while (i < n) out.push({ type: 'del', text: a[i], oldLine: i + 1 });
  while (j < m) out.push({ type: 'add', text: b[j], newLine: j + 1 });
  return out;
}

/** 从 diff 提取「删除行号集合」与「新增行块」(预览渲染消费) */
export function summarizeDiff(rows) {
  const deletedLines = [];
  const addedBlocks = [];
  let cur = null;
  for (const r of rows) {
    if (r.type === 'del') deletedLines.push(r.oldLine);
    if (r.type === 'add') {
      if (!cur) {
        cur = { afterLine: r.newLine - 1, lines: [] };
        addedBlocks.push(cur);
      } else {
        cur.afterLine = r.newLine - 1; // 连续 add:块尾随行进推进
      }
      cur.lines.push(r.text);
    }
  }
  return { deletedLines, addedBlocks };
}
