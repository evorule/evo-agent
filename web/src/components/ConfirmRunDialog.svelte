<!-- SPDX-License-Identifier: AGPL-3.0-or-later
Copyright (C) 2026 EvoRule Project
This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
首跑确认框:列明任务命令原文+cwd+风险提示;运行并记住/仅本次/取消(信任分级轻量版)。 -->
<script>
  import { createEventDispatcher } from 'svelte';

  export let task; // {label, commandDisplay, type}

  const dispatch = createEventDispatcher();
</script>

<div class="confirm-backdrop" onclick={(e) => { if (e.target === e.currentTarget) dispatch('cancel'); }} role="presentation">
  <div class="confirm" role="alertdialog" aria-label="确认运行任务">
    <div class="c-title">首次运行此任务，请确认命令</div>
    <div class="c-label mono">{task.label}</div>
    <pre class="c-cmd mono">{task.commandDisplay}</pre>
    <div class="c-note">
      任务配置来自工作区 <span class="mono">.evo/tasks.json</span>。命令将在工作区根目录执行；
      {#if task.type === 'shell'}<b>shell 型任务含任意 shell 语法（注入面），请逐字核对。</b>{:else}进程型任务以 argv 直跑（不经 shell）。{/if}
      确认后本命令将被记住，下次不再询问。
    </div>
    <div class="c-actions">
      <button class="c-btn primary" onclick={() => dispatch('confirm', { remember: true })}>运行并记住</button>
      <button class="c-btn" onclick={() => dispatch('confirm', { remember: false })}>仅本次运行</button>
      <button class="c-btn" onclick={() => dispatch('cancel')}>取消</button>
    </div>
  </div>
</div>

<style>
  .confirm-backdrop {
    background: rgba(0, 0, 0, 0.5);
    inset: 0;
    position: fixed;
    z-index: 210;
  }
  .confirm {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5);
    left: 50%;
    max-width: 560px;
    padding: 16px;
    position: absolute;
    top: 26%;
    transform: translateX(-50%);
    width: min(560px, 90vw);
  }
  .c-title {
    color: var(--fg);
    font-size: 14px;
    font-weight: 600;
    margin-bottom: 8px;
  }
  .c-label {
    color: var(--fg-dim);
    font-size: 12px;
    margin-bottom: 6px;
  }
  .c-cmd {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: 6px;
    color: var(--fg);
    font-size: 12px;
    margin: 0 0 10px;
    overflow-x: auto;
    padding: 8px 10px;
    white-space: pre-wrap;
    word-break: break-all;
  }
  .c-note {
    color: var(--fg-dim);
    font-size: 12px;
    line-height: 1.6;
    margin-bottom: 14px;
  }
  .c-actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
  }
  .c-btn {
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--fg);
    cursor: pointer;
    font-size: 12px;
    padding: 5px 14px;
  }
  .c-btn.primary {
    border-color: var(--accent);
  }
  .c-btn:hover {
    border-color: var(--accent);
  }
</style>
