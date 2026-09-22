<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 工作台布局底盘(Trae 范式):
     顶栏 / 左活动栏+侧面板(文件树) / 中编辑器群+底部面板 / 右对话侧栏 -->
<script>
  import TitleBar from './components/TitleBar.svelte';
  import ActivityBar from './components/ActivityBar.svelte';
  import Explorer from './components/Explorer.svelte';
  import EditorPane from './components/EditorPane.svelte';
  import BottomPanel from './components/BottomPanel.svelte';
  import ChatSidebar from './components/ChatSidebar.svelte';
  import { reconnectFromStorage } from './lib/ws.js';

  reconnectFromStorage();
</script>

<div class="app">
  <TitleBar />
  <div class="main">
    <ActivityBar />
    <Explorer />
    <div class="center">
      <EditorPane />
      <BottomPanel />
    </div>
    <ChatSidebar />
  </div>
</div>

<style>
  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }
  .main {
    display: flex;
    flex: 1;
    overflow: hidden;
    min-height: 0;
  }
  .center {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    background: var(--bg-card);
  }
</style>
