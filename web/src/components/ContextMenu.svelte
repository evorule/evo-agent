<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 通用右键菜单:固定定位 + 视口内收敛 + 点击外/Esc 关闭 + 上下键导航。
     纯展示组件,不持业务状态;配色复用全局 token(工作台暗色面板族)。 -->
<script>
  import { onMount } from 'svelte';
  import { createEventDispatcher } from 'svelte';

  /** 菜单锚点(视口坐标) */
  export let x = 0;
  export let y = 0;
  /** 菜单项:[{label, action(), danger?, separator?}] */
  export let items = [];

  const dispatch = createEventDispatcher();
  let el;

  onMount(() => {
    // 视口内收敛:先量菜单实际尺寸再修正位置(初始渲染在锚点,防闪烁交给下一帧)
    const r = el.getBoundingClientRect();
    el.style.left = `${Math.max(4, Math.min(x, window.innerWidth - r.width - 8))}px`;
    el.style.top = `${Math.max(4, Math.min(y, window.innerHeight - r.height - 8))}px`;
    // 首个可聚焦项获得焦点(键盘可达)
    const first = el.querySelector('.item');
    if (first) first.focus();

    const onDown = (e) => {
      if (el && !el.contains(e.target)) dispatch('close');
    };
    const onKey = (e) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        dispatch('close');
        return;
      }
      if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
      const buttons = [...el.querySelectorAll('.item')];
      if (!buttons.length) return;
      e.preventDefault();
      const idx = buttons.indexOf(document.activeElement);
      const next =
        e.key === 'ArrowDown'
          ? buttons[(idx + 1) % buttons.length]
          : buttons[(idx - 1 + buttons.length) % buttons.length];
      next.focus();
    };
    document.addEventListener('mousedown', onDown, true);
    document.addEventListener('keydown', onKey, true);
    return () => {
      document.removeEventListener('mousedown', onDown, true);
      document.removeEventListener('keydown', onKey, true);
    };
  });
</script>

<div class="ctx-menu" bind:this={el} style="left:{x}px; top:{y}px" role="menu">
  {#each items as it}
    {#if it.separator}
      <div class="sep" role="separator" />
    {:else}
      <button
        class="item"
        class:danger={it.danger}
        role="menuitem"
        onclick={() => {
          dispatch('close');
          it.action?.();
        }}
      >
        {it.label}
      </button>
    {/if}
  {/each}
</div>

<style>
  .ctx-menu {
    position: fixed;
    z-index: 1000;
    min-width: 160px;
    background: var(--bg-card);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    box-shadow: var(--sh-modal);
    padding: 4px;
    display: flex;
    flex-direction: column;
  }
  .item {
    display: block;
    width: 100%;
    text-align: left;
    font-size: var(--fs-sm);
    color: var(--text-primary);
    padding: 6px 10px;
    border-radius: 4px;
    white-space: nowrap;
  }
  .item:hover,
  .item:focus-visible {
    background: var(--bg-hover);
    outline: none;
  }
  .item.danger {
    color: var(--danger);
  }
  .item.danger:hover,
  .item.danger:focus-visible {
    background: var(--danger-bg);
  }
  .sep {
    height: 1px;
    margin: 4px 6px;
    background: var(--border);
  }
</style>
