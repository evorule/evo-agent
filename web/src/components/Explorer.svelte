<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 侧面板:文件树(真实目录浏览,懒加载;目录点击展开,文件点击进编辑器 tab)。
     渲染采用扁平行模型(展开目录按深度打平),避免递归组件。 -->
<script>
  import { onMount } from 'svelte';
  import { listDir } from '../lib/api.js';
  import { openFile, activePath } from '../lib/stores.js';

  let rootName = 'evo-agent(工作区)';
  let rootChildren = [];
  let rows = []; // [{node, depth}]
  let loadError = '';

  // path('' = 根) → {loaded, expanded, children}
  const dirState = new Map();

  async function loadChildren(path) {
    const st = dirState.get(path);
    if (st?.loaded) return st.children;
    const res = await listDir(path || undefined);
    const children = (res.entries || []).map((e) => ({
      name: e.name,
      kind: e.kind,
      path: path ? `${path}/${e.name}` : e.name,
    }));
    dirState.set(path, { ...(st || {}), loaded: true, children });
    return children;
  }

  function rebuild() {
    const out = [];
    const walk = (children, depth) => {
      for (const n of children) {
        out.push({ node: n, depth });
        if (n.kind === 'dir' && dirState.get(n.path)?.expanded) {
          walk(dirState.get(n.path)?.children || [], depth + 1);
        }
      }
    };
    walk(rootChildren, 0);
    rows = out;
  }

  async function toggle(node) {
    if (node.kind !== 'dir') return;
    const st = dirState.get(node.path) || {};
    if (st.expanded) {
      dirState.set(node.path, { ...st, expanded: false });
      rebuild();
      return;
    }
    try {
      await loadChildren(node.path);
      dirState.set(node.path, { ...dirState.get(node.path), expanded: true });
      rebuild();
    } catch (e) {
      loadError = String(e?.message || e);
    }
  }

  function open(node) {
    if (node.kind === 'dir') toggle(node);
    else openFile(node.path);
  }

  onMount(async () => {
    try {
      const res = await listDir();
      const seg = (res.dir || '').split(/[\\/]/).filter(Boolean).pop();
      if (seg) rootName = seg;
      await loadChildren('');
      rootChildren = dirState.get('')?.children || [];
      rebuild();
    } catch (e) {
      loadError = String(e?.message || e);
    }
  });
</script>

<div class="explorer">
  <div class="panel-title">资源管理器</div>
  <div class="workdir mono">
    <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
      <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
    </svg>
    {rootName}
  </div>
  {#if loadError}
    <div class="error">加载失败:{loadError}</div>
  {:else}
    <div class="tree" role="tree">
      {#each rows as { node, depth } (node.path)}
        <div
          class="node {node.kind}"
          style="padding-left: {10 + depth * 14}px"
          role="treeitem"
          aria-selected={node.kind === 'file' && node.path === $activePath}
          aria-expanded={node.kind === 'dir' ? !!dirState.get(node.path)?.expanded : undefined}
          tabindex="0"
          onclick={() => open(node)}
          onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && open(node)}
        >
          {#if node.kind === 'dir'}
            <svg
              class="caret"
              class:open={!!dirState.get(node.path)?.expanded}
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2.2"
            >
              <path d="M9 6l6 6-6 6" />
            </svg>
            <svg class="icon dir" viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
              <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
            </svg>
          {:else}
            <svg class="icon" viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.8">
              <path d="M7 3h7l4 4v14H7z" />
            </svg>
          {/if}
          <span class="name">{node.name}</span>
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .explorer {
    width: 220px;
    flex-shrink: 0;
    background: var(--sidebar-bg);
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    overflow-y: auto;
  }
  .panel-title {
    font-size: 11px;
    font-weight: var(--fw-sb);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: rgba(255, 255, 255, 0.4);
    padding: var(--sp-sm) var(--sp-md);
  }
  .workdir {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: var(--sp-xs) var(--sp-md);
    font-size: var(--fs-xs);
    color: var(--sidebar-text);
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  }
  .tree {
    padding: var(--sp-xs) 0;
  }
  .node {
    display: flex;
    align-items: center;
    gap: 5px;
    padding-right: var(--sp-md);
    height: 26px;
    font-size: var(--fs-sm);
    color: var(--sidebar-text);
    cursor: pointer;
    user-select: none;
    white-space: nowrap;
    overflow: hidden;
  }
  .node:hover {
    background: rgba(255, 255, 255, 0.06);
  }
  .node:focus-visible {
    outline: 1px solid var(--brand);
    outline-offset: -1px;
  }
  .caret {
    width: 10px;
    height: 10px;
    flex-shrink: 0;
    transition: transform 0.12s ease;
    color: var(--text-muted);
  }
  .caret.open {
    transform: rotate(90deg);
  }
  .icon {
    flex-shrink: 0;
    color: var(--text-muted);
  }
  .icon.dir {
    color: var(--brand);
    opacity: 0.85;
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .error {
    padding: var(--sp-md);
    font-size: var(--fs-xs);
    color: #f87171;
    line-height: var(--lh-normal);
  }
</style>
