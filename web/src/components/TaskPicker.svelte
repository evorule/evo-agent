<!-- SPDX-License-Identifier: AGPL-3.0-or-later
Copyright (C) 2026 EvoRule Project
This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
任务选择浮层:tasks.run 命令唤起,列出工作区任务(label+命令预览),回车/点击运行。 -->
<script>
  import { tick, onDestroy } from 'svelte';
  import { createEventDispatcher } from 'svelte';

  export let tasks = [];

  const dispatch = createEventDispatcher();
  let query = '';
  let inputEl = null;
  let selected = 0;
  let taskRows = [];

  $: filtered = tasks.filter((t) => {
    const q = query.trim().toLowerCase();
    if (!q) return true;
    return (
      t.label.toLowerCase().includes(q) ||
      String(t.commandDisplay || '').toLowerCase().includes(q)
    );
  });
  $: selected = Math.min(selected, Math.max(0, filtered.length - 1));

  async function focusSoon() {
    await tick();
    inputEl?.focus();
  }
  focusSoon();

  function pick(task) {
    dispatch('pick', task);
  }

  function onKeydown(e) {
    if (e.key === 'ArrowDown') {
      selected = Math.min(selected + 1, filtered.length - 1);
      e.preventDefault();
    } else if (e.key === 'ArrowUp') {
      selected = Math.max(selected - 1, 0);
      e.preventDefault();
    } else if (e.key === 'Enter') {
      const t = filtered[selected];
      if (t) pick(t);
      e.preventDefault();
    } else if (e.key === 'Escape') {
      dispatch('close');
    }
  }

  function onWindowKeydown(e) {
    if (e.key === 'Escape') dispatch('close');
  }

  const onWin = (e) => onKeydown(e);
  window.addEventListener('keydown', onWin);
  onDestroy(() => window.removeEventListener('keydown', onWin));
</script>

<svelte:window on:keydown={onWindowKeydown} />

<div class="picker-backdrop" onclick={(e) => { if (e.target === e.currentTarget) dispatch('close'); }} role="presentation">
  <div class="picker" role="dialog" aria-label="运行任务">
    <input
      class="picker-input"
      placeholder="输入任务名称筛选,回车运行"
      aria-label="任务筛选"
      bind:this={inputEl}
      bind:value={query}
      onkeydown={(e) => {
        if (e.key === 'Enter') {
          const t = filtered[selected];
          if (t) pick(t);
          e.preventDefault();
        }
        e.stopPropagation();
      }}
    />
    <div class="picker-list" role="listbox">
      {#if filtered.length === 0}
        <div class="picker-empty">没有匹配的任务。任务配置在工作区 .evo/tasks.json。</div>
      {:else}
        {#each filtered as t, i (t.label)}
          <div
            class="picker-item"
            class:selected={i === selected}
            role="option"
            aria-selected={i === selected}
            tabindex="0"
            onclick={() => pick(t)}
            onmouseenter={() => (selected = i)}
            onkeydown={(e) => e.key === 'Enter' && pick(t)}
          >
            <span class="pi-label mono">{t.label}</span>
            <span class="pi-cmd mono">{t.commandDisplay}</span>
          </div>
        {/each}
      {/if}
    </div>
  </div>
</div>

<style>
  .picker-backdrop {
    background: rgba(0, 0, 0, 0.45);
    inset: 0;
    position: fixed;
    z-index: 200;
  }
  .picker {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5);
    left: 50%;
    max-width: 640px;
    padding: 8px;
    position: absolute;
    top: 18%;
    transform: translateX(-50%);
    width: min(640px, 90vw);
  }
  .picker-input {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: 6px;
    box-sizing: border-box;
    color: var(--fg);
    font-size: 13px;
    margin-bottom: 6px;
    padding: 8px 10px;
    width: 100%;
  }
  .picker-list {
    max-height: 320px;
    overflow-y: auto;
  }
  .picker-item {
    border-radius: 4px;
    cursor: pointer;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 6px 10px;
  }
  .picker-item.selected {
    background: var(--accent-soft, rgba(29, 99, 237, 0.25));
  }
  .pi-label {
    color: var(--fg);
    font-size: 13px;
  }
  .pi-cmd {
    color: var(--fg-dim);
    font-size: 11px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .picker-empty {
    color: var(--fg-dim);
    font-size: 12px;
    padding: 14px 10px;
  }
</style>
