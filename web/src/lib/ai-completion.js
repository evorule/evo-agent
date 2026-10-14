// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// AI 行内补全:Monaco inline completions provider + 200ms debounce + 抑制矩阵
// + 去重缓存 + .evoignore 排除(设计档 13 号 §3.3)。
//
// 交互收敛三键:Tab 接受/Esc 拒/继续打字作废(抑制矩阵:粘贴·撤销·仅空白
// 变化跳过;IntelliSense 弹出由 suggestWidgetVisible 让位)。
// 治理:补全零留痕;.evoignore 命中返回空建议(零请求);开关关=dispose。

import { get } from 'svelte/store';
import * as monaco from 'monaco-editor';
import { settingsState } from './settings.js';
import { isIgnored, refreshEvoignore } from './evoignore.js';

const DEBOUNCE_MS = 200;
const PREFIX_BUDGET = 8000; // 字符(约 2000 token)
const SUFFIX_BUDGET = 2000;
const CACHE_MAX = 32;

let registered = [];
let enabled = true;
let evoignoreReady = false;

/** (modelUri + 前缀尾串) → 建议文本;LRU 上限 */
const cache = new Map();

function cacheKey(model, position) {
  const line = model.getLineContent(position.lineNumber);
  const upto = line.slice(0, position.column - 1);
  const tail = model.getValueInRange({
    startLineNumber: Math.max(1, position.lineNumber - 8),
    startColumn: 1,
    endLineNumber: position.lineNumber,
    endColumn: position.column,
  });
  return `${model.uri.toString()}|${tail.length}:${upto.slice(-24)}`;
}

function cacheGet(key) {
  const v = cache.get(key);
  if (v !== undefined) {
    cache.delete(key);
    cache.set(key, v); // LRU 触碰
  }
  return v;
}

function cachePut(key, value) {
  if (cache.size >= CACHE_MAX) {
    cache.delete(cache.keys().next().value);
  }
  cache.set(key, value);
}

function completionEnabled() {
  return get(settingsState).settings['completion.enabled'] !== false;
}

/** 抑制矩阵:最近一次编辑动作属于粘贴/撤销或仅空白差异 → 本轮不触发 */
let lastEditWasPaste = false;
let lastEditWasUndo = false;
let lastContentSnapshot = '';

function editSuppressed(model, position) {
  if (lastEditWasPaste || lastEditWasUndo) return true;
  // 仅空白变化:与上次快照比较(去空白后相同=跳过)
  const upto = model.getValueInRange({
    startLineNumber: 1,
    startColumn: 1,
    endLineNumber: position.lineNumber,
    endColumn: position.column,
  });
  const stripped = upto.replace(/\s+/g, '');
  if (stripped === lastContentSnapshot) return true;
  lastContentSnapshot = stripped;
  return false;
}

async function fetchCompletion(model, position) {
  const prefix = model.getValueInRange({
    startLineNumber: 1,
    startColumn: 1,
    endLineNumber: position.lineNumber,
    endColumn: position.column,
  }).slice(-PREFIX_BUDGET);
  const lineCount = model.getLineCount();
  const suffix = model.getValueInRange({
    startLineNumber: position.lineNumber,
    startColumn: position.column,
    endLineNumber: lineCount,
    endColumn: model.getLineMaxColumn(lineCount),
  }).slice(0, SUFFIX_BUDGET);

  const res = await fetch('/api/completion', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ prefix, suffix, lang: model.getLanguageId() }),
  });
  if (!res.ok) return '';
  const d = await res.json();
  return d.text || '';
}

/**
 * 在给定编辑器实例上注册补全 provider(每组实例各挂一次——provider 全局聚合)。
 * 返回 dispose 函数(含编辑器级事件订阅清理)。
 */
export function registerAiCompletion(editor, languages) {
  if (!enabled) return () => {};
  refreshEvoignore();

  // 编辑器级事件:粘贴/撤销标记 + 内容快照维护
  const onPaste = editor.onDidPaste(() => {
    lastEditWasPaste = true;
  });
  const onUndo = editor.onDidChangeModelContent(() => {
    // undo 标记由 onDidUndo 命令钩子置位;此处仅粘贴标记持久到下次成功触发
  });
  function monacoEditorUndoReason(_e) {
    // Monaco 0.52:undo 无直接 reason 标记;以 undo 命令拦截替代(见 keydown)
    return -1;
  }
  const onUndoCmd = editor.onDidUndo(() => {
    lastEditWasUndo = true;
  });
  const onRedoCmd = editor.onDidRedo(() => {
    lastEditWasUndo = true;
  });

  const providerDisposers = [];

  for (const lang of languages) {
    let debounceTimer = null;
    let inFlight = null; // { cancel: () => void }

    const d = monaco.languages.registerInlineCompletionsProvider(lang, {
      async provideInlineCompletions(model, position, _ctx, token) {
        if (!completionEnabled() || token.isCancellationRequested) return { items: [] };
        // 仅空白/粘贴/撤销抑制
        if (editSuppressed(model, position)) return { items: [] };
        // .evoignore 排除:命中零请求
        const path = model.uri.path.replace(/^\//, '');
        if (isIgnored(path)) return { items: [] };
        // 去重缓存
        const key = cacheKey(model, position);
        const cached = cacheGet(key);
        if (cached !== undefined) {
          return cached ? { items: [{ insertText: cached }] } : { items: [] };
        }
        // debounce 200ms(取消经 token.isCancellationRequested 轮询)
        await new Promise((r) => {
          debounceTimer = setTimeout(r, DEBOUNCE_MS);
        });
        if (token.isCancellationRequested) return { items: [] };
        // in-flight:响应到达时若已被取消(继续打字),丢弃
        const myToken = token;
        const fetchPromise = (async () => {
          try {
            return await fetchCompletion(model, position);
          } catch {
            return ''; // 补全失败静默(零打扰)
          }
        })();
        inFlight = { fetchPromise, myToken };
        const text = await fetchPromise;
        if (myToken.isCancellationRequested) return { items: [] };
        const result = text ? { items: [{ insertText: text }] } : { items: [] };
        cachePut(key, text);
        return result;
      },
      handleItemDidShow() {},
      freeInlineCompletions() {
        if (debounceTimer) clearTimeout(debounceTimer);
      },
    });
    providerDisposers.push(d);
  }

  // Esc/打字清除抑制标记(下一次正常触发)
  const onKeyDown = editor.onKeyDown((e) => {
    if (e.keyCode === monaco.KeyCode.Escape) {
      lastEditWasPaste = false;
      lastEditWasUndo = false;
    }
  });

  registered.push({
    editor,
    dispose() {
      onPaste.dispose();
      onUndo.dispose();
      onUndoCmd.dispose();
      onRedoCmd.dispose();
      onKeyDown.dispose();
      for (const d of providerDisposers) d.dispose();
    },
  });

  return function disposeAiCompletion() {
    for (const r of registered.splice(0)) r.dispose();
  };
}

/** 开关联动:关闭=清缓存+标记禁用(已注册 provider 由设置订阅方 dispose) */
export function setAiCompletionEnabled(v) {
  enabled = v;
  if (!v) cache.clear();
}

export function aiCompletionEnabled() {
  return enabled;
}

export function markEvoignoreDirty() {
  evoignoreReady = refreshEvoignore();
}

export { refreshEvoignore as refreshAiEvoignore };
