<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<script>
  import { onMount } from 'svelte';
  import { fsEvents, workspaceEpoch } from '../lib/stores.js';
  import { listDir, fetchRaw, readFile } from '../lib/api.js';
  import { openMediaTab } from '../lib/stores.js';
  import { EventTail } from '../lib/event-tail.js';
  import { buildProgress } from '../lib/stores.js';
  import { mergeProgressEvents } from '../lib/progress-events.js';
  import {
    aggregateChains,
    chainStatus,
    replayFromManifest,
    manifestPathFor,
  } from '../lib/pipeline-view.js';

  /** 制作看板(链级):Backlot 式流水线状态板——链级聚合/STALL 双窗显态/
   *  REPLAY 磁盘投影。数据源=已落盘事件+manifest(UI 是磁盘投影,不直读
   *  引擎内部状态);坏输入降级渲染不崩溃(never-block)。通道与「制作进度」
   *  tab 同源(EventTail 双通道冗余),按 seq 去重使重复投递无害。 */

  let projects = $state([]);
  let selected = $state('');
  let tailState = $state('idle'); // idle | following | waiting | offline
  let tail = null;
  let tailPath = $state('');
  let events = $state([]);
  let epoch = $state(0);
  let manifest = $state(null); // REPLAY 投影源(成功链拉取)
  let manifestNote = $state('');

  // 派生面:链板+最新成功链(回放区数据源)
  const chains = $derived(aggregateChains(events));
  const replayChain = $derived(chains.find((c) => c.ended && c.ok) || null);
  const timeline = $derived(replayFromManifest(manifest));

  async function discover() {
    try {
      const res = await listDir('video_engine/projects');
      projects = (res.entries || []).filter((e) => e.kind === 'dir').map((e) => e.name);
    } catch {
      projects = []; // 工作区无 video_engine(非视频工程)→ 空列表可手填
    }
  }

  // 成功链出现 → 拉 manifest 做磁盘投影(失败/缺失降级显态,不阻塞链板)
  $effect(() => {
    if (!replayChain || !tailPath) return;
    const p = manifestPathFor(tailPath);
    if (!p) return;
    readFile(p)
      .then((res) => {
        try {
          manifest = JSON.parse(res.content ?? '');
          manifestNote = '';
        } catch (e) {
          manifest = null;
          manifestNote = `manifest 解析失败(半写?): ${String(e?.message || e).slice(0, 120)}`;
        }
      })
      .catch((e) => {
        manifest = null;
        manifestNote = `manifest 不可用: ${String(e?.message || e).slice(0, 120)}`;
      });
  });

  function startTail(path) {
    if (tail) {
      tail.stop();
      tail = null;
    }
    events = [];
    manifest = null;
    manifestNote = '';
    tailPath = path;
    selected =
      path.startsWith('video_engine/projects/') && path.endsWith('/build.progress.ndjson')
        ? path.split('/')[2]
        : '';
    if (!path) return;
    tail = new EventTail({
      path,
      fetchImpl: (p, headers) => fetchRaw(p, headers),
      onEvents: (evs) => {
        events = mergeProgressEvents(events, [...evs].reverse(), 400);
      },
      onState: (s) => {
        tailState = s;
      },
    });
    tail.start();
  }

  function selectProject(e) {
    const name = e.target.value;
    if (name) startTail(`video_engine/projects/${name}/build.progress.ndjson`);
  }

  function retry() {
    if (tail) tail.retry();
  }

  // fs_events 一次性信号桥:文件变更转交尾随器(命中才拉,零轮询)
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

  // v2 typed 帧桥(主通道):帧到达即 following(观察链路活着)
  const unsubBp = buildProgress.subscribe((frame) => {
    if (!frame || !tail) return;
    buildProgress.set(null);
    if (frame.path !== tail.path) return;
    tailState = 'following';
    events = mergeProgressEvents(events, frame.events, 400);
  });

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
    manifest = null;
    manifestNote = '';
    selected = '';
    tailPath = '';
    tailState = 'idle';
    discover();
  });

  onMount(() => {
    discover();
    return () => {
      unsubFs();
      unsubBp();
      unsubEpoch();
      if (tail) tail.stop();
    };
  });

  const STAGE_LABELS = {
    script: '脚本',
    build: '构建',
    qc: '质检',
    gate: '人审闸',
    broadcast: '封装',
    chain_end: '收链',
  };

  function openShot(path) {
    if (!path) return;
    openMediaTab(path); // 媒体路由:shouldRouteToMediaStage 判定进 MediaStage
  }
</script>

