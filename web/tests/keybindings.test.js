// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect, afterEach } from 'vitest';
import {
  parseKeybinding,
  normalizeKeyEvent,
  formatKey,
  resolveKeybinding,
  getEffectiveRules,
  saveUserBindings,
  getEffectiveKeybinding,
  DEFAULT_KEYBINDINGS,
} from '../src/lib/keybindings.js';

describe('parseKeybinding', () => {
  it('已规范的串原样通过', () => {
    expect(parseKeybinding('ctrl+shift+p')).toBe('ctrl+shift+p');
    expect(parseKeybinding('ctrl+s')).toBe('ctrl+s');
  });

  it('修饰键重排为固定序', () => {
    expect(parseKeybinding('shift+ctrl+p')).toBe('ctrl+shift+p');
    expect(parseKeybinding('alt+ctrl+c')).toBe('ctrl+alt+c');
  });

  it('别名归一(cmd/win/super→meta,esc→escape,return→enter)', () => {
    expect(parseKeybinding('cmd+p')).toBe('meta+p');
    expect(parseKeybinding('win+p')).toBe('meta+p');
    expect(parseKeybinding('esc')).toBe('escape');
    expect(parseKeybinding('return')).toBe('enter');
  });

  it('非法形态返回 null', () => {
    expect(parseKeybinding('')).toBeNull();
    expect(parseKeybinding('ctrl')).toBeNull(); // 缺主键
    expect(parseKeybinding('ctrl+p+q')).toBeNull(); // 双主键
    expect(parseKeybinding('p+ctrl')).toBeNull(); // 修饰键在主键后
    expect(parseKeybinding(null)).toBeNull();
  });

  it('默认规则集全部为规范形态', () => {
    for (const rule of DEFAULT_KEYBINDINGS) {
      expect(parseKeybinding(rule.key)).toBe(rule.key);
    }
  });
});

describe('normalizeKeyEvent', () => {
  it('修饰键 + 主键组合', () => {
    expect(
      normalizeKeyEvent({ key: 'p', ctrlKey: true, shiftKey: true, altKey: false, metaKey: false }),
    ).toBe('ctrl+shift+p');
    expect(normalizeKeyEvent({ key: 'S', ctrlKey: true })).toBe('ctrl+s');
  });

  it('空格与别名键', () => {
    expect(normalizeKeyEvent({ key: ' ' })).toBe('space');
    expect(normalizeKeyEvent({ key: 'Escape', ctrlKey: false })).toBe('escape');
  });

  it('纯修饰键返回 null', () => {
    expect(normalizeKeyEvent({ key: 'Control', ctrlKey: true })).toBeNull();
    expect(normalizeKeyEvent({ key: 'Shift', shiftKey: true })).toBeNull();
  });

  it('meta 修饰', () => {
    expect(normalizeKeyEvent({ key: 'p', metaKey: true })).toBe('meta+p');
  });
});

describe('formatKey', () => {
  it('显示形态', () => {
    expect(formatKey('ctrl+shift+p')).toBe('Ctrl+Shift+P');
    expect(formatKey('ctrl+w')).toBe('Ctrl+W');
    expect(formatKey('escape')).toBe('Escape');
    expect(formatKey(null)).toBe('');
  });
});

describe('resolveKeybinding(自底向上首条命中)', () => {
  it('后条遮蔽前条(用户覆盖语义)', () => {
    const rules = [
      { key: 'ctrl+p', command: 'a', when: '' },
      { key: 'ctrl+p', command: 'b', when: '' },
    ];
    expect(resolveKeybinding({ key: 'p', ctrlKey: true }, rules).command).toBe('b');
  });

  it('when 不命中的规则被跳过,回落前条', () => {
    const rules = [
      { key: 'ctrl+s', command: 'fallback', when: '' },
      { key: 'ctrl+s', command: 'guarded', when: 'tabsOpen' },
    ];
    expect(resolveKeybinding({ key: 's', ctrlKey: true }, rules, { tabsOpen: false }).command).toBe(
      'fallback',
    );
    expect(resolveKeybinding({ key: 's', ctrlKey: true }, rules, { tabsOpen: true }).command).toBe(
      'guarded',
    );
  });

  it('纯修饰键/未注册键返回 null', () => {
    expect(resolveKeybinding({ key: 'Control', ctrlKey: true }, [])).toBeNull();
    expect(resolveKeybinding({ key: 'x', ctrlKey: true }, [])).toBeNull();
  });
});

describe('用户覆盖层', () => {
  afterEach(() => {
    delete globalThis.localStorage;
  });

  function withLocalStorage() {
    const map = new Map();
    globalThis.localStorage = {
      getItem: (k) => (map.has(k) ? map.get(k) : null),
      setItem: (k, v) => map.set(k, v),
      removeItem: (k) => map.delete(k),
    };
    return map;
  }

  it('无 localStorage 时有效规则 = 默认规则(降级安全)', () => {
    expect(getEffectiveRules()).toEqual(DEFAULT_KEYBINDINGS);
  });

  it('覆盖层追加在默认规则之后 = 遮蔽默认键位', () => {
    withLocalStorage();
    saveUserBindings([{ key: 'ctrl+b', command: 'user.toggle' }]);
    const rules = getEffectiveRules();
    expect(rules.length).toBe(DEFAULT_KEYBINDINGS.length + 1);
    expect(rules[rules.length - 1].command).toBe('user.toggle');
    expect(resolveKeybinding({ key: 'b', ctrlKey: true }, rules).command).toBe('user.toggle');
    expect(getEffectiveKeybinding('user.toggle', rules)).toBe('Ctrl+B');
  });

  it('saveUserBindings 过滤非法条目', () => {
    const map = withLocalStorage();
    saveUserBindings([
      { key: 'ctrl+1', command: 'ok.cmd' },
      { key: '', command: 'bad' },
      null,
      { key: 'ctrl+2' },
    ]);
    const saved = JSON.parse(map.get('evo_keybindings'));
    expect(saved).toEqual([{ key: 'ctrl+1', command: 'ok.cmd' }]);
  });

  it('损坏的存储内容降级为空覆盖层', () => {
    withLocalStorage().set('evo_keybindings', '{broken json');
    expect(getEffectiveRules()).toEqual(DEFAULT_KEYBINDINGS);
  });
});
