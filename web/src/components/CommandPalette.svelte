<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 命令面板(B1):Ctrl+Shift+P 命令模式 / Ctrl+P 文件模式,同一面板两数据源。
     键盘导航(↑↓ 循环/Home/End/Enter/Esc)+ 焦点陷阱 + combobox/listbox 语义;
     命令模式含 recently used 置顶(localStorage 上限 10)与快捷键提示列;
     文件模式 v1 数据源:已开 tab(工作区文件索引与会话列表随 Ctrl+P 批次接入)。
     配色/字体/间距全部复用工作台 CSS token,不自造颜色。 -->
<script>
  import { get } from 'svelte/store';
  import {
    paletteOpen,
    paletteMode,
    closePalette,
    tabs,
    openFile,
  } from '../lib/stores.js';
  import { listCommands, executeCommand } from '../lib/commands.js';
  import { setContextKey } from '../lib/context-keys.js';
  import { fuzzyMatch, highlightSegments } from '../lib/fuzzy.js';
  import { formatKey, getEffectiveKeybinding } from '../lib/keybindings.js';

  const RECENT_COMMANDS_KEY = 'evo_recent_commands';
  const RECENT_LIMIT = 10;

  let query = '';
  let activeIndex = 0;
  let inputEl = null;
  let listEl = null;
  let composing = false; // IME 组合中:Enter 不触发选择
  let execError = '';
  let restoreFocusEl = null;
  let wasOpen = false;

  $: mode = $paletteMode;

  // 开合联动:开→复位查询/聚焦输入/置 inPalette/document 捕获层兜底拦 Tab;关→还焦点/清 inPalette
  function trapTab(e) {
    if (e.key === 'Tab') {
      e.preventDefault(); // 焦点陷阱:唯一 tab 停靠点是输入框(捕获层拦截,不依赖焦点位置)
      if (inputEl) inputEl.focus();
    }
  }

  $: {
    if ($paletteOpen && !wasOpen) {
      wasOpen = true;
      query = '';
      activeIndex = 0;
      execError = '';
      restoreFocusEl = document.activeElement;
      setContextKey('inPalette', true);
      document.addEventListener('keydown', trapTab, true);
    } else if (!$paletteOpen && wasOpen) {
      wasOpen = false;
      setContextKey('inPalette', false);
      document.removeEventListener('keydown', trapTab, true);
      if (restoreFocusEl && typeof restoreFocusEl.focus === 'function') {
        restoreFocusEl.focus();
      }
      restoreFocusEl = null;
    }
  }

  /** 焦点陷阱唯一停靠点:输入框挂载即聚焦;键盘交互用原生监听
   *  (use action 内直接 addEventListener,不依赖框架事件委派时序) */
  function attachInput(node) {
    node.focus();
    node.addEventListener('keydown', onContainerKeydown);
    return {
      destroy() {
        node.removeEventListener('keydown', onContainerKeydown);
      },
    };
  }

  // ---- 最近使用命令(localStorage 展示层留痕,同产物留痕口径) ----

  function loadRecentCommands() {
    try {
      const raw = JSON.parse(localStorage.getItem(RECENT_COMMANDS_KEY) || '[]');
      return Array.isArray(raw) ? raw.filter((id) => typeof id === 'string').slice(0, RECENT_LIMIT) : [];
    } catch {
      return [];
    }
  }

  function recordRecentCommand(id) {
    try {
      const next = [id, ...loadRecentCommands().filter((x) => x !== id)].slice(0, RECENT_LIMIT);
      localStorage.setItem(RECENT_COMMANDS_KEY, JSON.stringify(next));
    } catch {
      /* 存储不可用时留痕只存活于内存会话 */
    }
  }

  // ---- 列表模型:过滤 + 排序 + 分组 ----

  /** @returns {{groups: {label: string, items: object[]}[]}} */
  function buildCommandItems(q) {
    const commands = listCommands();
    const matchOf = (cmd) => fuzzyMatch(q, cmd.title);
    const groups = [];
    if (!q) {
      const recents = loadRecentCommands()
        .map((id) => commands.find((c) => c.id === id))
        .filter(Boolean)
        .map((cmd) => ({ kind: 'command', cmd, match: null }));
      if (recents.length > 0) groups.push({ label: '最近使用', items: recents });
      const recentIds = new Set(recents.map((r) => r.cmd.id));
      const rest = commands
        .filter((c) => !recentIds.has(c.id))
        .map((cmd) => ({ kind: 'command', cmd, match: null }));
      pushCategoryGroups(groups, rest);
      return groups;
    }
    const scored = commands
      .map((cmd) => ({ kind: 'command', cmd, match: matchOf(cmd) }))
      .filter((r) => r.match !== null)
      .sort((a, b) => b.match.score - a.match.score);
    // 有查询时不再分组,按相关度直排(命中即可见)
    return scored.length > 0 ? [{ label: '', items: scored }] : [];
  }

  function pushCategoryGroups(groups, items) {
    let currentLabel = null;
    for (const it of items) {
      const label = it.cmd.category || '其他';
      if (label !== currentLabel) {
        groups.push({ label, items: [] });
        currentLabel = label;
      }
      groups[groups.length - 1].items.push(it);
    }
  }

  function buildFileItems(q) {
    // v1 数据源:已开 tab;工作区文件索引/最近打开/会话列表随 Ctrl+P 文件模式批次接入
    const items = get(tabs).map((t) => ({ kind: 'file', file: t }));
    if (!q) return [{ label: '', items }];
    return [
      {
        label: '',
        items: items
          .map((it) => {
            const byName = fuzzyMatch(q, it.file.name);
            const byPath = fuzzyMatch(q, it.file.path);
            const match = !byName ? byPath : !byPath ? byName : byName.score >= byPath.score ? byName : byPath;
            return { ...it, match };
          })
          .filter((it) => it.match !== null)
          .sort((a, b) => b.match.score - a.match.score),
      },
    ];
  }

  $: groups = $paletteOpen ? (mode === 'files' ? buildFileItems(query.trim()) : buildCommandItems(query.trim())) : [];
  $: flatItems = groups.flatMap((g) => g.items);
  $: if (activeIndex >= flatItems.length) activeIndex = Math.max(0, flatItems.length - 1);

  // ---- 执行 ----

  function runItem(item) {
    closePalette();
    if (item.kind === 'command') {
      recordRecentCommand(item.cmd.id);
      try {
        executeCommand(item.cmd.id);
      } catch (e) {
        // 面板已关,错误就地可见(顶栏事件流水是治理事件,不混用)
        execError = String(e?.message || e);
        setTimeout(() => (execError = ''), 4000);
      }
    } else {
      openFile(item.file.path);
    }
  }

  // ---- 键盘交互 ----

  function moveActive(delta) {
    const n = flatItems.length;
    if (n === 0) return;
    activeIndex = (activeIndex + delta + n) % n;
    scrollActiveIntoView();
  }

  function scrollActiveIntoView() {
    requestAnimationFrame(() => {
      const el = listEl && listEl.querySelector(`[data-idx="${activeIndex}"]`);
      if (el) el.scrollIntoView({ block: 'nearest' });
    });
  }

  // 面板打开期间吞掉非编辑类组合键,防浏览器默认行为抢焦点
  const EDIT_THRU = ['c', 'x', 'v', 'z', 'a', 'y'];

  function onContainerKeydown(e) {
    if (e.isComposing || e.keyCode === 229) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      closePalette();
      return;
    }
    if (e.key === 'Tab') {
      e.preventDefault(); // 焦点陷阱:唯一 tab 停靠点是输入框
      return;
    }
    const isTextEditing =
      (e.ctrlKey || e.metaKey) && !e.altKey && EDIT_THRU.includes(e.key.toLowerCase());
    if ((e.ctrlKey || e.altKey || e.metaKey) && !isTextEditing) {
      e.preventDefault();
      return;
    }
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault();
        moveActive(1);
        break;
      case 'ArrowUp':
        e.preventDefault();
        moveActive(-1);
        break;
      case 'Home':
        e.preventDefault();
        activeIndex = 0;
        scrollActiveIntoView();
        break;
      case 'End':
        e.preventDefault();
        activeIndex = Math.max(0, flatItems.length - 1);
        scrollActiveIntoView();
        break;
      case 'Enter':
        if (flatItems[activeIndex]) {
          e.preventDefault();
          runItem(flatItems[activeIndex]);
        }
        break;
      default:
        break;
    }
  }

  function optionId(idx) {
    return `palette-opt-${idx}`;
  }
