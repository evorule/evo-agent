// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// mentionSources(选区/终端/问题 provider)单测。pty.js 以 mock 替身注入
// (避免 xterm/CSS/WS 真依赖),实例注册表经导出的 instances Map 直接操纵。
import { describe, it, expect, vi, beforeAll, afterEach } from 'vitest';

vi.mock('../src/lib/pty.js', () => {
  const instances = new Map();
  return {
    instances,
    getActiveTerminal: vi.fn(() => null),
    setActiveTerminal: vi.fn(),
  };
});

import { secondaryGroups, activeGroupId } from '../src/lib/editorGroups.js';
import { registerModel } from '../src/lib/modelRegistry.js';
import { activePath } from '../src/lib/stores.js';
import { problems, SEVERITY } from '../src/lib/diagnostics.js';
import { instances, getActiveTerminal } from '../src/lib/pty.js';
import { getProvider } from '../src/lib/mentions.js';
import {
  registerMentionSourceProviders,
  getActiveSelection,
  activeTerminalRef,
  problemsTotal,
  readTerminalTail,
} from '../src/lib/mentionSources.js';

function fakeModel(selection, text) {
  return {
    getSelection: () => selection,
    getValueInRange: () => text,
  };
}

function fakeTermInst(termId, state, lines) {
  return {
    termId,
    state,
    term: {
      buffer: {
        active: {
          length: lines.length,
          getLine: (y) => ({ translateToString: () => lines[y] }),
        },
      },
    },
  };
}

function mkMarker(message, severity, line = 1, col = 1) {
  return {
    message,
    severity,
    startLineNumber: line,
    startColumn: col,
    endLineNumber: line,
    endColumn: col + 5,
  };
}

const EMPTY_PROBLEMS = { groups: [], counts: { errors: 0, warnings: 0, infos: 0, hints: 0 } };
const SEL = { startLineNumber: 2, startColumn: 1, endLineNumber: 4, endColumn: 10 };

beforeAll(() => {
  registerMentionSourceProviders();
});

afterEach(() => {
  activeGroupId.set('primary');
  secondaryGroups.set([]);
  activePath.set(null);
  instances.clear();
  vi.mocked(getActiveTerminal).mockReset();
  problems.set(EMPTY_PROBLEMS);
});

describe('注册', () => {
  it('注册 selection/terminal/problems 三个 provider(三接口齐全)', () => {
    for (const t of ['selection', 'terminal', 'problems']) {
      const p = getProvider(t);
      expect(p).not.toBeNull();
      expect(typeof p.candidates).toBe('function');
      expect(typeof p.collect).toBe('function');
      expect(typeof p.serialize).toBe('function');
    }
  });
});

describe('选区 provider', () => {
  it('candidates 恒为空(入口由快捷项承载)', async () => {
    expect(await getProvider('selection').candidates('x')).toEqual([]);
  });

  it('主组:activePath + model 取非空选区', () => {
    activePath.set('src/a.js');
    registerModel('src/a.js', fakeModel(SEL, 'const a = 1;'));
    expect(getActiveSelection()).toEqual({
      path: 'src/a.js',
      text: 'const a = 1;',
      startLine: 2,
      endLine: 4,
    });
  });

  it('空选区(行列相同)返回 null', () => {
    activePath.set('src/a.js');
    registerModel(
      'src/a.js',
      fakeModel({ startLineNumber: 3, startColumn: 5, endLineNumber: 3, endColumn: 5 }, ''),
    );
    expect(getActiveSelection()).toBeNull();
  });

  it('无活动文件/无 model 返回 null', () => {
    expect(getActiveSelection()).toBeNull();
    activePath.set('src/missing.js');
    expect(getActiveSelection()).toBeNull();
  });

  it('副组:取组内激活路径', () => {
    secondaryGroups.set([{ id: 'split-1', paths: ['b.js'], activePath: 'b.js', width: '50%' }]);
    activeGroupId.set('split-1');
    registerModel('b.js', fakeModel(SEL, 'b'));
    expect(getActiveSelection().path).toBe('b.js');
  });

  it('serialize 产 <selection> 块与摘要', () => {
    const out = getProvider('selection').serialize(
      {},
      { path: 'src/a.js', text: 'abc', startLine: 2, endLine: 3 },
    );
    expect(out.xml).toBe('<selection path="src/a.js" lines="2-3">\nabc\n</selection>');
    expect(out.summary).toBe('src/a.js:2-3(选区 2 行)');
  });
});

