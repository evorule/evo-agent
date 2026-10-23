<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 顶栏:与 console-cloud 头部同规格(52px / bg-header) -->
<script>
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import { connStatus, sessionId, stepCount, turnActive, toolWhitelist, signalCount, refreshGovBadges, sidebarView } from '../lib/stores.js';
  import { problems } from '../lib/diagnostics.js';
  import { openBottomPanel } from '../lib/panel.js';
  import { taskRuns } from '../lib/tasks.js';
  import { activePath } from '../lib/stores.js';
  import { addSecondaryGroup, splitOpen } from '../lib/editorGroups.js';
  import MenuBar from './MenuBar.svelte';

  const statusText = {
    connecting: '连接中',
    online: '已连接',
    offline: '未连接',
  };
  const statusClass = {
    connecting: 'warning',
    online: 'success',
    offline: 'neutral',
  };

  // 治理徽标初始加载(白名单 + 上次会话信号;均 fail-soft)
  onMount(() => {
    refreshGovBadges(localStorage.getItem('evo_session_id') || null);
    // 空闲期自愈:白名单徽标另有会话创建/每轮 Done 刷新,但页面挂后台期间
    // agent 定义被改(如补工具)时无轮次活动不会触发——回到前台即重拉。
    const onVisible = () => {
      if (document.visibilityState === 'visible') {
        refreshGovBadges(get(sessionId));
      }
    };
    document.addEventListener('visibilitychange', onVisible);
    window.addEventListener('focus', onVisible);
    return () => {
      document.removeEventListener('visibilitychange', onVisible);
      window.removeEventListener('focus', onVisible);
    };
  });
  // 任务 chip:运行中数量
  $: runningCount = Object.values($taskRuns).filter((r) => r.state === 'running').length;

  // 无边框窗口三键(window.__TAURI__ 由壳 withGlobalTauri 注入,权限见壳侧
  // capabilities/main-window.json 四项最小授权;dev 浏览器无此全局→整组隐藏)
  const winApi =
    typeof window !== 'undefined' && window.__TAURI__?.window ? window.__TAURI__.window : null;
  function winMin() {
    winApi?.getCurrentWindow().minimize();
  }
  function winMax() {
    winApi?.getCurrentWindow().toggleMaximize();
  }
  function winClose() {
    winApi?.getCurrentWindow().close();
  }
</script>

