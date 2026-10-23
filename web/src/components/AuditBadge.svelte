<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 审计徽章(audit-one-click;共享组件,ChatSidebar/MemoryTab 两处消费)。
     状态机口径见 lib/audit.js badgeStatus:
     绿=已审计(整链哈希校验通过,点击开报告卡)
     红=链校验未通过(哈希链断裂)
     灰=本条无链上存证(本地路径会话/未映射条目)
     措辞边界:锚点生产启用前不使用「不可篡改/防篡改/绝对可信」类表述。 -->
<script>
  let { status = 'hidden', clickable = false, onclick = null } = $props();

  const TITLES = {
    green: '已审计 · 整链哈希校验通过(点击查看报告)',
    red: '链校验未通过（哈希链断裂）',
    gray: '本条无链上存证',
  };
  const ICONS = { green: '✓', red: '!', gray: '·' };
</script>

{#if status !== 'hidden'}
  {#if clickable}
    <button
      class="audit-badge {status}"
      title={TITLES[status] || ''}
      aria-label={TITLES[status] || ''}
      onclick={onclick}
    >{ICONS[status] || '·'}</button>
  {:else}
    <span
      class="audit-badge {status}"
      title={TITLES[status] || ''}
      aria-label={TITLES[status] || ''}
    >{ICONS[status] || '·'}</span>
  {/if}
{/if}

<style>
  .audit-badge {
    flex: none;
    width: 15px;
    height: 15px;
    border-radius: 50%;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font-size: var(--fs-2xs);
    line-height: 1;
    padding: 0;
    margin: 0 2px;
    border: 1px solid transparent;
    vertical-align: middle;
    cursor: default;
  }
  button.audit-badge {
    cursor: pointer;
  }
  .audit-badge.green {
    color: var(--success);
    background: var(--success-bg);
  }
  .audit-badge.red {
    color: var(--danger);
    background: var(--danger-bg);
    font-weight: 700;
  }
  .audit-badge.gray {
    color: var(--text-muted);
    background: transparent;
    border-color: var(--border);
  }
</style>
