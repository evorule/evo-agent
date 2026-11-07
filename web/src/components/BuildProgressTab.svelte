<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<script>
  import { onMount } from 'svelte';
  import { fsEvents, workspaceEpoch } from '../lib/stores.js';
  import { listDir, fetchRaw } from '../lib/api.js';
  import { EventTail } from '../lib/event-tail.js';

  /** 制作进度实时面板:尾随 <project>/build.progress.ndjson(events/1.0
   *  NDJSON,engine 构建旁挂)。通道=fs_events 触发 + raw 端点 Range 游标
   *  增量(零新宿主端点,空闲零轮询);offline/waiting 显态不静默。 */

  const MAX_SHOWN = 50;

  let projects = $state([]);
  let selected = $state('');
  let customPath = $state('');
  let tailState = $state('idle'); // idle | following | waiting | offline
  let cursor = $state(0);
  let events = $state([]);
  let discoveryNote = $state('');
  let epoch = $state(0);
  let tail = null;

  async function discover() {
    discoveryNote = '';
    try {
      const res = await listDir('video_engine/projects');
      projects = (res.entries || []).filter((e) => e.kind === 'dir').map((e) => e.name);
    } catch (e) {
      // 工作区无 video_engine(非视频工程)→ 空列表可手填路径
      projects = [];
      discoveryNote = String(e?.message || e);
    }
  }

  function startTail(path) {
    if (tail) {
      tail.stop();
      tail = null;
    }
    events = [];
    cursor = 0;
    selected = path.startsWith('video_engine/projects/') && path.endsWith('/build.progress.ndjson')
      ? path.split('/')[2]
      : '';
    if (!path) return;
    tail = new EventTail({
      path,
      fetchImpl: (p, headers) => fetchRaw(p, headers),
      onEvents: (evs) => {
        if (tail) cursor = tail.cursor;
        events = [...[...evs].reverse(), ...events].slice(0, MAX_SHOWN);
      },
      onState: (s) => {
        tailState = s;
        if (tail) cursor = tail.cursor;
      },
    });
    tail.start();
  }

  function selectProject(e) {
    const name = e.target.value;
    if (name) startTail(`video_engine/projects/${name}/build.progress.ndjson`);
  }

  function applyCustom() {
    startTail(customPath.trim());
  }

  function retry() {
    if (tail) tail.retry();
  }

  // fs_events 一次性信号桥:批量路径转交尾随器(命中才拉,零轮询)
  const unsubFs = fsEvents.subscribe((ev) => {
    if (!ev || !tail) return;
    fsEvents.set(null);
    const paths = [
      ...(ev.added || []),
      ...(ev.updated || []),
      ...(ev.removed || []),
      ...(ev.moved || []).map((m) => m.to),
    ];
    tail.noteFsEvent(paths);
  });

  // 工作区切换:路径基准重锚,旧尾随作废重新发现
  const unsubEpoch = workspaceEpoch.subscribe((n) => {
    epoch = n;
  });

  $effect(() => {
    void epoch;
    if (tail) {
      tail.stop();
      tail = null;
    }
    events = [];
    selected = '';
    tailState = 'idle';
    cursor = 0;
    discover();
  });

  onMount(() => {
    discover();
    return () => {
      unsubFs();
      unsubEpoch();
      if (tail) tail.stop();
    };
  });

  function fmtTime(ts) {
    if (typeof ts !== 'number') return '—';
    const d = new Date(ts * 1000);
    const pad = (n) => String(n).padStart(2, '0');
    return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  }

  function pct(e) {
    return typeof e.progress === 'number' ? `${Math.round(e.progress * 100)}%` : '';
  }
</script>

