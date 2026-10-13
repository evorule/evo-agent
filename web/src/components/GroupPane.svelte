<!-- SPDX-License-Identifier: AGPL-3.0-or-later
Copyright (C) 2026 EvoRule Project
This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
分屏副组:普通文件 tab 的第二编辑器实例。与主组共享同一 model(ModelRegistry
按 evofile:/// Uri 单源)——同文件两组编辑内容/undo 实时同步;view state 独立。
diff/产物/设置页留在主组(轻量实现边界,完整组模型随后续批次)。 -->
<script>
  import { onMount, onDestroy, tick } from 'svelte';
  import { get } from 'svelte/store';
  import { setupMonaco, monaco } from '../lib/monaco-setup.js';
  import { splitPath, splitWidth, closeSplit, persistSplit, splitOpen } from '../lib/editorGroups.js';
  import { saveTab, markDirty, markSaved, tabs } from '../lib/stores.js';
  import { settingsState } from '../lib/settings.js';
  import {
    getModelForPath,
    hasModel,
    registerModel,
    getBaseline,
    setBaseline,
  } from '../lib/modelRegistry.js';
  import { uriSpecFor } from '../lib/diagnostics.js';
  import { notifySaved } from '../lib/lsp/index.js';

  let hostEl = null;
  let editor = null;
  /** 本组 view state(path → saveViewState 快照) */
  const viewStates = new Map();
  /** 当前挂载的 model(切走时 save view state) */
  let currentPath = null;

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
      theme: s['workbench.theme'] || 'evorule-dark',
      fontSize: typeof s['editor.fontSize'] === 'number' ? s['editor.fontSize'] : 14,
      minimap: { enabled: s['editor.minimap'] != null ? !!s['editor.minimap'] : false },
      wordWrap: s['editor.wordWrap'] === 'on' ? 'on' : 'off',
      tabSize: typeof s['editor.tabSize'] === 'number' ? s['editor.tabSize'] : 4,
    };
  }

  /** 打开路径:共享 model(ModelRegistry 单源,主组已开则直接挂;否则读文件建) */
  async function openInSplit(path) {
    if (!path) return;
    let model = getModelForPath(path);
    if (!model) {
      const tab = get(tabs).find((t) => t.path === path);
      let content = tab?.content ?? '';
      if (!tab) {
        try {
          const res = await fetch(`/api/files/read?path=${encodeURIComponent(path)}`, { headers: {} }).then((r) => r.json());
          content = res.content ?? '';
        } catch (e) {
          return; // 读取失败:静默(副组不建错误占位)
        }
      }
      model = monaco.editor.createModel(
        content,
        langOf(path),
        monaco.Uri.parse(uriSpecFor(path)),
      );
      model.updateOptions({ tabSize: appearance().tabSize });
      registerModel(path, model);
      if (!hasBaseline(path)) setBaseline(path, content);
      model.onDidChangeModelContent(() => {
        if (get(splitPath) === path) {
          markDirty(path, model.getValue() !== getBaseline(path));
        }
      });
      model.onWillDispose(() => {
        if (editor && editor.getModel() === model) editor.setModel(null);
      });
    }
    // 切 model 前 save 旧 view state,后 restore 新(组×path 独立)
    if (currentPath && editor.getModel()) {
      viewStates.set(currentPath, editor.saveViewState());
    }
    currentPath = path;
    editor.setModel(model);
    const vs = viewStates.get(path);
    if (vs) editor.restoreViewState(vs);
    editor.focus();
  }

  function hasBaseline(path) {
    return getBaseline(path) !== '';
  }

  async function saveActive() {
    const path = get(splitPath);
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

  function onKeydown(e) {
    // 副组聚焦时 Ctrl+S 保存(主组实例级 addCommand 不覆盖副组)
    if ((e.ctrlKey || e.metaKey) && e.key === 's') {
      if (document.activeElement && hostEl?.contains(document.activeElement)) {
        e.preventDefault();
        saveActive();
      }
    }
  }

  onMount(() => {
    setupMonaco();
    const o = appearance();
    editor = monaco.editor.create(hostEl, {
      theme: o.theme,
      fontSize: o.fontSize,
      minimap: o.minimap,
      wordWrap: o.wordWrap,
      automaticLayout: true,
      fontFamily: '"JetBrains Mono", Consolas, monospace',
      lineHeight: 22,
      lineNumbers: 'on',
      renderLineHighlight: 'none',
      scrollBeyondLastLine: false,
    });
    editor.onDidChangeModelContent(() => {
      const path = get(splitPath);
      const model = getModelForPath(path);
      if (model && editor.getModel() === model) {
        markDirty(path, model.getValue() !== getBaseline(path));
      }
    });
    if ($splitPath) openInSplit($splitPath);
    const unsub = splitPath.subscribe((p) => {
      if (p && editor) openInSplit(p);
    });
    const unsubPersist = splitOpen.subscribe(() => persistSplit());
    window.addEventListener('keydown', onKeydown);
    return () => {
      unsub();
      unsubPersist();
      window.removeEventListener('keydown', onKeydown);
      persistSplit();
    };
  });

  onDestroy(() => {
    if (editor) editor.dispose();
  });
</script>

<svelte:window on:keydown={(e) => { if ((e.ctrlKey || e.metaKey) && e.key === 's' && hostEl?.contains(document.activeElement)) { e.preventDefault(); saveActive(); } }} />

<div class="group-pane">
  <div class="gp-tabbar">
    <span class="gp-title mono">{$splitPath ? ($splitPath.split('/').pop() || $splitPath) : '空'}</span>
    <button class="gp-close" onclick={() => closeSplit()} title="关闭分屏">×</button>
  </div>
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
    justify-content: space-between;
    padding: 3px 8px;
  }
  .gp-title {
    color: var(--fg);
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .gp-close {
    background: transparent;
    border: none;
    color: var(--fg-dim);
    cursor: pointer;
    font-size: 14px;
  }
  .gp-close:hover {
    color: var(--fg);
  }
  .gp-host {
    flex: 1;
    min-height: 0;
  }
</style>
