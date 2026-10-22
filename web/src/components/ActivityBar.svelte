<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 活动栏(Trae 最左列):资源管理器/搜索(B2)/源代码管理(B3)等视图入口;后续阶段逐项点亮。
     设置入口在顶栏(齿轮钮,白名单左侧),不在此列。 -->
<script>
  import { sidebarView, homeRequested } from '../lib/stores.js';

  /** 条目点击外发(home/audit 等内容区入口的宿主接线;App.svelte 按 id 分发) */
  export let onitemclick = null;

  const items = [
    { id: 'home', label: 'evo-agent 工作台(首页)', enabled: true },
    { id: 'explorer', label: '资源管理器', enabled: true },
    { id: 'search', label: '搜索', enabled: true },
    { id: 'scm', label: '源代码管理', enabled: true },
    { id: 'orchestration', label: '编排(委托谱系)', enabled: true },
    { id: 'debug', label: '运行和调试', enabled: true },
    { id: 'extensions', label: '扩展', enabled: true },
    { id: 'audit', label: '审计', enabled: true },
  ];

  $: active = $sidebarView;

  function click(it) {
    if (!it.enabled) return;
    if (it.id === 'home') {
      // 首页是内容区视图而非侧面板:发信号给 EditorPane(切回首页,不清 tab),
      // 侧面板高亮态保持不变;自身不参与 active 高亮。
      homeRequested.update((n) => n + 1);
      if (onitemclick) onitemclick(it.id);
      return;
    }
    sidebarView.set(it.id);
    if (onitemclick) onitemclick(it.id);
  }
</script>

<nav class="activity-bar" aria-label="活动栏">
  {#each items as it}
    <button
      class="ab-item {it.enabled ? '' : 'disabled'} {active === it.id ? 'active' : ''}"
      title={it.label}
      aria-label={it.label}
      disabled={!it.enabled}
      onclick={() => click(it)}
    >
      {#if it.id === 'home'}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <path d="M4 11.5 12 4l8 7.5" />
          <path d="M6 10v10h4.5v-5h3v5H18V10" />
        </svg>
      {:else if it.id === 'explorer'}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
        </svg>
      {:else if it.id === 'search'}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <circle cx="11" cy="11" r="6" />
          <path d="m20 20-4.5-4.5" />
        </svg>
      {:else if it.id === 'scm'}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <circle cx="7" cy="6" r="2.4" />
          <circle cx="7" cy="18" r="2.4" />
          <circle cx="17" cy="9" r="2.4" />
          <path d="M7 8.4v7.2M17 11.4c0 3-2.5 3.6-7.6 3.6" />
        </svg>
      {:else if it.id === 'orchestration'}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <circle cx="5" cy="12" r="2.2" />
          <circle cx="18" cy="5" r="2.2" />
          <circle cx="18" cy="12" r="2.2" />
          <circle cx="18" cy="19" r="2.2" />
          <path d="M7.2 12h8.6M7.2 11.2C7.2 8 10 7.2 15.7 6.9M7.2 12.8c0 3.2 2.8 4 8.5 4.3" />
        </svg>
      {:else if it.id === 'debug'}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <ellipse cx="12" cy="13" rx="4.6" ry="5.6" />
          <path d="M9.4 6.2 10.6 4M14.6 6.2 13.4 4M7.4 11H3.8M7.2 15H4.2M20.2 11h-3.6M19.8 15h-3M12 7.4V5.4M7.4 13h9.2" />
        </svg>
      {:else if it.id === 'extensions'}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <path d="M12 3.5a2 2 0 0 1 2 2V7h3.5a1 1 0 0 1 1 1v3.5H17a2 2 0 0 0 0 4h1.5V19a1 1 0 0 1-1 1H14v-1.5a2 2 0 0 0-4 0V20H6.5a1 1 0 0 1-1-1v-3.5H4a2 2 0 0 1 0-4h1.5V8a1 1 0 0 1 1-1H10V5.5a2 2 0 0 1 2-2Z" />
        </svg>
      {:else if it.id === 'audit'}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <path d="M12 3 5 6v5c0 4.5 3 8.2 7 9.5 4-1.3 7-5 7-9.5V6l-7-3Z" />
          <path d="m9.5 12 2 2 3.5-4" />
        </svg>
      {/if}
    </button>
  {/each}
</nav>

<style>
  .activity-bar {
    width: 48px;
    flex-shrink: 0;
    background: var(--sidebar-bg);
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    align-items: center;
    padding: var(--sp-sm) 0;
    gap: var(--sp-xs);
  }
  .ab-item {
    width: 40px;
    height: 40px;
    border-radius: var(--r-sm);
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--sidebar-text);
    transition: background var(--tr-fast), color var(--tr-fast);
  }
  .ab-item:hover:not(.disabled) {
    color: var(--sidebar-text-active);
    background: var(--sidebar-hover);
  }
  .ab-item.active {
    color: var(--sidebar-text-active);
    background: var(--sidebar-active);
  }
  .ab-item.disabled {
    opacity: 0.35;
    cursor: default;
  }
</style>
