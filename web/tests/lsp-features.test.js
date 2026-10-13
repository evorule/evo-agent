// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// LSP 特性映射层单测:severity/CompletionItemKind/Hover/Location/诊断桥映射表
// + 过期推送丢弃判定。
import { describe, it, expect } from 'vitest';
import {
  lspSeverityToMonaco,
  lspRangeToMonacoRange,
  hoverToMarkdown,
  normalizeLocations,
  makeKindResolver,
  mapCompletionItems,
  diagnosticsToMarkers,
  uriToRelativeForJump,
  setWorkdirBase,
} from '../src/lib/lsp/features.js';
import { pathToFileUri } from '../src/lib/lsp/client.js';

describe('severity mapping (LSP 1/2/3/4 → Monaco 8/4/2/1)', () => {
  it('全量映射表', () => {
    expect(lspSeverityToMonaco(1)).toBe(8); // Error
    expect(lspSeverityToMonaco(2)).toBe(4); // Warning
    expect(lspSeverityToMonaco(3)).toBe(2); // Info
    expect(lspSeverityToMonaco(4)).toBe(1); // Hint
  });
  it('未知 severity 回落最严重(不静默吞错)', () => {
    expect(lspSeverityToMonaco(undefined)).toBe(8);
    expect(lspSeverityToMonaco(99)).toBe(8);
  });
});

describe('range mapping (0-based → 1-based)', () => {
  it('行列 +1,缺省端点回落起点', () => {
    expect(lspRangeToMonacoRange({ start: { line: 2, character: 4 }, end: { line: 2, character: 9 } })).toEqual({
      startLineNumber: 3,
      startColumn: 5,
      endLineNumber: 3,
      endColumn: 10,
    });
    expect(lspRangeToMonacoRange({ start: { line: 0, character: 0 } })).toEqual({
      startLineNumber: 1,
      startColumn: 1,
      endLineNumber: 1,
      endColumn: 1,
    });
  });
});

describe('hover mapping', () => {
  it('MarkupContent → IMarkdownString', () => {
    const out = hoverToMarkdown({
      contents: { kind: 'markdown', value: '**i32**' },
      range: { start: { line: 0, character: 0 }, end: { line: 0, character: 3 } },
    });
    expect(out.contents).toEqual([{ value: '**i32**' }]);
    expect(out.range.startColumn).toBe(1);
  });
  it('MarkedString(带语言)→ 代码围栏', () => {
    const out = hoverToMarkdown({ contents: { language: 'rust', value: 'let x = 5;' } });
    expect(out.contents[0].value).toContain('```rust');
    expect(out.contents[0].value).toContain('let x = 5;');
  });
  it('MarkedString 数组与纯字符串', () => {
    const out = hoverToMarkdown({ contents: ['plain text', { language: 'ts', value: 'const a' }] });
    expect(out.contents).toHaveLength(2);
    expect(out.contents[0].value).toBe('plain text');
  });
  it('空 hover → 空内容', () => {
    expect(hoverToMarkdown(null).contents).toEqual([]);
    expect(hoverToMarkdown({ contents: null }).contents).toEqual([]);
  });
});

describe('definition normalization', () => {
  it('单 Location / 数组 / null', () => {
    const loc = { uri: 'file:///a.rs', range: { start: { line: 1, character: 0 }, end: { line: 1, character: 2 } } };
    expect(normalizeLocations(loc)).toHaveLength(1);
    expect(normalizeLocations([loc, loc])).toHaveLength(2);
    expect(normalizeLocations(null)).toEqual([]);
  });
  it('LocationLink 取 targetUri/targetSelectionRange', () => {
    const out = normalizeLocations([
      { targetUri: 'file:///b.rs', targetRange: { start: { line: 9, character: 0 }, end: { line: 9, character: 0 } }, targetSelectionRange: { start: { line: 4, character: 3 }, end: { line: 4, character: 5 } } },
    ]);
    expect(out[0].uri).toBe('file:///b.rs');
    expect(out[0].range.start.line).toBe(4);
  });
});

