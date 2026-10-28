<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- B25 编排可视化面板(ActivityBar 独立视图):当前会话委托谱系树。
     数据双通道:WS delegate ToolCall/ToolResult 实时增量(orchestration.js
     store)+ REST journal 权威树合并;纯手写 SVG,零第三方图库。 -->
<script>
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import { sessionId } from '../lib/stores.js';
  import {
    orchestration,
    refreshOrchestration,
    loadOrchestrationAgents,
    layoutTree,
    edgePath,
  } from '../lib/orchestration.js';
  import { spawnBackgroundTask } from '../lib/api.js';
  import { push as pushNotification } from '../lib/notifications.js';

  const STATUS_TEXT = { running: '执行中', done: '完成', failed: '失败', unknown: '未知' };

  // ===== 后台任务直发(44 号方案乙:fork 父会话派生独立子会话) =====
  let taskGoal = $state('');
  let taskBusy = $state(false);
  let taskErr = $state('');

  async function dispatchTask() {
    const goal = taskGoal.trim();
    const sid = get(sessionId);
    if (!goal || !sid || taskBusy) return;
    taskBusy = true;
    taskErr = '';
    try {
      const agentType = get(orchestration).agents?.find((a) => a.can_delegate)?.agent_type;
      const r = await spawnBackgroundTask(agentType || 'general', goal, sid);
      pushNotification('info', '后台任务已派发', `${r.child_session_id} · ${goal}`);
      taskGoal = '';
      await refreshOrchestration(sid);
    } catch (e) {
      taskErr = String(e?.message || e);
    } finally {
      taskBusy = false;
    }
  }

  // ===== 后台任务完成通知:周期刷新 + spawn 节点收色跃迁检测 =====
  let prevSpawnStatus = new Map();
  $effect(() => {
    if (!$sessionId) return;
    const timer = setInterval(async () => {
      const before = new Map(
        get(orchestration)
          .nodes.filter((n) => n.kind === 'spawn')
          .map((n) => [n.session_id, n.status])
      );
      await refreshOrchestration(get(sessionId));
      for (const n of get(orchestration).nodes) {
        if (n.kind !== 'spawn') continue;
        const was = before.get(n.session_id) || 'unknown';
        if ((was === 'running' || was === 'unknown') && (n.status === 'done' || n.status === 'failed')) {
          pushNotification(
            'info',
            `后台任务${n.status === 'done' ? '完成' : '失败'}`,
            `派生会话 ${n.session_id} 已收尾，结果见会话列表`
          );
        }
      }
    }, 15000);
    return () => clearInterval(timer);
  });


  function nodeLabel(n) {
    if (n.parent === null || n.parent === undefined) return '本会话';
    return n.agent_type || (n.kind === 'spawn' ? '派生' : '子代理');
  }
  function nodeSub(n) {
    if (n.parent === null || n.parent === undefined) return n.session_id;
    if (n.temp) return '实时通道';
    // 委托窗口 token 归账可见(delegate 节点且有埋点;B25 观测面)
    const tok =
      n.tokens_used == null
        ? ''
        : n.tokens_used >= 1000
          ? ` · ${(n.tokens_used / 1000).toFixed(1)}k tok`
          : ` · ${n.tokens_used} tok`;
    return `${n.session_id}${tok}`;
  }

  // 挂载/换线自动加载一次(按会话 id 防重入环;刷新按钮走显式通道)
  let autoLoadedFor = $state(null);
  $effect(() => {
    if ($sessionId && autoLoadedFor !== $sessionId) {
      autoLoadedFor = $sessionId;
      refreshOrchestration($sessionId);
    }
  });

  onMount(() => {
    loadOrchestrationAgents();
  });

  const layout = $derived(layoutTree($orchestration.nodes));
  const posById = $derived(new Map(layout.placed.map((p) => [p.node.session_id, p])));
  const edges = $derived(
    $orchestration.edges
      .map((e) => {
        const p = posById.get(e.parent);
        const c = posById.get(e.child);
        return p && c ? { kind: e.kind, d: edgePath(p, c, layout.nodeW, layout.nodeH) } : null;
      })
      .filter(Boolean),
  );
  const hasChildren = $derived($orchestration.nodes.length > 1);