describe('终端 provider', () => {
  it('activeTerminalRef 来自 pty 活动上报', () => {
    expect(activeTerminalRef()).toBeNull();
    vi.mocked(getActiveTerminal).mockReturnValue({ termId: 'term-2' });
    expect(activeTerminalRef()).toBe('term-2');
  });

  it('candidates:空查询不出候选;非空按 termId 子串过滤', async () => {
    instances.set('term-1', fakeTermInst('term-1', 'ready', []));
    instances.set('term-2', fakeTermInst('term-2', 'exited', []));
    expect(await getProvider('terminal').candidates('')).toEqual([]);
    expect(await getProvider('terminal').candidates('   ')).toEqual([]);
    const all = await getProvider('terminal').candidates('term');
    expect(all.map((i) => i.ref)).toEqual(['term-1', 'term-2']);
    const one = await getProvider('terminal').candidates('2');
    expect(one).toHaveLength(1);
    expect(one[0].ref).toBe('term-2');
    expect(one[0].detail).toContain('exited');
  });

  it('readTerminalTail:取尾部并剔除尾随空行', () => {
    const inst = fakeTermInst('term-1', 'ready', ['a', 'b', '', '   ']);
    expect(readTerminalTail(inst)).toEqual({
      content: 'a\nb',
      count: 2,
      truncated: false,
    });
  });

  it('readTerminalTail:超过上限取尾部 N 行并标 truncated', () => {
    const inst = fakeTermInst('term-1', 'ready', ['l1', 'l2', 'l3', 'l4']);
    expect(readTerminalTail(inst, 2)).toEqual({ content: 'l3\nl4', count: 2, truncated: true });
  });

  it('readTerminalTail:全空缓冲返回 null', () => {
    expect(readTerminalTail(fakeTermInst('term-1', 'ready', ['', '']))).toBeNull();
    expect(readTerminalTail(null)).toBeNull();
  });

  it('collect:按 ref 取实例尾部;未知 ref 返回 null', async () => {
    instances.set('term-1', fakeTermInst('term-1', 'ready', ['$ ls', 'a.js  b.js']));
    const got = await getProvider('terminal').collect({ ref: 'term-1' });
    expect(got.termId).toBe('term-1');
    expect(got.content).toBe('$ ls\na.js  b.js');
    expect(got.truncated).toBe(false);
    expect(await getProvider('terminal').collect({ ref: 'term-9' })).toBeNull();
  });

  it('serialize 产 <terminal_output> 块(截断态带标记)', () => {
    const out = getProvider('terminal').serialize(
      {},
      { termId: 'term-1', content: 'x\ny', count: 2, truncated: true },
    );
    expect(out.xml).toBe(
      '<terminal_output term="term-1" lines="2" truncated="true">\nx\ny\n</terminal_output>',
    );
    expect(out.summary).toBe('term-1(终端输出 2 行,已截断)');
  });
});

describe('问题 provider', () => {
  const SNAP = {
    groups: [
      { path: 'src/x.js', markers: [mkMarker('oops', SEVERITY.Error, 12, 3)], counts: { errors: 1, warnings: 0, infos: 0, hints: 0 } },
      { path: 'docs/y.md', markers: [mkMarker('lint', SEVERITY.Warning, 2)], counts: { errors: 0, warnings: 1, infos: 0, hints: 0 } },
    ],
    counts: { errors: 1, warnings: 1, infos: 0, hints: 0 },
  };

  it('problemsTotal 汇总全工作区条数', () => {
    expect(problemsTotal()).toBe(0);
    problems.set(SNAP);
    expect(problemsTotal()).toBe(2);
  });

  it('candidates:空查询不出候选;非空按路径模糊匹配', async () => {
    problems.set(SNAP);
    expect(await getProvider('problems').candidates('')).toEqual([]);
    const got = await getProvider('problems').candidates('x.js');
    expect(got).toHaveLength(1);
    expect(got[0].ref).toBe('src/x.js');
    expect(got[0].detail).toContain('共 1 条');
  });

  it('collect:ref=path 取该文件条目;未知路径返回 null', async () => {
    problems.set(SNAP);
    const got = await getProvider('problems').collect({ ref: 'src/x.js' });
    expect(got.scope).toBe('src/x.js');
    expect(got.text).toBe('src/x.js:12:3 error oops');
    expect(got.count).toBe(1);
    expect(got.truncated).toBe(false);
    expect(await getProvider('problems').collect({ ref: 'src/none.js' })).toBeNull();
  });

  it('collect:ref=* 取全工作区条目', async () => {
    problems.set(SNAP);
    const got = await getProvider('problems').collect({ ref: '*' });
    expect(got.count).toBe(2);
    expect(got.text.split('\n')).toEqual([
      'src/x.js:12:3 error oops',
      'docs/y.md:2:1 warning lint',
    ]);
  });

  it('collect:超出上限截 50 条并标 truncated', async () => {
    const markers = Array.from({ length: 51 }, (_, i) => mkMarker(`m${i}`, SEVERITY.Info, i + 1));
    problems.set({
      groups: [{ path: 'big.js', markers, counts: { errors: 0, warnings: 0, infos: 51, hints: 0 } }],
      counts: { errors: 0, warnings: 0, infos: 51, hints: 0 },
    });
    const got = await getProvider('problems').collect({ ref: 'big.js' });
    expect(got.count).toBe(50);
    expect(got.truncated).toBe(true);
    expect(got.text.split('\n')).toHaveLength(50);
  });

  it('serialize:scope 按引用面呈现,summary 带条数', () => {
    const ws = getProvider('problems').serialize({}, { scope: '*', text: 'a', count: 2, truncated: false });
    expect(ws.xml).toBe('<problems scope="workspace" count="2">\na\n</problems>');
    expect(ws.summary).toBe('工作区问题 ×2');
    const file = getProvider('problems').serialize({}, { scope: 'src/x.js', text: 'a', count: 1, truncated: true });
    expect(file.xml).toBe('<problems scope="src/x.js" count="1" truncated="true">\na\n</problems>');
    expect(file.summary).toBe('src/x.js(问题 ×1)');
  });
});
