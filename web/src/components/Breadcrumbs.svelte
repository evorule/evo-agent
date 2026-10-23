<!-- SPDX-License-Identifier: AGPL-3.0-or-later
Copyright (C) 2026 EvoRule Project
This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
编辑器顶部面包屑(H2):路径段逐段可点 → pendingTreeReveal 信号联动文件树
(逐级展开+滚动定位);超宽省略中间段(实测溢出后折叠为 首段+…+末两段)。 -->
<script>
  import { pendingTreeReveal, sidebarView, explorerVisible } from '../lib/stores.js';

  /** 当前文件路径('' = 无文件 tab,隐藏面包屑) */
  export let path = '';

  let overflow = false;
  let clientW = 0;
  let contentW = 0;

  $: segs = path ? path.split('/').filter(Boolean) : [];
  // 溢出时折叠中间段(段数 > 4 才有折叠净收益):首段 + … + 末两段
  $: collapsible = segs.length > 4;
  $: overflow = collapsible && contentW > clientW + 1;
  $: displaySegs = overflow ? [segs[0], '…', segs[segs.length - 2], segs[segs.length - 1]] : segs;
  // 被折叠的段数(提示条文案用):末两段之外、首段之后
  $: hiddenCount = overflow ? segs.length - 3 : 0;

  function reveal(i) {
    const dir = segs.slice(0, i + 1).join('/');
    if (!dir) return;
    sidebarView.set('explorer'); // 侧栏在别的视图时先切回文件树
    explorerVisible.set(true);
    pendingTreeReveal.set({ path: dir });
  }

  /** '…' 段的点击 = 展开完整路径(临时解除折叠直到宽度再溢出) */
  function expandAll() {
    overflow = false;
  }
</script>

{#if path}
  <div class="crumbs" bind:clientWidth={clientW} role="navigation" aria-label="文件路径">
    <div class="crumb-row" bind:clientWidth={contentW}>
      {#each displaySegs as seg, i}
        {#if seg === '…'}
          <span
            class="ellipsis"
            title={`折叠 ${hiddenCount} 段,点击展开完整路径`}
            role="button"
            tabindex="0"
            onclick={expandAll}
            onkeydown={(e) => e.key === 'Enter' && expandAll()}>…</span
          >
        {:else}
          {@const realIdx = overflow ? (i === 0 ? 0 : segs.length - 4 + i) : i}
          {@const isLast = realIdx === segs.length - 1}
          <span
            class="seg"
            class:file={isLast}
            title={isLast ? `在文件树中定位「${seg}」` : `定位目录 ${segs.slice(0, realIdx + 1).join('/')}`}
            role="button"
            tabindex="0"
            onclick={() => reveal(realIdx)}
            onkeydown={(e) => e.key === 'Enter' && reveal(realIdx)}
          >{seg}</span
          >
          {#if !isLast}<span class="sep">/</span>{/if}
        {/if}
      {/each}
    </div>
  </div>
{/if}

<style>
  .crumbs {
    height: 22px;
    flex-shrink: 0;
    display: flex;
    align-items: center;
    overflow: hidden;
    padding: 0 10px;
    background: var(--bg-header, #161b22);
    border-bottom: 1px solid var(--border);
  }
  .crumb-row {
    display: flex;
    align-items: center;
    gap: 2px;
    min-width: 0;
    white-space: nowrap;
    flex: none;
    width: max-content; /* 宽度实测(溢出检测);自身超宽被容器裁剪 */
  }
  .seg {
    color: var(--fg-dim);
    font-size: var(--fs-2xs);
    cursor: pointer;
    padding: 1px 4px;
    border-radius: 3px;
    max-width: 180px;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .seg:hover {
    color: var(--fg);
    background: var(--bg-hover, rgba(255, 255, 255, 0.06));
  }
  .seg.file {
    color: var(--fg);
  }
  .sep {
    color: var(--text-muted);
    font-size: var(--fs-2xs);
    user-select: none;
  }
  .ellipsis {
    color: var(--fg-dim);
    font-size: var(--fs-2xs);
    cursor: pointer;
    padding: 0 4px;
  }
  .ellipsis:hover {
    color: var(--fg);
  }
</style>
