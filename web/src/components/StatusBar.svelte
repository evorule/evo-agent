<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- F1 底部状态栏(22px 全宽,纯读态+入口跳转):
     左=git 分支(点击→SCM)+代理运行态点;右=行列·编码·语言·连接态·
     通知铃铛(F2 下拉面板:最近 20 条+全部已读+清空)·版本号。
     数据源零新增后端:全部前端既有 store/光标位/包版本。 -->
<script>
  import { onMount } from 'svelte';
  import {
    gitStatus,
    turnActive,
    connStatus,
    cursorPos,
    explorerVisible,
    sidebarView,
  } from '../lib/stores.js';
  import {
    notifications,
    toasts,
    unreadCount,
    markAllRead,
    clearAll,
    dismissToast,
  } from '../lib/notifications.js';
  import { settingsState } from '../lib/settings.js';
  import {
    APPROVAL_MODE_KEY,
    APPROVAL_EXEMPTION_KEY,
    approvalPresetView,
  } from '../lib/approval.js';
  import { workspaceLabel } from '../lib/workspace.js';
  import { version } from '../../package.json';

  let panelOpen = false;
  let bellEl = null;
  let panelEl = null;

  // ---- 批 I 段 3:工作区 + 审批档位显性化(纯读态;入口跳转) ----
  let wsPath = '';
  function syncWs() {
    wsPath = localStorage.getItem('evo_workspace') || '';
  }
  $: wsText = wsPath ? `工作区:${workspaceLabel(wsPath) || wsPath}` : '工作区:未选择';
  $: wsTitle = wsPath || '未选择工作区——agent 目前只能访问程序启动目录,点右上角工作区按钮选择';
  $: approvalView = approvalPresetView(
    $settingsState.settings[APPROVAL_MODE_KEY],
    $settingsState.settings[APPROVAL_EXEMPTION_KEY],
  );

  function openExplorer() {
    sidebarView.set('explorer');
    explorerVisible.set(true);
  }

  const LANG_LABELS = {
    javascript: 'JavaScript',
    typescript: 'TypeScript',
    json: 'JSON',
    markdown: 'Markdown',
    rust: 'Rust',
    html: 'HTML',
    css: 'CSS',
    python: 'Python',
    yaml: 'YAML',
    ini: 'INI',
    shell: 'Shell',
    powershell: 'PowerShell',
    sql: 'SQL',
    xml: 'XML',
    plaintext: '纯文本',
  };

  $: unread = unreadCount($notifications);
  $: langLabel = $cursorPos ? LANG_LABELS[$cursorPos.language] || $cursorPos.language : '—';
  $: conn =
    $connStatus === 'online'
      ? { text: '已连接', cls: 'ok' }
      : $connStatus === 'connecting'
        ? { text: '连接中', cls: 'busy' }
        : { text: '已断开', cls: 'err' };

  function fmtTime(ts) {
    const d = new Date(ts);
    const p = (n) => String(n).padStart(2, '0');
    return `${p(d.getHours())}:${p(d.getMinutes())}`;
  }

  function openScm() {
    sidebarView.set('scm');
    explorerVisible.set(true);
  }

  function onDocClick(e) {
    if (!panelOpen) return;
    if (bellEl?.contains(e.target) || panelEl?.contains(e.target)) return;
    panelOpen = false;
  }

  function togglePanel() {
    panelOpen = !panelOpen;
    if (panelOpen) markAllRead(); // 展开即读(角标归零,条目保留)
  }

  onMount(() => {
    syncWs(); // 批 I:工作区显性化初值(此后随切换事件同步)
    window.addEventListener('evo:workspace-changed', syncWs);
    document.addEventListener('click', onDocClick);
    return () => {
      window.removeEventListener('evo:workspace-changed', syncWs);
      document.removeEventListener('click', onDocClick);
    };
  });
</script>

