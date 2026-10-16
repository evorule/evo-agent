<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 顶栏菜单栏(形制补全 F1):标准 IDE 菜单形制,B1 命令注册制的聚合呈现面——
     菜单项全部映射既有 workbench.*/editor.* 命令,不引入新执行语义 -->
<script>
  import { onMount, onDestroy } from 'svelte';
  import { executeCommand } from '../lib/commands.js';

  /**
   * 菜单结构(全部为既有命令 id;编辑/选择组依赖 EditorPane 挂载期注册的
   * editor.* 编辑器桥命令)。sep = 分组分隔线。
   */
  const MENUS = [
    {
      title: '文件(F)',
      items: [
        { label: '快速打开文件', command: 'workbench.action.file.quickOpen' },
        { label: '新建会话', command: 'workbench.action.session.new' },
        'sep',
        { label: '保存', command: 'workbench.action.file.save' },
        { label: '关闭标签', command: 'workbench.action.file.closeTab' },
        'sep',
        { label: '设置', command: 'workbench.action.openSettings' },
        { label: '打开 settings.json', command: 'workbench.action.openSettingsJson' },
      ],
    },
    {
      title: '编辑(E)',
      items: [
        { label: '撤销', command: 'editor.undo' },
        { label: '重做', command: 'editor.redo' },
        'sep',
        { label: '查找', command: 'editor.find' },
        { label: '替换', command: 'editor.replace' },
      ],
    },
    {
      title: '选择(S)',
      items: [{ label: '全选', command: 'editor.selectAll' }],
    },
    {
      title: '查看(V)',
      items: [
        { label: '命令面板', command: 'workbench.action.showCommands' },
        'sep',
        { label: '资源管理器', command: 'workbench.action.view.toggleExplorer' },
        { label: '搜索', command: 'workbench.action.search.show' },
        { label: '聊天', command: 'workbench.action.view.toggleChat' },
        { label: '面板', command: 'workbench.action.view.togglePanel' },
        'sep',
        { label: '分屏', command: 'view.splitEditor' },
        { label: '均分分屏', command: 'view.evenWidths' },
        { label: '最大化编辑器', command: 'view.maximize' },
        'sep',
        { label: '切换主题', command: 'workbench.toggleTheme' },
      ],
    },
    {
      title: '转到(G)',
      items: [
        { label: '转到文件', command: 'workbench.action.file.quickOpen' },
        { label: '下一问题', command: 'workbench.action.problems.next' },
        { label: '上一问题', command: 'workbench.action.problems.prev' },
        { label: '文件内下一问题', command: 'workbench.action.problems.nextInFile' },
      ],
    },
    {
      title: '运行(R)',
      items: [
        { label: '运行任务', command: 'workbench.action.tasks.run' },
        { label: '重新运行上次任务', command: 'workbench.action.tasks.rerunLast' },
        { label: '运行构建任务', command: 'workbench.action.tasks.build' },
        { label: '终止任务', command: 'workbench.action.tasks.terminate' },
      ],
    },
    {
      title: '终端(T)',
      items: [
        { label: '新建终端', command: 'workbench.action.terminal.new' },
        { label: '聚焦终端', command: 'workbench.action.terminal.focus' },
        { label: '清空终端', command: 'workbench.action.terminal.clear' },
        { label: '关闭终端会话', command: 'workbench.action.terminal.kill' },
      ],
    },
    {
      title: '帮助(H)',
      items: [
        { label: '重启语言服务器', command: 'workbench.action.lsp.restart' },
        { label: '刷新治理徽标', command: 'workbench.action.gov.refreshBadges' },
      ],
    },
  ];

  let openIndex = $state(-1);
  let rootEl = $state(null);

  function toggle(i) {
    openIndex = openIndex === i ? -1 : i;
  }
  function hover(i) {
    if (openIndex >= 0 && openIndex !== i) openIndex = i; // 打开态下悬停切换
  }
  function run(item) {
    if (item === 'sep') return;
    executeCommand(item.command);
    openIndex = -1;
  }
  function onDocClick(e) {
    if (rootEl && !rootEl.contains(e.target)) openIndex = -1;
  }
  function onDocKey(e) {
    if (e.key === 'Escape') openIndex = -1;
  }
  onMount(() => {
    document.addEventListener('click', onDocClick);
    document.addEventListener('keydown', onDocKey);
  });
  onDestroy(() => {
    document.removeEventListener('click', onDocClick);
    document.removeEventListener('keydown', onDocKey);
  });
</script>

<nav class="menubar" bind:this={rootEl} aria-label="主菜单">
  {#each MENUS as menu, i}
    <div class="menu-root">
      <button
        class="menu-title"
        class:open={openIndex === i}
        aria-haspopup="menu"
        aria-expanded={openIndex === i}
        onclick={() => toggle(i)}
        onmouseenter={() => hover(i)}
      >
        {menu.title}
      </button>
      {#if openIndex === i}
        <div class="dropdown" role="menu">
          {#each menu.items as item}
            {#if item === 'sep'}
              <div class="sep" role="separator"></div>
            {:else}
              <button class="menu-item" role="menuitem" onclick={() => run(item)}>
                {item.label}
              </button>
            {/if}
          {/each}
        </div>
      {/if}
    </div>
  {/each}
</nav>

<style>
  .menubar {
    display: flex;
    align-items: stretch;
    height: 100%;
    margin-left: 4px;
    user-select: none;
  }
  .menu-root {
    position: relative;
    display: flex;
    align-items: stretch;
  }
  .menu-title {
    border: none;
    background: transparent;
    color: var(--text-secondary);
    font-size: 12px;
    padding: 0 9px;
    cursor: pointer;
    white-space: nowrap;
  }
  .menu-title:hover,
  .menu-title.open {
    color: var(--text-primary);
    background: var(--bg-active);
  }
  .dropdown {
    position: absolute;
    top: 100%;
    left: 0;
    min-width: 200px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--r-md, 6px);
    box-shadow: var(--shadow-lg, 0 8px 24px rgba(0, 0, 0, 0.4));
    padding: 4px 0;
    z-index: 300;
  }
  .menu-item {
    display: block;
    width: 100%;
    text-align: left;
    border: none;
    background: transparent;
    color: var(--text-primary);
    font-size: 12px;
    padding: 6px 16px;
    cursor: pointer;
    white-space: nowrap;
  }
  .menu-item:hover {
    background: var(--bg-active);
  }
  .sep {
    height: 1px;
    background: var(--border);
    margin: 4px 8px;
  }
</style>
