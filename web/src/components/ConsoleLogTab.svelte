<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- G1 控制台日志 tab:serve 日志环形缓冲(GET /admin/logs,纯内存,重启即清)
     2s 轮询;贴底跟随/暂停,ERROR/WARN 行着色。排障视图,不充当审计链。 -->
<script>
  import { onMount, onDestroy } from 'svelte';
  import { getAdminLogs } from '../lib/api.js';

  let lines = [];
  let err = '';
  let follow = true;
  let paused = false;
  let bodyEl = null;
  let timer = null;
  let loading = false;

  function classify(line) {
    if (/\bERROR\b|\berror:/.test(line)) return 'err';
    if (/\bWARN\b/.test(line)) return 'warn';
    return '';
  }

  async function poll() {
    if (paused || loading) return;
    loading = true;
    try {
      const res = await getAdminLogs(200);
      lines = Array.isArray(res.lines) ? res.lines : [];
      err = '';
    } catch (e) {
      err = String(e?.message || e);
    } finally {
      loading = false;
    }
  }

  function onScroll() {
    if (!bodyEl) return;
    follow = bodyEl.scrollHeight - bodyEl.scrollTop - bodyEl.clientHeight < 24;
  }

  function scrollBottom() {
    if (bodyEl) bodyEl.scrollTop = bodyEl.scrollHeight;
  }

  $: if (bodyEl && lines.length && follow) requestAnimationFrame(scrollBottom);

  onMount(() => {
    poll();
    timer = setInterval(poll, 2000);
  });

  onDestroy(() => {
    if (timer) clearInterval(timer);
  });
</script>

<div class="clog-head">
  <span class="clog-note">serve 进程日志(内存环形缓冲,最近 500 条;重启即清,不充当审计链)</span>
  <label class="clog-pause">
    <input type="checkbox" bind:checked={paused} /> 暂停轮询
  </label>
  {#if !follow}
    <button class="clog-act" onclick={scrollBottom}>↓ 跟随</button>
  {/if}
</div>
<div class="clog-body mono" bind:this={bodyEl} onscroll={onScroll}>
  {#if err}
    <div class="clog-err">轮询失败:{err}(serve 未连接或端点不可达)</div>
  {:else if lines.length === 0}
    <div class="clog-empty">暂无日志。serve 进程的 tracing 输出将在此呈现。</div>
  {:else}
    {#each lines as line, i (i)}
      <div class="clog-line {classify(line)}">{line}</div>
    {/each}
  {/if}
</div>

<style>
  .clog-head {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    padding: var(--sp-xs) var(--sp-sm);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .clog-note {
    font-size: var(--fs-xs);
    color: var(--text-muted);
    margin-right: auto;
  }
  .clog-pause {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    display: inline-flex;
    align-items: center;
    gap: 4px;
    cursor: pointer;
  }
  .clog-act {
    font-size: var(--fs-xs);
    color: var(--brand);
    background: transparent;
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: 2px var(--sp-sm);
    cursor: pointer;
  }
  .clog-act:hover {
    border-color: var(--brand);
  }
  .clog-body {
    flex: 1;
    min-height: 0;
    overflow: auto;
    padding: var(--sp-xs) var(--sp-sm);
    font-size: var(--fs-2xs);
    line-height: 1.55;
    white-space: pre-wrap;
    word-break: break-all;
  }
  .clog-line {
    color: var(--text-secondary);
  }
  .clog-line.err {
    color: var(--danger);
  }
  .clog-line.warn {
    color: var(--warning);
  }
  .clog-empty,
  .clog-err {
    padding: var(--sp-xl) var(--sp-md);
    font-size: var(--fs-sm);
    text-align: center;
    color: var(--text-muted);
  }
  .clog-err {
    color: var(--danger);
  }
</style>
