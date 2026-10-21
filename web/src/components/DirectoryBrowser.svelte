<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 目录浏览器对话框(批 I 段 3):消费 GET /api/fs/browse 选工作区。
     根列表(盘符)↔ 子目录双向导航(面包屑上移),只列目录;选定经
     select 事件回传绝对路径,由调用方落库(本组件零持久化)。 -->
<script>
  import { createEventDispatcher, onMount } from 'svelte';
  import { browseFs } from '../lib/api.js';

  /** 打开时初始目录(空 = 从盘符根开始) */
  export let initialPath = '';
  export let title = '选择目录';

  const dispatch = createEventDispatcher();

  let cur = null; // 当前目录绝对路径(null = 根列表模式)
  let parent = null; // 上移目标(null = 已是根)
  let isRoots = true;
  let entries = [];
  let listError = ''; // fail-soft 读取失败信息(权限等)
  let reqError = ''; // HTTP 级错误(相对路径/不存在等,正常导航不应出现)
  let loading = false;

  async function load(path) {
    loading = true;
    reqError = '';
    try {
      const res = await browseFs(path || '');
      cur = res.path ?? null;
      parent = res.parent ?? null;
      isRoots = !!res.roots;
      entries = res.entries || [];
      listError = res.error || '';
    } catch (e) {
      reqError = String(e?.message || e);
    } finally {
      loading = false;
    }
  }

  function open(entry) {
    load(entry.path);
  }

  function up() {
    if (parent) load(parent);
  }

  /** 选定当前目录(根列表模式无「当前目录」可选) */
  function pick() {
    if (!cur) return;
    dispatch('select', { path: cur });
  }

  function onKeydown(e) {
    if (e.key === 'Escape') dispatch('close');
  }

  /** 面包屑分段(当前目录绝对路径 → 可点击祖先链) */
  $: crumbs = buildCrumbs(cur);

  function buildCrumbs(path) {
    if (!path) return [];
    const norm = String(path).replace(/[\\/]+/g, '/');
    const isWin = /^[a-zA-Z]:\//.test(norm);
    const parts = norm.split('/').filter(Boolean);
    const out = [];
    let acc = '';
    for (let i = 0; i < parts.length; i++) {
      if (i === 0 && isWin) {
        acc = `${parts[0]}\\`; // 盘符根保持反斜杠形态(可回填 browse 参数)
      } else {
        acc = `${acc}${i === 1 && isWin ? '' : '/'}${parts[i]}`;
      }
      out.push({ name: parts[i], path: acc });
    }
    return out;
  }

  onMount(() => {
    load(initialPath);
  });
</script>

<svelte:window on:keydown={onKeydown} />

<div
  class="fb-overlay"
  role="presentation"
  onclick={(e) => e.target === e.currentTarget && dispatch('close')}
