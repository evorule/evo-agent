<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 侧面板:文件树(真实目录浏览,懒加载;目录点击展开,文件点击进编辑器 tab)。
     渲染采用扁平行模型(展开目录按深度打平),避免递归组件。
     B7 增量:顶部操作钮 / inline 命名(新建+重命名,支持 "/" 建层级)/
     右键菜单 / 删除确认 / 折叠持久化 / WS fs_events 增量刷新 / tabs 联动。 -->
<script>
  import { onMount } from 'svelte';
  import { listDir, createFile, moveFile, deleteFile } from '../lib/api.js';
  import {
    openFile,
    closeTab,
    renameTabPath,
    activePath,
    fsEvents,
    connStatus,
  } from '../lib/stores.js';
  import ContextMenu from './ContextMenu.svelte';
  import ConfirmDialog from './ConfirmDialog.svelte';

  let rootName = 'evo-agent(工作区)';
  let rootChildren = [];
  let rows = []; // [{node, depth}]
  let loadError = '';

  // path('' = 根) → {loaded, expanded, children}
  const dirState = new Map();

  // 折叠状态持久化(localStorage;键位口径与工作台其他 localStorage 键一致)
  const STATE_KEY = 'evo_explorer_state';

  // ---- inline 命名状态 ----
  // {mode:'createFile'|'createDir', anchor: 目录路径('' = 根), depth, value, error}
  // {mode:'rename', target: node, depth, value, error}
  let naming = null;

  // ---- 右键菜单 / 删除确认 ----
  let menu = null; // {x, y, items}
  let confirmState = null; // {message, run()}

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

  /** 强制重读目录(绕过 loaded 缓存;目录已不存在时失效其状态) */
  async function reloadDir(path) {
    try {
      const res = await listDir(path || undefined);
      const children = (res.entries || []).map((e) => ({
        name: e.name,
        kind: e.kind,
        path: path ? `${path}/${e.name}` : e.name,
      }));
      dirState.set(path, { ...(dirState.get(path) || {}), loaded: true, children });
    } catch {
      dirState.delete(path);
    }
  }

  function persistExpanded() {
    try {
      const arr = [...dirState.entries()].filter(([, st]) => st.expanded).map(([p]) => p);
      localStorage.setItem(STATE_KEY, JSON.stringify(arr));
    } catch {
      /* 存储不可用时折叠状态只存活于内存 */
    }
  }

  function restoreExpanded() {
    try {
      const arr = JSON.parse(localStorage.getItem(STATE_KEY) || '[]');
      return Array.isArray(arr) ? arr.filter((p) => typeof p === 'string') : [];
    } catch {
      return [];
    }
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
      persistExpanded();
      rebuild();
      return;
    }
    try {
      await loadChildren(node.path);
      dirState.set(node.path, { ...dirState.get(node.path), expanded: true });
      persistExpanded();
      rebuild();
    } catch (e) {
      loadError = String(e?.message || e);
    }
  }

  function open(node) {
    if (node.kind === 'dir') toggle(node);
    else openFile(node.path);
  }

  // =========================================================================
  // inline 命名(新建/重命名共用一行输入;Enter 提交,Esc 取消,失焦提交)
  // =========================================================================

  function parentDir(p) {
    const i = p.lastIndexOf('/');
    return i === -1 ? '' : p.slice(0, i);
  }

  async function startCreate(mode, dirPath, depth = 0) {
    // 目录锚点未展开时先展开(输入行渲染在该目录子层)
    if (dirPath && dirState.get(dirPath)?.loaded !== true) {
      const node = rows.find((r) => r.node.path === dirPath)?.node;
      if (node) await toggle(node);
    } else if (dirPath && !dirState.get(dirPath)?.expanded) {
      dirState.set(dirPath, { ...dirState.get(dirPath), expanded: true });
      persistExpanded();
      rebuild();
    }
    naming = { mode, anchor: dirPath, depth: depth || 0, value: '', error: '' };
    if (dirPath) rebuild();
  }

  function startRename(node, depth) {
    naming = { mode: 'rename', target: node, depth, value: node.name, error: '' };
  }

  function cancelNaming() {
    naming = null;
  }

  async function commitNaming() {
    const n = naming;
    if (!n) return;
    // 规整用户输入:反斜杠 → 正斜杠,去首尾分隔符与空白(支持 "/" 建层级)
    const raw = n.value.trim().replace(/\\/g, '/').replace(/^\/+|\/+$/g, '');
    if (!raw) {
      naming = null;
      return;
    }
    if (n.mode === 'rename') {
      const parent = parentDir(n.target.path);
      const newPath = parent ? `${parent}/${raw}` : raw;
      if (newPath === n.target.path) {
        naming = null;
        return;
      }
      try {
        await moveFile(n.target.path, parent || '.', raw);
        naming = null;
        await reloadDir(parent);
        rebuild();
        persistExpanded();
        // tabs 联动:model 缓存随 lastRename 迁移,dirty 内容保留
        renameTabPath(n.target.path, newPath);
      } catch (e) {
        naming = { ...n, error: String(e?.message || e) };
      }
      return;
    }
    const dirPath = n.anchor;
    const path = dirPath ? `${dirPath}/${raw}` : raw;
    try {
      await createFile(path, n.mode === 'createDir' ? 'dir' : 'file');
      naming = null;
      await reloadDir(dirPath);
      dirState.set(dirPath, { ...dirState.get(dirPath), expanded: true });
      persistExpanded();
      rebuild();
      if (n.mode === 'createFile') openFile(path);
    } catch (e) {
      naming = { ...n, error: String(e?.message || e) };
    }
  }

  // =========================================================================
  // 右键菜单 / 删除确认
  // =========================================================================

  function copyPath(node) {
    try {
      navigator.clipboard.writeText(node.path);
    } catch {
      /* 剪贴板不可用(非安全上下文等)时静默跳过 */
    }
  }

  function menuFor(node) {
    const items = [];
    if (node.kind === 'dir') {
      items.push({ label: '新建文件', action: () => startCreate('createFile', node.path) });
      items.push({ label: '新建文件夹', action: () => startCreate('createDir', node.path) });
      items.push({ separator: true });
    } else {
      items.push({ label: '在编辑器打开', action: () => openFile(node.path) });
    }
    items.push({ label: '重命名', action: () => startRename(node) });
    items.push({ label: '复制路径', action: () => copyPath(node) });
    items.push({ separator: true });
    items.push({ label: '删除…', danger: true, action: () => askDelete(node) });
    return items;
  }

  function openMenu(e, node) {
    e.preventDefault();
    menu = { x: e.clientX, y: e.clientY, items: menuFor(node) };
  }

  function openRootMenu(e) {
    e.preventDefault();
    menu = {
      x: e.clientX,
      y: e.clientY,
      items: [
        { label: '新建文件', action: () => startCreate('createFile', '') },
        { label: '新建文件夹', action: () => startCreate('createDir', '') },
        { label: '刷新', action: refreshAll },
      ],
    };
  }

  function askDelete(node) {
    confirmState = {
      message: `删除「${node.name}」?内容将移入回收目录(.evo-trash),可手动找回。`,
      run: async () => {
        try {
          await deleteFile(node.path);
          // tabs 联动:已打开的 tab 一并关闭(dirty 内容随确认放弃)
          closeTab(node.path);
          await reloadDir(parentDir(node.path));
          rebuild();
        } catch (e) {
          loadError = String(e?.message || e);
        }
      },
    };
  }

  // =========================================================================
  // WS fs_events 增量刷新(受影响目录重读 → rebuild,不全树 refetch)
  // =========================================================================

  async function applyFsEvents(events) {
    const toReload = new Set();
    const toInvalidate = new Set();
    const touchParent = (p) => toReload.add(parentDir(p));
    const invalidate = (p) => {
      toInvalidate.add(p);
      touchParent(p);
    };
    for (const p of events.added || []) touchParent(p);
    for (const p of events.updated || []) touchParent(p);
    for (const p of events.removed || []) invalidate(p);
    for (const m of events.moved || []) {
      invalidate(m.from);
      touchParent(m.to);
      // 移动目录:旧子树的展开/懒加载状态全部失效
      toInvalidate.add(m.to);
      // tabs 联动:外部/agent 侧移动时已打开 tab 跟随迁移(dirty 保留)
      renameTabPath(m.from, m.to);
    }
    // 失效:删除以失效路径为前缀的全部 dirState 条目
    if (toInvalidate.size) {
      for (const key of [...dirState.keys()]) {
        if (toInvalidate.has(key)) {
          dirState.delete(key);
          continue;
        }
        for (const p of toInvalidate) {
          if (key.startsWith(`${p}/`)) {
            dirState.delete(key);
            break;
          }
        }
      }
    }
    for (const dir of toReload) await reloadDir(dir);
    rootChildren = dirState.get('')?.children || [];
    persistExpanded();
    rebuild();
  }

  /** 全量刷新:重读根与全部已加载目录(保留展开状态) */
  async function refreshAll() {
    const loaded = [...dirState.keys()].filter((p) => dirState.get(p)?.loaded);
    for (const p of ['', ...loaded]) await reloadDir(p);
    rootChildren = dirState.get('')?.children || [];
    rebuild();
  }

  onMount(async () => {
    try {
      const res = await listDir();
      const seg = (res.dir || '').split(/[\\/]/).filter(Boolean).pop();
      if (seg) rootName = seg;
      await loadChildren('');
      rootChildren = dirState.get('')?.children || [];
      // 恢复上次折叠状态(目录已不存在的条目静默跳过)
      for (const p of restoreExpanded()) {
        try {
          await loadChildren(p);
          dirState.set(p, { ...dirState.get(p), expanded: true });
        } catch {
          /* 目录已不存在 */
        }
      }
      rebuild();
    } catch (e) {
      loadError = String(e?.message || e);
    }
    // fs_events 一次性信号订阅:消费后置 null
    const unsubFs = fsEvents.subscribe((ev) => {
      if (!ev) return;
      fsEvents.set(null);
      applyFsEvents(ev);
    });
    return () => unsubFs();
  });
