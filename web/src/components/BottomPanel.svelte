<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 底部多 tab 面板(治理叠加阶段):
     置于中栏底部、宽度随中栏(不横跨通栏,左右面板不动);
     sash 拖拽向上拉升/向下拉低高度,可折叠,默认收起。
     实装 tab:输出(WS 系统事件流水)/ 审计(治理事件流 + console 审计页深链)/
     问题(诊断单源聚合树,ProblemsView);其余为占位禁用(tooltip 注明去向)。
     激活 tab 为全局 store(panel.js):计数 chip/命令跨组件直达指定 tab。
     纯展示层状态,不落盘、不冒充审计链。 -->
<script>
  import { sysEvents, govEvents, sessionId, panelVisible, openFile } from '../lib/stores.js';
  import { panelTab, openBottomPanel, outputChannel } from '../lib/panel.js';
  import ProblemsView from './ProblemsView.svelte';
  import TerminalView from './TerminalView.svelte';
  import DebugConsoleTab from './DebugConsoleTab.svelte';
  import ConsoleLogTab from './ConsoleLogTab.svelte';
  import MemoryTab from './MemoryTab.svelte';
  import TimeTravelTab from './TimeTravelTab.svelte';
  import Divider from './Divider.svelte';
  import { panelHeight, setPanelHeight, PANEL_MIN, PANEL_DEFAULT } from '../lib/layout.js';
  import { taskChannelIds, taskBufferVersion, taskLines, clearTaskChannel } from '../lib/tasks.js';

  // tabs:标准 IDE 标配 4 + evorule 专有 3(设计输入见立项文档)
  const tabs = [
    { id: 'terminal', label: '终端' },
    { id: 'output', label: '输出' },
    { id: 'problems', label: '问题' },
    { id: 'debugconsole', label: '调试控制台' },
    { id: 'console', label: '控制台日志' },
    { id: 'audit', label: '审计' },
    { id: 'timetravel', label: '时光机器' },
    { id: 'memory', label: '记忆' },
  ];

  // 开合状态已提升为全局 store(命令面板 Ctrl+J 可切换);激活 tab 同为全局 store。
  // 面板体高度为全局 store(E2):钳制 120–50vh,localStorage 持久化(evo_bottom_h)。
  let listEl = null;

  function clickTab(t) {
    if (t.disabled) return;
    if ($panelTab === t.id && $panelVisible) {
      panelVisible.set(false); // VS Code 惯例:再点激活 tab 折叠面板
      return;
    }
    openBottomPanel(t.id);
  }

  // ---- sash 拖拽调高(E2 Divider 接线):向上拖(dy<0)=变高;折叠态首事件
  //      先展开至下限;每次调整即钳制+落库。 ----
  function onDividerResize(e) {
    if (!$panelVisible) {
      setPanelHeight(PANEL_MIN);
      panelVisible.set(true);
      return;
    }
    setPanelHeight($panelHeight - e.detail.dy);
  }

  function onDividerReset() {
    setPanelHeight(PANEL_DEFAULT);
    panelVisible.set(true);
  }

  // 事件追加时滚动到底部
  $: if (listEl) {
    $sysEvents;
    $govEvents;
    requestAnimationFrame(() => listEl && listEl.scrollTo({ top: listEl.scrollHeight }));
  }

  // ---- 输出面板多 channel:贴底跟随/链接化(B10) ----
  let outBodyEl = null;
  let outFollow = true;

  function onOutScroll() {
    if (!outBodyEl) return;
    const nearBottom = outBodyEl.scrollHeight - outBodyEl.scrollTop - outBodyEl.clientHeight < 24;
    outFollow = nearBottom;
  }

  function scrollFollow(force = false) {
    outFollow = true;
    if (outBodyEl) outBodyEl.scrollTop = outBodyEl.scrollHeight;
    if (force) taskBufferVersion.update((n) => n + 1); // 触发重渲染滚底
  }

  /** file:line(:col)? 链接识别(行首匹配,ANSI 已由无 TTY 天然关色兜底) */
  function linkifyMatch(text) {
    const m = /^([^\s:][^:]*?:\d+(?::\d+)?)/.exec(text);
    if (!m) return null;
    return m;
  }

  function jumpFromLink(m) {
    const parts = m[0].split(':');
    const col = parts.length >= 3 ? parseInt(parts[parts.length - 1], 10) : 1;
    const line = parseInt(parts[parts.length - 2], 10);
    const path = parts.slice(0, parts.length - 2).join(':');
    if (path && line > 0) openFile(path, { line, col: Math.max(0, col - 1), endCol: col });
  }

  function fmtTime(ts) {
    const d = new Date(ts);
    const pad = (n) => String(n).padStart(2, '0');
    return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  }

  // 审计页深链:console 同源托管 /console/audit?session=<数字id>
  // (serve 静态托管形态同源直达,无需探活;dev 形态仅开发调试直连 5174)
  function auditDeepLink() {
    const sid = $sessionId;
    return sid && /^\d+$/.test(String(sid))
      ? `/console/audit?session=${sid}`
      : '/console/audit';
  }
