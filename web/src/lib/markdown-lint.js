// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// Markdown 轻量校验(纯文本逐行扫描,零依赖零 IO):4 规则保守集。
// severity:围栏未闭合=Error(8)/标题跳级与重复=Warning(4)/行尾空白=Hint(1)。
// 输出为诊断单源契约的 marker 形状(行列 1-based),由编辑器以 owner
// 'markdown.lint' 下发;规则彼此独立,单规则命中不中断其余扫描。

import { SEVERITY } from './diagnostics.js';

export const LINT_SOURCE = 'markdown.lint';

/** 扫描长度上限(调用方按 getValueLength 跳过超大文件,防卡顿) */
export const LINT_MAX_LENGTH = 2 * 1024 * 1024;

function mk(line, startCol, endCol, severity, code, message) {
  return {
    startLineNumber: line,
    startColumn: startCol,
    endLineNumber: line,
    endColumn: endCol,
    severity,
    code,
    source: LINT_SOURCE,
    message,
  };
}

const FENCE_RE = /^ {0,3}(`{3,}|~{3,})/;
const HEADING_RE = /^ {0,3}(#{1,6})(?:\s+(.+?))?\s*#*\s*$/;
const TRAILING_RE = /[ \t]+$/;

/** lint(text) → marker[](纯函数,同一输入恒同一输出) */
export function lintMarkdown(text) {
  const markers = [];
  if (typeof text !== 'string' || text === '') return markers;
  const lines = text.split(/\r\n|\r|\n/);
  let open = null; // 开启中的代码围栏:{char, len, line}
  let prevLevel = 0;
  const seen = new Map(); // 标题文本(小写) → 首次出现行

  lines.forEach((line, i) => {
    const lineNo = i + 1;
    const endCol = line.length + 1;

    // 规则 4:行尾空白(Hint,所有行含围栏内)
    const t = line.match(TRAILING_RE);
    if (t) {
      const start = line.length - t[0].length + 1;
      markers.push(mk(lineNo, start, endCol, SEVERITY.Hint, 'trailing-space', '行尾空白'));
    }

    // 规则 3:代码围栏配对状态机(同字符、不短于开启长度才闭合)
    if (open) {
      const closeRe =
        open.char === '`'
          ? new RegExp('^ {0,3}`{' + open.len + ',}\\s*$')
          : new RegExp('^ {0,3}~{' + open.len + ',}\\s*$');
      if (closeRe.test(line)) open = null;
    } else {
      const f = line.match(FENCE_RE);
      if (f) open = { char: f[1][0], len: f[1].length, line: lineNo };
    }

    // 规则 1/2:标题跳级与重复(Warning;仅围栏外)
    if (!open) {
      const h = line.match(HEADING_RE);
      if (h) {
        const level = h[1].length;
        const text = (h[2] || '').trim();
        if (prevLevel && level > prevLevel + 1) {
          markers.push(
            mk(
              lineNo,
              1,
              endCol,
              SEVERITY.Warning,
              'heading-jump',
              `标题跳级:H${prevLevel} 后直接出现 H${level}`,
            ),
          );
        }
        if (text) {
          const key = text.toLowerCase();
          if (seen.has(key)) {
            markers.push(
              mk(
                lineNo,
                1,
                endCol,
                SEVERITY.Warning,
                'heading-duplicate',
                `重复标题:「${text}」(首次出现于第 ${seen.get(key)} 行)`,
              ),
            );
          } else {
            seen.set(key, lineNo);
          }
        }
        prevLevel = level;
      }
    }
  });

  // 围栏到结尾仍未闭合 → Error 落在开启行
  if (open) {
    const line = lines[open.line - 1] || '';
    markers.push(
      mk(open.line, 1, line.length + 1, SEVERITY.Error, 'fence-unclosed', '代码围栏未闭合'),
    );
  }
  return markers;
}
