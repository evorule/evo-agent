// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect } from 'vitest';
import {
  registerCommand,
  unregisterCommand,
  executeCommand,
  getCommand,
  listCommands,
} from '../src/lib/commands.js';

describe('registerCommand / getCommand', () => {
  it('注册后可取回,字段齐全', () => {
    registerCommand({ id: 'test.cmd.a', title: '测试: 命令 A', category: '测试', run: () => 1 });
    const cmd = getCommand('test.cmd.a');
    expect(cmd).not.toBeNull();
    expect(cmd.title).toBe('测试: 命令 A');
    expect(cmd.category).toBe('测试');
    expect(cmd.keybinding).toBeNull();
    expect(cmd.when).toBe('');
    unregisterCommand('test.cmd.a');
  });

  it('缺 id/title/run 抛错', () => {
    expect(() => registerCommand({ title: 'x', run: () => {} })).toThrow();
    expect(() => registerCommand({ id: 't', run: () => {} })).toThrow();
    expect(() => registerCommand({ id: 't', title: 'x' })).toThrow();
  });

  it('同 id 重复注册 = 覆盖(幂等)', () => {
    registerCommand({ id: 'test.cmd.dup', title: 'v1', run: () => 'a' });
    registerCommand({ id: 'test.cmd.dup', title: 'v2', run: () => 'b' });
    expect(listCommands().filter((c) => c.id === 'test.cmd.dup')).toHaveLength(1);
    expect(executeCommand('test.cmd.dup')).toBe('b');
    unregisterCommand('test.cmd.dup');
  });
});

describe('executeCommand', () => {
  it('执行并透传参数与返回值', () => {
    registerCommand({ id: 'test.cmd.exec', title: 'x', run: (a, b) => a + b });
    expect(executeCommand('test.cmd.exec', 2, 3)).toBe(5);
    unregisterCommand('test.cmd.exec');
  });

  it('未知命令抛错', () => {
    expect(() => executeCommand('test.cmd.missing')).toThrow(/未知命令/);
  });

  it('异步 run 返回 promise', async () => {
    registerCommand({ id: 'test.cmd.async', title: 'x', run: async () => 'ok' });
    await expect(executeCommand('test.cmd.async')).resolves.toBe('ok');
    unregisterCommand('test.cmd.async');
  });
});

describe('unregisterCommand / listCommands', () => {
  it('注销后不可执行;重复注销返回 false', () => {
    registerCommand({ id: 'test.cmd.del', title: 'x', run: () => {} });
    expect(unregisterCommand('test.cmd.del')).toBe(true);
    expect(getCommand('test.cmd.del')).toBeNull();
    expect(unregisterCommand('test.cmd.del')).toBe(false);
  });

  it('listCommands 按 分类→标题 排序(确定性次序)', () => {
    registerCommand({ id: 't1', title: '乙', category: '视图', run: () => {} });
    registerCommand({ id: 't2', title: '甲', category: '文件', run: () => {} });
    registerCommand({ id: 't3', title: '丙', category: '视图', run: () => {} });
    const list = listCommands().filter((c) => ['t1', 't2', 't3'].includes(c.id));
    // 期望序与实现共用同一 collator 口径:先分类后标题,断言的是排序稳定性而非硬编码拼音序
    const cmp = new Intl.Collator('zh-Hans-CN').compare;
    const expected = [...list]
      .sort((a, b) => cmp(a.category, b.category) || cmp(a.title, b.title))
      .map((c) => c.id);
    expect(list.map((c) => c.id)).toEqual(expected);
    // 同分类必相邻(分组渲染的前提)
    const cats = list.map((c) => c.category);
    expect(cats).toEqual([...new Set(cats)].flatMap((cat) => cats.filter((x) => x === cat)));
    for (const id of ['t1', 't2', 't3']) unregisterCommand(id);
  });
});