</script>

{#if $paletteOpen}
  <!-- svelte-ignore a11y-click-events-have-key-events a11y-no-static-element-interactions -->
  <div class="palette-overlay" on:mousedown={() => closePalette()}>
    <div
      class="palette"
      role="dialog"
      aria-label={mode === 'files' ? '快速打开' : '命令面板'}
      tabindex="-1"
      on:mousedown|stopPropagation
    >
      <input
        bind:this={inputEl}
        bind:value={query}
        use:attachInput
        class="palette-input"
        type="text"
        role="combobox"
        aria-expanded="true"
        aria-controls="palette-listbox"
        aria-activedescendant={flatItems[activeIndex] ? optionId(activeIndex) : null}
        aria-label={mode === 'files' ? '按名称筛选文件' : '输入命令名称筛选'}
        placeholder={mode === 'files' ? '输入文件名,回车打开' : '输入命令名称,回车执行'}
        on:compositionstart={() => (composing = true)}
        on:compositionend={() => (composing = false)}
        on:input={() => (activeIndex = 0)}
      />
      <ul
        class="palette-list"
        id="palette-listbox"
        role="listbox"
        aria-label={mode === 'files' ? '文件列表' : '命令列表'}
        bind:this={listEl}
      >
        {#each groups as group (group.label)}
          {#if group.label}
            <li class="group-label" role="presentation">{group.label}</li>
          {/if}
          {#each group.items as item (item.kind === 'command' ? item.cmd.id : item.file.path)}
            {@const idx = flatItems.indexOf(item)}
            <!-- svelte-ignore a11y-mouse-events-have-key-events a11y-click-events-have-key-events -->
            <li
              id={optionId(idx)}
              data-idx={idx}
              role="option"
              aria-selected={idx === activeIndex}
              aria-label={item.kind === 'command'
                ? `${item.cmd.category ? item.cmd.category + ': ' : ''}${item.cmd.title}`
                : item.file.path}
              class="row"
              class:active={idx === activeIndex}
              on:mouseenter={() => (activeIndex = idx)}
              on:mousedown|preventDefault
              on:click={() => runItem(item)}
            >
              {#if item.kind === 'command'}
                <span class="row-title">
                  {#each highlightSegments(item.cmd.title, item.match ? item.match.positions : []) as seg}
                    {#if seg.hit}<mark>{seg.text}</mark>{:else}{seg.text}{/if}
                  {/each}
                </span>
                {#if item.cmd.keybinding || getEffectiveKeybinding(item.cmd.id)}
                  <kbd class="row-key mono">{getEffectiveKeybinding(item.cmd.id) || formatKey(item.cmd.keybinding)}</kbd>
                {/if}
              {:else}
                <span class="row-title">
                  {#each highlightSegments(item.file.name, item.match ? item.match.positions : []) as seg}
                    {#if seg.hit}<mark>{seg.text}</mark>{:else}{seg.text}{/if}
                  {/each}
                </span>
                <span class="row-detail mono" title={item.file.path}>{item.file.path}</span>
              {/if}
            </li>
          {/each}
        {/each}
        {#if flatItems.length === 0}
          <li class="empty" role="presentation">
            {mode === 'files' ? '没有匹配的文件(当前仅索引已打开的文件)' : '没有匹配的命令'}
          </li>
        {/if}
      </ul>
    </div>
  </div>
{/if}
{#if execError}
  <div class="palette-toast" role="alert">{execError}</div>
{/if}

<style>
  .palette-overlay {
    position: fixed;
    inset: 0;
    background: rgba(2, 6, 16, 0.55);
    z-index: 300;
    display: flex;
    justify-content: center;
    align-items: flex-start;
    padding-top: 12vh;
  }
  .palette {
    width: 560px;
    max-width: calc(100vw - 48px);
    background: var(--bg-card);
    border: 1px solid var(--border-strong);
    border-radius: var(--r-lg);
    box-shadow: var(--sh-modal);
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }
  .palette-input {
    border: none;
    outline: none;
    background: var(--bg-input);
    color: var(--text-primary);
    font-size: var(--fs-base);
    padding: var(--sp-sm) var(--sp-md);
    border-bottom: 1px solid var(--border);
  }
  .palette-input::placeholder {
    color: var(--text-muted);
  }
  .palette-list {
    list-style: none;
    max-height: 320px;
    overflow-y: auto;
    padding: var(--sp-xs) 0;
  }
  .group-label {
    padding: var(--sp-xs) var(--sp-md);
    font-size: 11px;
    font-weight: var(--fw-sb);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--text-muted);
    user-select: none;
  }
  .row {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    padding: 6px var(--sp-md);
    font-size: var(--fs-sm);
    color: var(--text-primary);
    cursor: pointer;
    user-select: none;
  }
  .row.active {
    background: var(--bg-active);
    box-shadow: inset 2px 0 0 var(--brand);
  }
  .row-title {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .row-title mark {
    background: transparent;
    color: var(--brand-ocean);
    font-weight: var(--fw-sb);
  }
  .row-detail {
    color: var(--text-muted);
    font-size: var(--fs-xs);
    max-width: 55%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex-shrink: 0;
  }
  .row-key {
    flex-shrink: 0;
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: 1px 6px;
    background: var(--bg-hover);
  }
  .empty {
    padding: var(--sp-md);
    color: var(--text-muted);
    font-size: var(--fs-sm);
  }
  .palette-toast {
    position: fixed;
    left: 50%;
    bottom: 40px;
    transform: translateX(-50%);
    background: var(--bg-card);
    border: 1px solid var(--danger);
    border-radius: var(--r-md);
    color: var(--danger);
    font-size: var(--fs-xs);
    padding: var(--sp-xs) var(--sp-md);
    box-shadow: var(--sh-modal);
    z-index: 301;
  }
</style>
