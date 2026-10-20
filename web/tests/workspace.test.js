// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect } from 'vitest';
import {
  validateWorkspaceInput,
  updateWorkspaceRecent,
  workspaceAnchor,
  workspaceLabel,
} from '../src/lib/workspace.js';

describe('validateWorkspaceInput', () => {
  it('空串 = 恢复缺省(ok, value 空)', () => {
    expect(validateWorkspaceInput('  ')).toEqual({ ok: true, value: '' });
  });

  it('Windows 盘符绝对路径合法(去首尾空白)', () => {
    const r = validateWorkspaceInput(' D:\\projects\\demo ');
    expect(r.ok).toBe(true);
    expect(r.value).toBe('D:\\projects\\demo');
  });

  it('UNC 路径合法', () => {
    expect(validateWorkspaceInput('\\\\srv\\share').ok).toBe(true);
  });

  it('POSIX 绝对路径合法', () => {
    expect(validateWorkspaceInput('/home/u/proj').ok).toBe(true);
  });

  it('相对路径拒绝并提示绝对路径', () => {
    const r = validateWorkspaceInput('projects/demo');
    expect(r.ok).toBe(false);
    expect(r.error).toContain('绝对路径');
  });
});

describe('updateWorkspaceRecent', () => {
  it('新条目置顶', () => {
    expect(updateWorkspaceRecent(['a', 'b'], 'c')).toEqual(['c', 'a', 'b']);
  });

  it('已有条目去重并提前', () => {
    expect(updateWorkspaceRecent(['a', 'b', 'c'], 'b')).toEqual(['b', 'a', 'c']);
  });

  it('超出容量截断(缺省 5)', () => {
    const r = updateWorkspaceRecent(['1', '2', '3', '4', '5'], '6');
    expect(r).toHaveLength(5);
    expect(r[0]).toBe('6');
  });

  it('空值不改动列表', () => {
    expect(updateWorkspaceRecent(['a'], '')).toEqual(['a']);
  });

  it('非数组输入容错', () => {
    expect(updateWorkspaceRecent(null, 'x')).toEqual(['x']);
  });
});

describe('workspaceAnchor', () => {
  const wd = 'D:\\repo\\evo-agent';

  it('无工作区 → workdir 根(空串)', () => {
    expect(workspaceAnchor(wd, '', {})).toBe('');
  });

  it('工作区 = workdir 本身 → 根(空串)', () => {
    expect(workspaceAnchor(wd, 'D:\\repo\\evo-agent\\', {})).toBe('');
  });

  it('workdir 子目录 → 相对锚点(正斜杠拼接)', () => {
    expect(workspaceAnchor(wd, 'D:\\repo\\evo-agent\\workspace\\verify', {})).toBe(
      'workspace/verify'
    );
  });

  it('workdir 之外 → null(树保持 workdir 根)', () => {
    expect(workspaceAnchor(wd, 'D:\\other\\proj', {})).toBe(null);
  });

  it('大小写不敏感匹配(Windows 口径)', () => {
    expect(workspaceAnchor(wd, 'd:\\REPO\\evo-agent\\sub', { caseInsensitive: true })).toBe(
      'sub'
    );
  });

  it('POSIX 路径段拼接', () => {
    expect(workspaceAnchor('/srv/work', '/srv/work/a/b', {})).toBe('a/b');
  });

  it('工作区段数少于 workdir → null', () => {
    expect(workspaceAnchor(wd, 'D:\\repo', {})).toBe(null);
  });
});

describe('workspaceLabel', () => {
  it('取末段路径', () => {
    expect(workspaceLabel('D:\\a\\b\\c')).toBe('c');
    expect(workspaceLabel('/srv/work')).toBe('work');
  });

  it('空值返回空串', () => {
    expect(workspaceLabel('')).toBe('');
  });
});
