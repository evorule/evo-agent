// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect } from 'vitest';
import { lintMarkdown, LINT_SOURCE } from '../src/lib/markdown-lint.js';
import { SEVERITY } from '../src/lib/diagnostics.js';

const codes = (ms) => ms.map((m) => m.code);
const lines = (ms) => ms.map((m) => m.startLineNumber);

describe('lintMarkdown 规则集', () => {
  it('标题跳级:H1 后直接 H3 命中 Warning;正常递进/首标题不命中', () => {
    const bad = lintMarkdown('# A\n\n### B');
    expect(codes(bad)).toEqual(['heading-jump']);
    expect(bad[0].severity).toBe(SEVERITY.Warning);
    expect(bad[0].message).toContain('H1');
    expect(bad[0].message).toContain('H3');
    expect(lintMarkdown('# A\n\n## B\n\n### C')).toHaveLength(0);
    expect(lintMarkdown('### 直接开头')).toHaveLength(0); // 首标题不判跳级
  });

  it('重复标题:同文本(大小写不敏感)二次出现命中;不同文本不命中', () => {
    const bad = lintMarkdown('## 概述\n\n内容\n\n## 概述');
    expect(codes(bad)).toEqual(['heading-duplicate']);
    expect(bad[0].startLineNumber).toBe(5);
    expect(bad[0].message).toContain('第 1 行');
    expect(lintMarkdown('# API\n\n## api')).toHaveLength(1); // 大小写不敏感
    expect(lintMarkdown('## A\n\n## B')).toHaveLength(0);
  });

  it('围栏配对:未闭合 Error 落在开启行;正确闭合不命中;围栏内标题/井号不参与', () => {
    const bad = lintMarkdown('# T\n\n```js\nconst a = 1;\n');
    const fence = bad.filter((m) => m.code === 'fence-unclosed');
    expect(fence).toHaveLength(1);
    expect(fence[0].severity).toBe(SEVERITY.Error);
    expect(fence[0].startLineNumber).toBe(3);

    expect(lintMarkdown('```js\nconst a = 1;\n```')).toHaveLength(0);
    // 围栏内的 # 不是标题(不判跳级/重复),波浪线内的 ## 也不参与
    expect(lintMarkdown('# A\n\n```\n#### 不算标题\n```\n## A')).toEqual([
      expect.objectContaining({ code: 'heading-duplicate' }),
    ]);
    // 闭合围栏必须同字符且不短于开启长度
    expect(lintMarkdown('~~~~\nx\n```')).toHaveLength(1);
    expect(lintMarkdown('~~~~\nx\n~~~~~')).toHaveLength(0);
  });

  it('行尾空白:Hint 级,列区间指向尾部空白;空行/无空白不命中', () => {
    const bad = lintMarkdown('正文  \n下一行\t\n干净');
    expect(codes(bad)).toEqual(['trailing-space', 'trailing-space']);
    expect(bad.every((m) => m.severity === SEVERITY.Hint)).toBe(true);
    expect(bad[0].startColumn).toBe(3); // "正文" 后两个空格
    expect(bad[0].endColumn).toBe(5);
    expect(lines(bad)).toEqual([1, 2]);
  });

  it('多规则并发且不因首错中断:跳级+围栏未闭合+行尾空白同时命中', () => {
    const text = '# A  \n\n### B\n\n```text\n未闭合';
    const ms = lintMarkdown(text);
    expect(codes(ms)).toEqual(['trailing-space', 'heading-jump', 'fence-unclosed']);
    expect(lines(ms)).toEqual([1, 3, 5]);
    expect(ms.every((m) => m.source === LINT_SOURCE)).toBe(true);
  });

  it('边界安全:空串/纯空行/非字符串返回空;CRLF 归一', () => {
    expect(lintMarkdown('')).toHaveLength(0);
    expect(lintMarkdown('\n\n\n')).toHaveLength(0);
    expect(lintMarkdown(null)).toHaveLength(0);
    expect(lintMarkdown(42)).toHaveLength(0);
    expect(codes(lintMarkdown('# A\r\n\r\n### B\r\n'))).toEqual(['heading-jump']);
  });
});
