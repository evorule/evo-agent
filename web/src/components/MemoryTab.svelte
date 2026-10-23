<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- G2 记忆 tab:当前会话结构化记忆事件(GET /api/sessions/{id}/events)。
     只读视图:关键词过滤+事件列表(时间/kind/摘要)+点击展开详情
     (content/因果链/实体/来源)。写面(单条删除等 HumanGate 通道)无
     对应端点,本面板不硬造;数据源 best-effort 同步,失败呈本地缓存。 -->
<script>
  import { sessionId } from '../lib/stores.js';
  import { listSessionEvents } from '../lib/api.js';
  import AuditBadge from './AuditBadge.svelte';
  import AuditCard from './AuditCard.svelte';
  import { auditState, badgeStatus, factForMemoryEvent } from '../lib/audit.js';

  let filter = $state('');
  let events = $state([]);
  let loading = $state(false);
  let err = $state('');
  let loadedFor = $state(null);
  let expandedId = $state(null);
  // audit-one-click:记忆条目挂共享审计徽章——有 fact_id 的条目按链上
  // 存证绿/灰,点开同款报告卡(按 fact_id 直查,不走消息映射)。
  let auditCardEntry = $state(null);

  function evBadge(ev) {
    const has = factForMemoryEvent($auditState, ev?.fact_id) != null;
    return {
      status: badgeStatus($auditState, has),
      entry: factForMemoryEvent($auditState, ev?.fact_id),
    };
  }

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
  /** 摘要:content 的关键值串(对象取值拼接,标量直显;超 90 字截断) */
  function summarize(ev) {
    const c = ev?.content;
    if (c == null) return '';
    let text = '';
    if (typeof c === 'string') text = c;
    else if (typeof c === 'object') {
      text = Object.entries(c)
        .filter(([, v]) => v != null && typeof v !== 'object')
        .map(([k, v]) => `${k}=${v}`)
        .join(' ');
      if (!text) text = JSON.stringify(c);
    } else text = String(c);
    return text.length > 90 ? `${text.slice(0, 90)}…` : text;
  }

  $effect(() => {
    const sid = $sessionId;
    if (!sid) return;
    loading = true;
    err = '';
    listSessionEvents(sid)
      .then((res) => {
        events = Array.isArray(res.events) ? res.events : [];
        loadedFor = sid;
        expandedId = null;
      })
      .catch((e) => {
        err = String(e?.message || e);
        events = [];
      })
      .finally(() => (loading = false));
  });

  // 客户端过滤:event_id/kind/subtype/摘要串命中即可
  const filtered = $derived.by(() => {
    const q = filter.trim().toLowerCase();
    if (!q) return events;
    return events.filter((ev) => {
      const hay = `${ev.event_id || ''} ${kindOf(ev)} ${subOf(ev)} ${summarize(ev)}`.toLowerCase();
      return hay.includes(q);
    });
  });
</script>

