// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 轻量模糊匹配(子序列 + 连续命中加分,自实现不引库)。
// fuzzyMatch 返回命中的字符位置(供面板高亮)与评分(供排序);
// highlightSegments 把位置切分为渲染片段。

/**
 * 模糊匹配。query/text 均按不区分大小写的子序列匹配。
 * 评分:每命中字符 +10;与前一次命中连续 +8;命中于词边界(串首或分隔符后)+6;串首前缀额外 +4。
 * @returns {{score: number, positions: number[]} | null} null = 未命中
 */
export function fuzzyMatch(query, text) {
  if (typeof query !== 'string' || typeof text !== 'string') return null;
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  if (!q) return { score: 0, positions: [] };
  const positions = [];
  let score = 0;
  let prev = -2;
  for (let i = 0; i < q.length; i++) {
    const ch = q[i];
    const start = i === 0 ? 0 : prev + 1;
    const idx = t.indexOf(ch, start);
    if (idx === -1) return null;
    score += 10;
    if (idx === prev + 1) score += 8;
    if (idx === 0 || '/_-., :\\'.includes(t[idx - 1])) score += 6;
    if (i === 0 && idx === 0) score += 4;
    positions.push(idx);
    prev = idx;
  }
  return { score, positions };
}

/**
 * 把命中位置切为高亮片段:[{text, hit}]
 * positions 必须升序(fuzzyMatch 产出保证)。
 */
export function highlightSegments(text, positions) {
  const set = new Set(positions || []);
  const out = [];
  let cur = null;
  for (let i = 0; i < text.length; i++) {
    const hit = set.has(i);
    if (!cur || cur.hit !== hit) {
      cur = { text: text[i], hit };
      out.push(cur);
    } else {
      cur.text += text[i];
    }
  }
  return out;
}
