// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
<!-- 调试视图(活动栏「运行和调试」):launch 配置选择+调试工具条+断点/变量/调用栈。
     数据面全部来自 lib/dap/index.js stores;面板为纯消费端(视图多元铁律)。 -->
<script>
  import { onMount } from 'svelte';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import {
    initDebug,
    startDebug,
    debugStop,
    debugContinue,
    debugNext,
    debugStepIn,
    debugStepOut,
    debugEvaluate,
    toggleBreakpoint,
    needsDebugConfirm,
    markDebugConfirmed,
    debugState,
    debugAdapters,
    debugLaunch,
    debugBreakpoints,
    debugStopped,
    debugThreads,
    debugFrames,
    debugVariables,
    debugConsole,
    DBG_STATES,
  } from '../lib/dap/index.js';
  import { openFile, sidebarView } from '../lib/stores.js';

  let adapter = '';
  let configName = '';
  let ready = false;
  let startError = '';

  // 工具条可用性(会话活动态)
  $: sessionActive = [
    DBG_STATES.RUNNING,
    DBG_STATES.STOPPED,
    DBG_STATES.LAUNCHING,
    DBG_STATES.CONNECTING,
    DBG_STATES.INITIALIZING,
  ].includes($debugState.state);
  $: hitStop = $debugState.state === DBG_STATES.STOPPED;

  // 选中适配器的 launch 配置(类型匹配;type 缺省=唯一适配器时不过滤)
  $: configsForAdapter = $debugLaunch.configs.filter((c) => !adapter || c.type === adapter);
  // 配置名未选(初值/换适配器)时自动选第一项,避免下拉空白
  $: if ((!configName || !configsForAdapter.some((c) => c.name === configName)) && configsForAdapter.length) {
    configName = configsForAdapter[0].name;
  }
  $: selectedConfig = configsForAdapter.find((c) => c.name === configName) || configsForAdapter[0] || null;

  // 断点列表展平(path → lines)
  $: bpEntries = Object.entries($debugBreakpoints).flatMap(([path, lines]) =>
    (lines || []).map((line) => ({ path, line })),
  );

  onMount(async () => {
    await initDebug();
    if ($debugAdapters.length && !adapter) adapter = $debugAdapters[0].name;
    ready = true;
  });

  /** 启动调试(首跑确认:适配器+程序指纹一次性信任,B10 先例) */
  function onStart() {
    startError = '';
    if (!adapter || !selectedConfig) {
      startError = !adapter ? '无白名单适配器——请在设置 dap.adapters 配置' : '无匹配 launch 配置';
      return;
    }
    if (needsDebugConfirm(selectedConfig)) {
      pendingConfirm = selectedConfig;
      return;
    }
    launch();
  }

  let pendingConfirm = null;

  function launch() {
    startError = '';
    startDebug(adapter, selectedConfig).catch((e) => {
      startError = String(e?.message || e);
    });
  }

  function onConfirm() {
    if (pendingConfirm) {
      markDebugConfirmed(pendingConfirm);
      launch();
    }
    pendingConfirm = null;
  }

  /** 栈帧/断点行跳转 */
  function jump(path, line) {
    if (path) openFile(path, { line });
  }

  const stateLabel = {
    idle: '未启动',
    connecting: '连接中…',
    initializing: '初始化…',
    launching: '启动中…',
    running: '运行中',
    stopped: '已停住',
    terminated: '已终止',
    failed: '失败',
    ended: '已结束',
  };
</script>