<div class="bp-root">
  <div class="bp-toolbar">
    <select class="bp-select" value={selected} onchange={selectProject} aria-label="选择项目">
      <option value="">选择项目…</option>
      {#each projects as p}
        <option value={p}>{p}</option>
      {/each}
    </select>
    <input
      class="bp-input"
      placeholder="或手填事件文件路径(相对工作区根)…"
      bind:value={customPath}
    />
    <button class="bp-btn" onclick={applyCustom} disabled={!customPath.trim()}>跟随</button>
    {#if tailState === 'offline'}
      <button class="bp-btn bp-retry" onclick={retry}>重试</button>
    {/if}
    <span class="bp-state bp-st-{tailState}">{tailState}</span>
    <span class="bp-cursor">{cursor} B</span>
  </div>
  {#if discoveryNote && !projects.length}
    <div class="bp-note">项目发现为空({discoveryNote})——可手填事件文件路径跟随。</div>
  {/if}
  {#if tailState === 'offline'}
    <div class="bp-banner bp-warn">连接中断(连续失败达阈值)——事件不补发;恢复靠「重试」或下一个文件事件。</div>
  {:else if tailState === 'waiting'}
    <div class="bp-banner">等待事件文件出现(engine 构建时重建)…</div>
  {/if}
  <div class="bp-list">
    {#each events as e}
      <div class="bp-ev">
        <span class="bp-seq">#{e.seq ?? '—'}</span>
        <span class="bp-time">{fmtTime(e.ts)}</span>
        <span class="bp-phase bp-ph-{e.phase || 'unknown'}">{e.phase || 'unknown'}{e.stage ? `/${e.stage}` : ''}</span>
        <span class="bp-shot">
          {e.shot_id ? `${e.shot_id}${e.total_shots ? ` (${e.shot_index ?? '?'}/${e.total_shots})` : ''}` : ''}
        </span>
        {#if typeof e.progress === 'number'}
          <span class="bp-bar">
            <span class="bp-fill" style="width: {Math.min(100, Math.max(0, Math.round(e.progress * 100)))}%"></span>
          </span>
          <span class="bp-pct">{pct(e)}</span>
        {/if}
        <span class="bp-msg">{e.message || ''}</span>
      </div>
    {:else}
      <div class="bp-empty">尚无事件——选择项目后随构建实时呈现(通道=文件事件触发,无轮询)。</div>
    {/each}
  </div>
</div>

<style>
  .bp-root {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    font-size: 12px;
    color: var(--fg, #ccc);
  }
  .bp-toolbar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 10px;
    border-bottom: 1px solid var(--border, #333);
  }
  .bp-select,
  .bp-input {
    background: var(--bg-card, #1d1d1d);
    color: var(--fg, #ccc);
    border: 1px solid var(--border, #333);
    border-radius: 4px;
    padding: 3px 6px;
    font-size: 12px;
  }
  .bp-input {
    flex: 1;
    min-width: 120px;
  }
  .bp-btn {
    background: var(--bg-card, #1d1d1d);
    color: var(--fg, #ccc);
    border: 1px solid var(--border, #333);
    border-radius: 4px;
    padding: 3px 10px;
    cursor: pointer;
  }
  .bp-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .bp-retry {
    border-color: var(--warn, #b58900);
  }
  .bp-state {
    padding: 2px 8px;
    border-radius: 10px;
    border: 1px solid var(--border, #333);
  }
  .bp-st-following {
    color: #4cc38a;
    border-color: #4cc38a;
  }
  .bp-st-waiting {
    color: #d7ba7d;
    border-color: #d7ba7d;
  }
  .bp-st-offline {
    color: #f14c4c;
    border-color: #f14c4c;
  }
  .bp-cursor {
    color: var(--fg-dim, #777);
  }
  .bp-note,
  .bp-banner {
    padding: 4px 10px;
    color: var(--fg-dim, #999);
  }
  .bp-warn {
    color: #f14c4c;
  }
  .bp-list {
    flex: 1;
    overflow: auto;
    min-height: 0;
    padding: 4px 0;
  }
  .bp-ev {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 3px 10px;
    border-bottom: 1px solid var(--border-dim, #262626);
  }
  .bp-seq {
    color: var(--fg-dim, #777);
    min-width: 36px;
  }
  .bp-time {
    color: var(--fg-dim, #777);
  }
  .bp-phase {
    font-weight: 600;
  }
  .bp-ph-done {
    color: #4cc38a;
  }
  .bp-ph-failed {
    color: #f14c4c;
  }
  .bp-shot {
    color: #d7ba7d;
  }
  .bp-bar {
    width: 90px;
    height: 6px;
    border-radius: 3px;
    background: var(--bg-card, #1d1d1d);
    overflow: hidden;
    display: inline-block;
  }
  .bp-fill {
    display: block;
    height: 100%;
    background: #4cc38a;
  }
  .bp-pct {
    min-width: 34px;
    color: #4cc38a;
  }
  .bp-msg {
    color: var(--fg-dim, #999);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .bp-empty {
    padding: 10px;
    color: var(--fg-dim, #777);
  }
</style>
