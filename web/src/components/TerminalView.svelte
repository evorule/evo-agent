<!-- SPDX-License-Identifier: AGPL-3.0-or-later
Copyright (C) 2026 EvoRule Project
This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
底部面板「终端」tab:xterm 多实例(chips 切换/新建/关闭/清屏)+查找框+退出态灰置。
治理口径(明示):用户自有终端,命令不经过审批,会话留痕于治理日志(审计 tab 可见)。 -->
<script>
  import { onMount, onDestroy, tick } from 'svelte';
  import { panelVisible } from '../lib/stores.js';
  import { terminalIntent } from '../lib/panel.js';
  import {
    createTerminalSession,
    disposeTerminalSession,
    killTerminalSession,
    listTerminalSessions,
    getTerminalSession,
    setTerminalNotify,
    refitTerminal,
    attachWebgl,
    detachWebgl,
    instances,
  } from '../lib/pty.js';

  /** termId 清单(pty.js 变更回调驱动刷新) */
  let ids = [];
  let activeId = null;
  let containerWrap = null;
  /** 每实例 DOM 挂载点(termId → node;use:action 注册) */
  const mounts = new Map();
  let showSearch = false;
  let searchQuery = '';

  $: ids = listTerminalSessions().map((s) => s.termId);
  $: if (activeId && activeId !== '__pending__' && !instances.has(activeId)) {
    activeId = ids.length ? ids[ids.length - 1] : null;
  }

  function refresh() {
    ids = listTerminalSessions().map((s) => s.termId);
    if (activeId && activeId !== '__pending__' && !instances.has(activeId)) {
      activeId = ids.length ? ids[ids.length - 1] : null;
    }
  }

  /** 新建:'__pending__' 槽位挂载时由 action 真正创建(等容器可测尺寸) */
  function newTerminal() {
    if (activeId === '__pending__') return;
    activeId = '__pending__';
  }

  function mountAction(node, id) {
    mounts.set(id, node);
    if (id === '__pending__') {
      const inst = createTerminalSession(node);
      inst.onSearchIntent = () => (showSearch = true);
      activeId = inst.termId;
      attachWebgl(inst);
      requestAnimationFrame(() => refitTerminal(inst));
    }
    return {
      destroy() {
        mounts.delete(id);
      },
    };
  }

  function activate(id) {
    if (id === activeId) return;
    const prev = getTerminalSession(activeId);
    if (prev) detachWebgl(prev);
    activeId = id;
    tick().then(() => {
      const inst = getTerminalSession(id);
      if (inst) {
        attachWebgl(inst);
        refitTerminal(inst);
        inst.term.focus();
      }
    });
  }

  function closeTerminal(id) {
    killTerminalSession(id);
    refresh();
  }

  function clearActive() {
    getTerminalSession(activeId)?.term.clear();
  }

  function restartTerminal() {
    // 退出态重启=关旧开新(v1 无进程复活)
    if (activeId) killTerminalSession(activeId);
    refresh();
    newTerminal();
  }

  function doSearch(backward = false) {
    const inst = getTerminalSession(activeId);
    if (!inst || !searchQuery) return;
    try {
      if (backward) inst.search.findPrevious(searchQuery);
      else inst.search.findNext(searchQuery);
    } catch {
      /* 空结果静默 */
    }
  }

  function closeSearch() {
    showSearch = false;
    getTerminalSession(activeId)?.term.focus();
  }

  // 命令意图消费(一次性):terminal.new/kill/clear → 本视图动作
  let lastIntent = null;
  $: if ($terminalIntent && $terminalIntent !== lastIntent) {
    lastIntent = $terminalIntent;
    if ($terminalIntent === 'new') newTerminal();
    else if ($terminalIntent === 'kill') closeTerminal(activeId);
    else if ($terminalIntent === 'clear') clearActive();
    terminalIntent.set(null);
  }

  onMount(() => {
    setTerminalNotify(refresh);
    // 面板尺寸变化(sash 拖拽/窗口缩放)→ refit active 实例
    const ro = new ResizeObserver(() => {
      const inst = getTerminalSession(activeId);
      if (inst) refitTerminal(inst);
    });
    if (containerWrap) ro.observe(containerWrap);
    return () => ro.disconnect();
  });

  onDestroy(() => {
    // 全部实例销毁(进程回收由后端断连语义保证)
    for (const id of [...instances.keys()]) disposeTerminalSession(id);
  });
</script>

