// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// H1 tab 拖拽:重排纯函数 + 组内重排 + 落点分发(store 环境容错,node 无
// localStorage 时 persistGroups 静默跳过,与工作台其他 store 测试同口径)
import { describe, it, expect, beforeEach } from 'vitest';
import { get } from 'svelte/store';
import {
  reorderedPaths,
  reorderTab,
  moveTabWithinSecondary,
  dispatchTabDrop,
  secondaryGroups,
} from '../src/lib/editorGroups.js';
import { tabs, activePath } from '../src/lib/stores.js';

describe('reorderedPaths(纯重排)', () => {
  it('后移:摘前插后', () => {
    expect(reorderedPaths(['a', 'b', 'c'], 0, 2)).toEqual(['b', 'c', 'a']);
  });

  it('前移:摘后插前', () => {
    expect(reorderedPaths(['a', 'b', 'c'], 2, 0)).toEqual(['c', 'a', 'b']);
  });

  it('同位:返回等序副本', () => {
    const src = ['a', 'b'];
    const out = reorderedPaths(src, 1, 1);
    expect(out).toEqual(['a', 'b']);
    expect(out).not.toBe(src); // 副本,不改输入
  });

  it('越界收拢:toIndex 超尾 = 移到最后', () => {
    expect(reorderedPaths(['a', 'b', 'c'], 0, 99)).toEqual(['b', 'c', 'a']);
    expect(reorderedPaths(['a', 'b', 'c'], 2, -3)).toEqual(['c', 'a', 'b']);
  });

  it('空表与非数组:原样安全返回', () => {
    expect(reorderedPaths([], 0, 1)).toEqual([]);
    expect(reorderedPaths(null, 0, 1)).toBeNull();
  });

  it('单元素:移位无效果', () => {
    expect(reorderedPaths(['a'], 0, 0)).toEqual(['a']);
  });
});

describe('reorderTab(主组重排)', () => {
  beforeEach(() => {
    tabs.set([
      { path: 'a.js', name: 'a.js', content: '', dirty: false, error: null },
      { path: 'b.js', name: 'b.js', content: '', dirty: false, error: null },
      { path: 'c.js', name: 'c.js', content: '', dirty: false, error: null },
    ]);
    activePath.set('a.js');
  });

  it('后移不改激活态', () => {
    reorderTab('a.js', 2);
    expect(get(tabs).map((t) => t.path)).toEqual(['b.js', 'c.js', 'a.js']);
    expect(get(activePath)).toBe('a.js');
  });

  it('path 不在 tabs:无操作', () => {
    reorderTab('x.js', 0);
    expect(get(tabs).map((t) => t.path)).toEqual(['a.js', 'b.js', 'c.js']);
  });
});

describe('moveTabWithinSecondary(副组组内重排)', () => {
  beforeEach(() => {
    secondaryGroups.set([{ id: 'split-1', paths: ['b.js', 'a.js', 'c.js'], activePath: 'a.js', width: '30%' }]);
  });

  it('重排成员顺序,激活态不动', () => {
    moveTabWithinSecondary('split-1', 'b.js', 2);
    const g = get(secondaryGroups).find((x) => x.id === 'split-1');
    expect(g.paths).toEqual(['a.js', 'c.js', 'b.js']);
    expect(g.activePath).toBe('a.js');
  });

  it('path 不在组内或组不存在:无操作', () => {
    moveTabWithinSecondary('split-1', 'x.js', 0);
    moveTabWithinSecondary('split-404', 'b.js', 0);
    expect(get(secondaryGroups).find((x) => x.id === 'split-1').paths).toEqual(['b.js', 'a.js', 'c.js']);
  });
});

describe('dispatchTabDrop(落点分发)', () => {
  beforeEach(() => {
    tabs.set([
      { path: 'a.js', name: 'a.js', content: '', dirty: false, error: null },
      { path: 'b.js', name: 'b.js', content: '', dirty: false, error: null },
    ]);
    activePath.set('a.js');
    secondaryGroups.set([{ id: 'split-1', paths: ['b.js'], activePath: 'b.js', width: '30%' }]);
  });

  it('主组→副组:副组登记置激活,主组关 tab', () => {
    dispatchTabDrop('primary', 'a.js', 'split-1', 0);
    const g = get(secondaryGroups).find((x) => x.id === 'split-1');
    expect(g.paths).toEqual(['b.js', 'a.js']);
    expect(g.activePath).toBe('a.js');
    expect(get(tabs).some((t) => t.path === 'a.js')).toBe(false);
  });

  it('副组→主组:摘成员 + 全局激活(tab 已存在则不重取内容)', () => {
    dispatchTabDrop('split-1', 'b.js', 'primary', 0);
    // 移动走 groupRemovePath(摘成员不杀组;组消亡仅 close 语义)
    expect(get(secondaryGroups).find((x) => x.id === 'split-1').paths).toEqual([]);
    expect(get(tabs).some((t) => t.path === 'b.js')).toBe(true);
    expect(get(activePath)).toBe('b.js');
  });

  it('副组→副组:摘源 + 目标组打开', () => {
    secondaryGroups.set([
      { id: 'split-1', paths: ['b.js'], activePath: 'b.js', width: '30%' },
      { id: 'split-2', paths: [], activePath: null, width: '30%' },
    ]);
    dispatchTabDrop('split-1', 'b.js', 'split-2', 0);
    const g1 = get(secondaryGroups).find((x) => x.id === 'split-1');
    const g2 = get(secondaryGroups).find((x) => x.id === 'split-2');
    expect(g1?.paths).toEqual([]);
    expect(g2?.paths).toEqual(['b.js']);
    expect(g2?.activePath).toBe('b.js');
  });

  it('同组:主组走重排,副组走组内重排', () => {
    dispatchTabDrop('primary', 'a.js', 'primary', 1);
    expect(get(tabs).map((t) => t.path)).toEqual(['b.js', 'a.js']);
    dispatchTabDrop('split-1', 'b.js', 'split-1', 0);
    expect(get(secondaryGroups).find((x) => x.id === 'split-1').paths).toEqual(['b.js']);
  });
});
