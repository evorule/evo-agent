<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 通用分隔条(E2 三处布局分隔共用:侧栏|编辑器、编辑器|底部面板、主组|副组)。
     pointer 拖拽 → resize{dx,dy}(自上一次事件的增量);双击 → reset(恢复缺省);
     方向键微调(Shift 加速);orient='h' 可开 toggle(Enter/Space)复用底部面板折叠。
     纯交互件,不持业务状态;钳制与持久化由消费方在 resize 内完成。 -->
<script>
  import { createEventDispatcher } from 'svelte';

  /** 'v' 竖条(左右拖,调宽) | 'h' 横条(上下拖,调高) */
  export let orient = 'v';
  export let title = '拖拽调整;双击恢复缺省';
  /** 横条可选:Enter/Space 触发 toggle 事件(底部面板折叠复用) */
  export let toggleOnEnter = false;

  const dispatch = createEventDispatcher();
  let dragging = false;
  let lastX = 0;
  let lastY = 0;

  function onPointerDown(e) {
    if (e.button !== 0) return;
    e.preventDefault();
    dragging = true;
    lastX = e.clientX;
    lastY = e.clientY;
    document.body.style.userSelect = 'none';
    window.addEventListener('pointermove', onPointerMove);
    window.addEventListener('pointerup', onPointerUp);
  }

  function onPointerMove(e) {
    if (!dragging) return;
    const dx = e.clientX - lastX;
    const dy = e.clientY - lastY;
    lastX = e.clientX;
    lastY = e.clientY;
    dispatch('resize', { dx, dy });
  }

  function onPointerUp() {
    if (!dragging) return;
    dragging = false;
    document.body.style.userSelect = '';
    window.removeEventListener('pointermove', onPointerMove);
    window.removeEventListener('pointerup', onPointerUp);
  }

  function onDblClick() {
    dispatch('reset');
  }

  function onKeydown(e) {
    const step = e.shiftKey ? 40 : 16;
    if (orient === 'v' && (e.key === 'ArrowLeft' || e.key === 'ArrowRight')) {
      dispatch('resize', { dx: e.key === 'ArrowLeft' ? -step : step, dy: 0 });
      e.preventDefault();
    } else if (orient === 'h' && (e.key === 'ArrowUp' || e.key === 'ArrowDown')) {
      dispatch('resize', { dx: 0, dy: e.key === 'ArrowUp' ? -step : step });
      e.preventDefault();
    } else if (toggleOnEnter && (e.key === 'Enter' || e.key === ' ')) {
      dispatch('toggle');
      e.preventDefault();
    }
  }
</script>

<div
  class="divider {orient}"
  class:dragging
  role="separator"
  tabindex="0"
  aria-orientation={orient === 'v' ? 'vertical' : 'horizontal'}
  {title}
  onpointerdown={onPointerDown}
  ondblclick={onDblClick}
  onkeydown={onKeydown}
/>

<style>
  .divider {
    flex-shrink: 0;
    background: transparent;
    transition: background var(--tr-fast);
    touch-action: none;
    z-index: 5;
  }
  .divider.v {
    width: 4px;
    margin: 0 -1px; /* 贴合相邻视图边框,视觉零占位 */
    cursor: col-resize;
  }
  .divider.h {
    height: 4px;
    cursor: row-resize;
  }
  .divider:hover,
  .divider:focus-visible,
  .divider.dragging {
    background: var(--brand);
    outline: none;
  }
</style>
