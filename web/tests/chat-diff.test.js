// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// M2:编辑类工具前端行级 diff 纯函数用例
import { describe, it, expect } from 'vitest';
import { lineDiff, toolDiffPayload } from '../src/lib/chat-diff.js';

describe('chat-diff 行级 diff(M2 编辑类工具卡)', () => {
  it('完全相等 → 全部 ctx', () => {
    const rows = lineDiff('a\nb\nc', 'a\nb\nc');
    expect(rows).toEqual([
      { t: 'ctx', s: 'a' },
      { t: 'ctx', s: 'b' },
      { t: 'ctx', s: 'c' },
    ]);
  });

  it('单行替换 → del 在前 add 在后', () => {
    const rows = lineDiff('x', 'y');
    expect(rows).toEqual([
      { t: 'del', s: 'x' },
      { t: 'add', s: 'y' },
    ]);
  });

  it('纯新增:公共行保留,新行尾随', () => {
    const rows = lineDiff('a', 'a\nb');
    expect(rows).toEqual([
      { t: 'ctx', s: 'a' },
      { t: 'add', s: 'b' },
    ]);
  });

  it('纯删除', () => {
    const rows = lineDiff('a\nb', 'a');
    expect(rows).toEqual([
      { t: 'ctx', s: 'a' },
      { t: 'del', s: 'b' },
    ]);
  });

  it('new_string 为空 → 全部删除(edit_file 以空 new 删段)', () => {
    const rows = lineDiff('a\nb', '');
    expect(rows).toEqual([
      { t: 'del', s: 'a' },
      { t: 'del', s: 'b' },
    ]);
  });

  it('CRLF 归一为 LF 后相等 → 全 ctx', () => {
    const rows = lineDiff('a\r\nb', 'a\nb');
    expect(rows.every((r) => r.t === 'ctx')).toBe(true);
  });

  it('中文行 diff', () => {
    const rows = lineDiff('函数一', '函数一\n函数二');
    expect(rows).toEqual([
      { t: 'ctx', s: '函数一' },
      { t: 'add', s: '函数二' },
    ]);
  });

  it('任一侧超 800 行 → null(降级信号)', () => {
    const big = Array.from({ length: 801 }, (_, i) => `L${i}`).join('\n');
    expect(lineDiff(big, 'x')).toBeNull();
    expect(lineDiff('x', big)).toBeNull();
    expect(lineDiff('x', 'y')).not.toBeNull();
  });

  it('toolDiffPayload:JSON 字符串形态的 edit_file args 可提取', () => {
    const p = toolDiffPayload({
      name: 'edit_file',
      args: JSON.stringify({ path: 'a.txt', old_string: 'x', new_string: 'y' }),
    });
    expect(p).toEqual({ oldText: 'x', newText: 'y' });
  });

  it('toolDiffPayload:对象形态(历史 transcript arguments)可提取', () => {
    const p = toolDiffPayload({ name: 'edit_file', args: { old_string: 'x', new_string: 'y' } });
    expect(p).toEqual({ oldText: 'x', newText: 'y' });
  });

  it('toolDiffPayload:非 edit_file/缺参数/残缺 JSON → null 降级', () => {
    expect(toolDiffPayload({ name: 'file_write', args: '{"old_string":"x"}' })).toBeNull();
    expect(toolDiffPayload({ name: 'edit_file', args: null })).toBeNull();
    expect(toolDiffPayload({ name: 'edit_file', args: '{"old_string":"x"' })).toBeNull();
    expect(
      toolDiffPayload({
        name: 'edit_file',
        args: JSON.stringify({ old_string: 1, new_string: 'y' }),
      })
    ).toBeNull();
    expect(toolDiffPayload(null)).toBeNull();
  });
});