<div class="terminal-view" bind:this={containerWrap}>
  <div class="term-toolbar">
    <div class="chips">
      {#each ids as id (id)}
        <button class="chip" class:active={id === activeId} onclick={() => activate(id)}>
          <span class="dot" class:exited={getTerminalSession(id)?.state === 'exited'}>●</span>
          {id}
        </button>
      {/each}
      {#if activeId === '__pending__'}
        <button class="chip active"><span class="dot">●</span>连接中…</button>
      {/if}
    </div>
    <div class="actions">
      <button class="tbtn" onclick={newTerminal} title="新建终端">＋</button>
      <button class="tbtn" onclick={clearActive} title="清屏" disabled={!activeId || activeId === '__pending__'}>清屏</button>
      <button class="tbtn" onclick={() => closeTerminal(activeId)} title="关闭当前终端" disabled={!activeId || activeId === '__pending__'}>×</button>
    </div>
  </div>

  {#if showSearch}
    <div class="search-bar">
      <input
        class="search-input"
        placeholder="查找"
        bind:value={searchQuery}
        onkeydown={(e) => {
          if (e.key === 'Enter') doSearch(e.shiftKey);
          if (e.key === 'Escape') closeSearch();
        }}
      />
      <button class="tbtn" onclick={() => doSearch(false)} title="下一个">↓</button>
      <button class="tbtn" onclick={() => doSearch(true)} title="上一个">↑</button>
      <button class="tbtn" onclick={closeSearch} title="关闭查找">×</button>
    </div>
  {/if}

  <div class="term-body">
    {#if ids.length === 0 && activeId !== '__pending__'}
      <div class="term-empty">
        <div>暂无终端会话。</div>
        <button class="new-btn" onclick={newTerminal}>新建终端</button>
        <div class="gov-note">用户自有终端：命令不经过审批，会话留痕于治理日志（审计 tab 可见）。</div>
      </div>
    {:else}
      {#each ids as id (id)}
        {#if id === activeId || getTerminalSession(id)}
          <div class="term-slot" style:display={id === activeId ? '' : 'none'} use:mountAction={id}></div>
        {/if}
      {/each}
      {#if activeId === '__pending__'}
        <div class="term-slot" use:mountAction={'__pending__'}></div>
      {/if}
    {/if}
  </div>

  {#if activeId && activeId !== '__pending__' && getTerminalSession(activeId)?.state === 'exited'}
    <div class="exit-overlay">
      <span>
        终端已退出{getTerminalSession(activeId)?.exitCode != null
          ? `（code ${getTerminalSession(activeId).exitCode}）`
          : ''}{getTerminalSession(activeId)?.hint ? ` — ${getTerminalSession(activeId).hint}` : ''}
      </span>
      <button class="new-btn" onclick={restartTerminal}>重启终端</button>
    </div>
  {/if}
  {#if activeId && activeId !== '__pending__' && getTerminalSession(activeId)?.state === 'error'}
    <div class="exit-overlay">
      <span>{getTerminalSession(activeId)?.hint || '终端启动失败'}</span>
      <button class="new-btn" onclick={restartTerminal}>重试</button>
    </div>
  {/if}
</div>

<style>
  .terminal-view {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    background: var(--bg-card);
  }
  .term-toolbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 3px 8px;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .chips {
    display: flex;
    gap: 6px;
    overflow-x: auto;
    min-width: 0;
  }
  .chip {
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--fg-dim);
    cursor: pointer;
    font-size: 12px;
    padding: 2px 8px;
    white-space: nowrap;
  }
  .chip.active {
    border-color: var(--accent);
    color: var(--fg);
  }
  .dot {
    color: #3fb950;
    font-size: 8px;
    margin-right: 4px;
    vertical-align: 1px;
  }
  .dot.exited {
    color: #f85149;
  }
  .actions {
    display: flex;
    gap: 4px;
  }
  .tbtn {
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--fg-dim);
    cursor: pointer;
    font-size: 12px;
    padding: 2px 8px;
  }
  .tbtn:hover:not(:disabled) {
    color: var(--fg);
    border-color: var(--accent);
  }
  .tbtn:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .search-bar {
    display: flex;
    gap: 4px;
    align-items: center;
    padding: 4px 8px;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .search-input {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--fg);
    flex: 1;
    font-size: 12px;
    max-width: 280px;
    padding: 3px 8px;
  }
  .term-body {
    flex: 1;
    min-height: 0;
    position: relative;
  }
  .term-slot {
    height: 100%;
    padding: 2px 6px;
  }
  .term-empty {
    align-items: center;
    color: var(--fg-dim);
    display: flex;
    flex-direction: column;
    font-size: 13px;
    gap: 10px;
    height: 100%;
    justify-content: center;
  }
  .gov-note {
    color: var(--fg-dim);
    font-size: 11px;
    max-width: 420px;
    opacity: 0.8;
    text-align: center;
  }
  .new-btn {
    background: transparent;
    border: 1px solid var(--accent);
    border-radius: 4px;
    color: var(--fg);
    cursor: pointer;
    font-size: 12px;
    padding: 4px 14px;
  }
  .exit-overlay {
    align-items: center;
    background: color-mix(in srgb, #f8514914, var(--bg-card));
    border-top: 1px solid var(--border);
    color: var(--fg-dim);
    display: flex;
    flex-shrink: 0;
    font-size: 12px;
    gap: 12px;
    justify-content: space-between;
    padding: 6px 12px;
  }
</style>
