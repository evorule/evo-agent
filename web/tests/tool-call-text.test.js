// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 工具卡单行摘要测试:命令族/路径族(编辑类补增删行数)/查询族/兜底键值/
// JSON 字符串与对象双载体/超长截断/空值安全。
import { describe, it, expect } from 'vitest';
import { toolCallSummary, approvalPreview } from '../src/lib/tool-call-text.js';

describe('toolCallSummary', () => {
  it('命令族:command/code/script 首选', () => {
    expect(toolCallSummary('shell_exec', { command: 'find src -name x' })).toBe('find src -name x');
    expect(toolCallSummary('python_exec', { code: 'print(1)' })).toBe('print(1)');
    expect(toolCallSummary('x', { cmd: 'dir' })).toBe('dir');
  });

  it('路径族:path/file_path;编辑类补增删行数', () => {
    expect(toolCallSummary('file_read', { path: 'src/lib.rs' })).toBe('src/lib.rs');
    expect(
      toolCallSummary('search_replace', {
        path: 'a.rs',
        old_string: 'l1\nl2',
        new_string: 'm1\nm2\nm3',
      }),
    ).toBe('a.rs (+3/-2 行)');
  });

  it('查询族:url/query/pattern', () => {
    expect(toolCallSummary('http_get', { url: 'https://x.y' })).toBe('https://x.y');
    expect(toolCallSummary('grep', { pattern: 'foo.*bar' })).toBe('foo.*bar');
  });

  it('JSON 字符串载体(实时卡)与对象载体(历史卡)等价', () => {
    expect(toolCallSummary('shell_exec', JSON.stringify({ command: 'ls' }))).toBe('ls');
    expect(toolCallSummary('shell_exec', { command: 'ls' })).toBe('ls');
  });

  it('超长截断带省略号;空白折叠', () => {
    const s = toolCallSummary('shell_exec', { command: `echo ${'a'.repeat(120)}` });
    expect(s.length).toBeLessThanOrEqual(64);
    expect(s.endsWith('…')).toBe(true);
    expect(toolCallSummary('shell_exec', { command: 'a\n\n  b' })).toBe('a b');
  });

  it('空值/非对象安全:返回空串', () => {
    expect(toolCallSummary('x', null)).toBe('');
    expect(toolCallSummary('x', {})).toBe('');
    expect(toolCallSummary('x', '{bad json')).toBe('{bad json');
    expect(toolCallSummary('x', [1, 2])).toBe('');
  });

  it('兜底:首个标量键值对', () => {
    expect(toolCallSummary('mystery', { foo: 'bar', n: 1 })).toBe('foo=bar');
    expect(toolCallSummary('mystery', { n: 3 })).toBe('n=3');
  });
});

describe('approvalPreview 审批卡预览分派', () => {
  it('文件族:按操作动词分派,command 文本即目标路径', () => {
    expect(approvalPreview('file_write', 'src/lib.rs')).toEqual({
      kind: 'file',
      verb: '覆写',
      path: 'src/lib.rs',
    });
    expect(approvalPreview('file_create', 'workspace/a.txt').verb).toBe('新建');
    expect(approvalPreview('file_delete', 'tmp/x').verb).toBe('删除');
    expect(approvalPreview('file_move', 'a -> b').verb).toBe('移动');
    expect(approvalPreview('search_replace', 'src/a.rs').verb).toBe('编辑');
    expect(approvalPreview('edit_file', 'src/b.rs').verb).toBe('编辑');
  });

  it('非文件族(命令族):kind=text 沿用命令文本块', () => {
    expect(approvalPreview('shell_exec', 'rm -rf /')).toEqual({ kind: 'text' });
    expect(approvalPreview('python_exec', 'print(1)')).toEqual({ kind: 'text' });
    expect(approvalPreview('delegate', 'researcher')).toEqual({ kind: 'text' });
  });

  it('空值安全:文件族空路径返回空串(text 不抛错)', () => {
    expect(approvalPreview('file_write', null)).toEqual({ kind: 'file', verb: '覆写', path: '' });
    expect(approvalPreview('file_write', '  ')).toEqual({ kind: 'file', verb: '覆写', path: '' });
  });
});
