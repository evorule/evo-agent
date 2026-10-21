// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { get } from 'svelte/store';
import {
  validateWorkspaceInput,
  updateWorkspaceRecent,
  workspaceAnchor,
  workspaceLabel,
  switchWorkspace,
} from '../src/lib/workspace.js';
import { tabs, workspaceEpoch } from '../src/lib/stores.js';

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

// ---- switchWorkspace(批 I 段 3 统一切换入口) ----
// node 环境:localStorage 桩 + window/CustomEvent 缺失由入口内 try-catch 静默

function memoryStorage() {
  const m = new Map();
  return {
    getItem: (k) => (m.has(k) ? m.get(k) : null),
    setItem: (k, v) => m.set(k, String(v)),
    removeItem: (k) => m.delete(k),
  };
}

describe('switchWorkspace', () => {
  beforeEach(() => {
    vi.stubGlobal('localStorage', memoryStorage());
    tabs.set([]);
    workspaceEpoch.set(0);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    tabs.set([]);
    workspaceEpoch.set(0);
  });

  it('相对路径校验失败:零副作用', async () => {
    const r = await switchWorkspace('relative/path');
    expect(r.ok).toBe(false);
    expect(r.error).toContain('绝对路径');
    expect(get(workspaceEpoch)).toBe(0);
    expect(get(tabs)).toHaveLength(0);
  });

  it('无 dirty tab:不询问确认,直接落库 + 关闭文件 tab + 纪元自增', async () => {
    tabs.set([{ path: 'a.rs', name: 'a.rs', content: 'x', dirty: false, error: null }]);
    let asked = false;
    const r = await switchWorkspace('D:\\projects\\demo', async () => {
      asked = true;
      return true;
    });
    expect(asked).toBe(false);
    expect(r).toEqual({ ok: true, value: 'D:\\projects\\demo' });
    expect(localStorage.getItem('evo_workspace')).toBe('D:\\projects\\demo');
    expect(JSON.parse(localStorage.getItem('evo_workspace_recent'))).toEqual([
      'D:\\projects\\demo',
    ]);
    expect(get(tabs)).toHaveLength(0);
    expect(get(workspaceEpoch)).toBe(1);
  });

  it('dirty tab + 确认拒绝 → cancelled,零副作用', async () => {
    tabs.set([{ path: 'b.rs', name: 'b.rs', content: 'x', dirty: true, error: null }]);
    let asked = '';
    const r = await switchWorkspace('D:\\x', async (m) => {
      asked = m;
      return false;
    });
    expect(r.ok).toBe(false);
    expect(r.cancelled).toBe(true);
    expect(asked).toContain('未保存');
    expect(localStorage.getItem('evo_workspace')).toBe(null);
    expect(get(tabs)).toHaveLength(1);
    expect(get(workspaceEpoch)).toBe(0);
  });

  it('dirty tab + 确认同意 → 落库并关闭全部文件 tab', async () => {
    tabs.set([
      { path: 'b.rs', name: 'b.rs', content: 'x', dirty: true, error: null },
      { path: 'c.rs', name: 'c.rs', content: 'y', dirty: false, error: null },
    ]);
    const r = await switchWorkspace('D:\\x', () => Promise.resolve(true));
    expect(r.ok).toBe(true);
    expect(get(tabs)).toHaveLength(0);
    expect(get(workspaceEpoch)).toBe(1);
  });

  it('空串恢复缺省:移除工作区键,纪元仍自增', async () => {
    localStorage.setItem('evo_workspace', 'D:\\old');
    const r = await switchWorkspace('  ');
    expect(r).toEqual({ ok: true, value: '' });
    expect(localStorage.getItem('evo_workspace')).toBe(null);
    expect(get(workspaceEpoch)).toBe(1);
  });

  it('agent 会话 tab(kind 非空)不算文件 tab:不触发确认,不参与关闭', async () => {
    tabs.set([{ path: null, kind: 'agent', name: '对话', dirty: false }]);
    const r = await switchWorkspace('D:\\x');
    expect(r.ok).toBe(true);
    expect(get(tabs)).toHaveLength(1);
    expect(get(workspaceEpoch)).toBe(1);
  });
});
