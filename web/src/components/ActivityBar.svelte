<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 活动栏(Trae 最左列):资源管理器常驻;后续阶段逐项点亮 -->
<script>
  let active = 'explorer';
  const items = [
    { id: 'explorer', label: '资源管理器', enabled: true },
    { id: 'search', label: '搜索(文件树阶段接入)', enabled: false },
    { id: 'audit', label: '审计(治理叠加阶段接入)', enabled: false },
    { id: 'settings', label: '设置(整合收口阶段接入)', enabled: false },
  ];
</script>

<nav class="activity-bar" aria-label="活动栏">
  {#each items as it}
    <button
      class="ab-item {it.enabled ? '' : 'disabled'} {active === it.id ? 'active' : ''}"
      title={it.label}
      aria-label={it.label}
      disabled={!it.enabled}
      onclick={() => it.enabled && (active = it.id)}
    >
      {#if it.id === 'explorer'}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
        </svg>
      {:else if it.id === 'search'}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <circle cx="11" cy="11" r="6" />
          <path d="m20 20-4.5-4.5" />
        </svg>
      {:else if it.id === 'audit'}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <path d="M12 3 5 6v5c0 4.5 3 8.2 7 9.5 4-1.3 7-5 7-9.5V6l-7-3Z" />
          <path d="m9.5 12 2 2 3.5-4" />
        </svg>
      {:else}
        <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.8">
          <circle cx="12" cy="12" r="3" />
          <path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M18.4 5.6l-2.1 2.1M7.7 16.3l-2.1 2.1" />
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
