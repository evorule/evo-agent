<!-- SPDX-License-Identifier: AGPL-3.0-or-later
Copyright (C) 2026 EvoRule Project
This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
分屏副组(普通文件 tab):与主组共享同一 model(ModelRegistry 按 evofile:/// Uri
单源)——同文件两组编辑内容/undo 实时同步;view state 按组×path 独立。
diff/产物/设置页留在主组(轻量实现边界,完整组模型随完整 EditorGroups 重构)。 -->
<script>
  import { onMount, onDestroy, tick } from 'svelte';
  import { get } from 'svelte/store';
  import { setupMonaco, monaco } from '../lib/monaco-setup.js';
  import { saveTab, markDirty, markSaved, tabs, closeTab, ensureTabOpen, publishCursor } from '../lib/stores.js';
  import { settingsState } from '../lib/settings.js';
  import {
    groupOpenPath,
    groupClosePath,
    groupFocusSignal,
    activeGroupId,
    dispatchTabDrop,
  } from '../lib/editorGroups.js';
  import Breadcrumbs from './Breadcrumbs.svelte';
  import {
    getModelForPath,
    hasModel,
    pathOfModel,
    registerModel,
    getBaseline,
    setBaseline,
  } from '../lib/modelRegistry.js';
  import { uriSpecFor } from '../lib/diagnostics.js';
  import { attachDebugDecorations } from '../lib/dap/decorations.js';
  import { resolveMode } from '../lib/theme.js';
  import { notifySaved } from '../lib/lsp/index.js';

  /** 组对象({id, paths, activePath, width})——父层 each 传键控引用 */
  export let group;

  let hostEl = null;
  let editor = null;
  let hostReady = false;
  /** 组×path view state 独立(设计档 §3.1) */
  const viewStates = new Map();
  /** 已挂 hook 的 model(同 model 多组引用时防重复订阅) */
  const hookedModels = new Set();
  let currentPath = null;
  let disposeDebugDecor = null; // 调试装饰清理函数(onMount 赋值)
  let activePathLocal = group.activePath || null;
  let groupPaths = group.paths || [];

  $: syncFromStore(group);
  function syncFromStore(g) {
    if (!g) return;
    groupPaths = g.paths || [];
    activePathLocal = g.activePath || null;
  }

  function langOf(path) {
    if (path.startsWith('evo://settings/')) return 'json';
    const ext = (path.split('.').pop() || '').toLowerCase();
    const map = {
      md: 'markdown', json: 'json', toml: 'ini', rs: 'rust',
      js: 'javascript', mjs: 'javascript', ts: 'typescript', svelte: 'html',
      html: 'html', css: 'css', py: 'python', yml: 'yaml', yaml: 'yaml',
      sh: 'shell', ps1: 'powershell', sql: 'sql', xml: 'xml',
    };
    return map[ext] || 'plaintext';
  }

  function appearance() {
    const s = get(settingsState).settings;
    return {
      theme: resolveMode(s['workbench.theme'] || 'auto') === 'light' ? 'evorule-light' : 'evorule-dark',
      fontSize: typeof s['editor.fontSize'] === 'number' ? s['editor.fontSize'] : 14,
      minimap: { enabled: s['editor.minimap'] != null ? !!s['editor.minimap'] : false },
      wordWrap: s['editor.wordWrap'] === 'on' ? 'on' : 'off',
      tabSize: typeof s['editor.tabSize'] === 'number' ? s['editor.tabSize'] : 4,
    };
  }

  // ---- 跨组共享建模(模块级 pending 去重:并发同路径只建一次) ----

  const pendingCreations = new Map();

  async function getOrCreateSharedModel(path) {
    if (hasModel(path)) return getModelForPath(path);
    const pending = pendingCreations.get(path);
    if (pending) return pending;
    const p = (async () => {
      const tab = await ensureTabOpen(path);
      let model = getModelForPath(path);
      if (model) return model;
      if (tab.error) {
        model = monaco.editor.createModel(`无法打开文件:${path}\n\n${tab.error}`, 'plaintext');
        registerModel(path, model);
        setBaseline(path, '');
        return model;
      }
      const uri = monaco.Uri.parse(uriSpecFor(path));
      try {
        model = monaco.editor.createModel(tab.content, langOf(path), uri);
      } catch {
        model = monaco.editor.getModel(uri) || null; // 并发建同 URI:复用
      }
      if (!model) return null;
      registerModel(path, model);
      setBaseline(path, tab.content);
      return model;
    })();
    pendingCreations.set(path, p);
    try {
      return await p;
    } finally {
      pendingCreations.delete(path);
    }
  }

  async function openInGroup(path) {
    if (!path || !editor) return;
    const model = await getOrCreateSharedModel(path);
    if (!model) return;
    if (!hookedModels.has(model)) {
      hookedModels.add(model);
      model.onDidChangeContent(() => {
        const p = pathOfModel(model);
        if (p) markDirty(p, model.getValue() !== getBaseline(p));
      });
      model.onWillDispose(() => {
        if (editor && editor.getModel() === model) editor.setModel(null);
      });
    }
    if (currentPath && editor.getModel()) {
      viewStates.set(currentPath, editor.saveViewState());
    }
    currentPath = path;
    editor.setModel(model);
    publishCursor(editor); // F1 状态栏:切 model 后光标位兜底发布
    const vs = viewStates.get(path);
    if (vs) editor.restoreViewState(vs);
    editor.focus();
  }

  function pathFromModel(model) {
    return pathOfModel(model);
  }

  async function saveActive() {
    const path = group.activePath;
    if (!path || !editor) return;
    const model = getModelForPath(path);
    if (!model) return;
    const content = model.getValue();
    const err = (await saveTab(path, content)) || '';
    if (!err) {
      setBaseline(path, content);
      markSaved(path, content);
      notifySaved(path);
    }
  }

  function closeGroupTab(path) {
    const { wasLastRef } = groupClosePath(group.id, path);
    if (wasLastRef) {
      if (currentPath === path) currentPath = null;
      closeTab(path); // 全局最后一个引用:真正关 tab(注册表释放随 tabs.subscribe)
    }
  }

  function closeThisGroup() {
    for (const p of [...groupPaths]) {
      const { wasLastRef } = groupClosePath(group.id, p);
      if (wasLastRef) closeTab(p);
    }
  }

  function activateTab(path) {
    groupOpenPath(group.id, path);
    groupFocusSignal.update((s) => ({ id: group.id, n: s.n + 1 }));
  }

  // ---- H1 tab 拖拽:同组重排 + 跨组移动(dispatchTabDrop 统一分发) ----
  let dragFrom = null; // {groupId, path}
  let dropIndex = -1; // 插入位指示(负 = 无)

  function onTabDragStart(e, path) {
    dragFrom = { groupId: group.id, path };
    e.dataTransfer.effectAllowed = 'move';
    e.dataTransfer.setData('text/plain', path);
  }

  function onTabDragOver(e, index) {
    if (!dragFrom) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
    const rect = e.currentTarget.getBoundingClientRect();
    dropIndex = e.clientX > rect.left + rect.width / 2 ? index + 1 : index;
  }

  function onTabDrop(e, index) {
    if (!dragFrom) return;
    e.preventDefault();
    const from = dragFrom;
    const to = dropIndex >= 0 ? dropIndex : index;
    endTabDrag();
    dispatchTabDrop(from.groupId, from.path, group.id, to);
  }

  function endTabDrag() {
    dragFrom = null;
    dropIndex = -1;
  }

  function focusEditor() {
    activeGroupId.set(group.id);
    tick().then(() => editor?.focus());
  }

  // 焦点信号:轮到本组→聚焦编辑器
  let lastFocusN = 0;
  $: if ($groupFocusSignal && $groupFocusSignal.id === group.id && $groupFocusSignal.n !== lastFocusN) {
    lastFocusN = $groupFocusSignal.n;
    tick().then(() => editor?.focus());
  }
  // 成为激活组→聚焦
  let wasActive = false;
  $: {
    const isActive = $activeGroupId === group.id;
    if (isActive && !wasActive && hostReady) {
      tick().then(() => editor?.focus());
    }
    wasActive = isActive;
  }

  onMount(() => {
    setupMonaco();
    // 调试装饰(断点 gutter 圆点+stopped 行高亮+gutter 点击增删;F3)——
    // 本组件的 editor 是文件编辑的真实实例(EditorPane 的 editor 是设置/diff 宿主)
    disposeDebugDecor = attachDebugDecorations({ monaco, editor, pathFromUri: pathOfModel });
    const o = appearance();
    editor = monaco.editor.create(hostEl, {
      theme: o.theme,
      fontSize: o.fontSize,
      minimap: o.minimap,
      wordWrap: o.wordWrap,
      automaticLayout: true,
      fontFamily: '"JetBrains Mono", Consolas, "Microsoft YaHei", monospace',
      lineHeight: 22,
      lineNumbers: 'on',
      renderLineHighlight: 'none',
      scrollBeyondLastLine: false,
    });
    editor.onDidChangeModelContent(() => {
      const path = currentPath;
      const model = getModelForPath(path);
      if (model && editor.getModel() === model) {
        markDirty(path, model.getValue() !== getBaseline(path));
      }
    });
    // F1 状态栏:光标移动即发布行列/语言(焦点编辑器事件,天然只反映当前焦点实例)
    editor.onDidChangeCursorPosition(() => publishCursor(editor));
    hostReady = true;
    if (group.activePath) openInGroup(group.activePath);
  });

  // 组内激活路径变化(父层 store 驱动)→ 打开该路径
  $: if (hostReady && group && group.activePath && group.activePath !== currentPath) {
    openInGroup(group.activePath);
  }

  onDestroy(() => {
    disposeDebugDecor?.(); // 调试装饰清理(先于 editor.dispose 摘订阅)
    if (editor) editor.dispose();
  });