<div class="mem">
  <div class="mem-head">
    <input
      class="mem-filter"
      type="text"
      placeholder="过滤事件(id/类型/内容关键词)…"
      bind:value={filter}
      aria-label="过滤记忆事件"
    />
    <span class="mem-count">{filtered.length}{filter ? ` / ${events.length}` : ''} 条</span>
  </div>
  {#if !$sessionId}
    <div class="mem-empty">未连接会话——新建会话后此处展示该会话的记忆事件。</div>
  {:else if loading && events.length === 0}
    <div class="mem-empty">正在加载记忆事件…</div>
  {:else if err && events.length === 0}
    <div class="mem-empty">记忆事件加载失败:{err}</div>
  {:else if filtered.length === 0}
    <div class="mem-empty">
      {events.length === 0 ? '本会话暂无记忆事件。' : '无匹配事件——调整过滤关键词。'}
    </div>
  {:else}
    <div class="mem-list">
      {#each filtered as ev (ev.event_id)}
        <button class="mem-item" class:open={expandedId === ev.event_id} onclick={() => (expandedId = expandedId === ev.event_id ? null : ev.event_id)}>
          <div class="mem-row">
            <span class="mem-time mono">{fmtTime(ev.timestamp)}</span>
            <span class="mem-kind">{kindOf(ev)}{subOf(ev) ? `·${subOf(ev)}` : ''}</span>
            <span class="mem-summary">{summarize(ev)}</span>
            <AuditBadge
              status={evBadge(ev).status}
              clickable={evBadge(ev).status === 'green'}
              onclick={() => (auditCardEntry = evBadge(ev).entry)}
            />
          </div>
          {#if expandedId === ev.event_id}
            <div class="mem-detail" onclick={(e) => e.stopPropagation()}>
              <div class="mem-kv"><span class="k">event_id</span><span class="v mono">{ev.event_id}</span></div>
              <div class="mem-kv"><span class="k">source</span><span class="v mono">{ev.source || '—'}</span></div>
              <div class="mem-kv"><span class="k">session</span><span class="v mono">{ev.session_id || '—'}</span></div>
              <div class="mem-kv"><span class="k">cause</span><span class="v mono">{ev.cause ?? '—'}</span></div>
              <div class="mem-kv"><span class="k">entities</span><span class="v mono">{(ev.entities || []).map((x) => x.entity_id || x).join(', ') || '—'}</span></div>
              <div class="mem-kv"><span class="k">content</span><span class="v mono">{JSON.stringify(ev.content, null, 1)}</span></div>
            </div>
          {/if}
        </button>
      {/each}
    </div>
  {/if}

  {#if auditCardEntry}
    <AuditCard
      entry={auditCardEntry}
      sid={$sessionId}
      verified={$auditState?.verified ?? null}
      factCount={$auditState?.factCount ?? null}
      onclose={() => (auditCardEntry = null)}
    />
  {/if}
</div>

<style>
  .mem {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
  }
  .mem-head {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    padding: var(--sp-xs) var(--sp-sm);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .mem-filter {
    flex: 1;
    min-width: 0;
    font-size: var(--fs-xs);
    padding: 3px var(--sp-sm);
    background: var(--bg-input, transparent);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
  }
  .mem-count {
    font-size: var(--fs-xs);
    color: var(--text-muted);
    flex-shrink: 0;
  }
  .mem-empty {
    padding: var(--sp-xl) var(--sp-md);
    font-size: var(--fs-sm);
    color: var(--text-muted);
    text-align: center;
  }
  .mem-list {
    flex: 1;
    min-height: 0;
    overflow: auto;
  }
  .mem-item {
    display: block;
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    border-bottom: 1px solid var(--border);
    padding: 6px var(--sp-sm);
    cursor: pointer;
    color: inherit;
  }
  .mem-item:hover {
    background: var(--bg-hover);
  }
  .mem-item.open {
    background: var(--bg-hover);
  }
  .mem-row {
    display: flex;
    align-items: baseline;
    gap: var(--sp-sm);
    min-width: 0;
  }
  .mem-row :global(.audit-badge) {
    margin-left: auto;
    align-self: center;
    flex: none;
  }
  .mem-time {
    font-size: var(--fs-2xs);
    color: var(--text-muted);
    flex-shrink: 0;
  }
  .mem-kind {
    font-size: var(--fs-2xs);
    color: var(--brand);
    flex-shrink: 0;
    white-space: nowrap;
  }
  .mem-summary {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .mem-detail {
    margin-top: 6px;
    padding: 6px 8px;
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    background: var(--bg-card);
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .mem-kv {
    display: flex;
    gap: var(--sp-sm);
    font-size: var(--fs-xs);
    min-width: 0;
  }
  .mem-kv .k {
    color: var(--text-muted);
    flex-shrink: 0;
    width: 64px;
  }
  .mem-kv .v {
    color: var(--text-secondary);
    white-space: pre-wrap;
    word-break: break-all;
  }
</style>
