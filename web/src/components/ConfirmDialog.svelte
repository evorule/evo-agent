<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 通用确认对话框:替代浏览器原生 confirm(风格 token 一致)。
     Esc/点击遮罩 = 取消;Enter = 确认;确认键自动聚焦。 -->
<script>
  import { tick } from 'svelte';
  import { createEventDispatcher } from 'svelte';

  export let open = false;
  export let title = '确认';
  export let message = '';
  export let confirmText = '删除';
  export let danger = true;

  const dispatch = createEventDispatcher();
  let confirmBtn;

  $: if (open && confirmBtn) {
    tick().then(() => confirmBtn?.focus());
  }

  function onKey(e) {
    if (!open) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      dispatch('cancel');
    } else if (e.key === 'Enter') {
      e.preventDefault();
      dispatch('confirm');
    }
  }
</script>

<svelte:window onkeydown={onKey} />

{#if open}
  <div
    class="overlay"
    role="presentation"
    onmousedown={(e) => {
      // 等价旧语法 on:mousedown|self:仅遮罩自身点击 = 取消
      if (e.target === e.currentTarget) dispatch('cancel');
    }}
  >
    <div class="dialog" role="alertdialog" aria-modal="true" aria-label={title}>
      <div class="title">{title}</div>
      <div class="message">{message}</div>
      <div class="actions">
        <button class="btn ghost" onclick={() => dispatch('cancel')}>取消</button>
        <button
          class="btn {danger ? 'danger' : 'primary'}"
          bind:this={confirmBtn}
          onclick={() => dispatch('confirm')}
        >
          {confirmText}
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 1100;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .dialog {
    width: 360px;
    max-width: calc(100vw - 32px);
    background: var(--bg-card);
    border: 1px solid var(--border-strong);
    border-radius: 8px;
    box-shadow: var(--sh-modal);
    padding: var(--sp-md);
  }
  .title {
    font-size: var(--fs-base);
    font-weight: var(--fw-sb);
    color: var(--text-primary);
    margin-bottom: var(--sp-sm);
  }
  .message {
    font-size: var(--fs-sm);
    color: var(--text-secondary);
    line-height: var(--lh-normal);
    margin-bottom: var(--sp-md);
    word-break: break-all;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--sp-sm);
  }
  .btn {
    font-size: var(--fs-sm);
    padding: 6px 14px;
    border-radius: 6px;
    border: 1px solid transparent;
  }
  .btn.ghost {
    color: var(--text-secondary);
    border-color: var(--border);
  }
  .btn.ghost:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .btn.primary {
    background: var(--brand);
    color: #ffffff;
  }
  .btn.primary:hover {
    background: var(--brand-hover);
  }
  .btn.danger {
    background: var(--danger);
    color: #ffffff;
  }
  .btn.danger:hover {
    filter: brightness(1.1);
  }
</style>