<div class="pl-root">
  <div class="pl-toolbar">
    <select class="pl-select" value={selected} onchange={selectProject} aria-label="选择项目">
      <option value="">选择项目…</option>
      {#each projects as p}
        <option value={p}>{p}</option>
      {/each}
    </select>
    {#if tailState === 'offline'}
      <button class="pl-btn pl-retry" onclick={retry}>重试</button>
    {/if}
    <span class="pl-state pl-st-{tailState}">{tailState}</span>
  </div>
  {#if tailState === 'offline'}
    <div class="pl-banner pl-warn">连接中断(连续失败达阈值)——恢复靠「重试」或下一个文件事件。</div>
  {:else if tailState === 'waiting'}
    <div class="pl-banner">等待事件文件出现(engine 构建时重建)…</div>
  {/if}

  {#if chains.length === 0}
    <div class="pl-empty">尚无流水线链——项目每次制作(链式跑五段)产生一条链级记录。</div>
  {/if}

  {#each chains as c (c.run_id)}
    {@const st = chainStatus(c)}
    <div class="pl-chain">
      <div class="pl-chain-head">
        <span class="pl-run">{c.run_id}</span>
        <span class="pl-badge pl-b-{st.state}">{st.state}</span>
        {#if st.state === 'STALL'}
          <span class="pl-stall-note">可能卡死(超时无新事件)——检查引擎进程</span>
        {/if}
      </div>
      <div class="pl-stages">
        {#each st.stages as s (s.stage)}
          <span
            class="pl-stage pl-s-{s.ts != null ? (s.ok ? 'ok' : 'fail') : 'pend'}"
            title={s.message || ''}
          >
            {STAGE_LABELS[s.stage] || s.stage}{s.ts != null ? (s.ok ? ' ✓' : ' ✗') : ''}
          </span>
        {/each}
      </div>
    </div>
  {/each}

  {#if replayChain}
    <div class="pl-replay">
      <div class="pl-replay-head">成片回放 · {replayChain.run_id}</div>
      {#if manifestNote}
        <div class="pl-note">{manifestNote}</div>
      {/if}
      {#if timeline && timeline.length}
        <div class="pl-tl">
          {#each timeline as shot, i}
            {#if shot}
              <button class="pl-shot" onclick={() => openShot(shot.path)} disabled={!shot.path}>
                <span class="pl-shot-id">{shot.shot_id || `#${i + 1}`}</span>
                {#if shot.duration != null}
                  <span class="pl-shot-dur">{shot.duration.toFixed(1)}s</span>
                {/if}
              </button>
            {:else}
              <span class="pl-shot pl-shot-bad">段数据缺失</span>
            {/if}
          {/each}
        </div>
      {:else if !manifestNote}
        <div class="pl-note">manifest 不可用——回放区待产物指纹落盘。</div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .pl-root {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    overflow: auto;
    font-size: 12px;
    color: var(--fg, #ccc);
    padding: 4px 0;
  }
  .pl-toolbar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 10px 8px;
    border-bottom: 1px solid var(--border, #333);
  }
  .pl-select,
  .pl-btn {
    background: var(--bg-card, #1d1d1d);
    color: var(--fg, #ccc);
    border: 1px solid var(--border, #333);
    border-radius: 4px;
    padding: 3px 6px;
    font-size: 12px;
  }
  .pl-retry {
    border-color: var(--warn, #b58900);
  }
  .pl-state {
    padding: 2px 8px;
    border-radius: 10px;
    border: 1px solid var(--border, #333);
  }
  .pl-st-following {
    color: #4cc38a;
    border-color: #4cc38a;
  }
  .pl-st-waiting {
    color: #d7ba7d;
    border-color: #d7ba7d;
  }
  .pl-st-offline {
    color: #f14c4c;
    border-color: #f14c4c;
  }
  .pl-banner,
  .pl-note {
    padding: 4px 10px;
    color: var(--fg-dim, #999);
  }
  .pl-warn {
    color: #f14c4c;
  }
  .pl-empty {
    padding: 10px;
    color: var(--fg-dim, #777);
  }
  .pl-chain {
    padding: 6px 10px;
    border-bottom: 1px solid var(--border-dim, #262626);
  }
  .pl-chain-head {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 4px;
  }
  .pl-run {
    color: var(--fg-dim, #777);
    font-family: var(--font-mono, monospace);
  }
  .pl-badge {
    padding: 1px 8px;
    border-radius: 10px;
    border: 1px solid var(--border, #333);
  }
  .pl-b-LIVE {
    color: #4cc38a;
    border-color: #4cc38a;
  }
  .pl-b-QUIET {
    color: #d7ba7d;
    border-color: #d7ba7d;
  }
  .pl-b-STALL {
    color: #f14c4c;
    border-color: #f14c4c;
  }
  .pl-b-ok {
    color: #4cc38a;
    border-color: #4cc38a;
  }
  .pl-b-failed {
    color: #f14c4c;
    border-color: #f14c4c;
  }
  .pl-stall-note {
    color: #f14c4c;
  }
  .pl-stages {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .pl-stage {
    padding: 1px 8px;
    border-radius: 3px;
    border: 1px solid var(--border-dim, #262626);
    color: var(--fg-dim, #777);
  }
  .pl-s-ok {
    color: #4cc38a;
    border-color: #4cc38a55;
  }
  .pl-s-fail {
    color: #f14c4c;
    border-color: #f14c4c55;
  }
  .pl-replay {
    padding: 8px 10px;
  }
  .pl-replay-head {
    font-weight: 600;
    margin-bottom: 6px;
    color: #d7ba7d;
  }
  .pl-tl {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .pl-shot {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: var(--bg-card, #1d1d1d);
    color: var(--fg, #ccc);
    border: 1px solid var(--border, #333);
    border-radius: 4px;
    padding: 4px 10px;
    cursor: pointer;
    font-size: 12px;
  }
  .pl-shot:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .pl-shot-bad {
    color: var(--fg-dim, #777);
    font-style: italic;
    cursor: default;
  }
  .pl-shot-dur {
    color: #4cc38a;
  }
</style>