<header class="header" data-tauri-drag-region>
  <div class="brand">
    <span class="logo" aria-hidden="true">
      <!-- EvoRule 内核徽标:金色双环内等号(与 evorule-console-cloud 一致) -->
      <svg viewBox="0 0 24 24" width="20" height="20" fill="none">
        <circle cx="12" cy="12" r="9.4" stroke="#D4AF37" stroke-width="1.5" />
        <circle cx="12" cy="12" r="6.9" stroke="#F0D27A" stroke-width="0.9" />
        <rect x="7.5" y="9.1" width="9" height="2.3" rx="0.9" fill="#D4AF37" />
        <rect x="7.5" y="12.6" width="9" height="2.3" rx="0.9" fill="#F0D27A" />
      </svg>
    </span>
    <span class="brand-text">evo-agent</span>
  </div>

  <MenuBar />

  <div class="actions">
    {#if $turnActive}
      <span class="step-chip mono" title="当前轮次步数">step {$stepCount}</span>
    {/if}
    {#if $problems.counts.errors > 0 || $problems.counts.warnings > 0}
      <button
        class="gov-chip mono diag-chip"
        title={`诊断:错误 ${$problems.counts.errors} / 警告 ${$problems.counts.warnings}(点击打开问题面板)`}
        onclick={() => openBottomPanel('problems')}
      >
        {#if $problems.counts.errors > 0}
          <span class="d-err">✕ {$problems.counts.errors}</span>
        {/if}
        {#if $problems.counts.warnings > 0}
          <span class="d-warn">△ {$problems.counts.warnings}</span>
        {/if}
      </button>
    {/if}
    <button
      class="gov-chip mono"
      class:active={$sidebarView === 'audit'}
      title="工作台:evorule console 治理面板;点击在 编辑区 ↔ 面板 间切换"
      onclick={() => sidebarView.set($sidebarView === 'audit' ? 'home' : 'audit')}
    >
      工作台
    </button>
    <span
      class="gov-chip mono"
      title={$toolWhitelist ? `agent 工具白名单(${$toolWhitelist.length}):${$toolWhitelist.join(', ')}` : '工具白名单未加载'}
    >
      白名单 {$toolWhitelist === null ? '—' : $toolWhitelist.length}
    </span>
    <span
      class="gov-chip mono"
      class:alert={$signalCount !== null && $signalCount > 0}
      title={$signalCount === null
        ? '违规信号未加载(需已建立会话且 evorule 可达)'
        : `当前会话违规信号累计 ${$signalCount} 条`}
    >
      信号 {$signalCount === null ? '—' : $signalCount}
    </span>
    <button
      class="gov-chip mono"
      class:alert={$splitOpen}
      title="分屏:把当前文件放到副组并排编辑"
      onclick={() => addSecondaryGroup($activePath)}
    >
      分屏
    </button>
    {#if $sessionId}
      {#if runningCount > 0}
        <button class="gov-chip mono task-chip" title={`任务运行中 ${runningCount}(点击打开输出面板)`} onclick={() => openBottomPanel('output')}>
          <span class="spin">◌</span> 任务 {runningCount}
        </button>
      {/if}
      <span class="session-chip mono" title="当前会话 id">{$sessionId}</span>
    {/if}
    <span class="conn {statusClass[$connStatus]}">
      <span class="dot"></span>{statusText[$connStatus]}
    </span>
    {#if winApi}
      <div class="win-controls">
        <button class="win-btn" title="最小化" aria-label="最小化" onclick={winMin}>
          <svg viewBox="0 0 12 12" width="12" height="12" aria-hidden="true">
            <path d="M2 6h8" stroke="currentColor" stroke-width="1.2" />
          </svg>
        </button>
        <button class="win-btn" title="最大化/还原" aria-label="最大化/还原" onclick={winMax}>
          <svg viewBox="0 0 12 12" width="12" height="12" aria-hidden="true">
            <rect x="2.5" y="2.5" width="7" height="7" stroke="currentColor" stroke-width="1.2" fill="none" />
          </svg>
        </button>
        <button class="win-btn win-close" title="关闭" aria-label="关闭" onclick={winClose}>
          <svg viewBox="0 0 12 12" width="12" height="12" aria-hidden="true">
            <path d="M2.5 2.5l7 7M9.5 2.5l-7 7" stroke="currentColor" stroke-width="1.2" />
          </svg>
        </button>
      </div>
    {/if}
  </div>
</header>

<style>
  .header {
    height: 52px;
    flex-shrink: 0;
    background: var(--bg-header);
    border-bottom: 1px solid var(--border);
    display: flex;
    align-items: center;
    padding: 0 var(--sp-md);
    gap: var(--sp-md);
    z-index: 100;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    min-width: 168px;
  }
  .logo {
    width: 32px;
    height: 32px;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .brand-text {
    font-weight: var(--fw-sb);
    font-size: var(--fs-lg);
    color: var(--text-primary);
  }
  .actions {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
  }
  .session-chip,
  .step-chip,
  .gov-chip {
    font-size: var(--fs-2xs);
    color: var(--text-secondary);
    background: var(--bg-hover);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: 2px var(--sp-sm);
    max-width: 220px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .gov-chip.alert {
    color: var(--warning);
    border-color: var(--warning);
    background: var(--warning-bg);
  }
  button.gov-chip {
    cursor: pointer;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  button.gov-chip:hover {
    color: var(--text-primary);
    border-color: var(--brand);
  }
  /* 面板驻留态(工作台钮):当前内容区正是 console 面板时高亮 */
  button.gov-chip.active {
    color: var(--brand);
    border-color: var(--brand);
    background: var(--bg-active);
  }
  .diag-chip .d-err {
    color: var(--danger);
  }
  .diag-chip .d-warn {
    color: var(--warning);
  }
  .conn {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: var(--fs-xs);
    padding: 4px var(--sp-sm);
    border-radius: var(--r-sm);
  }
  .conn .dot {
    width: 6px;
    height: 6px;
    background: currentColor;
    border-radius: var(--r-full);
  }
  .conn.success {
    background: var(--success-bg);
    color: var(--success);
  }
  .conn.warning {
    background: var(--warning-bg);
    color: var(--warning);
  }
  .conn.neutral {
    background: var(--bg-hover);
    color: var(--text-secondary);
  }
  /* 无边框窗口三键:低调图标钮,hover 提亮;close hover 红底白叉(现代应用惯例) */
  .win-controls {
    display: flex;
    align-items: center;
    gap: 2px;
    margin-left: var(--sp-xs);
  }
  .win-btn {
    width: 34px;
    height: 30px;
    display: flex;
    align-items: center;
    justify-content: center;
    border: none;
    background: transparent;
    color: var(--text-secondary);
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .win-btn:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .win-btn.win-close:hover {
    background: var(--danger);
    color: var(--text-inverse);
  }
</style>
