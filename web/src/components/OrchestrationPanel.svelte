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

  const STATUS_TEXT = { running: '执行中', done: '完成', failed: '失败', unknown: '未知' };

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
