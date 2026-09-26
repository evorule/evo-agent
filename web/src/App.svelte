<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 工作台布局底盘(Trae 范式):
     顶栏 / 左活动栏+侧面板(文件树) / 中编辑器群+底部面板 / 右对话侧栏。
     命令基础设施:全局键位路由(规则表解析→命令执行) + 命令面板挂载 +
     视图显隐状态(display 切换,保留组件状态)。编辑器内键位(含 Ctrl+S)
     统一由本路由分发(Monaco addCommand 不阻止 keydown 冒泡,双注册会双触发)。 -->
<script>
  import { onMount, onDestroy } from 'svelte';
  import { get } from 'svelte/store';
  import TitleBar from './components/TitleBar.svelte';
  import ActivityBar from './components/ActivityBar.svelte';
  import Explorer from './components/Explorer.svelte';
  import EditorPane from './components/EditorPane.svelte';
  import BottomPanel from './components/BottomPanel.svelte';
  import ChatSidebar from './components/ChatSidebar.svelte';
  import CommandPalette from './components/CommandPalette.svelte';
  import { reconnectFromStorage, newSession } from './lib/ws.js';
  import {
    activePath,
    closeTab,
    explorerVisible,
    chatVisible,
    panelVisible,
    paletteOpen,
    openPalette,
    refreshSessions,
    refreshGovBadges,
    sessionId,
    sidebarView,
    openSettingsTab,
    openSettingsJson,
  } from './lib/stores.js';
  import { loadSettings } from './lib/settings.js';
  import { registerCommand, unregisterCommand, executeCommand, getCommand } from './lib/commands.js';
  import { initContextTracking } from './lib/context-keys.js';
  import { getEffectiveRules, resolveKeybinding, migrateKeybindings } from './lib/keybindings.js';

  reconnectFromStorage();

  // 本组件注册的命令(卸载时注销;同 id 重复注册=覆盖,热替换安全)
  const OWNED_COMMANDS = [
    'workbench.action.showCommands',
    'workbench.action.file.quickOpen',
    'workbench.action.file.closeTab',
    'workbench.action.view.toggleExplorer',
    'workbench.action.view.toggleChat',
    'workbench.action.view.togglePanel',
    'workbench.action.session.new',
    'workbench.action.session.refresh',
    'workbench.action.session.switch',
    'workbench.action.gov.refreshBadges',
    'workbench.action.openSettings',
    'workbench.action.openSettingsJson',
  ];
  let cleanupTracking = null;

  /** 活动栏条目分发(设置等非文件树视图的宿主接线路由) */
  function handleActivityItem(id) {
    if (id === 'settings') openSettingsTab();
  }

  onMount(() => {
    registerCommand({
      id: 'workbench.action.showCommands',
      title: '显示全部命令',
      category: '帮助',
      keybinding: 'ctrl+shift+p',
      run: () => openPalette('commands'),
    });
    registerCommand({
      id: 'workbench.action.file.quickOpen',
      title: '快速打开文件',
      category: '文件',
      keybinding: 'ctrl+p',
      run: () => openPalette('files'),
    });
    registerCommand({
      id: 'workbench.action.file.closeTab',
      title: '关闭当前标签页',
      category: '文件',
      keybinding: 'ctrl+w', // ⚠ 浏览器保留键:部分浏览器不可拦(web 版限制,远期 PWA/桌面壳解)
      when: 'tabsOpen',
      run: () => {
        const path = get(activePath);
        if (path) closeTab(path);
      },
    });
    registerCommand({
      id: 'workbench.action.view.toggleExplorer',
      title: '切换文件树',
      category: '视图',
      keybinding: 'ctrl+b',
      run: () => explorerVisible.update((v) => !v),
    });
    registerCommand({
      id: 'workbench.action.view.toggleChat',
      title: '切换对话侧栏',
      category: '视图',
      keybinding: 'ctrl+alt+c',
      run: () => chatVisible.update((v) => !v),
    });
    registerCommand({
      id: 'workbench.action.view.togglePanel',
      title: '切换底部面板',
      category: '视图',
      keybinding: 'ctrl+j',
      run: () => panelVisible.update((v) => !v),
    });
    registerCommand({
      id: 'workbench.action.session.new',
      title: '新建会话',
      category: '会话',
      run: newSession,
    });
    registerCommand({
      id: 'workbench.action.session.refresh',
      title: '刷新会话列表',
      category: '会话',
      run: () => refreshSessions(),
    });
    registerCommand({
      id: 'workbench.action.session.switch',
      title: '切换会话',
      category: '会话',
      run: () => openPalette('files'),
    });
    registerCommand({
      id: 'workbench.action.gov.refreshBadges',
      title: '刷新治理徽标',
      category: '治理',
      run: () => refreshGovBadges(get(sessionId)),
    });
    registerCommand({
      id: 'workbench.action.openSettings',
      title: '打开设置',
      category: '首选项',
      run: () => openSettingsTab(),
    });
    registerCommand({
      id: 'workbench.action.openSettingsJson',
      title: '打开用户设置 (JSON)',
      category: '首选项',
      run: () => openSettingsJson('user'),
    });
    loadSettings(); // 设置快照加载(失败降级缓存只读;编辑器参数订阅在 EditorPane)
    migrateKeybindings(); // 旧键位覆盖层一次性迁移(失败保留旧键,下次启动重试)
    cleanupTracking = initContextTracking();
    return () => {
      for (const id of OWNED_COMMANDS) unregisterCommand(id);
      if (cleanupTracking) cleanupTracking();
    };
  });

  /** 全局键位路由:面板打开时让位(面板内部导航);其余按规则表分发。
   *  规则命中但命令未注册(后续批次才落)时仅吞键不执行。 */
  function onGlobalKeydown(e) {
    if (e.isComposing || e.keyCode === 229) return;
    if (get(paletteOpen)) return;
    const rule = resolveKeybinding(e, getEffectiveRules());
    if (rule) {
      e.preventDefault();
      if (getCommand(rule.command)) executeCommand(rule.command);
    }
  }
</script>

<svelte:window on:keydown={onGlobalKeydown} />

<div class="app">
  <TitleBar />
  <div class="main">
    <ActivityBar onitemclick={handleActivityItem} />
    <div style:display={$explorerVisible ? 'contents' : 'none'}>
      {#if $sidebarView === 'explorer'}
        <Explorer />
      {:else if $sidebarView === 'search'}
        <!-- SearchPanel 接入于 B2-PR5 -->
        <div class="side-placeholder"></div>
      {/if}
    </div>
    <div class="center">
      <EditorPane />
      <BottomPanel />
    </div>
    <div style:display={$chatVisible ? 'contents' : 'none'}>
      <ChatSidebar />
    </div>
  </div>
  <CommandPalette />
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
  .side-placeholder {
    width: 220px;
    flex-shrink: 0;
    background: var(--sidebar-bg);
    border-right: 1px solid var(--border);
  }
</style>