>
  <div class="fb-dialog" role="dialog" aria-modal="true" aria-label={title}>
    <div class="fb-title">{title}</div>
    <div class="fb-crumbs">
      {#if isRoots}
        <span class="fb-crumb cur">此电脑(盘符)</span>
      {:else}
        <button class="fb-crumb" onclick={() => load('')}>此电脑</button>
        {#each crumbs as c, i}
          <span class="fb-sep">›</span>
          {#if i === crumbs.length - 1}
            <span class="fb-crumb cur">{c.name}</span>
          {:else}
            <button class="fb-crumb" onclick={() => load(c.path)}>{c.name}</button>
          {/if}
        {/each}
      {/if}
      <span class="fb-spacer" />
      <button class="fb-up" disabled={!parent || loading} onclick={up} title="上一级">↑ 上移</button>
    </div>
    <div class="fb-list" role="listbox" aria-label="子目录列表">
      {#if loading}
        <div class="fb-empty">读取中…</div>
      {:else if reqError}
        <div class="fb-err">浏览失败:{reqError}</div>
      {:else if entries.length === 0}
        <div class="fb-empty">{listError ? `读取失败:${listError}` : '无子目录'}</div>
      {:else}
        {#each entries as e (e.path)}
          <div
            class="fb-item"
            role="option"
            aria-selected="false"
            aria-label={e.name}
            tabindex="0"
            ondblclick={() => open(e)}
            onkeydown={(ev) => ev.key === 'Enter' && open(e)}
          >
            <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
              <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
            </svg>
            <span class="fb-name">{e.name}</span>
            <button
              class="fb-go"
              title="进入该目录"
              onclick={(ev) => {
                ev.stopPropagation();
                open(e);
              }}>进入</button
            >
          </div>
        {/each}
      {/if}
    </div>
    <div class="fb-hint">双击目录进入;选定以当前目录为准(agent 读写边界 + 文件树根)。</div>
    <div class="fb-actions">
      <button class="fb-btn-plain" onclick={() => dispatch('close')}>取消</button>
      <button class="fb-btn-primary" disabled={!cur || loading} onclick={pick}>
        选择当前目录
      </button>
    </div>
  </div>
</div>

<style>
  .fb-overlay {
    position: fixed;
    inset: 0;
    z-index: 1200;
    background: rgba(0, 0, 0, 0.45);
    display: flex;
    align-items: flex-start;
    justify-content: center;
    padding-top: 12vh;
  }
  .fb-dialog {
    width: 520px;
    max-width: calc(100vw - 32px);
    max-height: 72vh;
    display: flex;
    flex-direction: column;
    background: var(--bg-card);
    border: 1px solid var(--border-strong);
    border-radius: 8px;
    box-shadow: var(--sh-modal);
    padding: var(--sp-md);
  }
  .fb-title {
    font-size: var(--fs-base);
    font-weight: var(--fw-sb);
    color: var(--text-primary);
    margin-bottom: 10px;
  }
  .fb-crumbs {
    display: flex;
    align-items: center;
    gap: 4px;
    flex-wrap: wrap;
    font-size: var(--fs-xs);
    margin-bottom: 8px;
    min-height: 22px;
  }
  .fb-crumb {
    background: transparent;
    border: none;
    color: var(--brand);
    font-size: var(--fs-xs);
    padding: 1px 4px;
    border-radius: 4px;
    cursor: pointer;
    max-width: 160px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .fb-crumb.cur {
    color: var(--text-primary);
    font-weight: var(--fw-sb);
    cursor: default;
  }
  .fb-sep {
    color: var(--text-muted);
  }
  .fb-spacer {
    flex: 1;
  }
  .fb-up {
    border: 1px solid var(--border);
    border-radius: 4px;
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-xs);
    padding: 2px 8px;
    cursor: pointer;
    flex-shrink: 0;
  }
  .fb-up:hover:not(:disabled) {
    color: var(--text-primary);
    background: var(--bg-hover, rgba(127, 127, 127, 0.12));
  }
  .fb-up:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .fb-list {
    flex: 1;
    min-height: 180px;
    max-height: 42vh;
    overflow-y: auto;
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: 4px;
  }
  .fb-item {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    color: var(--text-primary);
    font-size: var(--fs-sm);
    padding: 5px 8px;
    border-radius: 4px;
    cursor: pointer;
    box-sizing: border-box;
  }
  .fb-item:hover {
    background: var(--bg-hover, rgba(127, 127, 127, 0.12));
  }
  .fb-item :global(svg) {
    flex-shrink: 0;
    color: var(--brand);
    opacity: 0.85;
  }
  .fb-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .fb-go {
    flex-shrink: 0;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: transparent;
    color: var(--text-muted);
    font-size: var(--fs-xs);
    padding: 1px 8px;
    visibility: hidden;
  }
  .fb-item:hover .fb-go {
    visibility: visible;
  }
  .fb-go:hover {
    color: var(--brand);
    border-color: var(--brand);
  }
  .fb-empty,
  .fb-err {
    padding: var(--sp-md);
    font-size: var(--fs-xs);
    color: var(--text-muted);
    line-height: var(--lh-normal);
    word-break: break-all;
  }
  .fb-err {
    color: #f87171;
  }
  .fb-hint {
    margin-top: 8px;
    font-size: var(--fs-xs);
    color: var(--text-muted);
    line-height: var(--lh-normal);
  }
  .fb-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 12px;
  }
  .fb-btn-plain {
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-secondary);
    padding: 4px 14px;
  }
  .fb-btn-plain:hover {
    color: var(--text-primary);
    background: var(--bg-hover, rgba(127, 127, 127, 0.12));
  }
  .fb-btn-primary {
    border: 1px solid var(--brand);
    border-radius: 4px;
    background: var(--brand);
    color: #fff;
    padding: 4px 14px;
  }
  .fb-btn-primary:hover:not(:disabled) {
    filter: brightness(1.1);
  }
  .fb-btn-primary:disabled {
    opacity: 0.45;
    cursor: default;
  }
</style>