</script>

<div class="bottom-panel" class:open={$panelVisible} data-zone="panel">
  <!-- E2:编辑器↔底部面板分隔条(高 120–50vh,持久化 evo_bottom_h) -->
  <Divider
    orient="h"
    title="拖拽调整面板高度;双击恢复缺省;Enter 折叠/展开"
    toggleOnEnter
    on:resize={onDividerResize}
    on:reset={onDividerReset}
    on:toggle={() => panelVisible.update((v) => !v)}
  />
  <div class="tab-bar">
    {#each tabs as t (t.id)}
      <button
        class="tab"
        class:active={$panelVisible && $panelTab === t.id}
        disabled={t.disabled}
        title={t.tip || t.label}
        onclick={() => clickTab(t)}
      >
        {t.label}
      </button>
    {/each}
    <div class="tab-actions">
      <button class="collapse-btn" onclick={() => panelVisible.update((v) => !v)} aria-expanded={$panelVisible} title={$panelVisible ? '折叠面板' : '展开面板'}>
        {$panelVisible ? '▾' : '▴'}
      </button>
    </div>
  </div>

  {#if $panelVisible}
    <div class="panel-body" bind:this={listEl} style={`height:${$panelHeight}px`}>
      <!-- 终端视图常挂载(display 切换):切 tab 销毁会杀掉全部 PTY 会话,
           其余 tab 才走 if/else 挂载 -->
      <div class="panel-slot" style:display={$panelTab === 'terminal' ? '' : 'none'}>
        <TerminalView />
      </div>
      {#if $panelTab === 'output'}
        <div class="out-head">
          <select class="out-channel" bind:value={$outputChannel} aria-label="输出通道">
            <option value="syslog">系统流水</option>
            {#each $taskChannelIds as ch (ch)}
              <option value={ch}>任务: {ch.slice('task:'.length)}</option>
            {/each}
          </select>
          {#if $outputChannel !== 'syslog'}
            <button class="out-act" onclick={() => clearTaskChannel($outputChannel.slice('task:'.length))}>清空</button>
          {/if}
          {#if !outFollow}
            <button class="out-act follow" onclick={() => scrollFollow(true)}>↓ 跟随</button>
          {/if}
        </div>
        <div class="out-body" bind:this={outBodyEl}>
          {#if $outputChannel === 'syslog'}
            {#if $sysEvents.length === 0}
              <div class="empty">暂无输出。与 agent 对话后,系统事件(SessionCreated / Done / Info 等)在此流水呈现。</div>
            {:else}
              {#each $sysEvents as ev (ev.id)}
                <div class="ev-row">
                  <span class="ev-time mono">{fmtTime(ev.time)}</span>
                  <span class="ev-label mono">{ev.label}</span>
                  <span class="ev-detail">{ev.detail}</span>
                </div>
              {/each}
            {/if}
          {:else}
            {#if $taskLines.length === 0}
              <div class="empty">暂无输出。运行任务后 stdout/stderr 行流在此呈现(诊断经 matcher 进问题面板)。</div>
            {:else}
              {#each $taskLines as row, i (i)}
                <div class="out-row mono">
                  <span class="ev-time mono">{fmtTime(row.time)}</span>
                  {#if row.stream === 'stderr'}<span class="out-stream err">ERR</span>{:else if row.stream === 'sys'}<span class="out-stream">SYS</span>{:else}<span class="out-stream">OUT</span>{/if}
                  <span class="out-text">
                    {#if row.link}
                      <a
                        class="out-link mono"
                        href="#"
                        onclick={(e) => { e.preventDefault(); openFile(row.link.path, { line: row.link.line, col: row.link.col, endCol: row.link.endCol }); }}
                      >{row.link.linkText}</a>{row.text.slice(row.link.linkText.length)}
                    {:else}
                      {row.text}
                    {/if}
                  </span>
                </div>
              {/each}
            {/if}
          {/if}
        </div>
      {:else if $panelTab === 'problems'}
        <ProblemsView />
      {:else if $panelTab === 'debugconsole'}
        <DebugConsoleTab />
      {:else if $panelTab === 'console'}
        <ConsoleLogTab />
      {:else if $panelTab === 'timetravel'}
        <TimeTravelTab />
      {:else if $panelTab === 'memory'}
        <MemoryTab />
      {:else if $panelTab === 'audit'}
        <div class="audit-head">
          <span class="audit-note">当前会话治理事件流(展示层视图;权威审计链以审计页为准)</span>
          <a class="deep-link" href={auditDeepLink()} target="_blank" rel="noopener noreferrer">在审计页查看 →</a>
        </div>
        {#if $govEvents.length === 0}
          <div class="empty">暂无治理事件。工具调用、审批请求/结果与错误在此呈现。</div>
        {:else}
          {#each $govEvents as ev (ev.id)}
            <div class="ev-row" class:err={ev.level === 'error'} class:warn={ev.level === 'warn'}>
              <span class="ev-time mono">{fmtTime(ev.time)}</span>
              <span class="ev-label mono">{ev.label}</span>
              <span class="ev-detail">{ev.detail}</span>
            </div>
          {/each}
        {/if}
      {/if}
    </div>
  {/if}
</div>

<style>
  .bottom-panel {
    flex-shrink: 0;
    border-top: 1px solid var(--border);
    background: var(--bg-card);
    display: flex;
    flex-direction: column;
  }
  .tab-bar {
    display: flex;
    align-items: center;
    gap: 2px;
    height: 28px;
    padding: 0 var(--sp-sm);
    border-top: 1px solid transparent;
  }
  .tab {
    height: 100%;
    padding: 0 var(--sp-sm);
    font-size: var(--fs-xs);
    font-weight: var(--fw-med);
    color: var(--text-secondary);
    border: none;
    background: transparent;
    border-radius: var(--r-sm) var(--r-sm) 0 0;
  }
  .tab:hover:not(:disabled) {
    color: var(--text-primary);
    background: var(--bg-hover);
  }
  .tab.active {
    color: var(--brand);
    box-shadow: inset 0 -2px 0 var(--brand);
  }
  .tab:disabled {
    color: var(--text-muted);
    opacity: 0.55;
    cursor: not-allowed;
  }
  .tab-actions {
    margin-left: auto;
    display: flex;
    align-items: center;
  }
  .collapse-btn {
    font-size: var(--fs-2xs);
    color: var(--text-secondary);
    padding: 2px var(--sp-xs);
    border-radius: var(--r-sm);
  }
  .collapse-btn:hover {
    color: var(--text-primary);
    background: var(--bg-hover);
  }
  .out-head {
    align-items: center;
    border-bottom: 1px solid var(--border);
    display: flex;
    gap: 8px;
    padding: 4px 8px;
    flex-shrink: 0;
  }
  .out-channel {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--fg);
    font-size: var(--fs-xs);
    max-width: 260px;
    padding: 2px 6px;
  }
  .out-act {
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--fg-dim);
    cursor: pointer;
    font-size: var(--fs-xs);
    padding: 2px 8px;
  }
  .out-act.follow {
    border-color: var(--accent);
    color: var(--fg);
  }
  .out-body {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 4px 8px;
  }
  .out-row {
    align-items: baseline;
    color: var(--fg);
    display: flex;
    font-size: var(--fs-xs);
    gap: 8px;
    line-height: 1.55;
    white-space: pre-wrap;
    word-break: break-all;
  }
  .out-row .ev-time {
    flex-shrink: 0;
  }
  .out-stream {
    color: var(--fg-dim);
    flex-shrink: 0;
    font-size: var(--fs-2xs);
    width: 26px;
  }
  .out-stream.err {
    color: var(--danger);
  }
  .out-link {
    color: var(--info);
    cursor: pointer;
    text-decoration: underline;
  }
  .out-truncated {
    color: var(--fg-dim);
    font-size: var(--fs-2xs);
    padding: 2px 0;
  }
  .panel-slot {
    height: 100%;
    min-height: 0;
  }
  .panel-body {
    overflow-y: auto;
    padding: var(--sp-sm) var(--sp-md);
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    border-top: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .empty {
    color: var(--text-muted);
    padding: var(--sp-sm) 0;
  }
  .audit-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--sp-sm);
    padding-bottom: var(--sp-xs);
    margin-bottom: var(--sp-xs);
    border-bottom: 1px solid var(--border);
  }
  .audit-note {
    color: var(--text-muted);
  }
  .deep-link {
    font-size: var(--fs-xs);
    color: var(--brand-ocean);
    white-space: nowrap;
  }
  .deep-link:hover {
    text-decoration: underline;
  }
  .ev-row {
    display: flex;
    align-items: baseline;
    gap: var(--sp-sm);
    padding: 1px 0;
    line-height: 1.5;
  }
  .ev-row.err .ev-label,
  .ev-row.err .ev-detail {
    color: var(--danger);
  }
  .ev-row.warn .ev-label,
  .ev-row.warn .ev-detail {
    color: var(--warning);
  }
  .ev-time {
    color: var(--text-muted);
    flex-shrink: 0;
  }
  .ev-label {
    color: var(--brand-ocean);
    flex-shrink: 0;
    min-width: 110px;
  }
  .ev-detail {
    word-break: break-all;
    white-space: pre-wrap;
  }
</style>
