<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 使用指南页(evo://guide 特殊 tab):左侧目录锚点 + 右侧 markdown 正文。
     内容源=lib/guide.js(单一内容源,新增功能随批更新)。 -->
<script>
  import { marked } from 'marked';
  import { GUIDE_MD, GUIDE_TITLE } from '../lib/guide.js';

  const html = marked.parse(GUIDE_MD);
  // 目录=全部 h2;slug 与正文渲染的 id 对应(marked 默认不给 id,自算序号锚点)
  const sections = Array.from(GUIDE_MD.matchAll(/^## (.+)$/gm)).map((m) => m[1].trim());
  let active = $state(0);

  function jump(i) {
    active = i;
    const el = document.getElementById(`guide-sec-${i}`);
    el?.scrollIntoView({ behavior: 'smooth', block: 'start' });
  }
</script>

<div class="guide" data-zone="editor">
  <aside class="guide-toc">
    <div class="guide-title">{GUIDE_TITLE}</div>
    {#each sections as s, i}
      <button type="button" class="guide-toc-item" class:on={active === i} onclick={() => jump(i)}>
        {s}
      </button>
    {/each}
  </aside>
  <div class="guide-body">
    <h1>{GUIDE_TITLE}</h1>
    {@html intro}
    {#each sections as s, i}
      <h2 id={`guide-sec-${i}`}>{s}</h2>
      {@html segments[i]}
    {/each}
  </div>
</div>

<script context="module">
  // 正文按 h2 切段:parts[0]=引言(h2 前内容),其后每段对应一个目录项
  const parts = GUIDE_MD.split(/^## .+$/gm);
  const intro = marked.parse(parts[0] ?? '');
  const segments = parts.slice(1).map((b) => marked.parse(b));
</script>

<style>
  .guide {
    display: flex;
    height: 100%;
    min-height: 0;
    overflow: hidden;
  }
  .guide-toc {
    width: 240px;
    flex: none;
    border-right: 1px solid var(--border);
    padding: var(--sp-md) var(--sp-sm);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .guide-title {
    font-size: var(--fs-md);
    font-weight: var(--fw-sb);
    color: var(--text-primary);
    margin-bottom: var(--sp-sm);
    padding: 0 var(--sp-xs);
  }
  .guide-toc-item {
    text-align: left;
    padding: 5px var(--sp-xs);
    font-size: var(--fs-sm);
    color: var(--text-secondary);
    background: transparent;
    border: none;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .guide-toc-item:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .guide-toc-item.on {
    background: var(--bg-hover);
    color: var(--brand);
  }
  .guide-body {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: var(--sp-lg) var(--sp-xl, 32px);
    color: var(--text-primary);
    font-size: var(--fs-md);
    line-height: 1.7;
  }
  .guide-body h1 {
    font-size: 22px;
    margin: 0 0 var(--sp-md);
  }
  .guide-body h2 {
    font-size: 17px;
    margin: 28px 0 10px;
    padding-top: 10px;
    border-top: 1px solid var(--border);
    scroll-margin-top: 12px;
  }
  .guide-body :global(p) {
    margin: 8px 0;
  }
  .guide-body :global(ul),
  .guide-body :global(ol) {
    margin: 8px 0;
    padding-left: 22px;
  }
  .guide-body :global(li) {
    margin: 4px 0;
  }
  .guide-body :global(code) {
    font-family: var(--font-mono, monospace);
    font-size: 0.92em;
    background: var(--bg-page);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: 1px 5px;
  }
  .guide-body :global(table) {
    border-collapse: collapse;
    margin: 10px 0;
    width: 100%;
  }
  .guide-body :global(th),
  .guide-body :global(td) {
    border: 1px solid var(--border);
    padding: 6px 10px;
    text-align: left;
    font-size: var(--fs-sm);
  }
  .guide-body :global(th) {
    background: var(--bg-page);
    color: var(--text-secondary);
  }
  .guide-body :global(blockquote) {
    margin: 10px 0;
    padding: 6px 14px;
    border-left: 3px solid var(--brand);
    background: var(--bg-page);
    color: var(--text-secondary);
  }
  .guide-body :global(hr) {
    border: none;
    border-top: 1px solid var(--border);
    margin: 16px 0;
  }
</style>
