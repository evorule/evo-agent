<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 编辑器群(Monaco):多 tab + 打开/编辑/保存(文件 REST 面,file_write 同通道)。
     每文件一个 Monaco model(保留 undo 栈),切换即 setModel;Ctrl+S 保存落盘。 -->
<script>
  import { onMount, onDestroy } from 'svelte';
  import { get } from 'svelte/store';
  import { setupMonaco, monaco } from '../lib/monaco-setup.js';
  import {
    tabs,
    activePath,
    closeTab,
    markDirty,
    saveTab,
  } from '../lib/stores.js';

  const welcomeText = `# evo-agent 工作台

这里是 evo-agent 的工作台 —— 以标准编程 IDE 为底盘,
叠加 evorule 治理能力(agent 会话 / 审批 / 审计 / 规则)。

## 现在可以做什么

- 左侧文件树浏览工作目录,点击文件即可打开编辑
- 编辑器内 Ctrl+S 保存落盘(与 agent 写文件同一安全通道)
- 在右侧对话侧栏与 general agent 对话(真实模型流式回复)
- agent 的工具调用会以摘要卡片呈现在对话流中

## 路线图

- 对话历史与会话恢复
- 治理叠加(审批交互 / 审计抽屉 / 治理状态徽标)
- agent 产物与人协作编辑(agent 草稿 → 人定稿 → 落盘)

本编辑器区域即未来的产物协作编辑主场。`;

  let editorEl;
  let editor = null;
  let welcomeModel = null;
  /** path → monaco model(含 undo 栈与编辑状态) */
  const models = new Map();
  /** path → 保存基线内容(对比判 dirty) */
  const baseline = new Map();
  /** path → 打开失败的错误占位 model(缓存防重复创建) */
  const errorModels = new Map();
  /** 保存失败的临时提示,保存成功即清 */
  let saveError = '';

  function langOf(path) {
    const ext = (path.split('.').pop() || '').toLowerCase();
    const map = {
      md: 'markdown',
      json: 'json',
      toml: 'ini',
      rs: 'rust',
      js: 'javascript',
      mjs: 'javascript',
      ts: 'typescript',
      svelte: 'html',
      html: 'html',
      css: 'css',
      py: 'python',
      yml: 'yaml',
      yaml: 'yaml',
      sh: 'shell',
      ps1: 'powershell',
      sql: 'sql',
      xml: 'xml',
    };
    return map[ext] || 'plaintext';
  }

  function modelFor(tab) {
    if (models.has(tab.path)) return models.get(tab.path);
    const model = monaco.editor.createModel(tab.content, langOf(tab.path));
    baseline.set(tab.path, tab.content);
    models.set(tab.path, model);
    return model;
  }

  function showTab(path) {
    if (!editor) return;
    if (!path) {
      editor.setModel(welcomeModel);
      return;
    }
    const tab = get(tabs).find((t) => t.path === path) || null;
    if (!tab) {
      editor.setModel(welcomeModel);
      return;
    }
    if (tab.error) {
      // 打开失败的占位:只读展示错误(缓存防重复创建)
      if (!errorModels.has(tab.path)) {
        errorModels.set(
          tab.path,
          monaco.editor.createModel(`无法打开文件:${tab.path}\n\n${tab.error}`, 'plaintext'),
        );
      }
      editor.setModel(errorModels.get(tab.path));
      return;
    }
    editor.setModel(modelFor(tab));
  }

  // activePath 变化 → 切换 model(订阅在 onMount 中建立)
  let unsubActive = null;

  async function saveActive() {
    const path = $activePath;
    if (!path || !editor) return;
    const model = models.get(path);
    if (!model) return;
    const content = model.getValue();
    saveError = (await saveTab(path, content)) || '';
    if (!saveError) baseline.set(path, content);
  }

  function onKeyDown(e) {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') {
      e.preventDefault();
      saveActive();
    }
  }

  onMount(() => {
    setupMonaco();
    welcomeModel = monaco.editor.createModel(welcomeText, 'markdown');
    editor = monaco.editor.create(editorEl, {
      model: welcomeModel,
      theme: 'evorule-dark',
      readOnly: false,
      automaticLayout: true,
      fontSize: 14,
      fontFamily: '"JetBrains Mono", Consolas, monospace',
      lineHeight: 22,
      minimap: { enabled: false },
      lineNumbers: 'on',
      renderLineHighlight: 'none',
      scrollBeyondLastLine: false,
      padding: { top: 16, bottom: 16 },
    });
    editor.onDidChangeModelContent(() => {
      const path = $activePath;
      const model = models.get(path);
      if (model && editor.getModel() === model) {
        markDirty(path, model.getValue() !== baseline.get(path));
      }
    });
    // 编辑器内 Ctrl+S(Monaco 拦截浏览器默认行为)
    editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, saveActive);
    // 焦点不在编辑器时(tab 栏 / 侧栏)的全局兜底
    window.addEventListener('keydown', onKeyDown);

    unsubActive = activePath.subscribe((p) => showTab(p));
    return () => {
      window.removeEventListener('keydown', onKeyDown);
      if (unsubActive) unsubActive();
    };
  });

  onDestroy(() => {
    for (const m of models.values()) m.dispose();
    if (welcomeModel) welcomeModel.dispose();
    if (editor) editor.dispose();
  });
</script>

<div class="editor-pane">
  <div class="tabbar">
    {#if $tabs.length === 0}
      <div class="etab active">欢迎</div>
    {:else}
      {#each $tabs as t (t.path)}
        <div
          class="etab file"
          class:active={t.path === $activePath}
          role="tab"
          tabindex="0"
          onclick={() => activePath.set(t.path)}
          onkeydown={(e) => e.key === 'Enter' && activePath.set(t.path)}
        >
          <span class="tab-name">{t.name}</span>
          {#if t.dirty}<span class="dot" title="未保存"></span>{/if}
          <button
            class="close"
            title="关闭"
            onclick={(e) => {
              e.stopPropagation();
              closeTab(t.path);
            }}>×</button
          >
        </div>
      {/each}
    {/if}
    {#if saveError}
      <div class="save-error" title={saveError}>保存失败</div>
    {/if}
  </div>
  <div class="editor-host" bind:this={editorEl}></div>
</div>

<style>
  .editor-pane {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .tabbar {
    display: flex;
    align-items: stretch;
    background: var(--bg-header);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
    overflow-x: auto;
  }
  .etab {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: var(--sp-sm) var(--sp-md);
    font-size: var(--fs-sm);
    font-weight: var(--fw-med);
    color: var(--text-secondary);
    border-right: 1px solid var(--border);
    cursor: pointer;
    user-select: none;
    white-space: nowrap;
  }
  .etab.active {
    color: var(--text-primary);
    border-bottom: 2px solid var(--brand);
    background: var(--bg-card);
  }
  .etab.file {
    max-width: 220px;
  }
  .tab-name {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--brand);
    flex-shrink: 0;
  }
  .close {
    border: none;
    background: transparent;
    color: var(--text-muted);
    font-size: 14px;
    line-height: 1;
    padding: 0 2px;
    cursor: pointer;
    border-radius: 3px;
  }
  .close:hover {
    color: var(--text-primary);
    background: rgba(255, 255, 255, 0.1);
  }
  .save-error {
    align-self: center;
    margin-left: var(--sp-sm);
    font-size: var(--fs-xs);
    color: #f87171;
  }
  .editor-host {
    flex: 1;
    min-height: 0;
  }
</style>