<div class="dbg-panel">
  <div class="dbg-head">
    <span class="dbg-title">运行和调试</span>
    <span class="dbg-state st-{$debugState.state}">{stateLabel[$debugState.state] || $debugState.state}</span>
  </div>

  {#if ready && !$debugAdapters.length}
    <div class="dbg-empty">
      dap.adapters 白名单为空(fail-closed)。<br />在设置中配置适配器后方可调试。
      <div class="dbg-empty-actions">
        <button class="dbg-btn" onclick={() => sidebarView.set('settings')}>打开设置</button>
      </div>
    </div>
  {:else}
    <div class="dbg-launch">
      <select class="dbg-select" bind:value={adapter} disabled={sessionActive} title="适配器">
        {#each $debugAdapters as a (a.name)}
          <option value={a.name}>{a.name}{a.langs?.length ? ` (${a.langs.join(',')})` : ''}</option>
        {/each}
      </select>
      <select class="dbg-select" bind:value={configName} disabled={sessionActive} title="launch 配置">
        {#each configsForAdapter as c (c.name)}
          <option value={c.name}>{c.name}</option>
        {/each}
        {#if !configsForAdapter.length}
          <option value="">(无匹配 launch 配置)</option>
        {/if}
      </select>
      {#if sessionActive}
        <button class="dbg-btn danger" onclick={() => debugStop().catch(() => {})}>停止</button>
      {:else}
        <button class="dbg-btn primary" onclick={onStart} disabled={!selectedConfig}>启动调试</button>
      {/if}
    </div>
    {#if startError}
      <div class="dbg-err">{startError}</div>
    {/if}
  {/if}

  {#if sessionActive}
    <div class="dbg-toolbar">
      <button class="dbg-btn" title="继续 (F5)" disabled={!hitStop} onclick={() => debugContinue().catch(() => {})}>▶</button>
      <button class="dbg-btn" title="单步跳过" disabled={!hitStop} onclick={() => debugNext().catch(() => {})}>⤵</button>
      <button class="dbg-btn" title="单步进入" disabled={!hitStop} onclick={() => debugStepIn().catch(() => {})}>↓</button>
      <button class="dbg-btn" title="单步跳出" disabled={!hitStop} onclick={() => debugStepOut().catch(() => {})}>↑</button>
    </div>
  {/if}

  {#if $debugStopped}
    <div class="dbg-hit">已停住:<button class="dbg-link mono" onclick={() => jump($debugStopped.path, $debugStopped.line)}>{$debugStopped.path}:{$debugStopped.line}</button></div>
  {/if}

  <!-- 调用栈 -->
  {#if $debugFrames.length}
    <div class="dbg-section">
      <div class="dbg-section-title">调用栈</div>
      {#each $debugFrames as fr, i (fr.id)}
        <button class="dbg-frame {i === 0 ? 'top' : ''}" onclick={() => jump(fr.source?.path ? fr.source.path : '', fr.line)}>
          <span class="dbg-frame-name">{fr.name || '(匿名)'}</span>
          {#if fr.source?.path}<span class="dbg-frame-loc mono">{fr.source.name || ''}:{fr.line}</span>{/if}
        </button>
      {/each}
    </div>
  {/if}

  <!-- 变量( stopped 期首层作用域) -->
  {#if $debugVariables.length}
    <div class="dbg-section">
      <div class="dbg-section-title">变量</div>
      {#each $debugVariables as group (group.scope)}
        <div class="dbg-scope">
          <div class="dbg-scope-name">{group.scope}</div>
          {#each group.variables.slice(0, 50) as v (v.name)}
            <div class="dbg-var">
              <span class="dbg-var-name">{v.name}</span>
              <span class="dbg-var-value mono">{v.value}</span>
            </div>
          {/each}
        </div>
      {/each}
    </div>
  {/if}

  <!-- 断点 -->
  <div class="dbg-section">
    <div class="dbg-section-title">断点{bpEntries.length ? ` (${bpEntries.length})` : ''}</div>
    {#if bpEntries.length}
      {#each bpEntries as bp (bp.path + ':' + bp.line)}
        <div class="dbg-bp">
          <span class="dbg-bp-dot" title="断点"></span>
          <button class="dbg-link mono" onclick={() => jump(bp.path, bp.line)}>{bp.path}:{bp.line}</button>
          <button class="dbg-bp-del" title="移除断点" onclick={() => toggleBreakpoint(bp.path, bp.line)}>×</button>
        </div>
      {/each}
    {:else}
      <div class="dbg-empty small">编辑器 gutter 行号区点击可加断点。</div>
    {/if}
  </div>

  {#if $debugConsole.length}
    <div class="dbg-section">
      <div class="dbg-section-title">调试输出(控制台在底部面板)</div>
      {#each $debugConsole.slice(-6) as l (l.time + l.text)}
        <div class="dbg-out mono {l.stream}">{l.text}</div>
      {/each}
    </div>
  {/if}
</div>

{#if pendingConfirm}
  <ConfirmDialog
    open
    title="首次运行确认"
    message={`即将启动调试会话:\n${adapter} → ${pendingConfirm.name || ''}\n${pendingConfirm.program || ''}\n\n确认后本次安装不再逐次询问。`}
    confirmText="启动"
    danger={false}
    on:confirm={onConfirm}
    on:cancel={() => (pendingConfirm = null)}
  />
{/if}

<style>
  .dbg-panel {
    padding: var(--sp-md);
    overflow-y: auto;
    height: 100%;
  }
  .dbg-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: var(--sp-md);
  }
  .dbg-title {
    font-weight: 600;
    font-size: var(--fs-base);
    color: var(--text-primary);
  }
  .dbg-state {
    font-size: var(--fs-xs);
    padding: 2px 8px;
    border-radius: var(--r-full);
    background: var(--bg-hover);
    color: var(--text-secondary);
  }
  .dbg-state.st-running { color: var(--success); }
  .dbg-state.st-stopped { color: var(--warning); }
  .dbg-state.st-failed { color: var(--danger); }
  .dbg-launch {
    display: flex;
    flex-direction: column;
    gap: var(--sp-xs);
    margin-bottom: var(--sp-sm);
  }
  .dbg-select {
    background: var(--bg-input);
    color: var(--text-primary);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: 6px 8px;
    font-size: var(--fs-sm);
  }
  .dbg-btn {
    background: var(--bg-hover);
    color: var(--text-primary);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: 5px 12px;
    font-size: var(--fs-sm);
    cursor: pointer;
  }
  .dbg-btn:hover:not(:disabled) { background: var(--bg-active); }
  .dbg-btn:disabled { opacity: 0.4; cursor: not-allowed; }
  .dbg-btn.primary { background: var(--brand); border-color: var(--brand); color: #fff; }
  .dbg-btn.primary:hover:not(:disabled) { background: var(--brand-hover); }
  .dbg-btn.danger { background: var(--danger-bg); border-color: var(--danger); color: var(--danger); }
  .dbg-toolbar {
    display: flex;
    gap: var(--sp-xs);
    margin-bottom: var(--sp-sm);
  }
  .dbg-err, .dbg-err.mono {
    color: var(--danger);
    font-size: var(--fs-xs);
    margin-bottom: var(--sp-sm);
    word-break: break-all;
  }
  .dbg-hit {
    font-size: var(--fs-xs);
    color: var(--warning);
    margin-bottom: var(--sp-sm);
  }
  .dbg-section { margin-bottom: var(--sp-md); }
  .dbg-section-title {
    font-size: var(--fs-xs);
    font-weight: 600;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    margin-bottom: var(--sp-xs);
  }
  .dbg-frame {
    display: flex;
    justify-content: space-between;
    gap: var(--sp-xs);
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    color: var(--text-primary);
    font-size: var(--fs-xs);
    padding: 3px 4px;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .dbg-frame:hover, .dbg-frame.top { background: var(--bg-hover); }
  .dbg-frame-loc { color: var(--text-muted); }
  .dbg-scope { margin-bottom: var(--sp-xs); }
  .dbg-scope-name {
    font-size: var(--fs-xs);
    font-weight: 600;
    color: var(--text-secondary);
  }
  .dbg-var {
    display: flex;
    justify-content: space-between;
    gap: var(--sp-sm);
    font-size: var(--fs-xs);
    padding: 1px 4px 1px 10px;
  }
  .dbg-var-name { color: var(--brand-light); }
  .dbg-var-value {
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dbg-bp {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    font-size: var(--fs-xs);
    padding: 2px 4px;
  }
  .dbg-bp-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--danger);
    flex: none;
  }
  .dbg-bp-del {
    margin-left: auto;
    background: none;
    border: none;
    color: var(--text-muted);
    cursor: pointer;
    padding: 0 4px;
  }
  .dbg-bp-del:hover { color: var(--danger); }
  .dbg-link {
    background: none;
    border: none;
    color: var(--brand-light);
    cursor: pointer;
    font-size: var(--fs-xs);
    padding: 0;
    text-align: left;
    word-break: break-all;
  }
  .dbg-link:hover { text-decoration: underline; }
  .dbg-out {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    padding: 1px 0;
    word-break: break-all;
  }
  .dbg-out.stderr { color: var(--danger); }
  .dbg-out.repl { color: var(--brand-light); }
  .dbg-out.system { color: var(--text-muted); }
  .dbg-empty {
    font-size: var(--fs-sm);
    color: var(--text-secondary);
    padding: var(--sp-md) 0;
    line-height: 1.7;
  }
  .dbg-empty.small { padding: var(--sp-xs) 0; font-size: var(--fs-xs); }
  .dbg-empty-actions { margin-top: var(--sp-sm); }
  .mono { font-family: var(--font-mono); }
</style>
