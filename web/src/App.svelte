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
  import SearchPanel from './components/SearchPanel.svelte';
  import ScmView from './components/ScmView.svelte';
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
    searchIntent,
    openSettingsTab,
    openSettingsJson,
    refreshGitStatus,
    gitStatus,
    openGitDiff,
    openFile,
  } from './lib/stores.js';
  import { loadSettings } from './lib/settings.js';
  import { registerCommand, unregisterCommand, executeCommand, getCommand } from './lib/commands.js';
  import { initContextTracking } from './lib/context-keys.js';
  import { getEffectiveRules, resolveKeybinding, migrateKeybindings } from './lib/keybindings.js';
  import { gitStage } from './lib/api.js';
  import { nextProblem, markerToReveal } from './lib/diagnostics.js';
  import { openBottomPanel, terminalIntent, outputChannel } from './lib/panel.js';
  import TaskPicker from './components/TaskPicker.svelte';
  import ConfirmRunDialog from './components/ConfirmRunDialog.svelte';
  import { taskList, refreshTaskList, runTaskWithConfirm, cancelTaskRun, lastTaskLabel } from './lib/tasks.js';
  import GroupPane from './components/GroupPane.svelte';
  import { splitOpen, toggleSplit, closeSplit, splitPath, splitWidth } from './lib/editorGroups.js';
  import { lspBanner, restartGate, gateSnapshot } from './lib/lsp/index.js';
  import { pushPanelEvent } from './lib/stores.js';

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
    'workbench.action.search.show',
    'workbench.action.search.replace',
    'workbench.action.search.clear',
    'workbench.action.git.commit',
    'workbench.action.git.stageAll',
    'workbench.action.git.refresh',
    'workbench.action.git.openDiff',
    'workbench.action.problems.focus',
    'workbench.action.problems.next',
    'workbench.action.problems.prev',
    'workbench.action.problems.nextInFile',
    'workbench.action.lsp.restart',
    'workbench.action.lsp.status',
    'workbench.action.terminal.new',
    'workbench.action.terminal.kill',
    'workbench.action.terminal.clear',
    'workbench.action.terminal.focus',
    'workbench.action.tasks.run',
    'workbench.action.tasks.rerunLast',
    'workbench.action.tasks.terminate',
    'workbench.action.tasks.build',
    'view.splitEditor',
    'view.closeSplit',
  ];
  let cleanupTracking = null;

  /** 活动栏条目分发(设置等非文件树视图的宿主接线路由) */
  function handleActivityItem(id) {
    if (id === 'settings') openSettingsTab();
  }

  // ---- 任务面(B10):选择器/首跑确认/运行编排 ----
  let taskPickerOpen = false;
  let confirmTask = null;
  let confirmResolve = null;

  async function openTaskPicker() {
    await refreshTaskList();
    taskPickerOpen = true;
  }

  async function runByLabel(label) {
    await refreshTaskList();
    const task = (get(taskList).tasks || []).find((t) => t.label === label);
    if (!task) {
      pushPanelEvent('sys', { label: '[task]', detail: `任务不存在: ${label}`, level: 'error' });
      return;
    }
    executeRun(task);
  }

  async function runDefaultBuild() {
    await refreshTaskList();
    const builds = (get(taskList).tasks || []).filter((t) => t.group === 'build');
    const target = builds.find((t) => t.isDefault) || builds[0];
    if (!target) {
      pushPanelEvent('sys', { label: '[task]', detail: '未配置构建任务(group=build)', level: 'warn' });
      await refreshTaskList();
      taskPickerOpen = true;
      return;
    }
    executeRun(target);
  }

  function executeRun(task) {
    openBottomPanel('output');
    outputChannel.set(`task:${task.label}`);
    runTaskWithConfirm(task, {
      onConfirm: (t) =>
        new Promise((resolve) => {
          confirmTask = t;
          confirmResolve = resolve;
        }),
    })
      .then((runId) => {
        if (runId) refreshTaskList();
      })
      .catch((e) => {
        pushPanelEvent('sys', { label: '[task]', detail: String(e?.message || e), level: 'error' });
      });
  }

  function onTaskPicked(e) {
    taskPickerOpen = false;
    const task = e.detail;
    openBottomPanel('output');
    outputChannel.set(`task:${task.label}`);
    executeRun(task);
  }

  function onConfirmRun(e) {
    confirmResolve?.(true);
    confirmResolve = null;
    confirmTask = null;
    if (!e.detail.remember) {
      // 仅本次:立即可再确认(不做持久化)
    }
  }

  function onCancelConfirm() {
    confirmResolve?.(false);
    confirmResolve = null;
    confirmTask = null;
  }

  /** 搜索视图入口:切视图 + 保证侧栏可见 + 发一次性意图信号(SearchPanel 消费) */
  function showSearch(intent = 'show') {
    sidebarView.set('search');
    explorerVisible.set(true);
    searchIntent.set(intent);
  }

  /** 源代码管理视图入口:切视图 + 保证侧栏可见 + 立即刷新状态 */
  function showScm() {
    sidebarView.set('scm');
    explorerVisible.set(true);
    refreshGitStatus(0);
  }

  /** 问题导航(F8 系):按确定性序取下一个/上一个问题并聚焦。
   *  inFile=仅在当前文件内循环;无问题时不动作(不自动弹面板)。 */
  function gotoProblem({ inFile = false, backward = false } = {}) {
    const m = nextProblem({ activePath: get(activePath), inFile, backward });
    if (!m) return;
    openFile(m.path, markerToReveal(m));
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
    registerCommand({
      id: 'workbench.action.search.show',
      title: '在文件中搜索',
      category: '搜索',
      keybinding: 'ctrl+shift+f',
      run: () => showSearch('show'),
    });
    registerCommand({
      id: 'workbench.action.search.replace',
      title: '在文件中替换',
      category: '搜索',
      keybinding: 'ctrl+shift+h',
      run: () => showSearch('replace'),
    });
    registerCommand({
      id: 'workbench.action.search.clear',
      title: '清除搜索结果',
      category: '搜索',
      run: () => searchIntent.set('clear'),
    });
    registerCommand({
      id: 'workbench.action.git.commit',
      title: 'Git: 提交',
      category: 'Git',
      run: showScm, // 提交需消息输入 → 聚焦源代码管理视图(提交框)
    });
    registerCommand({
      id: 'workbench.action.git.stageAll',
      title: 'Git: 暂存全部更改',
      category: 'Git',
      run: async () => {
        const st = get(gitStatus);
        const paths = (st?.changes || []).map((e) => e.path);
        if (paths.length) await gitStage(paths);
        refreshGitStatus(0);
      },
    });
    registerCommand({
      id: 'workbench.action.git.refresh',
      title: 'Git: 刷新状态',
      category: 'Git',
      run: () => refreshGitStatus(0),
    });
    registerCommand({
      id: 'workbench.action.git.openDiff',
      title: 'Git: 打开差异视图',
      category: 'Git',
      run: (path) => {
        if (path) openGitDiff(path);
        else showScm(); // 无参调用(命令面板)→ 聚源代码管理视图选文件
      },
    });
    registerCommand({
      id: 'workbench.action.problems.focus',
      title: '打开问题面板',
      category: '问题',
      run: () => openBottomPanel('problems'),
    });
    registerCommand({
      id: 'workbench.action.problems.next',
      title: '转到下一个问题',
      category: '问题',
      run: () => gotoProblem(),
    });
    registerCommand({
      id: 'workbench.action.problems.prev',
      title: '转到上一个问题',
      category: '问题',
      run: () => gotoProblem({ backward: true }),
    });
    registerCommand({
      id: 'workbench.action.problems.nextInFile',
      title: '转到文件内下一个问题',
      category: '问题',
      run: () => gotoProblem({ inFile: true }),
    });
    registerCommand({
      id: 'view.splitEditor',
      title: '视图: 分屏(开关副组)',
      category: '视图',
      keybinding: 'ctrl+\\',
      run: () => toggleSplit(get(activePath)),
    });
    registerCommand({
      id: 'view.closeSplit',
      title: '视图: 关闭分屏',
      category: '视图',
      run: () => closeSplit(),
    });
    registerCommand({
      id: 'workbench.action.tasks.run',
      title: '任务: 运行任务',
      category: '任务',
      run: () => openTaskPicker(),
    });
    registerCommand({
      id: 'workbench.action.tasks.rerunLast',
      title: '任务: 重新运行上次任务',
      category: '任务',
      run: () => {
        const last = get(lastTaskLabel);
        if (last) runByLabel(last);
      },
    });
    registerCommand({
      id: 'workbench.action.tasks.terminate',
      title: '任务: 终止运行中任务',
      category: '任务',
      run: () => {
        const runId = newestActiveRun();
        if (runId) cancelTaskRun(runId).catch((e) => pushPanelEvent('sys', { label: '[task]', detail: String(e?.message || e), level: 'error' }));
      },
    });
    registerCommand({
      id: 'workbench.action.tasks.build',
      title: '任务: 运行生成任务',
      category: '任务',
      keybinding: 'ctrl+shift+b',
      run: () => runDefaultBuild(),
    });
    registerCommand({
      id: 'workbench.action.lsp.restart',
      title: 'LSP: 重启语言服务器',
      category: '语言服务器',
      run: () => {
        // 无参调用(命令面板/横幅按钮):重启所有已建连的语言门
        const gates = gateSnapshot();
        let restarted = 0;
        for (const [lang, info] of Object.entries(gates)) {
          if (info && (info.state !== 'idle' || info.pathAvailable)) {
            restartGate(lang);
            restarted++;
          }
        }
        if (!restarted) {
          pushPanelEvent('sys', { label: '[lsp]', detail: '无活动语言门(打开对应语言文件后可重启)' });
        }
      },
    });
    registerCommand({
      id: 'workbench.action.terminal.new',
      title: '终端: 新建终端',
      category: '终端',
      run: () => {
        terminalIntent.set('new');
        openBottomPanel('terminal');
      },
    });
    registerCommand({
      id: 'workbench.action.terminal.kill',
      title: '终端: 关闭当前终端',
      category: '终端',
      run: () => {
        terminalIntent.set('kill');
        openBottomPanel('terminal');
      },
    });
    registerCommand({
      id: 'workbench.action.terminal.clear',
      title: '终端: 清屏',
      category: '终端',
      run: () => {
        terminalIntent.set('clear');
        openBottomPanel('terminal');
      },
    });
    registerCommand({
      id: 'workbench.action.terminal.focus',
      title: '终端: 聚焦终端',
      category: '终端',
      run: () => {
        openBottomPanel('terminal');
      },
    });
    registerCommand({
      id: 'workbench.action.lsp.status',
      title: 'LSP: 语言服务器状态',
      category: '语言服务器',
      run: () => {
        const gates = gateSnapshot();
        for (const [lang, info] of Object.entries(gates)) {
          if (info) {
            pushPanelEvent('sys', {
              label: '[lsp]',
              detail: `${lang}: state=${info.state} PATH=${info.pathAvailable ? 'ok' : 'missing'}${info.version ? ` (${info.version})` : ''} docs=${info.docs}`,
            });
          }
        }
        openBottomPanel('output');
      },
    });
    loadSettings(); // 设置快照加载(失败降级缓存只读;编辑器参数订阅在 EditorPane)
    migrateKeybindings(); // 旧键位覆盖层一次性迁移(失败保留旧键,下次启动重试)
    cleanupTracking = initContextTracking();
    refreshGitStatus(0); // git 状态启动加载(B3;后续 fs_events 防抖自愈)
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
        <SearchPanel />
      {:else if $sidebarView === 'scm'}
        <ScmView />
      {/if}
    </div>
    <div class="center">
      {#if $lspBanner}
        <div class="lsp-banner" role="alert">
          <span class="lsp-banner-text">
            语言服务器失败（{$lspBanner.serverName}）：{$lspBanner.detail}
          </span>
          <button class="lsp-banner-retry" onclick={() => restartGate($lspBanner.lang)}>重启</button>
          <button class="lsp-banner-dismiss" onclick={() => lspBanner.set(null)} aria-label="关闭提示">×</button>
        </div>
      {/if}
      <div class="editors-row">
        <EditorPane />
        {#if $splitOpen}
          <div class="split-pane" style:width={`${Math.round($splitWidth * 100)}%`}>
            <GroupPane />
          </div>
        {/if}
      </div>
      <BottomPanel />
    </div>
    <div style:display={$chatVisible ? 'contents' : 'none'}>
      <ChatSidebar />
    </div>
  </div>
  {#if taskPickerOpen}
    <TaskPicker tasks={get(taskList).tasks || []} on:pick={onTaskPicked} on:close={() => (taskPickerOpen = false)} />
  {/if}
  {#if confirmTask}
    <ConfirmRunDialog task={confirmTask} on:confirm={onConfirmRun} on:cancel={onCancelConfirm} />
  {/if}
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
  .editors-row {
    display: flex;
    flex: 1;
    min-height: 0;
    min-width: 0;
    overflow: hidden;
  }
  .editors-row :global(.editor-pane) {
    flex: 1;
    min-width: 0;
  }
  .lsp-banner {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 12px;
    background: var(--warn-bg, #3d2e00);
    border-bottom: 1px solid var(--border, #30363d);
    color: var(--warn-fg, #f0c674);
    font-size: 12px;
    flex-shrink: 0;
  }
  .lsp-banner-text {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .lsp-banner-retry {
    background: transparent;
    border: 1px solid currentcolor;
    border-radius: 4px;
    color: inherit;
    cursor: pointer;
    font-size: 12px;
    padding: 2px 10px;
  }
  .lsp-banner-dismiss {
    background: transparent;
    border: none;
    color: inherit;
    cursor: pointer;
    font-size: 14px;
    line-height: 1;
    padding: 2px 6px;
  }
</style>
