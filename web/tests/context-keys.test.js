// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect } from 'vitest';
import { get } from 'svelte/store';
import { contextKeys, setContextKey, evaluateWhen } from '../src/lib/context-keys.js';
import { tabs } from '../src/lib/stores.js';

describe('evaluateWhen(v1 切分求值)', () => {
  const T = { a: true, b: false, c: true };

  it('空串恒真', () => {
    expect(evaluateWhen('', T)).toBe(true);
    expect(evaluateWhen('   ', T)).toBe(true);
    expect(evaluateWhen(null, T)).toBe(true);
  });

  it('单键真值', () => {
    expect(evaluateWhen('a', T)).toBe(true);
    expect(evaluateWhen('b', T)).toBe(false);
    expect(evaluateWhen('missing', T)).toBe(false); // 未知键 = falsy
  });

  it('一元 !', () => {
    expect(evaluateWhen('!a', T)).toBe(false);
    expect(evaluateWhen('!b', T)).toBe(true);
    expect(evaluateWhen('!!a', T)).toBe(true);
  });

  it('&& 全真才真', () => {
    expect(evaluateWhen('a && c', T)).toBe(true);
    expect(evaluateWhen('a && b', T)).toBe(false);
  });

  it('|| 一真即真', () => {
    expect(evaluateWhen('a || b', T)).toBe(true);
    expect(evaluateWhen('b || b', T)).toBe(false);
  });

  it('优先级:&& 高于 ||(先 || 切分)', () => {
    // a || b && c → a || (b && c)
    expect(evaluateWhen('a || b && c', T)).toBe(true);
    expect(evaluateWhen('b || a && b', T)).toBe(false);
    expect(evaluateWhen('b || b && c', T)).toBe(false);
  });

  it('混合:! 与 &&', () => {
    expect(evaluateWhen('!b && a', T)).toBe(true);
    expect(evaluateWhen('!a && b', T)).toBe(false);
  });
});

describe('上下文键 store', () => {
  it('setContextKey 置位并可订阅观察', () => {
    setContextKey('inPalette', true);
    expect(get(contextKeys).inPalette).toBe(true);
    setContextKey('inPalette', false);
    expect(get(contextKeys).inPalette).toBe(false);
  });

  it('tabsOpen 派生:有 tab 即真,清空即假', () => {
    const before = get(tabs);
    tabs.set([{ path: 'a.md', name: 'a.md', content: '', dirty: false, error: null }]);
    expect(get(contextKeys).tabsOpen).toBe(true);
    tabs.set([]);
    expect(get(contextKeys).tabsOpen).toBe(false);
    tabs.set(before);
  });

  it('同值置位不触发新对象(引用稳定)', () => {
    const s1 = get(contextKeys);
    setContextKey('chatFocus', s1.chatFocus);
    expect(get(contextKeys)).toBe(s1);
  });
});
