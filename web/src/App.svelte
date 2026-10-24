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
  import OrchestrationPanel from './components/OrchestrationPanel.svelte';
  import ExtensionsPanel from './components/ExtensionsPanel.svelte';
  import DebugView from './components/DebugView.svelte';
  import EditorPane from './components/EditorPane.svelte';
  import BottomPanel from './components/BottomPanel.svelte';
  import ChatSidebar from './components/ChatSidebar.svelte';
  import StatusBar from './components/StatusBar.svelte';
  import CommandPalette from './components/CommandPalette.svelte';
  import { reconnectFromStorage, newSession, openSession } from './lib/ws.js';
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
    scmGenerateIntent,
    connBanner,
  } from './lib/stores.js';
  import { loadSettings, settingsState } from './lib/settings.js';
  import { applyTheme, readStoredMode, resolveMode } from './lib/theme.js';
  import * as monaco from 'monaco-editor';
  import { registerCommand, unregisterCommand, executeCommand, getCommand } from './lib/commands.js';
  import { initContextTracking } from './lib/context-keys.js';
  import { getEffectiveRules, resolveKeybinding, migrateKeybindings } from './lib/keybindings.js';
  import { gitStage, indexStatus, indexRebuild } from './lib/api.js';
  import { setSetting } from './lib/settings.js';
  import { nextProblem, markerToReveal } from './lib/diagnostics.js';
  import { openBottomPanel, terminalIntent, outputChannel } from './lib/panel.js';
  import TaskPicker from './components/TaskPicker.svelte';
  import ConfirmRunDialog from './components/ConfirmRunDialog.svelte';
  import { taskList, refreshTaskList, runTaskWithConfirm, cancelTaskRun, lastTaskLabel } from './lib/tasks.js';
  import GroupPane from './components/GroupPane.svelte';
  import ConsoleView from './components/ConsoleView.svelte';
  import Divider from './components/Divider.svelte';
  import {
    sidebarWidth,
    setSidebarWidth,
    applySidebarWidth,
    hasStoredSidebarWidth,
    SIDEBAR_DEFAULT,
    chatWidth,
    setChatWidth,
    CHAT_DEFAULT,
    groupPercentFromDrag,
  } from './lib/layout.js';
  import { splitOpen, toggleSplit, closeSplit, secondaryGroups, activeGroupId, addSecondaryGroup, closeSecondaryGroup, moveActiveTabTarget, groupRemovePath, evenWidths as doEvenWidths, groupFocusSignal, restoreGroups, maximizedGroupId, toggleMaximize, setGroupWidth } from './lib/editorGroups.js';
  import { lspBanner, restartGate, gateSnapshot } from './lib/lsp/index.js';
  import { pushPanelEvent } from './lib/stores.js';

  reconnectFromStorage();

  // UI 基准字号接线:设置快照变化 -> :root 缩放因子(全部 --fs-* 令牌 calc 联动);
  // 顶层订阅:缓存回放值在首帧渲染前生效,零闪变。编辑器/终端字号独立设置,不随本键。
  let lastUiFontSize = null;
  const unsubUiFontSize = settingsState.subscribe((s) => {
    const v = s.settings['workbench.uiFontSize'];
    if (typeof v === 'number' && v > 0 && v !== lastUiFontSize) {
      lastUiFontSize = v;
      document.documentElement.style.setProperty('--ui-font-scale', String(v / 13));
    }
  });

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
    'workbench.action.git.generateCommitMessage',
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
    'view.moveTabToNextGroup',
    'view.evenWidths',
    'view.focusGroup1',
    'view.focusGroup2',
    'view.focusGroup3',
    'workbench.toggleTheme',
    'workbench.selectTheme',
  ];
  let cleanupTracking = null;

  // ---- E2 divider:侧栏/副组尺寸接线 ----
  let editorsRowEl = null; // 副组宽换算的分母(行实际像素宽)
  let sidebarSlotEl = null; // 侧栏视图宿主(未定制宽度时从活动视图实测初始化)
  let sidebarVarApplied = false; // CSS 变量生效旗标(生效前各视图用自身缺省宽)

  /** 侧栏 divider 拖拽(dx>0 = 变宽;首事件从活动视图实测宽度初始化,防跳变) */
  function onSidebarResize(e) {
    if (!sidebarVarApplied) {
      sidebarVarApplied = true;
      const el = sidebarSlotEl && sidebarSlotEl.firstElementChild;
      const w = el ? Math.round(el.getBoundingClientRect().width) : $sidebarWidth;
      setSidebarWidth(w);
    }
    setSidebarWidth($sidebarWidth + e.detail.dx);
  }

  /** 副组 divider 拖拽:分隔条在目标组左侧,dx>0(右拖) = 目标组变窄 */
  function nudgeGroupWidth(id, dx, rowEl) {
    const g = get(secondaryGroups).find((x) => x.id === id);
    if (!g || !rowEl || !(rowEl.clientWidth > 0)) return;
    const start = parseFloat(String(g.width)) || 0;
    const pct = groupPercentFromDrag(start, -dx, rowEl.clientWidth);
    setGroupWidth(id, `${pct}%`);
  }

  /** 活动栏点击(Trae 语义):侧面板视图点在已激活项=收起侧栏,再点=展开;
   *  home 是内容区入口;设置在菜单行、console 面板入口在顶栏工作台钮。 */
  function handleActivityItem(id) {
    if (id === 'home') return;
    if (get(sidebarView) === id && get(explorerVisible)) {
      explorerVisible.set(false);
    } else {
      explorerVisible.set(true);
    }
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
    restoreGroups(); // 恢复上次分屏状态(组结构+成员路径,localStorage)
    // E2 侧栏宽度:有持久定制则立即使 CSS 变量生效;否则各视图用自身缺省宽,
    // 首次拖拽时实测初始化(防跳变)。订阅仅在其后生效期间同步变量。
    if (hasStoredSidebarWidth()) {
      sidebarVarApplied = true;
      applySidebarWidth($sidebarWidth);
    }
    const unsubSidebarW = sidebarWidth.subscribe((w) => {
      if (sidebarVarApplied) applySidebarWidth(w);
    });
    // console 回链深链(/?session=N,console 审计页「在工作台打开」目标):
    // 换线打开对应会话;非法 sid 忽略(fail-soft);读后清参防刷新重放。
    try {
      const sp = new URLSearchParams(window.location.search);
      const sid = sp.get('session');
      if (sid && /^\d+$/.test(sid)) {
        openSession(sid);
        window.history.replaceState(null, '', window.location.pathname);
      }
    } catch {}
    window.addEventListener('unhandledrejection', (e) => {
      try { localStorage.setItem('dbg_rej', String(e.reason).slice(0, 250)); } catch {}
    });
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
    // B15:AI 生成提交消息(聚焦 SCM 视图 + 意图信号;无 staged 时 ScmView 提示先暂存)
    registerCommand({
      id: 'workbench.action.git.generateCommitMessage',
      title: 'Git: AI 生成提交消息',
      category: 'Git',
      run: () => {
        showScm();
        scmGenerateIntent.update((n) => n + 1);
      },
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
      id: 'view.moveTabToNextGroup',
      title: '视图: 当前 tab 移至相邻组',
      category: '视图',
      keybinding: 'ctrl+alt+arrowright',
      run: () => {
        const mv = moveActiveTabTarget(get(activePath));
        if (!mv) return;
        if (mv.fromId === 'primary') {
          // 主组成员不登记在 groups(主组 tab 面自管):关主组 tab、副组打开
          groupOpenPath(mv.toId, mv.path);
          closeTab(mv.path);
        } else {
          groupRemovePath(mv.fromId, mv.path);
          if (mv.toId === 'primary') {
            openFile(mv.path);
          } else {
            groupOpenPath(mv.toId, mv.path);
          }
        }
      },
    });
    registerCommand({
      id: 'view.evenWidths',
      title: '视图: 均分各组宽度',
      category: '视图',
      run: () => doEvenWidths(),
    });
    registerCommand({
      id: 'workbench.toggleTheme',
      title: '视图: 明暗主题切换',
      category: '视图',
      run: () => {
        const cur = resolveMode(get(settingsState).settings['workbench.theme'] || 'auto');
        const next = cur === 'light' ? 'evorule-dark' : 'evorule-light';
        setSetting('workbench.theme', next).catch((e) => pushPanelEvent('sys', { label: '[theme]', detail: String(e?.message || e), level: 'error' }));
      },
    });
    registerCommand({
      id: 'workbench.selectTheme',
      title: '视图: 打开主题设置',
      category: '视图',
      run: () => openSettingsTab(),
    });
    registerCommand({
      id: 'view.maximize',
      title: '视图: 最大化/还原当前组',
      category: '视图',
      run: () => {
        const active = get(activeGroupId);
        toggleMaximize(active === 'primary' && !get(maximizedGroupId) ? 'primary' : active);
      },
    });
    for (const n of [1, 2, 3]) {
      registerCommand({
        id: `view.focusGroup${n}`,
        title: `视图: 聚焦编辑器组 ${n}`,
        category: '视图',
        keybinding: `ctrl+${n}`,
        run: () => {
          if (n === 1) {
            activeGroupId.set('primary');
            return;
          }
          const groups = get(secondaryGroups);
          const target = groups[n - 2]?.id;
          if (target) {
            activeGroupId.set(target);
            groupFocusSignal.update((s) => ({ id: target, n: s.n + 1 }));
          }
        },
      });
    }
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
    registerCommand({
      id: 'workbench.action.index.status',
      title: '索引: 查看代码库索引状态',
      category: '索引',
      run: () => {
        // 关闭态(index.enabled=false)服务端 503 → 错误事件明示开启路径
        indexStatus()
          .then((s) => {
            pushPanelEvent('sys', {
              label: '[index]',
              detail: `files=${s.files} symbols=${s.symbols} refs=${s.refs} ${s.building ? 'building' : s.ready ? 'ready' : 'empty'}`,
            });
          })
          .catch((e) => pushPanelEvent('gov', { label: '[index]', detail: String(e?.message || e), level: 'error' }));
        openBottomPanel('output');
      },
    });
    registerCommand({
      id: 'workbench.action.index.rebuild',
      title: '索引: 全量重建代码库符号索引',
      category: '索引',
      run: () => {
        // 同步重建(blocking 池执行):大仓数秒级,起止均落事件流水
        pushPanelEvent('gov', { label: '[index]', detail: 'rebuild started (全量重建进行中,大仓可能需数秒)' });
        openBottomPanel('output');
        indexRebuild()
          .then((s) => {
            pushPanelEvent('sys', {
              label: '[index]',
              detail: `rebuild done: files=${s.files} symbols=${s.symbols} refs=${s.refs}`,
            });
          })
          .catch((e) => pushPanelEvent('gov', { label: '[index]', detail: `rebuild failed: ${String(e?.message || e)}`, level: 'error' }));
      },
    });
    loadSettings(); // 设置快照加载(失败降级缓存只读;编辑器参数订阅在 EditorPane)
    migrateKeybindings(); // 旧键位覆盖层一次性迁移(失败保留旧键,下次启动重试)
    cleanupTracking = initContextTracking();
    refreshGitStatus(0); // git 状态启动加载(B3;后续 fs_events 防抖自愈)
    // 主题接线(B11):设置快照变化 -> applyTheme(Monaco+CSS 变量+订阅者);
    // storage 事件跨标签页同步;auto 模式由 applyTheme 内部 matchMedia 解析
    let lastThemeSetting = null;
    const unsubTheme = settingsState.subscribe((s) => {
      const t = s.settings['workbench.theme'];
      if (t && t !== lastThemeSetting) {
        lastThemeSetting = t;
        applyTheme(t, monaco);
        try {
          localStorage.setItem('evo_theme_mode', t);
        } catch {
          /* 存储不可用:仅影响 FOUC 镜像 */
        }
      }
    });
    const onThemeStorage = (e) => {
      if (e.key === 'evo_theme_mode') applyTheme(readStoredMode(), monaco);
    };
    window.addEventListener('storage', onThemeStorage);
    return () => {
      for (const id of OWNED_COMMANDS) unregisterCommand(id);
      if (cleanupTracking) cleanupTracking();
      unsubTheme();
      unsubUiFontSize();
      unsubSidebarW();
      window.removeEventListener('storage', onThemeStorage);
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
    <div style:display={$explorerVisible ? 'contents' : 'none'} bind:this={sidebarSlotEl}>
      {#if $sidebarView === 'explorer'}
        <Explorer />
      {:else if $sidebarView === 'search'}
        <SearchPanel />
      {:else if $sidebarView === 'scm'}
        <ScmView />
      {:else if $sidebarView === 'orchestration'}
        <OrchestrationPanel />
      {:else if $sidebarView === 'extensions'}
        <ExtensionsPanel />
      {:else if $sidebarView === 'debug'}
        <DebugView />
      {/if}
    </div>
    {#if $explorerVisible}
      <!-- E2:侧栏↔编辑器分隔条(宽 180–480px,持久化 evo_sidebar_w) -->
      <Divider
        orient="v"
        title="拖拽调整侧栏宽度;双击恢复缺省"
        on:resize={onSidebarResize}
        on:reset={() => setSidebarWidth(SIDEBAR_DEFAULT)}
      />
    {/if}
    <div class="center">
      {#if $connBanner}
        <!-- 断连横幅按连接状态翻转渲染(断连期间显示,重连成功即清,零残留) -->
        <div class="conn-banner" role="alert">
          <span class="conn-banner-text">{$connBanner}</span>
        </div>
      {/if}
      {#if $lspBanner}
        <div class="lsp-banner" role="alert">
          <span class="lsp-banner-text">
            语言服务器失败（{$lspBanner.serverName}）：{$lspBanner.detail}
          </span>
          <button class="lsp-banner-retry" onclick={() => restartGate($lspBanner.lang)}>重启</button>
          <button class="lsp-banner-dismiss" onclick={() => lspBanner.set(null)} aria-label="关闭提示">×</button>
        </div>
      {/if}
      <div class="editors-row" style:display={$sidebarView === 'audit' ? 'none' : ''} bind:this={editorsRowEl}>
        <div style:display={$maximizedGroupId && $maximizedGroupId !== 'primary' ? 'none' : ''} class="editor-slot">
          <EditorPane />
        </div>
        {#each $secondaryGroups as g (g.id)}
          {#if !$maximizedGroupId || $maximizedGroupId === g.id}
            <!-- E2:主组↔副组分隔条(宽 10–80%,持久化随 evo_editor_groups_v2) -->
            <Divider
              orient="v"
              title="拖拽调整组宽;双击均分各组"
              on:resize={(e) => nudgeGroupWidth(g.id, e.detail.dx, editorsRowEl)}
              on:reset={() => doEvenWidths()}
            />
            <div class="split-pane" style:width={$maximizedGroupId === g.id ? '100%' : g.width}>
              <GroupPane group={g} />
            </div>
          {/if}
        {/each}
      </div>
      <!-- console 审计视图(audit 活动栏):常挂载 display 切换,iframe 销毁会丢
           console 登录态(同 BottomPanel TerminalView 先例) -->
      <div class="console-slot" style:display={$sidebarView === 'audit' ? 'flex' : 'none'}>
        <ConsoleView />
      </div>
      <BottomPanel />
    </div>
    <div style:display={$chatVisible ? 'contents' : 'none'}>
      <!-- E2:编辑器↔对话侧栏分隔条(宽 300–560px,持久化 evo_chat_w;
           分隔条在对话栏左缘,dx>0(右拖) = 对话栏变窄) -->
      <Divider
        orient="v"
        title="拖拽调整对话栏宽度;双击恢复缺省"
        on:resize={(e) => setChatWidth($chatWidth - e.detail.dx)}
        on:reset={() => setChatWidth(CHAT_DEFAULT)}
      />
      <ChatSidebar />
    </div>
  </div>
  <!-- F1:底部状态栏(全宽 22px;铃铛下拉+右下 toast 由其内部承载) -->
  <StatusBar />
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
  .console-slot {
    flex: 1;
    min-height: 0;
    min-width: 0;
    overflow: hidden;
  }
  .editors-row :global(.editor-pane) {
    flex: 1;
    min-width: 0;
    height: 100%;
  }
  .editor-slot {
    display: flex;
    flex: 1;
    min-width: 0;
    min-height: 0;
  }
  .editor-slot :global(.editor-pane) {
    flex: 1;
    min-width: 0;
  }
  .conn-banner {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 12px;
    background: var(--warn-bg, #3d2e00);
    border-bottom: 1px solid var(--border, #30363d);
    color: var(--warn-fg, #f0c674);
    font-size: var(--fs-xs);
    flex-shrink: 0;
  }
  .conn-banner-text {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .lsp-banner {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 12px;
    background: var(--warn-bg, #3d2e00);
    border-bottom: 1px solid var(--border, #30363d);
    color: var(--warn-fg, #f0c674);
    font-size: var(--fs-xs);
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
    font-size: var(--fs-xs);
    padding: 2px 10px;
  }
  .lsp-banner-dismiss {
    background: transparent;
    border: none;
    color: inherit;
    cursor: pointer;
    font-size: var(--fs-base);
    line-height: 1;
    padding: 2px 6px;
  }
</style>
