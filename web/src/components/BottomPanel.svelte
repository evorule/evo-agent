<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 底部面板区:审计抽屉等(默认收起,按需展开) -->
<script>
  let open = false;
</script>

<div class="bottom-panel" class:open>
  <button class="panel-strip" onclick={() => (open = !open)} aria-expanded={open}>
    <span class="chevron" class:open>{open ? '▾' : '▸'}</span>
    <span>审计</span>
    <span class="hint">治理事件流在治理叠加阶段接入</span>
  </button>
  {#if open}
    <div class="panel-body">审计抽屉占位 —— 将按会话呈现治理事件(审批 / 违规信号 / 审计链入口)。</div>
  {/if}
</div>

<style>
  .bottom-panel {
    flex-shrink: 0;
    border-top: 1px solid var(--border);
    background: var(--bg-card);
    display: flex;
    flex-direction: column;
  }
  .panel-strip {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    height: 28px;
    padding: 0 var(--sp-md);
    font-size: var(--fs-xs);
    font-weight: var(--fw-med);
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    text-align: left;
  }
  .panel-strip:hover {
    color: var(--text-primary);
    background: var(--bg-hover);
  }
  .chevron {
    font-size: 10px;
  }
  .hint {
    text-transform: none;
    letter-spacing: 0;
    font-weight: var(--fw-reg);
    color: var(--text-muted);
  }
  .panel-body {
    height: 180px;
    padding: var(--sp-md);
    font-size: var(--fs-sm);
    color: var(--text-secondary);
    border-top: 1px solid var(--border);
  }
</style>