</script>

<div class="explorer">
  <div class="panel-title">
    资源管理器
    {#if $connStatus === 'offline'}
      <span class="offline" title="连接离线,文件树实时刷新不可用(手动刷新兜底)">离线</span>
    {/if}
  </div>
  <div class="actions">
    <span class="workdir mono">
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
        <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
      </svg>
      {rootName}
    </span>
    <span class="spacer" />
    <button class="action-btn" title="新建文件" onclick={() => startCreate('createFile', '')}>
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
        <path d="M13 3H7a1 1 0 0 0-1 1v16a1 1 0 0 0 1 1h10a1 1 0 0 0 1-1V8l-5-5Z" />
        <path d="M12 11v6M9 14h6" />
      </svg>
    </button>
    <button class="action-btn" title="新建文件夹" onclick={() => startCreate('createDir', '')}>
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
        <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
        <path d="M10 13h6" />
      </svg>
    </button>
    <button class="action-btn" title="刷新" onclick={refreshAll}>
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
        <path d="M20 12a8 8 0 1 1-2.34-5.66M20 4v4h-4" />
      </svg>
    </button>
  </div>
  {#if loadError}
    <div class="error">加载失败:{loadError}</div>
  {:else}
    <div class="tree" role="tree">
      {#if naming && naming.mode !== 'rename' && naming.anchor === ''}
        <div class="node naming" style="padding-left: 10px">
          <input
            class="name-input"
            bind:value={naming.value}
            placeholder="文件名(支持 a/b/c 建层级)"
            aria-label="新建名称"
            onkeydown={(e) => {
              if (e.key === 'Enter') commitNaming();
              else if (e.key === 'Escape') cancelNaming();
            }}
            onblur={commitNaming}
          />
        </div>
      {/if}
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
          oncontextmenu={(e) => openMenu(e, node)}
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
          {#if naming && naming.mode === 'rename' && naming.target.path === node.path}
            <input
              class="name-input"
              bind:value={naming.value}
              aria-label="重命名"
              onkeydown={(e) => {
                if (e.key === 'Enter') commitNaming();
                else if (e.key === 'Escape') cancelNaming();
              }}
              onblur={commitNaming}
            />
          {:else}
            <span class="name">{node.name}</span>
          {/if}
        </div>
        {#if naming && naming.mode !== 'rename' && naming.anchor === node.path}
          <div class="node naming" style="padding-left: {10 + (depth + 1) * 14}px">
            <input
              class="name-input"
              bind:value={naming.value}
              placeholder="名称(支持 a/b/c 建层级)"
              aria-label="新建名称"
              onkeydown={(e) => {
                if (e.key === 'Enter') commitNaming();
                else if (e.key === 'Escape') cancelNaming();
              }}
              onblur={commitNaming}
            />
          </div>
        {/if}
      {/each}
      {#if naming?.error}
        <div class="naming-error">{naming.error}</div>
      {/if}
    </div>
  {/if}
</div>

{#if menu}
  <ContextMenu x={menu.x} y={menu.y} items={menu.items} on:close={() => (menu = null)} />
{/if}

<ConfirmDialog
  open={!!confirmState}
  title="删除确认"
  message={confirmState?.message || ''}
  confirmText="删除"
  on:confirm={() => {
    const run = confirmState?.run;
    confirmState = null;
    run?.();
  }}
  on:cancel={() => (confirmState = null)}
/>

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
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
  }
  .offline {
    font-size: 10px;
    text-transform: none;
    letter-spacing: 0;
    color: var(--warning);
    border: 1px solid var(--warning);
    border-radius: 4px;
    padding: 0 4px;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 2px;
    padding: var(--sp-xs) var(--sp-sm);
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  }
  .workdir {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: var(--fs-xs);
    color: var(--sidebar-text);
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .spacer {
    flex: 1;
  }
  .action-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 22px;
    height: 22px;
    border-radius: 4px;
    color: var(--sidebar-text);
    flex-shrink: 0;
  }
  .action-btn:hover {
    background: var(--sidebar-hover);
    color: var(--sidebar-text-active);
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
  .node.naming {
    cursor: default;
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
  .name-input {
    flex: 1;
    min-width: 0;
    background: var(--bg-input);
    border: 1px solid var(--brand);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: var(--fs-sm);
    padding: 2px 6px;
    outline: none;
  }
  .naming-error {
    margin: var(--sp-xs) var(--sp-md);
    font-size: var(--fs-xs);
    color: #f87171;
    line-height: var(--lh-normal);
    word-break: break-all;
  }
  .error {
    padding: var(--sp-md);
    font-size: var(--fs-xs);
    color: #f87171;
    line-height: var(--lh-normal);
  }
</style>
