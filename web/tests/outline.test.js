// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// H3 大纲:JSON 词法扫描(键定位)与 navTree 扁平化(纯函数)
import { describe, it, expect } from 'vitest';
import { jsonSymbols, navTreeToSymbols, symbolGlyph } from '../src/lib/outline.js';

describe('jsonSymbols(JSON 键定位)', () => {
  it('单层键:行列(1-based)与深度', () => {
    // 1:{ 2:"a" 5:: 6:1 7:, 8:"b"
    expect(jsonSymbols('{"a":1,"b":2}')).toEqual([
      { name: 'a', kind: 'key', depth: 0, line: 1, col: 2 },
      { name: 'b', kind: 'key', depth: 0, line: 1, col: 8 },
    ]);
  });

  it('嵌套对象:深度随层级', () => {
    // 1:{ 2:"a" 5:: 6:{ 7:"b"
    expect(jsonSymbols('{"a":{"b":1}}')).toEqual([
      { name: 'a', kind: 'key', depth: 0, line: 1, col: 2 },
      { name: 'b', kind: 'key', depth: 1, line: 1, col: 7 },
    ]);
  });

  it('多行文本:键行号随换行推进', () => {
    const syms = jsonSymbols('{\n  "name": "x",\n  "items": {\n    "deep": true\n  }\n}');
    expect(syms.map((s) => [s.name, s.line, s.depth])).toEqual([
      ['name', 2, 0],
      ['items', 3, 0],
      ['deep', 4, 1],
    ]);
  });

  it('数组内对象:键照常收集', () => {
    // 1=[ 2=1 3=, 4={ 5="x"
    expect(jsonSymbols('[1,{"x":2}]')).toEqual([
      { name: 'x', kind: 'key', depth: 0, line: 1, col: 5 },
    ]);
  });

  it('转义字符串:键名解码,收尾引号不误判', () => {
    // raw 键 = "a\nb"(字面反斜杠),JSON.parse 后解码为换行
    expect(jsonSymbols('{"a\\nb":1}')[0].name).toBe('a\nb');
    expect(jsonSymbols('{"say \\"hi\\": ok":1}')[0].name).toBe('say "hi": ok');
  });

  it('值字符串中的冒号/花括号不产生键', () => {
    expect(jsonSymbols('{"a":"x:y{z}"}')).toEqual([
      { name: 'a', kind: 'key', depth: 0, line: 1, col: 2 },
    ]);
  });

  it('残缺 JSON:尽力返回已扫出的键,不抛错', () => {
    expect(jsonSymbols('{"a": ')).toEqual([{ name: 'a', kind: 'key', depth: 0, line: 1, col: 2 }]);
    // 值字符串未闭合:键 'a' 已扫出,值丢弃
    expect(jsonSymbols('{"a": "未闭合')).toEqual([
      { name: 'a', kind: 'key', depth: 0, line: 1, col: 2 },
    ]);
    expect(jsonSymbols('')).toEqual([]);
  });
});

describe('navTreeToSymbols(ts 导航树扁平化)', () => {
  const positionAt = (offset) => ({ lineNumber: offset + 1, column: offset + 1 });
  const tree = {
    childItems: [
      { text: 'mod.js', kind: 'script', spans: [{ start: 0, length: 3 }], childItems: [] },
      {
        text: 'Foo',
        kind: 'class',
        spans: [{ start: 10, length: 3 }],
        childItems: [
          { text: 'bar', kind: 'method', spans: [{ start: 20, length: 3 }] },
          { text: '_unused', kind: 'keyword', spans: [{ start: 30, length: 3 }] },
        ],
      },
      { text: 'run', kind: 'function', spans: [{ start: 40, length: 3 }] },
    ],
  };

  it('跳过 script/keyword,嵌套深度递增,起点换算正确', () => {
    const syms = navTreeToSymbols(tree, positionAt);
    expect(syms).toEqual([
      { name: 'Foo', kind: 'class', depth: 0, line: 11, col: 11 },
      { name: 'bar', kind: 'method', depth: 1, line: 21, col: 21 },
      { name: 'run', kind: 'function', depth: 0, line: 41, col: 41 },
    ]);
  });

  it('无树/空树与 module 文本数组名:安全处理', () => {
    expect(navTreeToSymbols(null, positionAt)).toEqual([]);
    expect(navTreeToSymbols({}, positionAt)).toEqual([]);
    const arr = { childItems: [{ text: ['n', 's'], kind: 'module', spans: [{ start: 0, length: 2 }] }] };
    expect(navTreeToSymbols(arr, positionAt)[0].name).toBe('ns');
  });
});

describe('symbolGlyph(kind 展示形制)', () => {
  it('常见 kind 映射', () => {
    expect(symbolGlyph('class')).toEqual({ glyph: 'C', cls: 'class' });
    expect(symbolGlyph('method')).toEqual({ glyph: 'ƒ', cls: 'fn' });
    expect(symbolGlyph('key')).toEqual({ glyph: '{}', cls: 'key' });
  });

  it('未知 kind 落缺省', () => {
    expect(symbolGlyph('')).toEqual({ glyph: '·', cls: 'other' });
    expect(symbolGlyph('weird')).toEqual({ glyph: '·', cls: 'other' });
  });
});
