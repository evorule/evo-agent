// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// Inline Chat 预览行级 diff + .evoignore v1 子集匹配 单测
import { describe, it, expect, beforeEach } from 'vitest';
import { diffLines, summarizeDiff } from '../src/lib/inline-chat-diff.js';
import { parseEvoignore, isIgnored, parseIgnoreLines, matchIgnore } from '../src/lib/evoignore.js';

describe('inline-chat-diff 行级 LCS', () => {
  it('纯新增', () => {
    const rows = diffLines('a\nb', 'a\nb\nc');
    expect(rows.some((r) => r.type === 'add' && r.text === 'c')).toBe(true);
  });
  it('纯删除', () => {
    const rows = diffLines('a\nb\nc', 'a\nc');
    expect(rows.some((r) => r.type === 'del' && r.text === 'b')).toBe(true);
  });
  it('重排/替换产出 del+add 混合', () => {
    const rows = diffLines('function old() {}', 'function renamed() {}');
    expect(rows.some((r) => r.type === 'del')).toBe(true);
    expect(rows.some((r) => r.type === 'add')).toBe(true);
  });
  it('超大输入走全量替换语义(内存护栏)', () => {
    // 输入须越过护栏阈值:diffLines 按 n*m>4_000_000 cells 触发全量替换,
    // 2101*2101≈4.41M 越阈;1500 行(≈2.25M)不触发,属正常精确 diff 区间
    const big = 'x\n'.repeat(2100);
    const rows = diffLines(big, big + 'y');
    expect(rows.length).toBeLessThanOrEqual(2);
  });
  it('summarizeDiff 聚合删除行号与新增块', () => {
    const { deletedLines, addedBlocks } = summarizeDiff([
      { type: 'same', text: 'a' },
      { type: 'del', text: 'old', oldLine: 2 },
      { type: 'add', text: 'new1', newLine: 2 },
      { type: 'add', text: 'new2', newLine: 3 },
    ]);
    expect(deletedLines).toEqual([2]);
    expect(addedBlocks.length).toBe(1);
    expect(addedBlocks[0].lines).toEqual(['new1', 'new2']);
  });
});

describe('evoignore v1 子集匹配', () => {
  beforeEach(() => {
    parseEvoignore(['config/', '*.secret', 'build.js'].join('\n'));
  });
  it('目录前缀命中', () => {
    expect(isIgnored('config/main.json')).toBe(true);
    expect(isIgnored('src/config/main.json')).toBe(true);
  });
  it('扩展名命中', () => {
    expect(isIgnored('a/b.secret')).toBe(true);
  });
  it('简式文件名命中', () => {
    expect(isIgnored('build.js')).toBe(true);
    expect(isIgnored('src/build.js')).toBe(true);
  });
  it('未命中放行', () => {
    expect(isIgnored('src/main.rs')).toBe(false);
    expect(isIgnored('configs/a.json')).toBe(false);
  });
  it('注释与空行跳过', () => {
    parseEvoignore(['# comment', '', 'target/', ''].join('\n'));
    expect(isIgnored('target/a')).toBe(true);
    expect(isIgnored('target')).toBe(true);
  });
});

describe('evoignore v1.1 negation 与根锚定(gitignore 语义;Explorer .gitignore 消费)', () => {
  it('negation 例外重新包含(最后命中规则胜出)', () => {
    parseEvoignore(['.env', '.env.*', '!.env.example'].join('\n'));
    expect(isIgnored('.env')).toBe(true);
    expect(isIgnored('.env.local')).toBe(true);
    expect(isIgnored('.env.example')).toBe(false); // 例外胜出
  });
  it('目录例外:被忽略目录的重新包含子树', () => {
    parseEvoignore(['data/', '!src/agent/data/'].join('\n'));
    expect(isIgnored('data/db.sqlite')).toBe(true);
    expect(isIgnored('data')).toBe(true);
    expect(isIgnored('src/agent/data/foo.json')).toBe(false); // 例外目录内容放行
    expect(isIgnored('src/agent/data')).toBe(false);
  });
  it('根锚定:前导 / 只命中根下路径', () => {
    parseEvoignore(['/evo-agent.toml', '/build/'].join('\n'));
    expect(isIgnored('evo-agent.toml')).toBe(true);
    expect(isIgnored('src/evo-agent.toml')).toBe(false); // 嵌套同名放行
    expect(isIgnored('build/out.js')).toBe(true);
    expect(isIgnored('src/build/out.js')).toBe(false);
  });
  it('含路径分隔符的规则天然根锚定(gitignore 标准)', () => {
    parseEvoignore('scripts/ops/ops.local.json');
    expect(isIgnored('scripts/ops/ops.local.json')).toBe(true);
    expect(isIgnored('a/scripts/ops/ops.local.json')).toBe(false);
  });
  it('parseIgnoreLines/matchIgnore 纯函数(独立规则集,不触单例)', () => {
    const mine = parseIgnoreLines('*.log\n!keep.log');
    parseEvoignore('config/'); // 单例被改写,不影响 mine
    expect(matchIgnore(mine, 'a/b.log')).toBe(true);
    expect(matchIgnore(mine, 'keep.log')).toBe(false);
    expect(matchIgnore(mine, 'config/x.json')).toBe(false);
    expect(isIgnored('config/x.json')).toBe(true); // 单例照常工作
  });
});
