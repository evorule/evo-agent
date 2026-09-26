// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import {
  shouldSkip,
  collectFiles,
  MAX_FILES,
  RECENT_FILES_KEY,
  RECENT_FILES_LIMIT,
  loadRecentFiles,
  recordRecentFile,
  nameOf,
} from '../src/lib/quick-open.js';

/** 构造 fake lister:dirSpec = { '': [entries], 'src': [...] },entry = ['name','dir'|'file'] */
function fakeLister(dirSpec) {
  return async (dir) => {
    const key = dir || '';
    if (!(key in dirSpec)) throw new Error(`HTTP 404`);
    return {
      entries: dirSpec[key].map(([name, kind]) => ({ name, kind })),
    };
  };
}

describe('shouldSkip', () => {
  it('依赖与产物目录在名单内', () => {
    expect(shouldSkip('node_modules')).toBe(true);
    expect(shouldSkip('.git')).toBe(true);
    expect(shouldSkip('dist')).toBe(true);
    expect(shouldSkip('target')).toBe(true);
  });

  it('普通目录与文件不在名单内', () => {
    expect(shouldSkip('src')).toBe(false);
    expect(shouldSkip('README.md')).toBe(false);
  });
});

describe('collectFiles', () => {
  it('递归收集文件为相对路径,跳过名单目录', async () => {
    const lister = fakeLister({
      '': [
        ['README.md', 'file'],
        ['src', 'dir'],
        ['node_modules', 'dir'],
      ],
      src: [
        ['main.js', 'file'],
        ['lib', 'dir'],
      ],
      'src/lib': [['util.js', 'file']],
      node_modules: [['pkg', 'dir']],
    });
    const { files, truncated } = await collectFiles(lister);
    expect(truncated).toBe(false);
    expect(files.map((f) => f.path)).toEqual(['README.md', 'src/main.js', 'src/lib/util.js']);
    expect(files[1].name).toBe('main.js');
  });

  it('条目达上限即截断并标记 truncated', async () => {
    const entries = Array.from({ length: MAX_FILES + 50 }, (_, i) => [`f${i}.txt`, 'file']);
    const lister = fakeLister({ '': entries });
    const { files, truncated } = await collectFiles(lister);
    expect(files.length).toBe(MAX_FILES);
    expect(truncated).toBe(true);
  });

  it('单层列表失败静默跳过,其余部分照常返回', async () => {
    const lister = async (dir) => {
      if (dir === 'bad') throw new Error('HTTP 500');
      if (dir === undefined)
        return {
          entries: [
            { name: 'ok.txt', kind: 'file' },
            { name: 'bad', kind: 'dir' },
          ],
        };
      return { entries: [] };
    };
    const { files, truncated } = await collectFiles(lister);
    expect(files).toEqual([{ path: 'ok.txt', name: 'ok.txt' }]);
    expect(truncated).toBe(false);
  });

  it('根列表失败返回空(整体降级不抛)', async () => {
    const lister = async () => {
      throw new Error('network down');
    };
    const { files } = await collectFiles(lister);
    expect(files).toEqual([]);
  });
});

describe('recent files', () => {
  beforeEach(() => {
    globalThis.localStorage = {
      store: {},
      getItem(k) {
        return this.store[k] ?? null;
      },
      setItem(k, v) {
        this.store[k] = String(v);
      },
    };
  });

  afterEach(() => {
    delete globalThis.localStorage;
  });

  it('record 置顶去重,上限 10', () => {
    for (let i = 0; i < RECENT_FILES_LIMIT + 3; i++) recordRecentFile(`f${i}.txt`);
    let list = loadRecentFiles();
    expect(list.length).toBe(RECENT_FILES_LIMIT);
    expect(list[0]).toBe(`f${RECENT_FILES_LIMIT + 2}.txt`);
    recordRecentFile('f5.txt');
    list = loadRecentFiles();
    expect(list[0]).toBe('f5.txt');
    expect(list.filter((p) => p === 'f5.txt').length).toBe(1);
  });

  it('空/非串/非法 JSON 降级为空数组', () => {
    expect(loadRecentFiles()).toEqual([]);
    localStorage.setItem(RECENT_FILES_KEY, 'not-json');
    expect(loadRecentFiles()).toEqual([]);
    localStorage.setItem(RECENT_FILES_KEY, JSON.stringify(['ok.txt', '', 42]));
    expect(loadRecentFiles()).toEqual(['ok.txt']);
  });
});

describe('nameOf', () => {
  it('取末段', () => {
    expect(nameOf('a/b/c.js')).toBe('c.js');
    expect(nameOf('README.md')).toBe('README.md');
  });
});
