// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 上下文键:键位 when 条件与命令可见性的求值依据。
// v1 裁剪:求值仅支持 !/&&/|| 切分,完整 context key 表达式语法留 v2。

import { writable } from 'svelte/store';
import { tabs, sessionId } from './stores.js';

/** 单一上下文对象(组件局部焦点键由 data-zone 焦点事件维护;派生键由 store 订阅维护) */
export const contextKeys = writable({
  editorFocus: false, // 焦点在编辑器(Monaco 宿主区)
  chatFocus: false, // 焦点在对话侧栏
  panelFocus: false, // 焦点在底部面板
  inPalette: false, // 命令面板打开中(路由让位给面板内部导航)
  tabsOpen: false, // 有已打开的编辑器 tab(派生)
  sessionOpen: false, // 有已建立的 agent 会话(派生)
});

/** 面板等非焦点事件来源的键,由组件直接置位 */
export function setContextKey(name, value) {
  contextKeys.update((ctx) => (ctx[name] === value ? ctx : { ...ctx, [name]: value }));
}

// ---- 派生键:tabs / sessionId(模块级订阅,导入即生效) ----

tabs.subscribe((list) => setContextKey('tabsOpen', Array.isArray(list) && list.length > 0));
sessionId.subscribe((sid) => setContextKey('sessionOpen', sid !== null && sid !== undefined));

// ---- 焦点键:focusin/focusout + data-zone 标记 ----

function zoneOf(target) {
  const el = target && target.closest ? target.closest('[data-zone]') : null;
  return el ? el.getAttribute('data-zone') : null;
}

function applyZone(zone) {
  contextKeys.update((ctx) => {
    const editorFocus = zone === 'editor';
    const chatFocus = zone === 'chat';
    const panelFocus = zone === 'panel';
    if (ctx.editorFocus === editorFocus && ctx.chatFocus === chatFocus && ctx.panelFocus === panelFocus) {
      return ctx;
    }
    return { ...ctx, editorFocus, chatFocus, panelFocus };
  });
}

/** 安装焦点追踪(App 挂载时调用一次;返回清理函数)。 */
export function initContextTracking() {
  const onFocusIn = (e) => applyZone(zoneOf(e.target));
  const onFocusOut = () => {
    // focusout 后 activeElement 已迁移到相邻可聚焦元素或 body;下一宏任务再读稳态
    setTimeout(() => applyZone(zoneOf(document.activeElement)), 0);
  };
  window.addEventListener('focusin', onFocusIn);
  window.addEventListener('focusout', onFocusOut);
  applyZone(zoneOf(document.activeElement));
  return () => {
    window.removeEventListener('focusin', onFocusIn);
    window.removeEventListener('focusout', onFocusOut);
  };
}

// ---- when 求值:v1 切分法(先 || 后 &&,天然实现 && 优先级更高) ----

/**
 * 求值 when 表达式。空串 = 恒真;未知键 = 假(VS Code 语义:未定义上下文为 falsy)。
 * 支持一元 ! 与二元 && / ||;不支持括号(v1 裁剪)。
 */
export function evaluateWhen(expr, ctx) {
  if (!expr || !expr.trim()) return true;
  for (const orPart of expr.split('||')) {
    let ok = true;
    for (const andPart of orPart.split('&&')) {
      let term = andPart.trim();
      if (!term) {
        ok = false;
        break;
      }
      let negated = false;
      while (term.startsWith('!')) {
        negated = !negated;
        term = term.slice(1).trim();
      }
      const value = Boolean(ctx[term]);
      if (negated ? value : !value) {
        ok = false;
        break;
      }
    }
    if (ok) return true;
  }
  return false;
}
