<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 编辑器群(Monaco):多 tab + 打开/编辑/保存(文件 REST 面,file_write 同通道)。
     每文件一个 Monaco model(保留 undo 栈),切换即 setModel;Ctrl+S 保存落盘。
     S4 产物协作:agent file_write 产物自动打开,含草稿基线时可切 diff 视图
     (左=agent 草稿,右=当前可编辑);保存即定稿(工作台层留痕,见 stores.js)。 -->
<script>
  import { onMount, onDestroy } from 'svelte';
  import { get } from 'svelte/store';
  import { setupMonaco, monaco } from '../lib/monaco-setup.js';
  import { registerCommand, unregisterCommand } from '../lib/commands.js';
  import {
    tabs,
    activePath,
    artifacts,
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
- agent 写文件后产物自动打开:可切「差异视图」对比 agent 草稿,
  直接增删改,Ctrl+S 保存即定稿(草稿与定稿留痕在工作台内)

## 路线图

- 治理状态徽标下钻(点击查看白名单/信号明细)
- 更多 evorule 专有面板(时光机器 / 记忆)

本编辑器区域即产物协作编辑主场。`;

  let editorEl;
  let diffEl;
  let editor = null;
  let diffEditor = null;
  let welcomeModel = null;
  /** path → monaco model(含 undo 栈与编辑状态) */
  const models = new Map();
  /** path → 保存基线内容(对比判 dirty) */
  const baseline = new Map();
  /** path → 打开失败的错误占位 model(缓存防重复创建) */
  const errorModels = new Map();
  /** path → agent 草稿基线 model(diff 视图左栏) */
  const draftModels = new Map();
  /** 当前激活 tab 是否处于差异视图 */
  let diffOn = false;
  /** 保存失败的临时提示,保存成功即清 */
  let saveError = '';

  /** 当前激活 tab 对应的产物登记(null = 非产物文件) */
  $: activeArtifact = $activePath
    ? $artifacts.find((a) => a.path === $activePath) || null
    : null;

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

  function draftModelFor(artifact) {
    if (draftModels.has(artifact.path)) return draftModels.get(artifact.path);
    const model = monaco.editor.createModel(artifact.draftContent, langOf(artifact.path));
    draftModels.set(artifact.path, model);
    return model;
  }

  function showHost(which) {
    if (editorEl) editorEl.style.display = which === 'edit' ? '' : 'none';
    if (diffEl) diffEl.style.display = which === 'diff' ? '' : 'none';
  }

  function ensureDiffEditor() {
    if (!diffEditor) {
      diffEditor = monaco.editor.createDiffEditor(diffEl, {
        theme: 'evorule-dark',
        automaticLayout: true,
        fontSize: 14,
        fontFamily: '"JetBrains Mono", Consolas, monospace',
        lineHeight: 22,
        minimap: { enabled: false },
        renderLineHighlight: 'none',
        scrollBeyondLastLine: false,
        renderSideBySide: true,
        padding: { top: 16, bottom: 16 },
      });
    }
    return diffEditor;
  }

  function renderActive(path) {
    if (!editor) return;
    if (!path) {
      editor.setModel(welcomeModel);
      showHost('edit');
      return;
    }
    const tab = get(tabs).find((t) => t.path === path) || null;
    if (!tab) {
      editor.setModel(welcomeModel);
      showHost('edit');
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
      showHost('edit');
      return;
    }
    const model = modelFor(tab);
    const artifact = get(artifacts).find((a) => a.path === path) || null;
    if (diffOn && artifact) {
      const de = ensureDiffEditor();
      de.setModel({ original: draftModelFor(artifact), modified: model });
      showHost('diff');
    } else {
      editor.setModel(model);
      showHost('edit');
    }
  }

  function toggleDiff() {
    diffOn = !diffOn;
    renderActive(get(activePath));
  }

  // activePath 变化 → 切换 model(每 tab 默认编辑模式;订阅在 onMount 中建立)
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

  function fmtTime(ts) {
    const d = new Date(ts);
    const p = (n) => String(n).padStart(2, '0');
    return `${p(d.getHours())}:${p(d.getMinutes())}`;
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
    // 本组件命令自注册(命令面板/键位路由统一入口;卸载时注销)。
    // 编辑器内 Ctrl+S 也统一走全局键位路由(when editorFocus 命中)——
    // Monaco addCommand 不阻止 keydown 冒泡,双注册会双触发(实测)。
    registerCommand({
      id: 'workbench.action.file.save',
      title: '保存当前文件',
      category: '文件',
      keybinding: 'ctrl+s',
      when: 'editorFocus || tabsOpen',
      run: saveActive,
    });
    registerCommand({
      id: 'editor.action.toggleDiff',
      title: '切换差异视图',
      category: '编辑器',
      when: 'tabsOpen',
      run: toggleDiff,
    });

    unsubActive = activePath.subscribe((p) => {
      diffOn = false; // 切 tab 回到编辑模式
      renderActive(p);
    });
    return () => {
      unregisterCommand('workbench.action.file.save');
      unregisterCommand('editor.action.toggleDiff');
      if (unsubActive) unsubActive();
    };
  });

  onDestroy(() => {
    for (const m of models.values()) m.dispose();
    for (const m of draftModels.values()) m.dispose();
    for (const m of errorModels.values()) m.dispose();
    if (welcomeModel) welcomeModel.dispose();
    if (editor) editor.dispose();
    if (diffEditor) diffEditor.dispose();
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
  {#if activeArtifact}
    <div class="artifact-bar">
      <span class="ab-tag">agent 产物</span>
      <span class="ab-meta mono" title={activeArtifact.path}>
        {activeArtifact.name} · {activeArtifact.bytes} B · 草稿 {fmtTime(activeArtifact.draftAt)}
      </span>
      <span class="ab-status" class:final={activeArtifact.finalizedAt !== null}>
        {activeArtifact.finalizedAt !== null
          ? `已定稿 ${fmtTime(activeArtifact.finalizedAt)}`
          : '草稿 · 编辑后 Ctrl+S 保存即定稿'}
      </span>
      <button class="ab-diff" onclick={toggleDiff}>
        {diffOn ? '返回编辑' : '查看与草稿差异'}
      </button>
    </div>
  {/if}
  <div class="editor-host" bind:this={editorEl} data-zone="editor"></div>
  <div class="editor-host diff-host" bind:this={diffEl} style="display:none" data-zone="editor"></div>
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
  .artifact-bar {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    padding: 4px var(--sp-md);
    background: var(--bg-header);
    border-bottom: 1px solid var(--border);
    font-size: var(--fs-xs);
    flex-shrink: 0;
  }
  .ab-tag {
    color: var(--brand);
    font-weight: var(--fw-med);
    flex-shrink: 0;
  }
  .ab-meta {
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ab-status {
    color: var(--text-muted);
    flex-shrink: 0;
  }
  .ab-status.final {
    color: var(--ok, #4ade80);
  }
  .ab-diff {
    margin-left: auto;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-xs);
    padding: 2px 10px;
    border-radius: 4px;
    cursor: pointer;
    flex-shrink: 0;
  }
  .ab-diff:hover {
    color: var(--text-primary);
    border-color: var(--brand);
  }
</style>
