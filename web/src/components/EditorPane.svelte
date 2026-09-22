<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 编辑器群(Monaco):S0 以欢迎页验证编辑器内核与主题集成;
     文件打开/编辑/保存与多 tab 在标准 IDE 基础阶段接入 -->
<script>
  import { onMount } from 'svelte';
  import { setupMonaco, monaco } from '../lib/monaco-setup.js';

  const welcomeText = `# evo-agent 工作台

这里是 evo-agent 的工作台 —— 以标准编程 IDE 为底盘,
叠加 evorule 治理能力(agent 会话 / 审批 / 审计 / 规则)。

## 现在可以做什么

- 在右侧对话侧栏与 general agent 对话(真实模型流式回复)
- agent 的工具调用会以摘要卡片呈现在对话流中
- 需要人工审批的高风险工具调用会出现审批卡片

## 路线图

- 文件树与文件编辑(打开 / 修改 / 保存,入审批与审计)
- 对话历史与会话恢复
- 治理叠加(审批交互 / 审计抽屉 / 治理状态徽标)
- agent 产物与人协作编辑(agent 草稿 → 人定稿 → 落盘)

本编辑器区域即未来的产物协作编辑主场。`;

  let editorEl;
  let editor;

  onMount(() => {
    setupMonaco();
    editor = monaco.editor.create(editorEl, {
      value: welcomeText,
      language: 'markdown',
      theme: 'evorule-dark',
      readOnly: true,
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
    return () => editor.dispose();
  });
</script>

<div class="editor-pane">
  <div class="tabbar">
    <div class="etab active">欢迎</div>
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
    background: var(--bg-header);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .etab {
    padding: var(--sp-sm) var(--sp-md);
    font-size: var(--fs-sm);
    font-weight: var(--fw-med);
    color: var(--text-primary);
    border-bottom: 2px solid var(--brand);
    background: var(--bg-card);
  }
  .editor-host {
    flex: 1;
    min-height: 0;
  }
</style>