<div class="statusbar" role="status">
  <div class="sb-left">
    <button class="sb-item branch" onclick={openScm} title="当前分支(点击打开源代码管理)">
      <svg viewBox="0 0 16 16" width="12" height="12" fill="currentColor" aria-hidden="true">
        <circle cx="4" cy="3.5" r="2" />
        <circle cx="4" cy="12.5" r="2" />
        <circle cx="12" cy="5.5" r="2" />
        <path d="M4 5.5v5M4 6.5h4.5a2 2 0 0 1 2 2v0a2 2 0 0 1-2 2H8" fill="none" stroke="currentColor" stroke-width="1.3" />
      </svg>
      {$gitStatus?.branch || '无分支'}
    </button>
    <!-- 批 I 段 3:工作区显性化(点击 → 文件树;切换入口在树根/首页卡) -->
    <button class="sb-item ws" onclick={openExplorer} title={wsTitle}>
      <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
        <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
      </svg>
      {wsText}
    </button>
    <!-- 批 I 段 3:审批档位显性化(调整入口在首页卡/对话侧栏) -->
    <span class="sb-item mode" title={`审批档位(在首页卡或对话侧栏调整):${approvalView.label}`}>
      {approvalView.label}
    </span>
    <span class="sb-item agent" title={$turnActive ? '代理执行中' : '代理空闲'}>
      <span class="dot {$turnActive ? 'run' : 'idle'}" />
      {$turnActive ? '代理运行中' : '代理空闲'}
    </span>
  </div>

  <div class="sb-right">
    <span class="sb-item pos" title="光标行列">行 {$cursorPos?.line ?? '—'}, 列 {$cursorPos?.column ?? '—'}</span>
    <span class="sb-item" title="文件编码">UTF-8</span>
    <span class="sb-item" title="语言模式">{langLabel}</span>
    <span class="sb-item conn {conn.cls}" title="工作台连接态">{conn.text}</span>
    <button
      class="sb-item bell"
      bind:this={bellEl}
      onclick={togglePanel}
      title="通知中心"
      aria-expanded={panelOpen}
    >
      <svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true">
        <path d="M8 2a4 4 0 0 0-4 4v2.5L2.8 11h10.4L12 8.5V6a4 4 0 0 0-4-4Z" />
        <path d="M6.5 13a1.5 1.5 0 0 0 3 0" />
      </svg>
      {#if unread > 0}<span class="badge">{unread > 99 ? '99+' : unread}</span>{/if}
    </button>
    <span class="sb-item ver" title="工作台版本">v{version}</span>
  </div>

  {#if panelOpen}
    <div class="notif-panel" bind:this={panelEl} role="dialog" aria-label="通知面板">
      <div class="np-head">
        <span class="np-title">通知</span>
        <button class="np-act" onclick={markAllRead} disabled={unread === 0}>全部标为已读</button>
        <button class="np-act" onclick={clearAll} disabled={$notifications.length === 0}>清空</button>
      </div>
      {#if $notifications.length === 0}
        <div class="np-empty">暂无通知</div>
      {:else}
        <div class="np-list">
          {#each $notifications.slice(0, 20) as n (n.id)}
            <div class="np-item {n.level}" title={n.body}>
              <span class="np-dot" />
              <div class="np-main">
                <div class="np-item-title">
                  {n.title}
                  <span class="np-time">{fmtTime(n.ts)}</span>
                </div>
                {#if n.body}<div class="np-body">{n.body}</div>{/if}
              </div>
            </div>
          {/each}
        </div>
      {/if}
    </div>
  {/if}
</div>

<!-- F2:右下 toast 队列(err 常驻点叉,其余 4s 自动收) -->
{#if $toasts.length > 0}
  <div class="toast-stack" aria-live="polite">
    {#each $toasts as t (t.id)}
      <div class="toast {t.level}" role="alert">
        <span class="toast-title">{t.title}</span>
        {#if t.body}<span class="toast-body">{t.body}</span>{/if}
        <button class="toast-x" onclick={() => dismissToast(t.id)} aria-label="关闭">×</button>
      </div>
    {/each}
  </div>
{/if}

<style>
  .statusbar {
    height: 22px;
    flex-shrink: 0;
    display: flex;
    align-items: center;
    justify-content: space-between;
    background: var(--brand);
    color: var(--text-inverse);
    font-size: var(--fs-2xs);
    user-select: none;
    position: relative;
    z-index: 30;
  }
  .sb-left,
  .sb-right {
    display: flex;
    align-items: center;
    height: 100%;
    min-width: 0;
  }
  .sb-item {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    height: 100%;
    padding: 0 8px;
    color: inherit;
    background: transparent;
    border: none;
    font-size: var(--fs-2xs);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  button.sb-item {
    cursor: pointer;
  }
  button.sb-item:hover,
  .sb-item.agent:hover {
    background: rgba(255, 255, 255, 0.14);
  }
  .branch svg {
    flex-shrink: 0;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    display: inline-block;
  }
  .dot.idle {
    background: rgba(255, 255, 255, 0.45);
  }
  .dot.run {
    background: #7ee787;
    box-shadow: 0 0 4px #7ee787;
    animation: sb-pulse 1.4s ease-in-out infinite;
  }
  @keyframes sb-pulse {
    50% {
      opacity: 0.55;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .dot.run {
      animation: none;
    }
  }
  .conn.ok {
    color: #d8ffe0;
  }
  .conn.busy {
    color: #ffe9a8;
  }
  .conn.err {
    color: #ffd2d2;
    font-weight: var(--fw-sb);
  }
  .bell {
    position: relative;
  }
  .badge {
    position: absolute;
    top: 2px;
    right: 1px;
    background: var(--danger);
    color: var(--text-inverse);
    font-size: 9px;
    line-height: 1;
    padding: 2px 4px;
    border-radius: 999px;
    pointer-events: none;
  }
  .ver {
    opacity: 0.85;
  }
  .notif-panel {
    position: absolute;
    right: 8px;
    bottom: 26px;
    width: 320px;
    max-height: 320px;
    display: flex;
    flex-direction: column;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--r-md);
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.35);
    overflow: hidden;
    color: var(--text-primary);
  }
  .np-head {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    padding: var(--sp-xs) var(--sp-sm);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .np-title {
    font-size: var(--fs-sm);
    font-weight: var(--fw-sb);
    margin-right: auto;
  }
  .np-act {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    background: transparent;
    border: none;
    cursor: pointer;
    padding: 2px 4px;
    border-radius: var(--r-sm);
  }
  .np-act:hover:not(:disabled) {
    color: var(--brand);
    background: var(--hover-bg, rgba(127, 127, 127, 0.12));
  }
  .np-act:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .np-empty {
    padding: var(--sp-xl) var(--sp-md);
    font-size: var(--fs-sm);
    color: var(--text-muted);
    text-align: center;
  }
  .np-list {
    overflow-y: auto;
    min-height: 0;
  }
  .np-item {
    display: flex;
    gap: 8px;
    padding: 7px var(--sp-sm);
    border-bottom: 1px solid var(--border);
    font-size: var(--fs-xs);
  }
  .np-item:last-child {
    border-bottom: none;
  }
  .np-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    margin-top: 4px;
    flex-shrink: 0;
    background: var(--text-muted);
  }
  .np-item.info .np-dot {
    background: var(--brand);
  }
  .np-item.warn .np-dot {
    background: #d29922;
  }
  .np-item.err .np-dot {
    background: var(--danger);
  }
  .np-main {
    min-width: 0;
  }
  .np-item-title {
    font-weight: var(--fw-sb);
    display: flex;
    justify-content: space-between;
    gap: 8px;
  }
  .np-time {
    color: var(--text-muted);
    font-weight: var(--fw-n);
    flex-shrink: 0;
  }
  .np-body {
    margin-top: 2px;
    color: var(--text-secondary);
    overflow: hidden;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    -webkit-box-orient: vertical;
    word-break: break-all;
  }
  .toast-stack {
    position: fixed;
    right: 12px;
    bottom: 30px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    z-index: 200;
    width: 300px;
  }
  .toast {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    padding: 8px 10px;
    border-radius: var(--r-md);
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-left: 3px solid var(--brand);
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.35);
    font-size: var(--fs-xs);
    animation: toast-in 0.18s ease-out;
  }
  .toast.warn {
    border-left-color: #d29922;
  }
  .toast.err {
    border-left-color: var(--danger);
  }
  @keyframes toast-in {
    from {
      transform: translateY(6px);
      opacity: 0;
    }
    to {
      transform: none;
      opacity: 1;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .toast {
      animation: none;
    }
  }
  .toast-title {
    font-weight: var(--fw-sb);
  }
  .toast-body {
    color: var(--text-secondary);
    word-break: break-all;
  }
  .toast-x {
    margin-left: auto;
    flex-shrink: 0;
    background: transparent;
    border: none;
    color: var(--text-muted);
    cursor: pointer;
    font-size: var(--fs-sm);
    line-height: 1;
    padding: 0 2px;
  }
  .toast-x:hover {
    color: var(--text-primary);
  }
</style>
