<!-- SPDX-License-Identifier: AGPL-3.0-or-later
Copyright (C) 2026 EvoRule Project
This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
Inline Chat 输入条+diff 预览(Cursor 式单模式):输入指令→改后全文→本地
computeDiff 行级预览(删除=红删除线,新增=绿底块)→Accept=applyEdits/Discard。 -->
<script>
  import { onDestroy } from 'svelte';
  import { tick } from 'svelte';
  import {
    inlineChat,
    setInstruction,
    closeInlineChat,
    cancelInlineChat,
    acceptInlineChat,
  } from '../lib/inlineChat.js';
  import { diffLines, summarizeDiff } from '../lib/inline-chat-diff.js';

  let inputEl = null;
  let previewDisposables = null;
  let lastResultKey = null;

  $: if ($inlineChat?.editor && $inlineChat.result && $inlineChat.result !== lastResultKey) {
    lastResultKey = $inlineChat.result;
    applyPreview($inlineChat.editor, $inlineChat.startLine, $inlineChat.endLine, $inlineChat.result);
  }
  $: if ($inlineChat === null && previewDisposables) {
    clearPreview();
  }

  function applyPreview(editor, startLine, endLine, result) {
    clearPreview();
    const original = editor.getModel().getValueInRange({
      startLineNumber: startLine,
      startColumn: 1,
      endLineNumber: endLine,
      endColumn: editor.getModel().getLineMaxColumn(endLine),
    });
    const rows = diffLines(original, result);
    const { deletedLines, addedBlocks } = summarizeDiff(rows);
    const delSet = new Set(deletedLines);
    const decorations = [];
    for (const ln of deletedLines) {
      decorations.push({
        range: { startLineNumber: ln, startColumn: 1, endLineNumber: ln, endColumn: 1 },
        options: {
          isWholeLine: true,
          className: 'ic-del-line',
          inlineClassName: 'ic-del-text',
        },
      });
    }
    previewDisposables = editor.createDeltaDecorations(decorations, []);
    // 新增行块:绿底 view zone(注于各块前一行之后;v1 统一注于范围尾前)
    const zones = addedBlocks.map((b) => ({
      afterLineNumber: Math.min(b.afterLine + (startLine - 1), endLine),
      heightInLines: Math.min(b.lines.length, 12),
      domNode: createZoneNode(b.lines),
    }));
    editor.changeViewZones((accessor) => {
      for (const z of zones) accessor.addZone(z);
    });
    previewDisposables = {
      dispose() {
        editor.changeViewZones((accessor) => {
          for (const z of zones) accessor.removeZone(z.id);
        });
        editor.deltaDecorations([], []);
      },
    };
  }

  function createZoneNode(lines) {
    const div = document.createElement('div');
    div.className = 'ic-add-zone';
    const pre = document.createElement('pre');
    pre.textContent = lines.join('\n');
    div.appendChild(pre);
    return div;
  }

  function clearPreview() {
    if (previewDisposables) {
      try {
        previewDisposables.dispose();
      } catch {
        /* 编辑器可能已销毁 */
      }
      previewDisposables = null;
    }
  }

  async function runWithFocus() {
    const s = $inlineChat;
    if (!s) return;
    await import('../lib/inlineChat.js').then((m) =>
      m.runInlineChat(s.editor, s.model, s.instruction, s.startLine, s.endLine, s.path || (s.model ? s.model.uri.path.replace(/^\//, '') : '')),
    );
  }

  function onInputKeydown(e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      if (!$inlineChat?.busy) runWithFocus();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      closeInlineChat();
    }
  }

  async function focusSoon() {
    await tick();
    inputEl?.focus();
  }
  focusSoon();

  onDestroy(() => clearPreview());
</script>

<div class="ic-widget" role="dialog" aria-label="Inline Chat">
  <div class="ic-row">
    <span class="ic-badge mono">AI</span>
    <input
      class="ic-input mono"
      placeholder="指令(如:给函数加注释) — Enter 执行,Esc 关闭"
      aria-label="Inline Chat 指令"
      bind:this={inputEl}
      bind:value={$inlineChat.instruction}
      onkeydown={(e) => {
        e.stopPropagation();
        if (e.key === 'Enter' && !e.shiftKey) {
          e.preventDefault();
          runWithFocus();
        } else if (e.key === 'Escape') {
          e.preventDefault();
          closeInlineChat();
        }
      }}
    />
    {#if $inlineChat.busy}
      <button class="ic-btn" onclick={cancelInlineChat}>取消</button>
    {:else}
      <button class="ic-btn primary" onclick={runWithFocus}>运行</button>
    {/if}
    <button class="ic-btn" onclick={closeInlineChat}>×</button>
  </div>
  {#if $inlineChat.error}
    <div class="ic-error mono">{$inlineChat.error}</div>
  {/if}
  {#if $inlineChat.result}
    <div class="ic-actions">
      <button class="ic-btn primary" onclick={acceptInlineChat}>接受(Accept)</button>
      <button class="ic-btn" onclick={discardInlineChat}>放弃(Discard)</button>
    </div>
  {/if}
</div>

<style>
  .ic-widget {
    background: var(--bg-card);
    border: 1px solid var(--brand);
    border-radius: var(--r-md);
    box-shadow: var(--sh-modal);
    left: 50%;
    max-width: 620px;
    padding: 8px;
    position: fixed;
    top: 90px;
    transform: translateX(-50%);
    width: min(620px, 80vw);
    z-index: 150;
  }
  .ic-row {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .ic-badge {
    background: var(--brand-bg);
    border-radius: var(--r-sm);
    color: var(--brand);
    font-size: 11px;
    padding: 2px 6px;
  }
  .ic-input {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--fg);
    flex: 1;
    font-size: 13px;
    padding: 5px 8px;
  }
  .ic-btn {
    background: transparent;
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--fg);
    cursor: pointer;
    font-size: 12px;
    padding: 4px 10px;
  }
  .ic-btn.primary {
    border-color: var(--brand);
    color: var(--brand);
  }
  .ic-error {
    color: var(--danger);
    font-size: 12px;
    padding: 6px 2px 0;
  }
  .ic-actions {
    display: flex;
    gap: 6px;
    justify-content: flex-end;
    padding-top: 8px;
  }
  /* global: Inline Chat 预览 decoration/zone(需非 scoped) */
  :global(.ic-del-text) {
    opacity: 0.7;
    text-decoration: line-through;
  }
  :global(.ic-del-line) {
    background: rgba(231, 76, 60, 0.12);
  }
  :global(.ic-add-zone) {
    background: rgba(46, 204, 113, 0.12);
    border-left: 2px solid #2ecc71;
  }
  :global(.ic-add-zone pre) {
    color: var(--fg);
    font-family: var(--font-mono);
    font-size: 13px;
    margin: 0;
    padding: 2px 8px;
    white-space: pre;
  }
</style>


