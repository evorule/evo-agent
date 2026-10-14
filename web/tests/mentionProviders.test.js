// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 内置 mention provider 单测:候选排序/文件夹摘要/文件截断链/发送前对账。
// lsp/index.js 以 mock 替身注入(避免 monaco 模块图),symbolStore 直接注入。
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';

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

vi.mock('../src/lib/lsp/index.js', async () => {
  const { writable } = await import('svelte/store');
  return { symbolStore: writable(new Map()) };
});

import { listDir, readFile } from '../src/lib/api.js';
import { registerMentionProviders, rankFiles, reconcileMentions } from '../src/lib/mentionProviders.js';
import { getProvider, assembleContext } from '../src/lib/mentions.js';
import { registerModel, releaseModel } from '../src/lib/modelRegistry.js';
import { symbolStore } from '../src/lib/lsp/index.js';

const INDEX = [
  { path: 'docs/readme.md', name: 'readme.md', isDir: false },
  { path: 'src/app.rs', name: 'app.rs', isDir: false },
  { path: 'src/lib', name: 'lib', isDir: true },
  { path: 'src/lib/fuzzy.js', name: 'fuzzy.js', isDir: false },
  { path: 'src/modules/ui/app.rs', name: 'app.rs', isDir: false },
];

// ensureFileIndex 爬树用(手输 token 解析走真实索引)
const TREE = {
  '': [
    { name: 'docs', kind: 'dir' },
    { name: 'src', kind: 'dir' },
  ],
  docs: [{ name: 'readme.md', kind: 'file' }],
  src: [
    { name: 'app.rs', kind: 'file' },
    { name: 'lib', kind: 'dir' },
    { name: 'modules', kind: 'dir' },
  ],
  'src/lib': [{ name: 'fuzzy.js', kind: 'file' }],
  'src/modules': [{ name: 'ui', kind: 'dir' }],
  'src/modules/ui': [{ name: 'app.rs', kind: 'file' }],
};

/** Monaco model 替身:sel 空/缺失=无选区;lines 供光标窗口取行 */
function fakeModel({ sel = null, lines = [], cursorLine = 1 } = {}) {
  return {
    getSelection: () => sel,
    getValueInRange: () => 'SELECTED',
    getLineCount: () => lines.length,
    getPosition: () => ({ lineNumber: cursorLine }),
    getLineContent: (n) => lines[n - 1] ?? '',
  };
}

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

  afterEach(() => {
    releaseModel('live.js');
    releaseModel('win.js');
    symbolStore.set(new Map());
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
    expect(s.summary).toBe('a.rs(1 行)');
  });

  it('截断链·选区:已打开且有选区只带选区', async () => {
    registerModel(
      'live.js',
      fakeModel({
        sel: { startLineNumber: 2, startColumn: 1, endLineNumber: 2, endColumn: 5 },
        lines: ['a', 'bcde', 'c'],
      }),
    );
    const p = getProvider('file');
    const c = await p.collect({ type: 'file', ref: 'live.js' });
    expect(c.mode).toBe('selection');
    expect(c.content).toBe('SELECTED');
    expect(c.startLine).toBe(2);
    const s = p.serialize({ type: 'file', ref: 'live.js' }, c);
    expect(s.xml).toContain('<file_content path="live.js" lines="2-2" mode="selection">');
    expect(s.summary).toBe('live.js:2-2(选区)');
  });

  it('截断链·光标邻近:无选区取 ±120 行窗口', async () => {
    const lines = Array.from({ length: 400 }, (_, i) => `L${i + 1}`);
    registerModel('win.js', fakeModel({ lines, cursorLine: 200 }));
    const p = getProvider('file');
    const c = await p.collect({ type: 'file', ref: 'win.js' });
    expect(c.mode).toBe('window');
    expect(c.startLine).toBe(80);
    expect(c.endLine).toBe(320);
    expect(c.content.split('\n')).toHaveLength(241);
    const s = p.serialize({ type: 'file', ref: 'win.js' }, c);
    expect(s.summary).toBe('win.js 80-320/400 行(光标邻近)');
  });

  it('截断链·大纲:>600 行未打开降级符号清单,标已截断', async () => {
    const big = Array.from({ length: 601 }, (_, i) => `L${i + 1}`).join('\n');
    vi.mocked(readFile).mockResolvedValueOnce({ content: big });
    symbolStore.set(new Map([['big.rs', [{ name: 'main', kind: 12, range: { start: { line: 10 } } }]]]));
    const p = getProvider('file');
    const c = await p.collect({ type: 'file', ref: 'big.rs' });
    expect(c.mode).toBe('outline');
    expect(c.lineCount).toBe(601);
    expect(c.outline).toBe('11:fn main');
    const s = p.serialize({ type: 'file', ref: 'big.rs' }, c);
    expect(s.xml).toBe(
      '<file_outline path="big.rs" total_lines="601" truncated="true">\n11:fn main\n</file_outline>',
    );
    expect(s.summary).toBe('big.rs(大纲,已截断,共 601 行)');
  });

  it('截断链·大纲:无符号取文件首段兜底', async () => {
    const big = Array.from({ length: 700 }, (_, i) => `L${i + 1}`).join('\n');
    vi.mocked(readFile).mockResolvedValueOnce({ content: big });
    const c = await getProvider('file').collect({ type: 'file', ref: 'big2.rs' });
    expect(c.mode).toBe('outline');
    expect(c.outline.split('\n')).toHaveLength(120);
    expect(c.outline.split('\n')[0]).toBe('L1');
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

describe('reconcileMentions(发送前对账)', () => {
  beforeEach(() => {
    registerMentionProviders();
    vi.mocked(listDir).mockImplementation(async (dir = '') => ({ entries: TREE[dir] || [] }));
  });

  it('token 已被用户删除 → 移除元数据', async () => {
    const { kept, removed, added } = await reconcileMentions('正文已删掉引用', [
      { type: 'file', ref: 'docs/readme.md', insertAt: 0, token: '@readme.md' },
    ]);
    expect(kept).toHaveLength(0);
    expect(removed).toHaveLength(1);
    expect(added).toEqual([]);
  });

  it('token 仍在文本中 → 元数据保留', async () => {
    const m = { type: 'file', ref: 'src/lib/fuzzy.js', insertAt: 4, token: '@fuzzy.js' };
    const { kept, removed, added } = await reconcileMentions('看下 @fuzzy.js 的打分', [m]);
    expect(kept).toEqual([m]);
    expect(removed).toEqual([]);
    expect(added).toEqual([]);
  });

  it('手输 @文件名:索引唯一命中自动登记', async () => {
    const { kept, added } = await reconcileMentions('参考 @readme.md 写说明', []);
    expect(added).toEqual(['@readme.md']);
    expect(kept).toEqual([
      { type: 'file', ref: 'docs/readme.md', insertAt: 3, token: '@readme.md' },
    ]);
  });

  it('手输 @文件名:索引歧义(同名多文件)不猜,保持纯文本', async () => {
    const { kept, added } = await reconcileMentions('对比 @app.rs 两处实现', []);
    expect(kept).toEqual([]);
    expect(added).toEqual([]);
  });

  it('已登记 token 不重复解析', async () => {
    const m = { type: 'file', ref: 'docs/readme.md', insertAt: 0, token: '@readme.md' };
    const { kept, added } = await reconcileMentions('@readme.md 再看一遍', [m]);
    expect(kept).toEqual([m]);
    expect(added).toEqual([]);
  });
});
