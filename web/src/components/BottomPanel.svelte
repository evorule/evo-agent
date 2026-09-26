<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 底部多 tab 面板(治理叠加阶段):
     置于中栏底部、宽度随中栏(不横跨通栏,左右面板不动);
     sash 拖拽向上拉升/向下拉低高度,可折叠,默认收起。
     实装 tab:输出(WS 系统事件流水)/ 审计(治理事件流 + console 审计页深链);
     其余为占位禁用(tooltip 注明去向)。纯展示层状态,不落盘、不冒充审计链。 -->
<script>
  import { sysEvents, govEvents, sessionId, panelVisible } from '../lib/stores.js';

  // tabs:标准 IDE 标配 4 + evorule 专有 3(设计输入见立项文档)
  const tabs = [
    { id: 'terminal', label: '终端', disabled: true, tip: '真实终端(PTY)将拆独立子阶段接入' },
    { id: 'output', label: '输出' },
    { id: 'problems', label: '问题', disabled: true, tip: '诊断数据源在后续阶段接入' },
    { id: 'console', label: '控制台日志', disabled: true, tip: 'serve 日志流在后续阶段接入' },
    { id: 'audit', label: '审计' },
    { id: 'timetravel', label: '时光机器', disabled: true, tip: '回放面板在后续阶段接入(引擎侧回放已就绪)' },
    { id: 'memory', label: '记忆', disabled: true, tip: '记忆面板在后续阶段接入' },
  ];

  // 开合状态已提升为全局 store(命令面板 Ctrl+J 可切换;B1 命令基础设施)
  let activeTab = 'output';
  let bodyHeight = 180; // 面板体高度(sash 拖拽可调,px)
  let listEl = null;

  const MIN_H = 100;
  const MAX_H = Math.floor(window.innerHeight * 0.6);

  function clickTab(t) {
    if (t.disabled) return;
    if (activeTab === t.id && $panelVisible) {
      panelVisible.set(false); // VS Code 惯例:再点激活 tab 折叠面板
      return;
    }
    activeTab = t.id;
    panelVisible.set(true);
  }

  // ---- sash 拖拽调高(向上拉升/向下拉低) ----
  let dragState = null;

  function clampHeight(h) {
    return Math.min(MAX_H, Math.max(MIN_H, Math.round(h)));
  }

  function onSashMousedown(e) {
    dragState = { startY: e.clientY, startH: open ? bodyHeight : MIN_H };
    window.addEventListener('mousemove', onDragMove);
    window.addEventListener('mouseup', onDragEnd);
    e.preventDefault();
  }

  function onDragMove(e) {
    if (!dragState) return;
    const delta = dragState.startY - e.clientY; // 向上拖 = 变高
    bodyHeight = clampHeight(dragState.startH + delta);
    panelVisible.set(true);
  }

  function onDragEnd() {
    dragState = null;
    window.removeEventListener('mousemove', onDragMove);
    window.removeEventListener('mouseup', onDragEnd);
  }

  function onSashKeydown(e) {
    if (e.key === 'ArrowUp') {
      bodyHeight = clampHeight(($panelVisible ? bodyHeight : MIN_H) + 20);
      panelVisible.set(true);
      e.preventDefault();
    } else if (e.key === 'ArrowDown') {
      bodyHeight = clampHeight(($panelVisible ? bodyHeight : MIN_H) - 20);
      panelVisible.set(true);
      e.preventDefault();
    } else if (e.key === 'Enter' || e.key === ' ') {
      panelVisible.update((v) => !v);
      e.preventDefault();
    }
  }

  // 事件追加时滚动到底部
  $: if (listEl) {
    $sysEvents;
    $govEvents;
    requestAnimationFrame(() => listEl && listEl.scrollTo({ top: listEl.scrollHeight }));
  }

  function fmtTime(ts) {
    const d = new Date(ts);
    const pad = (n) => String(n).padStart(2, '0');
    return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  }

  // 审计页深链:console-cloud /audit?session=<数字id>(Agent 会话台同款先例)
  const CONSOLE_ORIGIN_KEY = 'evo_console_origin';
  function consoleOrigin() {
    return localStorage.getItem(CONSOLE_ORIGIN_KEY) || 'http://localhost:5174';
  }
  function auditDeepLink() {
    const sid = $sessionId;
    return sid && /^\d+$/.test(String(sid))
      ? `${consoleOrigin()}/audit?session=${sid}`
      : `${consoleOrigin()}/audit`;
  }
  // 深链点击前探活:console 未运行时不跳死链,就地明示引导
  let consoleDown = '';
  async function openAuditLink(ev) {
    ev.preventDefault();
    const origin = consoleOrigin();
    try {
      await fetch(origin + '/', { mode: 'no-cors', cache: 'no-store' });
      consoleDown = '';
      window.open(auditDeepLink(), '_blank', 'noopener,noreferrer');
    } catch {
      consoleDown = origin;
    }
  }
</script>

<div class="bottom-panel" class:open={$panelVisible}>
  <div
    class="sash"
    role="separator"
    aria-orientation="horizontal"
    aria-expanded={$panelVisible}
    tabindex="0"
    title="拖拽调整面板高度"
    onmousedown={onSashMousedown}
    onkeydown={onSashKeydown}
  ></div>
  <div class="tab-bar">
    {#each tabs as t (t.id)}
      <button
        class="tab"
        class:active={$panelVisible && activeTab === t.id}
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
    <div class="panel-body" bind:this={listEl} style={`height:${bodyHeight}px`}>
      {#if activeTab === 'output'}
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
      {:else if activeTab === 'audit'}
        <div class="audit-head">
          <span class="audit-note">当前会话治理事件流(展示层视图;权威审计链以审计页为准)</span>
          <a class="deep-link" href={auditDeepLink()} onclick={openAuditLink} target="_blank" rel="noopener noreferrer">在审计页查看 →</a>
        </div>
        {#if consoleDown}
          <div class="probe-warn">console 审计页未运行（{consoleDown}）：在 evo-agent.toml 配置 [workbench] console_dir 指向 console 仓目录后重启 serve 可自动拉起；或手动在 console 仓执行 npm run dev。</div>
        {/if}
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
  .sash {
    height: 4px;
    cursor: ns-resize;
    background: transparent;
    transition: background var(--tr-fast);
  }
  .sash:hover,
  .sash:focus-visible {
    background: var(--brand);
    outline: none;
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
    font-size: 10px;
    color: var(--text-secondary);
    padding: 2px var(--sp-xs);
    border-radius: var(--r-sm);
  }
  .collapse-btn:hover {
    color: var(--text-primary);
    background: var(--bg-hover);
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
  .probe-warn {
    color: var(--warning);
    font-size: var(--fs-xs);
    line-height: 1.5;
    padding: 2px 0;
    border-bottom: 1px solid var(--border);
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
