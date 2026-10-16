// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect, beforeEach } from 'vitest';
import {
  MENTION_GROUPS,
  registerProvider,
  getProvider,
  registeredTypes,
  resetProviders,
  assembleContext,
  collectCandidates,
  xmlEscape,
  xmlBlock,
  estimateComposerTokens,
  estimateTokens,
  tokenLevel,
  TOKEN_LIMITS,
} from '../src/lib/mentions.js';

// 假 provider:ref='missing.rs' 时 collect 失败,ctx 原样透传断言
function makeFileProvider() {
  return {
    seenCtx: null,
    candidates() {
      return [];
    },
    async collect(m, ctx) {
      this.seenCtx = ctx;
      return m.ref === 'missing.rs' ? null : { content: 'hello' };
    },
    serialize(m, c) {
      return {
        xml: xmlBlock('file_content', { path: m.ref }, c.content),
        summary: `${m.ref}(5 行)`,
      };
    },
  };
}

describe('provider 注册表', () => {
  it('三接口齐全才允许注册', () => {
    expect(registerProvider('good', makeFileProvider())).toBe(true);
    expect(registerProvider('bad1', { collect: () => {} })).toBe(false);
    expect(registerProvider('bad2', { candidates: () => {}, serialize: () => {} })).toBe(false);
    expect(registerProvider('', {})).toBe(false);
    expect(registerProvider(null, makeFileProvider())).toBe(false);
    expect(getProvider('bad1')).toBeNull();
    expect(getProvider('good')).not.toBeNull();
    expect(registeredTypes()).toContain('good');
  });

  it('同类型重复注册时替换', () => {
    const a = makeFileProvider();
    const b = makeFileProvider();
    expect(registerProvider('dup', a)).toBe(true);
    expect(registerProvider('dup', b)).toBe(true);
    expect(getProvider('dup')).toBe(b);
  });

  it('分组顺序固定且含六组', () => {
    expect(MENTION_GROUPS.map((g) => g.type)).toEqual([
      'file',
      'folder',
      'symbol',
      'selection',
      'terminal',
      'problems',
    ]);
  });
});

describe('assembleContext', () => {
  it('按 mentions 顺序组装 <evo-context>,used 带回显摘要', async () => {
    registerProvider('file', makeFileProvider());
    const { xml, used, dropped } = await assembleContext(
      [
        { type: 'file', ref: 'b.rs' },
        { type: 'file', ref: 'a.rs' },
      ],
      { fileIndex: [] },
    );
    expect(xml).toContain('<evo-context>');
    const iB = xml.indexOf('<file_content path="b.rs">');
    const iA = xml.indexOf('<file_content path="a.rs">');
    expect(iB).toBeGreaterThan(-1);
    expect(iA).toBeGreaterThan(iB); // 顺序保持
    expect(used).toEqual([
      { type: 'file', ref: 'b.rs', summary: 'b.rs(5 行)' },
      { type: 'file', ref: 'a.rs', summary: 'a.rs(5 行)' },
    ]);
    expect(dropped).toEqual([]);
  });

  it('ctx 透传给 provider.collect', async () => {
    const p = makeFileProvider();
    registerProvider('ctxprobe', p);
    const ctx = { fileIndex: ['x.rs'] };
    await assembleContext([{ type: 'ctxprobe', ref: 'x.rs' }], ctx);
    expect(p.seenCtx).toBe(ctx);
  });

  it('collect 失败降级:记入 dropped,不阻断其余 mention', async () => {
    registerProvider('file', makeFileProvider());
    const { xml, used, dropped } = await assembleContext([
      { type: 'file', ref: 'missing.rs' },
      { type: 'file', ref: 'ok.rs' },
    ]);
    expect(dropped).toEqual([{ type: 'file', ref: 'missing.rs', reason: 'collect_failed' }]);
    expect(used.map((u) => u.ref)).toEqual(['ok.rs']);
    expect(xml).not.toContain('missing.rs');
  });

  it('未知类型记 no_provider,畸形 mention 记 malformed', async () => {
    const { dropped } = await assembleContext([
      { type: 'ghost', ref: 'x' },
      { type: 'file' },
      null,
    ]);
    expect(dropped.map((d) => d.reason)).toEqual(['no_provider', 'malformed', 'malformed']);
  });

  it('全部失败时 xml 为空串', async () => {
    registerProvider('file', makeFileProvider());
    const { xml, used } = await assembleContext([{ type: 'file', ref: 'missing.rs' }]);
    expect(xml).toBe('');
    expect(used).toEqual([]);
  });

  it('serialize 抛异常按 empty 降级', async () => {
    registerProvider('boom', {
      candidates: () => [],
      collect: async () => ({}),
      serialize: () => {
        throw new Error('x');
      },
    });
    const { dropped } = await assembleContext([{ type: 'boom', ref: 'y' }]);
    expect(dropped).toEqual([{ type: 'boom', ref: 'y', reason: 'empty' }]);
  });

  it('空 mentions 返回空 xml', async () => {
    const { xml, used, dropped } = await assembleContext([]);
    expect(xml).toBe('');
    expect(used).toEqual([]);
    expect(dropped).toEqual([]);
  });
});

describe('xml 工具', () => {
  it('xmlEscape 转义四类字符', () => {
    expect(xmlEscape('a<b>&"c"d')).toBe('a&lt;b&gt;&amp;&quot;c&quot;d');
  });

  it('xmlBlock 有 body 时换行包体', () => {
    expect(xmlBlock('file_content', { path: 'a.rs' }, 'hi')).toBe(
      '<file_content path="a.rs">\nhi\n</file_content>',
    );
  });

  it('xmlBlock 空 body 自闭合', () => {
    expect(xmlBlock('terminal_output', { lines: '1-2' }, '')).toBe('<terminal_output lines="1-2"/>');
  });

  it('xmlBlock 属性值转义且 null 属性省略', () => {
    expect(xmlBlock('t', { p: 'a"b', q: null }, 'x')).toBe('<t p="a&quot;b">\nx\n</t>');
  });
});

