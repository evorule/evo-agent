// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect } from 'vitest';
import {
  DEFAULT_VISIBLE,
  sortNotices,
  sliceForDisplay,
  levelClass,
} from '../src/lib/notices.js';

describe('sortNotices', () => {
  it('pinned 置顶,date 降序', () => {
    const out = sortNotices([
      { id: 'a', date: '2026-10-01', pinned: false },
      { id: 'b', date: '2026-10-20', pinned: false },
      { id: 'c', date: '2026-10-05', pinned: true },
    ]);
    expect(out.map((n) => n.id)).toEqual(['c', 'b', 'a']);
  });

  it('不改入参(纯函数)', () => {
    const input = [{ id: 'a', date: '2026-10-01' }];
    sortNotices(input);
    expect(input[0].id).toBe('a');
  });

  it('非数组容错', () => {
    expect(sortNotices(null)).toEqual([]);
  });
});

describe('sliceForDisplay', () => {
  const list = [1, 2, 3, 4].map((i) => ({ id: `n${i}` }));

  it('缺省裁剪到 DEFAULT_VISIBLE 条', () => {
    expect(sliceForDisplay(list, false)).toHaveLength(DEFAULT_VISIBLE);
  });

  it('展开返回全部', () => {
    expect(sliceForDisplay(list, true)).toHaveLength(4);
  });
});

describe('levelClass', () => {
  it('契约词表映射', () => {
    expect(levelClass('info')).toBe('lv-info');
    expect(levelClass('warn')).toBe('lv-warn');
    expect(levelClass('feature')).toBe('lv-feature');
  });

  it('未知值兜底 info', () => {
    expect(levelClass('urgent')).toBe('lv-info');
    expect(levelClass('')).toBe('lv-info');
  });
});
