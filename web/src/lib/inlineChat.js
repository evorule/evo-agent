// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// Inline Chat 编排:Ctrl+K → 输入条 → POST /api/inline-chat → diff 预览 →
// Accept(applyEdits+undo stop)/Discard。补全设计档 §3.3(v1 Cursor 式单模式)。
// 治理:每次调用 pushPanelEvent gov 留痕(指令摘要+path+accepted);.evoignore
// 命中直接拒绝并明示(零外发)。

import { writable, get } from 'svelte/store';
import { pushPanelEvent } from './stores.js';
import { isIgnored } from './evoignore.js';

/** widget 可见态:{editor, selection, instruction, busy, result, error} | null */
export const inlineChat = writable(null);

// E4 诊断临器:双实例比对(收口后移除)
if (typeof window !== 'undefined') { window.__moduleInline = inlineChat; }

let activeController = null;

/** 唤起(编辑器内 Ctrl+K):selection 空则以当前行为段 */
export function openInlineChat(editor, model) {
  if (!editor || !model) return;
  const selection = editor.getSelection();
  const startLine = selection?.startLineNumber || editor.getPosition()?.lineNumber || 1;
  const endLine = selection?.endLineNumber || startLine;
  inlineChat.set({
    editor,
    model,
    startLine,
    endLine,
    instruction: '',
    busy: false,
    result: null,
    error: null,
    accepted: false,
  });
}

export function closeInlineChat() {
  if (activeController) activeController.abort();
  activeController = null;
  inlineChat.set(null);
}

export function setInstruction(text) {
  inlineChat.update((s) => (s ? { ...s, instruction: text } : s));
}

export function cancelInlineChat() {
  if (activeController) activeController.abort();
  activeController = null;
  inlineChat.update((s) => (s ? { ...s, busy: false } : s));
}

function govLog(action, path, instruction, accepted) {
  pushPanelEvent('gov', {
    label: action,
    detail: `${path || '(未存)'} · ${(instruction || '').slice(0, 40)}${accepted != null ? ` · accepted=${accepted}` : ''}`,
  });
}

/**
 * 执行改写:读选区代码→POST /api/inline-chat→diff 预览态。
 * Accept 时 applyEdits(全文替换选区范围)+undo stop(Discard=Ctrl+Z 可回)。
 */
export async function runInlineChat(editor, model, instruction, startLine, endLine, path) {
  const lang = model.getLanguageId?.() || model.getLanguageId;
  const code = model.getValueInRange({
    startLineNumber: startLine,
    startColumn: 1,
    endLineNumber: endLine,
    endColumn: model.getLineMaxColumn(endLine),
  });
  if (isIgnored(path)) {
    inlineChat.update((s) =>
      s ? { ...s, error: '该文件已被 .evoignore 排除——内容零外发', busy: false } : s,
    );
    govLog('inlinechat.rejected', path, instruction, null);
    return;
  }
  inlineChat.update((s) => (s ? { ...s, busy: true, error: null, result: null } : s));
  activeController = new AbortController();
  govLog('inlinechat.requested', path, instruction, null);
  try {
    const res = await fetch('/api/inline-chat', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      signal: activeController.signal,
      body: JSON.stringify({ instruction, code, path, lang }),
    });
    if (!res.ok) {
      const d = await res.json().catch(() => ({}));
      throw new Error(d.error || `HTTP ${res.status}`);
    }
    const d = await res.json();
    const text = stripFence(d.text || '');
    if (!text.trim()) {
      inlineChat.update((s) =>
        s ? { ...s, busy: false, error: '模型返回为空——请改写指令后重试' } : s,
      );
      return;
    }
    inlineChat.update((s) => (s ? { ...s, busy: false, result: text } : s));
  } catch (e) {
    if (e.name === 'AbortError') {
      inlineChat.update((s) => (s ? { ...s, busy: false } : s));
      return;
    }
    inlineChat.update((s) =>
      s ? { ...s, busy: false, error: String(e?.message || e) } : s,
    );
  }
}

/** 剥离 markdown 代码围栏(模型偶发带围栏;Aider 基准:全文返回) */
function stripFence(text) {
  const m = /^```[^\n]*\n([\s\S]*?)\n?```$/.exec(text.trim());
  return m ? m[1] : text;
}

/** Accept:把改后全文 applyEdits 到选区范围+undo stop(Discard=Ctrl+Z) */
export function acceptInlineChat() {
  const s = get(inlineChat);
  if (!s || !s.result) return;
  const { editor, model, startLine, endLine, result } = s;
  editor.pushUndoStop();
  editor.executeEdits('inline-chat', [
    {
      range: {
        startLineNumber: startLine,
        startColumn: 1,
        endLineNumber: endLine,
        endColumn: model.getLineMaxColumn(endLine),
      },
      text: result,
    },
  ]);
  editor.pushUndoStop();
  govLog('inlinechat.accepted', s.path || currentPathOf(model), s.instruction, true);
  inlineChat.set(null);
  editor.focus();
}

function currentPathOf(model) {
  return model.uri.path.replace(/^\//, '');
}

/** Discard:还原(清除预览;model 未动,无需还原操作) */
export function discardInlineChat() {
  const s = get(inlineChat);
  if (s) govLog('inlinechat.discarded', s.path || '', s.instruction, false);
  closeInlineChat();
}
