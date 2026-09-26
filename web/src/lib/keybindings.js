// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 键位规则系统:规则四元组 {key, command, when}(args 留 v2)。
// 解析:keydown → 规范化 key 串 → 过滤 when 命中 → 自底向上首条命中
// (默认规则在前、用户覆盖在后 = 后者天然遮蔽前者,对齐 VS Code 追加语义)。
// Monaco 协调:Monaco 内建编辑器键(F12/Ctrl+D 等)不在此注册避免语义打架;
// Monaco addCommand 不阻止 keydown 冒泡,编辑器内键位(含 Ctrl+S)统一由
// 全局路由分发,不双注册。

import { get } from 'svelte/store';
import { contextKeys, evaluateWhen } from './context-keys.js';

export const USER_KEYBINDINGS_STORAGE_KEY = 'evo_keybindings';

/**
 * 默认规则集(次序即优先级,后条遮蔽前条)。
 * 保存命令的执行体由 EditorPane 自注册;编辑器内 Ctrl+S 亦由全局路由
 * 统一分发(when editorFocus 命中),本规则同样覆盖「焦点在编辑器之外但 tab 已开」的场景。
 */
export const DEFAULT_KEYBINDINGS = [
  { key: 'ctrl+shift+p', command: 'workbench.action.showCommands', when: '!inPalette' },
  { key: 'ctrl+p', command: 'workbench.action.file.quickOpen', when: '!inPalette' },
  { key: 'ctrl+s', command: 'workbench.action.file.save', when: 'editorFocus || tabsOpen' },
  { key: 'ctrl+w', command: 'workbench.action.file.closeTab', when: 'tabsOpen' }, // ⚠ 浏览器保留键,部分环境不可拦
  { key: 'ctrl+b', command: 'workbench.action.view.toggleExplorer', when: '!inPalette' },
  { key: 'ctrl+alt+c', command: 'workbench.action.view.toggleChat', when: '!inPalette' },
  { key: 'ctrl+j', command: 'workbench.action.view.togglePanel', when: '!inPalette' },
];

// ---- key 规范化 ----

const MODIFIER_ALIASES = { cmd: 'meta', super: 'meta', win: 'meta', control: 'ctrl', option: 'alt' };
const KEY_ALIASES = { esc: 'escape', return: 'enter', spacebar: 'space' };
const MODIFIER_ORDER = ['ctrl', 'alt', 'shift', 'meta'];

/** 规范化字符串键位:'ctrl+shift+p' 形态(修饰键固定序,key 小写)。非法返回 null。 */
export function parseKeybinding(str) {
  if (typeof str !== 'string') return null;
  const parts = str.trim().toLowerCase().split('+').filter(Boolean);
  if (parts.length === 0) return null;
  const mods = new Set();
  let key = null;
  for (const raw of parts) {
    const part = MODIFIER_ALIASES[raw] || raw;
    if (MODIFIER_ORDER.includes(part)) {
      if (key !== null) return null; // 修饰键出现在主键之后
      mods.add(part);
    } else if (key === null) {
      key = KEY_ALIASES[part] || part;
    } else {
      return null; // 多个主键
    }
  }
  if (!key) return null;
  const head = MODIFIER_ORDER.filter((m) => mods.has(m));
  return [...head, key].join('+');
}

/** event → 规范化 key 串。纯修饰键按下返回 null(非绑定键)。 */
export function normalizeKeyEvent(e) {
  if (!e || typeof e.key !== 'string') return null;
  const key = e.key === ' ' ? 'space' : e.key.toLowerCase();
  // 纯修饰键本身不构成绑定
  if (['control', 'alt', 'shift', 'meta'].includes(key)) return null;
  const parts = [];
  if (e.ctrlKey) parts.push('ctrl');
  if (e.altKey) parts.push('alt');
  if (e.shiftKey) parts.push('shift');
  if (e.metaKey) parts.push('meta');
  parts.push(KEY_ALIASES[key] || key);
  return parts.join('+');
}

/** 键位显示形态:'ctrl+shift+p' → 'Ctrl+Shift+P' */
export function formatKey(key) {
  if (!key) return '';
  return key
    .split('+')
    .map((p) => (p.length === 1 ? p.toUpperCase() : p.charAt(0).toUpperCase() + p.slice(1)))
    .join('+');
}

// ---- 规则解析 ----

/**
 * 解析命中:在 rules 中自底向上找「key 相等且 when 命中」的首条规则。
 * ctx 缺省时取当前上下文 store 快照。
 */
export function resolveKeybinding(e, rules, ctx = get(contextKeys)) {
  const key = normalizeKeyEvent(e);
  if (!key) return null;
  for (let i = rules.length - 1; i >= 0; i--) {
    const rule = rules[i];
    if (rule.key === key && evaluateWhen(rule.when || '', ctx)) return rule;
  }
  return null;
}

// ---- 用户覆盖层(localStorage;编辑 UI 归 B6,本层只留数据与读取) ----

function userBindingsUnsafe() {
  if (typeof localStorage === 'undefined') return [];
  try {
    const raw = JSON.parse(localStorage.getItem(USER_KEYBINDINGS_STORAGE_KEY) || '[]');
    if (!Array.isArray(raw)) return [];
    return raw
      .filter((r) => r && typeof r.key === 'string' && typeof r.command === 'string')
      .map((r) => ({ key: parseKeybinding(r.key), command: r.command, when: r.when || '' }))
      .filter((r) => r.key !== null);
  } catch {
    return [];
  }
}

/** 有效规则集:默认规则 + 用户覆盖(追加在后 = 遮蔽默认)。 */
export function getEffectiveRules() {
  return DEFAULT_KEYBINDINGS.concat(userBindingsUnsafe());
}

/** 写入用户覆盖层(B6 编辑 UI 的数据落点;格式 [{key, command}],非法条目丢弃) */
export function saveUserBindings(list) {
  if (typeof localStorage === 'undefined') return;
  localStorage.setItem(
    USER_KEYBINDINGS_STORAGE_KEY,
    JSON.stringify(
      (Array.isArray(list) ? list : []).filter(
        (r) =>
          r &&
          typeof r.key === 'string' &&
          r.key.trim() &&
          typeof r.command === 'string' &&
          r.command.trim(),
      ),
    ),
  );
}

/** 命令的生效键位(提示列显示):规则表自底向上找该命令的首条绑定;无则回退注册默认键。 */
export function getEffectiveKeybinding(commandId, rules = getEffectiveRules()) {
  for (let i = rules.length - 1; i >= 0; i--) {
    if (rules[i].command === commandId) return formatKey(rules[i].key);
  }
  return null;
}
