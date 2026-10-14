// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 内置 mention provider 单测:文件候选排序(rankFiles)与文件夹摘要(collectFolder)。
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('../src/lib/api.js', () => ({
  listSessions: vi.fn(async () => []),
  listDir: vi.fn(async () => ({ entries: [] })),
  readFile: vi.fn(async () => ({ content: '' })),
  writeFile: vi.fn(async () => ({})),
  getAgentDef: vi.fn(async () => ({})),
  getEvolutionSignals: vi.fn(async () => []),
  getWorkbenchSettings: vi.fn(async () => ({})),
  getWorkbenchSettingsSchema: vi.fn(async () => ({ entries: [] })),
  putWorkbenchSetting: vi.fn(async () => ({})),
}));

import { listDir, readFile } from '../src/lib/api.js';
import { registerMentionProviders, rankFiles } from '../src/lib/mentionProviders.js';
import { getProvider, assembleContext } from '../src/lib/mentions.js';

const INDEX = [
  { path: 'docs/readme.md', name: 'readme.md', isDir: false },
  { path: 'src/app.rs', name: 'app.rs', isDir: false },
  { path: 'src/lib', name: 'lib', isDir: true },
  { path: 'src/lib/fuzzy.js', name: 'fuzzy.js', isDir: false },
  { path: 'src/modules/ui/app.rs', name: 'app.rs', isDir: false },
];

describe('rankFiles', () => {
  it('basename 命中加权:同名文件短路径排前', () => {
    const out = rankFiles(INDEX, 'app');
    expect(out[0].ref).toBe('src/app.rs');
    expect(out[0].type).toBe('file');
    expect(out[0].label).toBe('app.rs');
    expect(out[0].positions).not.toHaveLength(0);
  });

  it('文件夹候选带 folder 类型', () => {
    const out = rankFiles(INDEX, 'lib');
    expect(out[0].type).toBe('folder');
    expect(out[0].ref).toBe('src/lib');
  });

  it('空查询按字典序截取(无位置)', () => {
    const out = rankFiles(INDEX, '');
    expect(out.length).toBe(INDEX.length);
    expect(out[0].ref).toBe('docs/readme.md');
    expect(out[0].positions).toEqual([]);
  });

  it('limit 截取', () => {
    expect(rankFiles(INDEX, '', 2)).toHaveLength(2);
  });
});

describe('file/folder provider', () => {
  beforeEach(() => {
    registerMentionProviders();
  });

  it('file 与 folder 注册同一实例(候选去重前提)', () => {
    expect(getProvider('file')).toBe(getProvider('folder'));
  });

  it('collect(file)→serialize 产 file_content 块', async () => {
    vi.mocked(readFile).mockResolvedValueOnce({ content: 'hello' });
    const p = getProvider('file');
    const c = await p.collect({ type: 'file', ref: 'a.rs' });
    const s = p.serialize({ type: 'file', ref: 'a.rs' }, c);
    expect(s.xml).toBe('<file_content path="a.rs">\nhello\n</file_content>');
    expect(s.summary).toBe('a.rs');
  });

  it('collect(file) 失败抛错(assembleContext 捕获降级)', async () => {
    vi.mocked(readFile).mockRejectedValueOnce(new Error('nope'));
    const p = getProvider('file');
    await expect(p.collect({ type: 'file', ref: 'gone.rs' })).rejects.toThrow('nope');
  });

  it('collect(folder) 目录树深度 2 + 截断标记,serialize 产 dir_summary 块', async () => {
    const tree = {
      root: [
        { name: 'sub', kind: 'dir' },
        { name: 'z.txt', kind: 'file' },
      ],
      'root/sub': [{ name: 'deep', kind: 'dir' }],
      'root/sub/deep': [{ name: 'x.txt', kind: 'file' }],
    };
    vi.mocked(listDir).mockImplementation(async (dir = '') => ({ entries: tree[dir] || [] }));
    const p = getProvider('folder');
    const c = await p.collect({ type: 'folder', ref: 'root' });
    expect(c.tree).toContain('├─ sub/');
    expect(c.tree).toContain('└─ z.txt');
    expect(c.tree).toContain('deep/'); // 深度 2:一层子目录内容在列
    expect(c.tree).not.toContain('x.txt'); // 深度 3 不下钻
    const s = p.serialize({ type: 'folder', ref: 'root' }, c);
    expect(s.xml).toContain('<dir_summary path="root">');
    expect(s.summary).toBe('root/(目录摘要)');
  });

  it('collect(folder) 条目超 50 标省略', async () => {
    const many = Array.from({ length: 60 }, (_, i) => ({ name: `f${String(i).padStart(2, '0')}.txt`, kind: 'file' }));
    vi.mocked(listDir).mockImplementation(async () => ({ entries: many }));
    const p = getProvider('folder');
    const c = await p.collect({ type: 'folder', ref: 'big' });
    expect(c.tree).toContain('…(已截断)');
  });

  it('collect(folder) 根目录读取失败返回 null', async () => {
    vi.mocked(listDir).mockRejectedValueOnce(new Error('denied'));
    const p = getProvider('folder');
    await expect(p.collect({ type: 'folder', ref: 'gone' })).resolves.toBeNull();
  });

  it('端到端:assembleContext 组文件夹 mention', async () => {
    vi.mocked(listDir).mockImplementation(async (dir = '') =>
      dir === 'root' ? { entries: [{ name: 'a.txt', kind: 'file' }] } : { entries: [] },
    );
    const { xml, dropped } = await assembleContext([{ type: 'folder', ref: 'root' }]);
    expect(dropped).toEqual([]);
    expect(xml).toContain('<evo-context>');
    expect(xml).toContain('<dir_summary path="root">');
  });
});
