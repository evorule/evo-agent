// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect } from 'vitest';
import { fuzzyMatch, fuzzyMatchPath, highlightSegments } from '../src/lib/fuzzy.js';

describe('fuzzyMatch', () => {
  it('空查询匹配一切(零分,无位置)', () => {
    expect(fuzzyMatch('', 'anything')).toEqual({ score: 0, positions: [] });
  });

  it('子序列命中并返回升序位置', () => {
    const r = fuzzyMatch('abc', 'xaybzc');
    expect(r).not.toBeNull();
    expect(r.positions).toEqual([1, 3, 5]);
  });

  it('非子序列返回 null', () => {
    expect(fuzzyMatch('ax', 'ba')).toBeNull();
    expect(fuzzyMatch('zz', 'file.ts')).toBeNull();
  });

  it('不区分大小写', () => {
    const r = fuzzyMatch('BS', 'base');
    expect(r).not.toBeNull();
    expect(r.positions).toEqual([0, 2]);
  });

  it('连续命中得分高于离散命中', () => {
    const consecutive = fuzzyMatch('bs', 'bsae');
    const scattered = fuzzyMatch('bs', 'base');
    expect(consecutive.score).toBeGreaterThan(scattered.score);
  });

  it('词边界命中加分(分隔符后)', () => {
    const boundary = fuzzyMatch('r', 'my-readme'); // 唯一 r 在分隔符后
    const mid = fuzzyMatch('r', 'xrx'); // 唯一 r 在词中
    expect(boundary.score).toBeGreaterThan(mid.score);
  });

  it('串首前缀额外加分', () => {
    const prefix = fuzzyMatch('re', 'readme');
    const inner = fuzzyMatch('re', 'xare');
    expect(prefix.score).toBeGreaterThan(inner.score);
  });

  it('非法输入返回 null', () => {
    expect(fuzzyMatch(null, 'x')).toBeNull();
    expect(fuzzyMatch('x', undefined)).toBeNull();
  });
});

describe('highlightSegments', () => {
  it('按命中位置切分片段', () => {
    expect(highlightSegments('readme', [0, 1, 2])).toEqual([
      { text: 'rea', hit: true },
      { text: 'dme', hit: false },
    ]);
  });

  it('离散位置交替切分', () => {
    expect(highlightSegments('xaybzc', [1, 3, 5])).toEqual([
      { text: 'x', hit: false },
      { text: 'a', hit: true },
      { text: 'y', hit: false },
      { text: 'b', hit: true },
      { text: 'z', hit: false },
      { text: 'c', hit: true },
    ]);
  });

  it('空位置集 = 单一未命中片段', () => {
    expect(highlightSegments('abc', [])).toEqual([{ text: 'abc', hit: false }]);
  });
});

describe('fuzzyMatchPath', () => {
  it('basename 命中得分高于仅路径命中', () => {
    const onBase = fuzzyMatchPath('main', 'src/bin/main.rs');
    const onPath = fuzzyMatchPath('bin', 'src/bin/main.rs');
    expect(onBase.matchedOn).toBe('basename');
    expect(onPath.matchedOn).toBe('path');
    expect(onBase.score).toBeGreaterThan(onPath.score);
  });

  it('basename 未命中但全路径命中时退化为路径匹配', () => {
    const r = fuzzyMatchPath('lib', 'src/lib/fuzzy.js');
    expect(r).not.toBeNull();
    expect(r.matchedOn).toBe('path');
  });

  it('同 basename 时路径更短者胜', () => {
    const short = fuzzyMatchPath('app', 'src/app.rs');
    const deep = fuzzyMatchPath('app', 'src/modules/ui/app.rs');
    expect(short.score).toBeGreaterThan(deep.score);
  });

  it('basename 命中位置映射回全路径(可直接高亮)', () => {
    const r = fuzzyMatchPath('read', 'docs/readme.md');
    expect(r.positions).toEqual([5, 6, 7, 8]);
    expect(highlightSegments('docs/readme.md', r.positions)).toEqual([
      { text: 'docs/', hit: false },
      { text: 'read', hit: true },
      { text: 'me.md', hit: false },
    ]);
  });

  it('根文件名(无分隔符)按 basename 命中', () => {
    const r = fuzzyMatchPath('fu', 'fuzzy.js');
    expect(r.matchedOn).toBe('basename');
    expect(r.positions).toEqual([0, 1]);
  });

  it('basename 与全路径均未命中返回 null', () => {
    expect(fuzzyMatchPath('zzz', 'src/lib/fuzzy.js')).toBeNull();
  });

  it('非法输入返回 null', () => {
    expect(fuzzyMatchPath(null, 'x')).toBeNull();
    expect(fuzzyMatchPath('x', undefined)).toBeNull();
  });
});
