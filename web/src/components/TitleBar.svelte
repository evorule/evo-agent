<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 顶栏:与 console-cloud 头部同规格(52px / bg-header) -->
<script>
  import { connStatus, sessionId, stepCount, turnActive } from '../lib/stores.js';

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
</script>

<header class="header">
  <div class="brand">
    <span class="logo" aria-hidden="true">
      <svg viewBox="0 0 24 24" width="20" height="20" fill="none">
        <path
          d="M13 2 4.5 13.5H11L9.5 22 19.5 9.5H13L13 2Z"
          fill="var(--brand-ocean)"
        />
      </svg>
    </span>
    <span class="brand-text">evo-agent</span>
    <span class="brand-sub">工作台</span>
  </div>

  <div class="actions">
    {#if $turnActive}
      <span class="step-chip mono" title="当前轮次步数">step {$stepCount}</span>
    {/if}
    {#if $sessionId}
      <span class="session-chip mono" title="当前会话 id">{$sessionId}</span>
    {/if}
    <span class="conn {statusClass[$connStatus]}">
      <span class="dot"></span>{statusText[$connStatus]}
    </span>
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
  .brand-sub {
    font-weight: var(--fw-reg);
    color: var(--text-secondary);
    font-size: var(--fs-sm);
  }
  .actions {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
  }
  .session-chip,
  .step-chip {
    font-size: 11px;
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
</style>