describe('completion kind mapping', () => {
  const fakeTable = { Text: 18, Method: 0, Function: 1, Class: 5, EnumMember: 16, Snippet: 25 };
  it('LSP kind 序号按名字对齐 Monaco 表', () => {
    const resolve = makeKindResolver(fakeTable, 18);
    expect(resolve(2)).toBe(0); // Method
    expect(resolve(3)).toBe(1); // Function
    expect(resolve(7)).toBe(5); // Class
    expect(resolve(20)).toBe(16); // EnumMember
    expect(resolve(15)).toBe(25); // Snippet
  });
  it('未知 kind 回落 fallback', () => {
    const resolve = makeKindResolver(fakeTable, 18);
    expect(resolve(999)).toBe(18);
    expect(resolve(undefined)).toBe(18);
  });
});

describe('completion items mapping', () => {
  const resolve = makeKindResolver({ Text: 18, Function: 1 }, 18);
  it('label/insertText/detail/documentation/sortText 全量映射', () => {
    const out = mapCompletionItems(
      [
        { label: 'foo', kind: 3, detail: 'fn foo()', documentation: 'docs', sortText: '0000' },
        { label: 'bar', insertText: 'bar()', kind: 2 },
      ],
      resolve,
      { startLineNumber: 1, startColumn: 1, endLineNumber: 1, endColumn: 3 },
    );
    expect(out).toHaveLength(2);
    expect(out[0]).toMatchObject({ label: 'foo', kind: 1, insertText: 'foo', detail: 'fn foo()', documentation: { value: 'docs' }, sortText: '0000' });
    expect(out[1].insertText).toBe('bar()');
  });
  it('MarkupKind documentation 对象归一', () => {
    const out = mapCompletionItems([{ label: 'x', documentation: { kind: 'markdown', value: '*em*' } }], resolve, {});
    expect(out[0].documentation).toEqual({ value: '*em*' });
  });
  it('items 容器形态(CompletionList)与非法项过滤', () => {
    const out = mapCompletionItems({ items: [{ label: 'a' }, null, {}] }, resolve, {});
    expect(out).toHaveLength(1);
  });
});

describe('diagnostics bridge', () => {
  it('evoPath 缺失(工作区外)返回 null(灰置)', () => {
    expect(diagnosticsToMarkers({ uri: 'file:///elsewhere/x.rs', diagnostics: [] })).toBeNull();
    expect(diagnosticsToMarkers(null)).toBeNull();
  });
  it('severity/range/source/code 全量映射', () => {
    const { path, markers } = diagnosticsToMarkers({
      evoPath: 'src/main.rs',
      version: 7,
      diagnostics: [
        { range: { start: { line: 4, character: 2 }, end: { line: 4, character: 7 } }, message: 'borrow error', severity: 1, code: 'E0382', source: 'rustc' },
        { range: { start: { line: 9, character: 0 }, end: { line: 9, character: 1 } }, message: 'unused', severity: 2 },
      ],
    });
    expect(path).toBe('src/main.rs');
    expect(markers[0]).toMatchObject({
      severity: 8,
      startLineNumber: 5,
      startColumn: 3,
      endLineNumber: 5,
      endColumn: 8,
      code: 'E0382',
      source: 'rustc',
    });
    expect(markers[1].severity).toBe(4);
  });
  it('diagnostics 非数组容错为空集(全清语义)', () => {
    const out = diagnosticsToMarkers({ evoPath: 'a.rs', diagnostics: null });
    expect(out.markers).toEqual([]);
  });
});

describe('file URI building and jump path recovery', () => {
  it('pathToFileUri 逐段编码', () => {
    expect(pathToFileUri('D:/work/my proj', 'src/a b.rs')).toBe('file:///D:/work/my%20proj/src/a%20b.rs');
    expect(pathToFileUri('D:/work/', '/src/x.rs')).toBe('file:///D:/work/src/x.rs');
  });
  it('uriToRelativeForJump 按工作区基址截取', () => {
    setWorkdirBase('D:/work/proj');
    expect(uriToRelativeForJump('file:///D:/work/proj/src/main.rs')).toBe('src/main.rs');
    // 工作区外:返回解码绝对路径(前端按未知路径处理)
    expect(uriToRelativeForJump('file:///D:/elsewhere/a.rs')).toBe('D:/elsewhere/a.rs');
    // 中文路径解码
    expect(uriToRelativeForJump('file:///D:/work/proj/%E6%96%87%E6%A1%A3.md')).toBe('文档.md');
  });
});
