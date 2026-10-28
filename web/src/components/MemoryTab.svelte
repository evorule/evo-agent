<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- G2 记忆 tab:当前会话结构化记忆事件(GET /api/sessions/{id}/events)。
     只读视图:关键词过滤+事件列表(时间/kind/摘要)+点击展开详情
     (content/因果链/实体/来源)。写面(单条删除等 HumanGate 通道)无
     对应端点,本面板不硬造;数据源 best-effort 同步,失败呈本地缓存。 -->
<script>
  import { sessionId, sessions } from '../lib/stores.js';
  import {
    listSessionEvents,
    listUserNotes,
    putUserNote,
    deleteUserNote,
    browseMemory,
  } from '../lib/api.js';
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

  // ===== 用户笔记(记忆用户主权面:stable.user.* 跨会话持久,召回权威最高) =====
  let notes = $state([]);
  let notesErr = $state('');
  let notesBusy = $state(false);
  let newKey = $state('');
  let newValue = $state('');
  // 内联编辑态(key → 草稿文本);null = 无编辑行
  let editingKey = $state(null);
  let editDraft = $state('');
  // 删除两段式确认(3 秒回弹,同检查点 tab 形态)
  let armedKey = $state(null);
  let armTimer = $state(null);

  /** 当前会话的 agent_type(sessions 索引回查;user-notes 端点按类型解析
   *  namespace,与事件端点的服务端同源解析口径一致) */
  function noteAgentType() {
    const s = $sessions.find((x) => x.session_id === $sessionId);
    return s?.agent_type || 'general';
  }

  async function refreshNotes() {
    if (!$sessionId) return;
    notesErr = '';
    try {
      const r = await listUserNotes(noteAgentType());
      notes = r.notes || [];
    } catch (e) {
      notesErr = String(e?.message || e);
    }
  }

  async function addNote() {
    const key = newKey.trim();
    if (!key || !newValue.trim() || notesBusy) return;
    notesBusy = true;
    notesErr = '';
    try {
      await putUserNote(noteAgentType(), key, newValue.trim(), $sessionId);
      newKey = '';
      newValue = '';
      await refreshNotes();
    } catch (e) {
      notesErr = String(e?.message || e);
    } finally {
      notesBusy = false;
    }
  }

  async function saveEdit(key) {
    if (notesBusy) return;
    notesBusy = true;
    notesErr = '';
    try {
      await putUserNote(noteAgentType(), key, editDraft, $sessionId);
      editingKey = null;
      editDraft = '';
      await refreshNotes();
    } catch (e) {
      notesErr = String(e?.message || e);
    } finally {
      notesBusy = false;
    }
  }

  function armDelete(key) {
    if (armTimer) clearTimeout(armTimer);
    armedKey = key;
    armTimer = setTimeout(() => (armedKey = null), 3000);
  }

  async function removeNote(key) {
    if (notesBusy) return;
    notesBusy = true;
    notesErr = '';
    try {
      await deleteUserNote(noteAgentType(), key, $sessionId);
      armedKey = null;
      await refreshNotes();
    } catch (e) {
      notesErr = String(e?.message || e);
    } finally {
      notesBusy = false;
    }
  }

  function onDelClick(key) {
    if (armedKey === key) return removeNote(key);
    armDelete(key);
  }

  // ===== 跨会话记忆浏览(只读投影;scope 白名单 stable/candidates/handovers) =====
  let browseScope = $state('stable');
  let browseEntries = $state([]);
  let browseLoading = $state(false);
  let browseErr = $state('');

  async function refreshBrowse() {
    if (!$sessionId) return;
    browseLoading = true;
    browseErr = '';
    try {
      const r = await browseMemory(noteAgentType(), browseScope);
      browseEntries = r.entries || [];
    } catch (e) {
      browseErr = String(e?.message || e);
    } finally {
      browseLoading = false;
    }
  }

  function switchScope(s) {
    if (s === browseScope) return;
    browseScope = s;
    refreshBrowse();
  }

  /** path 尾段作展示键(shared.{ns}.stable.llm.m2 → stable.llm.m2 相对尾段) */
  function shortPath(p) {
    if (typeof p !== 'string') return '—';
    const prefix = `shared.${noteAgentType()}.`;
    return p.startsWith(prefix) ? p.slice(prefix.length) : p;
  }

  function browseSummary(v) {
    if (v == null) return '';
    let text = typeof v === 'string' ? v : JSON.stringify(v);
    return text.length > 110 ? `${text.slice(0, 110)}…` : text;
  }

  $effect(() => {
    // 会话切换:笔记区随动(事件区由既有 loadedFor 机制驱动);浏览区懒加载
    if ($sessionId) {
      refreshNotes();
      refreshBrowse();
    }
    return () => {
      if (armTimer) clearTimeout(armTimer);
    };
  });

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
  <div class="notes">
    <div class="notes-head">
      <span class="notes-title">用户笔记</span>
      <span class="notes-hint">跨会话持久 · 召回面最高权威(agent 不可改写,遗忘权在您)</span>
    </div>
    {#if notesErr}<div class="notes-err">{notesErr}</div>{/if}
    {#if $sessionId}
      {#if notes.length > 0}
        <ul class="notes-list">
          {#each notes as n (n.key)}
            <li class="note-row">
              {#if editingKey === n.key}
                <div class="note-edit">
                  <textarea
                    class="note-value-input"
                    rows="2"
                    bind:value={editDraft}
                    disabled={notesBusy}
                  ></textarea>
                  <div class="note-actions">
                    <button class="primary" onclick={() => saveEdit(n.key)} disabled={notesBusy}>保存</button>
                    <button onclick={() => (editingKey = null)} disabled={notesBusy}>取消</button>
                  </div>
                </div>
              {:else}
                <span class="note-key mono">{n.key}</span>
                <span class="note-value">{n.value}</span>
                <span class="note-spacer"></span>
                <button
                  class="ghost"
                  onclick={() => {
                    editingKey = n.key;
                    editDraft = typeof n.value === 'string' ? n.value : JSON.stringify(n.value);
                  }}
                  disabled={notesBusy}>编辑</button
                >
                <button
                  class="danger"
                  class:armed={armedKey === n.key}
                  onclick={() => onDelClick(n.key)}
                  disabled={notesBusy}
                >
                  {armedKey === n.key ? '确认删除?' : '删除'}
                </button>
              {/if}
            </li>
          {/each}
        </ul>
      {:else}
        <div class="notes-empty">暂无笔记——写下项目约定、偏好或长期有效的指令,它们在所有会话的召回面以最高权重生效。</div>
      {/if}
      {#if editingKey === null}
        <div class="note-add">
          <input
            class="note-key-input mono"
            placeholder="键(如 coding-style)"
            bind:value={newKey}
            maxlength="96"
            disabled={notesBusy}
          />
          <input
            class="note-value-input"
            placeholder="笔记内容…"
            bind:value={newValue}
            onkeydown={(e) => e.key === 'Enter' && addNote()}
            disabled={notesBusy}
          />
          <button class="primary" onclick={addNote} disabled={notesBusy || !newKey.trim() || !newValue.trim()}>
            添加
          </button>
        </div>
      {/if}
    {:else}
      <div class="notes-empty">未连接会话——笔记写入需挂在当前会话账上(审计可溯源),先选择或新建会话。</div>
    {/if}
  </div>

  <div class="notes browse">
    <div class="notes-head">
      <span class="notes-title">跨会话记忆</span>
      <span class="notes-hint">只读投影(最新版本视图,墓碑已抑制)——写入面仅限上方用户笔记</span>
      <span class="notes-spacer"></span>
      {#each ['stable', 'knowledge_candidates', 'handovers'] as s}
        <button
          class="scope-btn"
          class:active={browseScope === s}
          onclick={() => switchScope(s)}
          disabled={browseLoading || !$sessionId}
        >
          {s === 'stable' ? '长期事实' : s === 'knowledge_candidates' ? '知识候选' : '交接档'}
        </button>
      {/each}
    </div>
    {#if !$sessionId}
      <div class="notes-empty">未连接会话。</div>
    {:else if browseErr}
      <div class="notes-err">{browseErr}</div>
    {:else if browseLoading}
      <div class="notes-empty">正在加载…</div>
    {:else if browseEntries.length === 0}
      <div class="notes-empty">该分区暂无跨会话记忆。</div>
    {:else}
      <ul class="notes-list">
        {#each browseEntries as en (en.path)}
          <li class="note-row">
            <span class="note-key mono browse-path" title={en.path}>{shortPath(en.path)}</span>
            <span class="note-value">{browseSummary(en.value)}</span>
          </li>
        {/each}
      </ul>
    {/if}
  </div>

  <div class="mem-head">
    <span class="notes-title">会话记忆事件</span>
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
  /* ===== 用户笔记区 ===== */
  .notes {
    border-bottom: 1px solid var(--border);
    padding: var(--sp-xs) var(--sp-sm) var(--sp-sm);
    flex-shrink: 0;
    max-height: 45%;
    overflow-y: auto;
  }
  .notes-head {
    display: flex;
    align-items: baseline;
    gap: var(--sp-sm);
    margin-bottom: 6px;
  }
  .notes-title {
    font-weight: 600;
    font-size: var(--fs-xs);
  }
  .notes-hint {
    font-size: 11px;
    color: var(--fg-muted, #888);
  }
  .notes-list {
    list-style: none;
    margin: 0 0 8px;
    padding: 0;
  }
  .note-row {
    display: flex;
    align-items: flex-start;
    gap: var(--sp-sm);
    padding: 4px 0;
    border-bottom: 1px dashed var(--border-dim, #262626);
  }
  .note-key {
    color: var(--accent, #b8860b);
    min-width: 110px;
    max-width: 160px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex-shrink: 0;
  }
  .note-value {
    flex: 1;
    min-width: 0;
    white-space: pre-wrap;
    word-break: break-word;
    font-size: var(--fs-xs);
  }
  .note-spacer {
    flex: 0;
  }
  .note-edit {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .note-actions {
    display: flex;
    gap: 6px;
  }
  .note-add {
    display: flex;
    gap: 6px;
    margin-top: 6px;
  }
  .note-key-input {
    width: 160px;
    flex-shrink: 0;
  }
  .note-value-input {
    flex: 1;
    min-width: 0;
  }
  .note-key-input,
  .note-value-input {
    font-size: var(--fs-xs);
    padding: 3px var(--sp-sm);
    background: var(--bg-input, #1c1c1c);
    color: var(--fg, #ddd);
    border: 1px solid var(--border);
    border-radius: 4px;
  }
  .notes button {
    font-size: 11px;
    padding: 2px 10px;
    cursor: pointer;
    background: var(--bg-btn, #2a2a2a);
    color: var(--fg, #ddd);
    border: 1px solid var(--border, #3a3a3a);
    border-radius: 4px;
    white-space: nowrap;
  }
  .notes button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .notes button.primary {
    background: var(--accent, #b8860b);
    border-color: var(--accent, #b8860b);
    color: #111;
    font-weight: 600;
  }
  .notes button.danger.armed {
    background: #8b2020;
    border-color: #c04040;
    color: #fff;
  }
  .notes-err {
    color: #e07070;
    font-size: 11px;
    padding: 2px 0;
  }
  .notes-empty {
    color: var(--fg-muted, #888);
    font-size: var(--fs-xs);
    padding: 4px 0 8px;
  }
  /* 跨会话浏览区 */
  .browse {
    max-height: 38%;
  }
  .notes-spacer {
    flex: 1;
  }
  .scope-btn {
    font-size: 11px;
    padding: 2px 10px;
    cursor: pointer;
    background: var(--bg-btn, #2a2a2a);
    color: var(--fg, #ddd);
    border: 1px solid var(--border, #3a3a3a);
    border-radius: 4px;
  }
  .scope-btn.active {
    background: var(--accent, #b8860b);
    border-color: var(--accent, #b8860b);
    color: #111;
    font-weight: 600;
  }
  .scope-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .browse-path {
    max-width: 260px;
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