describe('collectCandidates', () => {
  // 注册表为模块单例:本组涉及全局截取/分组顺序断言,先清场保证隔离
  beforeEach(() => {
    resetProviders();
  });

  function staticProvider(items) {
    return {
      calls: 0,
      async candidates() {
        this.calls++;
        return items;
      },
      async collect() {
        return null;
      },
      serialize() {
        return null;
      },
    };
  }

  it('同实例多类型注册只询问一次,分组按 MENTION_GROUPS 顺序', async () => {
    const p = staticProvider([
      { type: 'folder', ref: 'src', label: 'src', detail: 'src', positions: [] },
      { type: 'file', ref: 'a.rs', label: 'a.rs', detail: 'a.rs', positions: [] },
    ]);
    registerProvider('file', p);
    registerProvider('folder', p);
    const { groups } = await collectCandidates('a');
    expect(p.calls).toBe(1);
    expect(groups.map((g) => g.type)).toEqual(['file', 'folder']);
    expect(groups[0].label).toBe('文件');
    expect(groups[0].items[0].ref).toBe('a.rs');
  });

  it('全局 limit 截取并置 truncated', async () => {
    const items = Array.from({ length: 60 }, (_, i) => ({
      type: 'file',
      ref: `f${i}.rs`,
      label: `f${i}`,
      detail: '',
      positions: [],
    }));
    registerProvider('solo', staticProvider(items));
    const { groups, total, truncated } = await collectCandidates('', {}, 50);
    expect(total).toBe(60);
    expect(truncated).toBe(true);
    expect(groups[0].items).toHaveLength(50);
  });

  it('组间公平配额:大组不独占预算,其余组保底可见', async () => {
    const files = Array.from({ length: 50 }, (_, i) => ({
      type: 'file',
      ref: `f${i}.rs`,
      label: `f${i}`,
      detail: '',
      positions: [],
    }));
    registerProvider('many', staticProvider(files));
    registerProvider(
      'few',
      staticProvider([
        { type: 'folder', ref: 'src', label: 'src', detail: '', positions: [] },
        { type: 'selection', ref: 'a.rs:1-2', label: '选区', detail: '', positions: [] },
      ]),
    );
    const { groups } = await collectCandidates('', {}, 50);
    const byType = Object.fromEntries(groups.map((g) => [g.type, g.items.length]));
    // 首轮限额=25:file 与 folder/selection 各保底可见,二轮分摊剩余
    expect(byType['file']).toBeGreaterThanOrEqual(25);
    expect(byType['file']).toBeLessThanOrEqual(48);
    expect(byType['folder']).toBe(1);
    expect(byType['selection']).toBe(1);
  });

  it('candidates 抛错的 provider 不阻断其余', async () => {
    registerProvider('boom2', {
      candidates() {
        throw new Error('x');
      },
      collect: async () => null,
      serialize: () => null,
    });
    registerProvider('calm', staticProvider([{ type: 'file', ref: 'b.rs', label: 'b', detail: '', positions: [] }]));
    const { groups, total } = await collectCandidates('');
    expect(total).toBeGreaterThanOrEqual(1);
    expect(groups.some((g) => g.items.some((it) => it.ref === 'b.rs'))).toBe(true);
  });

  it('未注册进 MENTION_GROUPS 的类型排在已知组之后,标签用 type 原文', async () => {
    registerProvider('widgets', staticProvider([{ type: 'widget', ref: 'w1', label: 'w', detail: '', positions: [] }]));
    const { groups } = await collectCandidates('');
    const w = groups.find((g) => g.type === 'widget');
    expect(w).toBeTruthy();
    expect(w.label).toBe('widget');
    expect(groups[groups.length - 1].type).toBe('widget');
  });
});

describe('token 估算', () => {
  it('estimateTokens ≈ 4 字符/token,向上取整', () => {
    expect(estimateTokens('')).toBe(0);
    expect(estimateTokens(null)).toBe(0);
    expect(estimateTokens('abcd')).toBe(1);
    expect(estimateTokens('abcde')).toBe(2);
  });

  it('tokenLevel 分段边界', () => {
    expect(tokenLevel(TOKEN_LIMITS.safe - 1)).toBe('safe');
    expect(tokenLevel(TOKEN_LIMITS.safe)).toBe('warn');
    expect(tokenLevel(TOKEN_LIMITS.warn)).toBe('warn');
    expect(tokenLevel(TOKEN_LIMITS.warn + 1)).toBe('over');
  });

  it('estimateComposerTokens = 正文 + @文件字节数权重(组装口径)', () => {
    expect(estimateComposerTokens('', [])).toBe(0);
    expect(estimateComposerTokens('abcd', [])).toBe(1);
    const mentions = [
      { type: 'file', ref: 'a.js', size: 4000 },
      { type: 'folder', ref: 'src' },
      { type: 'file', ref: 'b.js' },
      { type: 'selection', ref: 'a.js:1-2' },
    ];
    // 4000 字节文件计入;folder/无 size/动态源不计(低估不虚报)
    expect(estimateComposerTokens('abcd', mentions)).toBe(Math.ceil((4 + 4000) / 4));
  });

  it('estimateComposerTokens 超 32k 判 over(大文件警示生效)', () => {
    const mentions = [{ type: 'file', ref: 'big.js', size: 200000 }];
    expect(tokenLevel(estimateComposerTokens('', mentions))).toBe('over');
  });
});