</script>

<div class="group-pane" onmousedown={() => focusEditor()} role="presentation">
  <div class="gp-tabbar">
    <div class="gp-tabs">
      {#each groupPaths as p, i (p)}
        <button
          class="gp-tab"
          class:active={p === activePathLocal}
          class:drop-before={dropIndex === i}
          class:drop-after={dropIndex === groupPaths.length && i === groupPaths.length - 1}
          draggable="true"
          onclick={() => activateTab(p)}
          title={p}
          ondragstart={(e) => onTabDragStart(e, p)}
          ondragover={(e) => onTabDragOver(e, i)}
          ondrop={(e) => onTabDrop(e, i)}
          ondragend={endTabDrag}
        >
          {(p.split('/').pop() || p)}
          {#if $tabs.find((t) => t.path === p)?.dirty}<span class="gp-dirty">●</span>{/if}
          <span
            class="gp-tab-close"
            role="button"
            tabindex="-1"
            title="关闭"
            onclick={(e) => { e.stopPropagation(); closeGroupTab(p); }}
            onkeydown={(e) => e.key === 'Enter' && closeGroupTab(p)}
          >×</span>
        </button>
      {/each}
    </div>
    <button class="gp-group-close" onclick={closeThisGroup} title="关闭本组">×</button>
  </div>
  <Breadcrumbs path={activePathLocal || ''} />
  <div class="gp-host" bind:this={hostEl}></div>
</div>

<style>
  .group-pane {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-width: 0;
    background: var(--bg-card);
    border-left: 1px solid var(--border);
  }
  .gp-tabbar {
    align-items: center;
    background: var(--bg-header, #161b22);
    border-bottom: 1px solid var(--accent);
    display: flex;
    flex-shrink: 0;
    gap: 6px;
    justify-content: space-between;
    padding: 3px 8px;
  }
  .gp-tabs {
    display: flex;
    gap: 4px;
    min-width: 0;
    overflow-x: auto;
  }
  .gp-tab {
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--fg-dim);
    cursor: pointer;
    font-size: 12px;
    padding: 2px 8px;
    white-space: nowrap;
  }
  .gp-tab.active {
    border-color: var(--accent);
    color: var(--fg);
  }
  /* H1 拖拽插入位指示(inset 阴影不引发回流) */
  .gp-tab.drop-before {
    box-shadow: inset 2px 0 0 var(--brand);
  }
  .gp-tab.drop-after {
    box-shadow: inset -2px 0 0 var(--brand);
  }
  .gp-dirty {
    color: #e3b341;
    font-size: 9px;
    margin-left: 2px;
  }
  .gp-tab-close {
    color: var(--fg-dim);
    cursor: pointer;
    margin-left: 6px;
  }
  .gp-tab-close:hover {
    color: #f85149;
  }
  .gp-group-close {
    background: transparent;
    border: none;
    color: var(--fg-dim);
    cursor: pointer;
    font-size: 14px;
  }
  .gp-group-close:hover {
    color: #f85149;
  }
  .gp-host {
    flex: 1;
    min-height: 0;
  }
</style>
