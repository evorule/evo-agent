<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- G3 时光机器 tab:会话回放事件链可视化(GET /api/sessions/{id}/replay)。
     纯可视化:垂直时间线(最新步=当前步高亮)+点击步骤展开详情
     (指令/结果/状态/因果)。按设计不回写——时间旅行写入走既有会话
     恢复链,本面板绝不发起;引擎账为空时 serve 侧 journal 合成
     (source=local_journal_synth)呈徽标明示数据来源。 -->
<script>
  import { sessionId } from '../lib/stores.js';
  import { replaySession } from '../lib/api.js';

  let events = $state([]);
  let source = $state('');
  let message = $state('');
  let loading = $state(false);
  let err = $state('');
  let expandedId = $state(null);

  function kindOf(ev) {
    return ev?.event_type?.kind || '?';
  }
  function subOf(ev) {
    const s = ev?.event_type?.subtype;
    return typeof s === 'string' && s ? s : '';
  }
  function fmtTime(ts) {
    if (!ts) return '—';
    const d = new Date(Number(ts) * 1000);
    const p = (n) => String(n).padStart(2, '0');
    return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
  }
  /** 步骤一句话:content 里按优先级取可读字段 */
  function stepText(ev) {
    const c = ev?.content;
    if (c == null) return '';
    if (typeof c === 'string') return c.length > 120 ? `${c.slice(0, 120)}…` : c;
    if (typeof c === 'object') {
      const keys = ['goal', 'tool', 'command', 'milestone', 'text', 'status', 'output'];
      const parts = keys
        .filter((k) => c[k] != null && typeof c[k] !== 'object')
        .map((k) => `${k}=${typeof c[k] === 'string' && c[k].length > 60 ? `${c[k].slice(0, 60)}…` : c[k]}`);
      return parts.join(' ') || '';
    }
    return String(c);
  }

  $effect(() => {
    const sid = $sessionId;
    if (!sid) return;
    loading = true;
    err = '';
    replaySession(sid)
      .then((res) => {
        events = Array.isArray(res.events) ? res.events : [];
        source = res.source || '';
        message = res.message || '';
        expandedId = null;
      })
      .catch((e) => {
        err = String(e?.message || e);
        events = [];
      })
      .finally(() => (loading = false));
  });
</script>

<div class="tt">
  <div class="tt-head">
    <span class="tt-note">
      会话回放链(纯可视化,不回写;时间旅行写入走既有会话恢复链)
      {#if source === 'local_journal_synth'}<span class="tt-synth" title={message}>本地账合成</span>{/if}
    </span>
    <span class="tt-count">{events.length} 步</span>
  </div>
  {#if !$sessionId}
    <div class="tt-empty">未连接会话——新建会话后此处展示回放时间线。</div>
  {:else if loading && events.length === 0}
    <div class="tt-empty">正在加载回放链…</div>
  {:else if err && events.length === 0}
    <div class="tt-empty">回放链加载失败:{err}</div>
  {:else if events.length === 0}
    <div class="tt-empty">本会话暂无回放事件。</div>
  {:else}
    <div class="tt-list">
      {#each events as ev, i (ev.event_id || i)}
        <button
          class="tt-step"
          class:current={i === events.length - 1}
          class:open={expandedId === (ev.event_id || i)}
          onclick={() => (expandedId = expandedId === (ev.event_id || i) ? null : ev.event_id || i)}
        >
          <span class="tt-idx mono">{i + 1}</span>
          <div class="tt-main">
            <div class="tt-row">
              <span class="tt-kind">{kindOf(ev)}{subOf(ev) ? `·${subOf(ev)}` : ''}</span>
              <span class="tt-time mono">{fmtTime(ev.timestamp)}</span>
              {#if i === events.length - 1}<span class="tt-now">当前</span>{/if}
            </div>
            {#if stepText(ev)}<div class="tt-text">{stepText(ev)}</div>{/if}
            {#if expandedId === (ev.event_id || i)}
              <div class="tt-detail" onclick={(e) => e.stopPropagation()}>
                <div class="tt-kv"><span class="k">event_id</span><span class="v mono">{ev.event_id || '—'}</span></div>
                <div class="tt-kv"><span class="k">source</span><span class="v mono">{ev.source || '—'}</span></div>
                <div class="tt-kv"><span class="k">cause</span><span class="v mono">{ev.cause ?? '—'}</span></div>
                <div class="tt-kv"><span class="k">entities</span><span class="v mono">{(ev.entities || []).map((x) => x.entity_id || x).join(', ') || '—'}</span></div>
                <div class="tt-kv"><span class="k">content</span><span class="v mono">{JSON.stringify(ev.content, null, 1)}</span></div>
              </div>
            {/if}
          </div>
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .tt {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
  }
  .tt-head {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    padding: var(--sp-xs) var(--sp-sm);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .tt-note {
    font-size: var(--fs-xs);
    color: var(--text-muted);
    margin-right: auto;
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .tt-synth {
    color: var(--warning);
    border: 1px solid var(--warning);
    border-radius: 999px;
    padding: 0 7px;
    font-size: var(--fs-2xs);
  }
  .tt-count {
    font-size: var(--fs-xs);
    color: var(--text-muted);
    flex-shrink: 0;
  }
  .tt-empty {
    padding: var(--sp-xl) var(--sp-md);
    font-size: var(--fs-sm);
    color: var(--text-muted);
    text-align: center;
  }
  .tt-list {
    flex: 1;
    min-height: 0;
    overflow: auto;
  }
  .tt-step {
    display: flex;
    gap: var(--sp-sm);
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    border-bottom: 1px solid var(--border);
    padding: 6px var(--sp-sm);
    cursor: pointer;
    color: inherit;
    align-items: flex-start;
  }
  .tt-step:hover {
    background: var(--bg-hover);
  }
  .tt-step.current {
    background: var(--bg-hover);
    box-shadow: inset 2px 0 0 var(--brand);
  }
  .tt-step.open {
    background: var(--bg-hover);
  }
  .tt-idx {
    font-size: var(--fs-2xs);
    color: var(--text-muted);
    border: 1px solid var(--border);
    border-radius: 50%;
    width: 18px;
    height: 18px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    margin-top: 1px;
  }
  .tt-step.current .tt-idx {
    color: var(--brand);
    border-color: var(--brand);
  }
  .tt-main {
    flex: 1;
    min-width: 0;
  }
  .tt-row {
    display: flex;
    align-items: baseline;
    gap: var(--sp-sm);
  }
  .tt-kind {
    font-size: var(--fs-2xs);
    color: var(--brand);
    white-space: nowrap;
  }
  .tt-time {
    font-size: var(--fs-2xs);
    color: var(--text-muted);
  }
  .tt-now {
    font-size: var(--fs-2xs);
    color: var(--brand);
    border: 1px solid var(--brand);
    border-radius: 999px;
    padding: 0 6px;
    margin-left: auto;
  }
  .tt-text {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    margin-top: 2px;
  }
  .tt-detail {
    margin-top: 6px;
    padding: 6px 8px;
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    background: var(--bg-card);
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .tt-kv {
    display: flex;
    gap: var(--sp-sm);
    font-size: var(--fs-xs);
    min-width: 0;
  }
  .tt-kv .k {
    color: var(--text-muted);
    flex-shrink: 0;
    width: 64px;
  }
  .tt-kv .v {
    color: var(--text-secondary);
    white-space: pre-wrap;
    word-break: break-all;
  }
</style>