</script>

<div class="orch">
  <div class="orch-header">
    <span class="orch-title">编排</span>
    <button
      class="orch-refresh"
      onclick={() => $sessionId && refreshOrchestration($sessionId)}
      disabled={!$sessionId}
      title="重新拉取当前会话的委托谱系(journal 权威)"
    >刷新</button>
  </div>

  {#if !$sessionId}
    <div class="orch-empty">未连接会话——新建会话后此处展示委托谱系。</div>
  {:else if $orchestration.loading && !hasChildren}
    <div class="orch-empty">正在加载谱系…</div>
  {:else if !hasChildren}
    <div class="orch-empty">
      本会话暂无委托
      {#if $orchestration.error}
        <div class="orch-err">谱系拉取失败:{$orchestration.error}(实时通道不受影响)</div>
      {/if}
    </div>
  {:else}
    <div class="orch-canvas">
      <svg
        width={layout.width}
        height={layout.height}
        viewBox="0 0 {layout.width} {layout.height}"
        role="img"
        aria-label="委托谱系树"
      >
        {#each edges as e (e.d)}
          <path class="oedge {e.kind}" d={e.d} />
        {/each}
        {#each layout.placed as p (p.node.session_id)}
          <g class="onode {p.node.status}" transform="translate({p.x},{p.y})">
            <rect class="obox" width={layout.nodeW} height={layout.nodeH} rx="8" />
            <text class="otype" x="10" y="19">{nodeLabel(p.node)}</text>
            <text class="osid" x="10" y="34">{nodeSub(p.node)}</text>
            <circle class="odot" cx={layout.nodeW - 12} cy="13" r="4" />
            <title>{p.node.session_id} · {STATUS_TEXT[p.node.status] || p.node.status}</title>
          </g>
        {/each}
      </svg>
    </div>
    <div class="orch-legend">
      <span class="lg lg-running">执行中</span>
      <span class="lg lg-done">完成</span>
      <span class="lg lg-failed">失败</span>
      <span class="lg lg-unknown">未知</span>
    </div>
  {/if}

  <!-- 后台任务直发(44 号方案乙:fork 父会话派生独立子会话,护栏继承 spawn 链) -->
  <div class="task-dispatch">
    <input
      class="task-goal"
      type="text"
      placeholder="后台任务目标(如:把测试套件跑完并汇总结果)…"
      bind:value={taskGoal}
      onkeydown={(e) => e.key === 'Enter' && dispatchTask()}
      disabled={taskBusy || !$sessionId}
    />
    <button class="task-go" onclick={dispatchTask} disabled={taskBusy || !$sessionId || !taskGoal.trim()}>
      {taskBusy ? '派发中…' : '后台派发'}
    </button>
  </div>
  {#if taskErr}<div class="task-err">{taskErr}</div>{/if}

  {#if $orchestration.agents.length > 0 && hasChildren}
    <div class="orch-agents">
      {#each $orchestration.agents as a (a.agent_type)}
        <span class="oa" title={a.description} class:can={a.can_delegate}>
          {a.agent_type}{a.can_delegate ? ' ⇉' : ''}
        </span>
      {/each}
    </div>
  {/if}
</div>

<style>
  .orch {
    width: var(--sidebar-w, 300px); /* E2:divider 可调,根变量统一侧栏宽 */
    flex-shrink: 0;
    background: var(--sidebar-bg);
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .orch-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--sp-sm) var(--sp-md);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .orch-title {
    font-size: var(--fs-2xs);
    font-weight: var(--fw-sb);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--text-secondary);
  }
  .orch-refresh {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    padding: 3px var(--sp-sm);
    border-radius: var(--r-sm);
    border: 1px solid var(--border);
    background: transparent;
    cursor: pointer;
  }
  .orch-refresh:hover:not(:disabled) {
    color: var(--text-primary);
    border-color: var(--brand);
  }
  .orch-refresh:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .orch-empty {
    padding: var(--sp-xl) var(--sp-md);
    font-size: var(--fs-sm);
    color: var(--text-muted);
    text-align: center;
  }
  .orch-err {
    margin-top: var(--sp-xs);
    font-size: var(--fs-xs);
    color: var(--danger);
    word-break: break-all;
  }
  .orch-canvas {
    flex: 1;
    min-height: 0;
    overflow: auto;
    padding: var(--sp-xs);
  }
  .task-dispatch {
    display: flex;
    gap: 6px;
    padding: 8px 12px;
    border-top: 1px solid var(--border, #333);
    flex-shrink: 0;
  }
  .task-goal {
    flex: 1;
    min-width: 0;
    font-size: 12px;
    padding: 4px 8px;
    background: var(--bg-input, #1c1c1c);
    color: var(--fg, #ddd);
    border: 1px solid var(--border, #3a3a3a);
    border-radius: 4px;
  }
  .task-go {
    font-size: 12px;
    padding: 4px 12px;
    cursor: pointer;
    background: var(--accent, #b8860b);
    border: 1px solid var(--accent, #b8860b);
    color: #111;
    border-radius: 4px;
    white-space: nowrap;
    font-weight: 600;
  }
  .task-go:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .task-err {
    color: #e07070;
    font-size: 11px;
    padding: 2px 12px 4px;
  }
  .orch-legend {
    display: flex;
    gap: var(--sp-sm);
    padding: var(--sp-xs) var(--sp-md);
    border-top: 1px solid var(--border);
    flex-shrink: 0;
  }
  .lg {
    font-size: var(--fs-2xs);
    color: var(--text-muted);
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .lg::before {
    content: '';
    width: 8px;
    height: 8px;
    border-radius: 50%;
    display: inline-block;
  }
  .lg-running::before { background: var(--brand); }
  .lg-done::before { background: var(--text-muted); }
  .lg-failed::before { background: var(--danger); }
  .lg-unknown::before { background: transparent; border: 1px dashed var(--text-muted); }
  .orch-agents {
    padding: var(--sp-xs) var(--sp-md) var(--sp-sm);
    border-top: 1px solid var(--border);
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    flex-shrink: 0;
  }
  .oa {
    font-size: var(--fs-2xs);
    color: var(--text-muted);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 1px 7px;
  }
  .oa.can {
    color: var(--brand);
    border-color: var(--brand);
  }
  .oedge {
    fill: none;
    stroke: var(--border);
    stroke-width: 1.6;
    stroke-linecap: round;
  }
  .oedge.spawn {
    stroke-dasharray: 4 3;
  }
  .obox {
    fill: var(--bg-card);
    stroke: var(--border);
    stroke-width: 1.2;
  }
  .onode.running .obox {
    stroke: var(--brand);
    stroke-dasharray: 5 3;
    animation: orch-dash 1.2s linear infinite;
  }
  .onode.running .odot { fill: var(--brand); }
  .onode.done .obox { stroke: var(--text-muted); }
  .onode.done .odot { fill: var(--text-muted); }
  .onode.failed .obox { stroke: var(--danger); }
  .onode.failed .odot { fill: var(--danger); }
  .onode.unknown .obox { stroke-dasharray: 3 3; }
  .onode.unknown .odot { fill: transparent; stroke: var(--text-muted); stroke-width: 1; }
  .otype {
    font-size: var(--fs-xs);
    font-weight: var(--fw-sb);
    fill: var(--text-primary);
  }
  .osid {
    font-size: var(--fs-2xs);
    fill: var(--text-muted);
  }
  @keyframes orch-dash {
    to { stroke-dashoffset: -16; }
  }
  @media (prefers-reduced-motion: reduce) {
    .onode.running .obox { animation: none; }
  }
</style>
