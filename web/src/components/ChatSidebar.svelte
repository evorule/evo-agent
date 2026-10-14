<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 对话侧栏:与 general agent 的真实流式会话(G16 WS 协议) -->
<script>
  import {
    connStatus,
    sessionId,
    turnActive,
    messages,
    sessions,
    refreshSessions,
    updateMessage,
    artifacts,
    openFile,
    composerMentions,
    activePath,
  } from '../lib/stores.js';
  import { sendMessage, interrupt, newSession, openSession, resumeSession } from '../lib/ws.js';
  import { approveProposal, listResumableRuns } from '../lib/api.js';
  import { onMount, tick } from 'svelte';
  import { collectCandidates, MENTION_GROUP_LABELS } from '../lib/mentions.js';
  import { highlightSegments } from '../lib/fuzzy.js';
  import { registerMentionProviders } from '../lib/mentionProviders.js';

  // Svelte 5:组件用了 $effect 等 rune 即进入 runes 模式,
  // 顶层状态必须 $state() 声明,普通 let 不具备响应性(历史面板/发送按钮会失效)
  let draft = $state('');
  let listEl = $state(null);
  let showHistory = $state(false);
  // 可恢复崩溃会话集合(扫尾端点投影;与历史索引交集后展示「恢复」入口)
  let resumableIds = $state(new Set());

  onMount(() => {
    registerMentionProviders();
    refreshSessions();
    loadResumable();
  });

  async function loadResumable() {
    try {
      const res = await listResumableRuns();
      resumableIds = new Set((res.resumable || []).map((r) => r.session_id));
    } catch {
      // fail-soft:扫尾端点不可用时不展示恢复入口(历史继续对话不受影响)
      resumableIds = new Set();
    }
  }

  function fmtTime(unix) {
    if (!unix) return '';
    const d = new Date(unix * 1000);
    const pad = (n) => String(n).padStart(2, '0');
    return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }

  function toggleHistory() {
    showHistory = !showHistory;
    if (showHistory) {
      refreshSessions();
      loadResumable();
    }
  }

  // 恢复前确认(提示条):挂起审批不恢复的明示是恢复入口的硬性文案
  const RESUME_DISCLOSURE =
    'agent 将加载会话历史并续完中断的轮次;挂起的人工审批不会被恢复,须重走审批。';
  function askResume(sid) {
    const target = sid ? `会话 ${sid}` : '当前会话';
    if (!confirm(`恢复${target}?${RESUME_DISCLOSURE}`)) return;
    resumeSession(sid);
  }

  // 消息变化时滚动到底部
  $effect(() => {
    $messages;
    if (listEl) {
      requestAnimationFrame(() => listEl.scrollTo({ top: listEl.scrollHeight }));
    }
  });

  // ---- @上下文 mention(B13)----
  // 触发:光标前缀形如 (^|\s)@<query>,300ms 防抖后汇总 provider 候选;
  // 键盘:↑↓ 导航 / Enter 选中 / Esc 关闭;选中即插 @token + 登记元数据(chip)

  let draftEl = $state(null);
  let popupEl = $state(null);
  // 弹层状态:{query, insertAt, groups:[{type,label,items:[{...,idx}]}], truncated, sel}
  let mp = $state(null);
  let mpTimer = null;
  let mpSeq = 0;

  const flatCandidates = $derived(mp ? mp.groups.flatMap((g) => g.items) : []);

  // 选中行滚入可视区
  $effect(() => {
    if (!mp) return;
    mp.sel;
    popupEl?.querySelector('.m-row.sel')?.scrollIntoView({ block: 'nearest' });
  });

  function detectMention() {
    const el = draftEl;
    if (!el) return;
    const pos = el.selectionStart ?? draft.length;
    const m = draft.slice(0, pos).match(/(^|\s)@([^\s@]*)$/);
    if (!m) {
      closeMention();
      return;
    }
    openMention(m[2], pos - m[2].length - 1);
  }

  function openMention(query, insertAt) {
    const seq = ++mpSeq;
    if (mpTimer) clearTimeout(mpTimer);
    mpTimer = setTimeout(async () => {
      try {
        const res = await collectCandidates(query, {});
        if (seq !== mpSeq) return; // 过期响应丢弃
        const groups = withQuickItems(res.groups);
        let idx = 0;
        for (const g of groups) g.items = g.items.map((it) => ({ ...it, idx: idx++ }));
        mp = { query, insertAt, groups, truncated: res.truncated, sel: 0 };
      } catch {
        mp = null;
      }
    }, 300);
  }

  function closeMention() {
    if (mpTimer) {
      clearTimeout(mpTimer);
      mpTimer = null;
    }
    mpSeq++;
    mp = null;
  }

  // 空查询快捷项:当前文件(选区/终端快捷项随对应 provider 接线)
  function withQuickItems(groups) {
    const ap = $activePath;
    if (!ap) return groups;
    const item = {
      type: 'file',
      ref: ap,
      label: ap.split('/').pop() || ap,
      detail: `${ap}(当前文件)`,
      positions: [],
    };
    const g0 = groups.find((g) => g.type === 'file');
    if (g0) g0.items.unshift(item);
    else groups.unshift({ type: 'file', label: '文件', items: [item] });
    return groups;
  }

  async function pickCandidate(c) {
    if (!mp) return;
    const token = `@${c.label}`;
    const start = mp.insertAt;
    const cursor = draftEl?.selectionStart ?? draft.length;
    draft = draft.slice(0, start) + token + ' ' + draft.slice(cursor);
    composerMentions.update((arr) => [...arr, { type: c.type, ref: c.ref, insertAt: start, token }]);
    closeMention();
    const caret = start + token.length + 1;
    await tick();
    if (draftEl) {
      draftEl.focus();
      draftEl.selectionStart = draftEl.selectionEnd = caret;
    }
  }

  // chip 删除:同步移除 draft 中的 @token(insertAt 漂移时全局找首处兜底)
  function removeMention(i) {
    const m = $composerMentions[i];
    if (!m) return;
    let d = draft;
    if (m.token) {
      if (d.startsWith(m.token, m.insertAt)) {
        d = d.slice(0, m.insertAt) + d.slice(m.insertAt + m.token.length);
      } else {
        const at = d.indexOf(m.token);
        if (at >= 0) d = d.slice(0, at) + d.slice(at + m.token.length);
      }
    }
    draft = d;
    composerMentions.update((arr) => arr.filter((_, idx) => idx !== i));
  }

  function submit() {
    const text = draft.trim();
    if (!text || $turnActive) return;
    draft = '';
    closeMention();
    composerMentions.set([]);
    sendMessage(text);
  }

  function onKeydown(e) {
    if (mp) {
      const n = flatCandidates.length;
      if (e.key === 'ArrowDown' && n > 0) {
        e.preventDefault();
        mp.sel = (mp.sel + 1) % n;
        return;
      }
      if (e.key === 'ArrowUp' && n > 0) {
        e.preventDefault();
        mp.sel = (mp.sel - 1 + n) % n;
        return;
      }
      if (e.key === 'Enter') {
        e.preventDefault();
        if (n > 0) pickCandidate(flatCandidates[mp.sel]);
        else closeMention();
        return;
      }
      if (e.key === 'Escape') {
        e.preventDefault();
        closeMention();
        return;
      }
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  }

  // 治理叠加:审批决定走既有 G8 /approve 通道(60s 超时自动拒绝)
  function decideApproval(m, approved) {
    if (!m.proposalId || m.decision === 'sending' || m.decision === 'sent') return;
    if (!$sessionId) {
      updateMessage(m.id, { decision: 'error', errText: '会话未建立,无法提交审批' });
      return;
    }
    updateMessage(m.id, { decision: 'sending', errText: null });
    approveProposal('general', $sessionId, approved, m.proposalId)
      .then(() => updateMessage(m.id, { decision: 'sent', sentApproved: approved }))
      .catch((err) => {
        const status = err?.status;
        const text =
          status === 404
            ? '审批已失效(60 秒超时自动拒绝,或该会话无待审批项)'
            : status === 400
              ? `审批被拒:${String(err?.message || err)}`
              : `提交失败:${String(err?.message || err)}`;
        updateMessage(m.id, { decision: 'error', errText: text });
      });
  }
</script>

<aside class="chat" data-zone="chat">
  <div class="chat-header">
    <span class="title">对话</span>
    <div class="header-actions">
      <button class="new-btn" class:active={showHistory} onclick={toggleHistory} title="历史会话">历史</button>
      {#if $sessionId}
        <button class="new-btn" onclick={() => askResume()} title="恢复当前崩溃会话(加载历史并续完中断轮)">恢复</button>
      {/if}
      <button class="new-btn" onclick={newSession} title="断开并新建会话">新建会话</button>
    </div>
  </div>

  {#if showHistory}
    <div class="session-list">
      {#if $sessions.length === 0}
        <div class="session-empty">暂无历史会话</div>
      {:else}
        {#each $sessions as s (s.session_id)}
          <div class="session-row">
            <button
              class="session-item"
              class:current={s.session_id === $sessionId}
              onclick={() => openSession(s.session_id)}
              title={s.session_id}
            >
              <span class="s-title">{s.title || '(无标题)'}</span>
              <span class="s-time mono">{fmtTime(s.last_active)}</span>
            </button>
            {#if resumableIds.has(s.session_id)}
              <button
                class="resume-btn"
                onclick={() => askResume(s.session_id)}
                title="恢复崩溃会话(挂起审批不恢复,须重走审批)"
              >恢复</button>
            {/if}
          </div>
        {/each}
      {/if}
    </div>
  {/if}

  <div class="msg-list" bind:this={listEl}>
    {#if $messages.length === 0}
      <div class="empty">
        <div class="empty-title">开始与 agent 对话</div>
        <div class="empty-desc">
          {$connStatus === 'online'
            ? '直接输入任务,agent 会调用工具并流式回复。'
            : $connStatus === 'connecting'
              ? '正在建立连接…'
              : '连接已断开,请确认 evo-agent serve 正在运行。'}
        </div>
      </div>
    {/if}
    {#each $messages as m (m.id)}
      {#if m.kind === 'user'}
        <div class="row user"><div class="bubble user-bubble">{m.text}</div></div>
      {:else if m.kind === 'assistant'}
        <div class="row">
          <div class="bubble assistant-bubble">
            {m.text}<span class="cursor" class:on={m.streaming}></span>
          </div>
        </div>
      {:else if m.kind === 'tool'}
        <div class="tool-card" class:running={m.running}>
          <span class="tool-name mono">{m.name}</span>
          <span class="tool-state">{m.running ? '执行中…' : '完成'}</span>
          {#if m.result !== undefined}
            <pre class="tool-payload mono">{m.result}</pre>
          {/if}
        </div>
      {:else if m.kind === 'info'}
        <div class="row center"><span class="sys mono">{m.text}</span></div>
      {:else if m.kind === 'error'}
        <div class="row"><div class="bubble error-bubble">{m.text}</div></div>
      {:else if m.kind === 'artifact'}
        <div class="artifact-card">
          <div class="ac-head">
            <span class="ac-label">agent 产物</span>
            <span
              class="ac-state"
              class:final={$artifacts.some((a) => a.path === m.path && a.finalizedAt)}
            >
              {$artifacts.some((a) => a.path === m.path && a.finalizedAt) ? '已定稿' : '草稿'}
            </span>
          </div>
          <div class="ac-path mono" title={m.path}>{m.path}({m.bytes} B)</div>
          <button class="ac-open" onclick={() => openFile(m.path)}>在编辑器打开</button>
        </div>
      {:else if m.kind === 'approval'}
        <div class="approval-card">
          <div class="ap-head">需要审批 · {m.toolName}</div>
          <pre class="ap-cmd mono">{m.command}</pre>
          {#if m.alternative}
            <div class="ap-alt">替代方案:{m.alternative}</div>
          {/if}
          <div class="ap-foot">风险等级:{m.risk}</div>
          {#if !m.proposalId}
            <div class="ap-err">缺少提案 ID,无法在工作台审批(请到会话台处理)</div>
          {:else if m.decision === 'sending'}
            <div class="ap-foot">审批提交中…</div>
          {:else if m.decision === 'sent'}
            <div class="ap-ok">已提交{m.sentApproved ? '批准' : '拒绝'},等待 agent 执行结果…</div>
          {:else}
            <div class="ap-actions">
              <button class="ap-btn approve" onclick={() => decideApproval(m, true)}>批准</button>
              <button class="ap-btn reject" onclick={() => decideApproval(m, false)}>拒绝</button>
            </div>
            {#if m.decision === 'error' && m.errText}
              <div class="ap-err">{m.errText}</div>
            {/if}
          {/if}
        </div>
      {/if}
    {/each}
  </div>

  <div class="composer">
    {#if mp}
      <div class="mention-pop" bind:this={popupEl}>
        {#each mp.groups as g (g.type)}
          <div class="m-label">{g.label}</div>
          {#each g.items as c (c.idx)}
            <button
              class="m-row"
              class:sel={mp.sel === c.idx}
              onmousedown={(e) => e.preventDefault()}
              onclick={() => pickCandidate(c)}
            >
              <span class="m-path mono">
                {#each highlightSegments(c.detail || c.ref, c.positions) as seg}
                  <span class:hit={seg.hit}>{seg.text}</span>
                {/each}
              </span>
              <span class="m-kind">{MENTION_GROUP_LABELS[c.type] || c.type}</span>
            </button>
          {/each}
        {/each}
        {#if flatCandidates.length === 0}
          <div class="m-empty">无匹配候选</div>
        {/if}
        {#if mp.truncated}
          <div class="m-more">候选较多,仅显示前 50——继续输入可缩小范围</div>
        {/if}
      </div>
    {/if}
    {#if $composerMentions.length > 0}
      <div class="chip-tray">
        {#each $composerMentions as cm, i}
          <span class="chip">
            <span class="chip-kind">{MENTION_GROUP_LABELS[cm.type] || cm.type}</span>
            <span class="chip-ref mono" title={cm.ref}>{cm.ref}</span>
            <button class="chip-x" title="移除该引用" onclick={() => removeMention(i)}>×</button>
          </span>
        {/each}
      </div>
    {/if}
    <textarea
      class="draft"
      rows="2"
      placeholder={$turnActive ? 'agent 执行中…(可中断)' : '输入任务,@ 引用文件/上下文,Enter 发送'}
      bind:value={draft}
      bind:this={draftEl}
      oninput={detectMention}
      onkeydown={onKeydown}
      onblur={closeMention}
      disabled={$connStatus !== 'online'}
    ></textarea>
    <div class="composer-actions">
      {#if $turnActive}
        <button class="btn danger" onclick={interrupt}>中断</button>
      {:else}
        <button class="btn primary" onclick={submit} disabled={!draft.trim() || $connStatus !== 'online'}>发送</button>
      {/if}
    </div>
  </div>
</aside>

<style>
  .chat {
    width: 380px;
    flex-shrink: 0;
    background: var(--sidebar-bg);
    border-left: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .chat-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--sp-sm) var(--sp-md);
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
    flex-shrink: 0;
  }
  .title {
    font-size: 11px;
    font-weight: var(--fw-sb);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: rgba(255, 255, 255, 0.4);
  }
  .new-btn {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    padding: 3px var(--sp-sm);
    border-radius: var(--r-sm);
    border: 1px solid var(--border);
  }
  .new-btn:hover {
    color: var(--text-primary);
    background: var(--sidebar-hover);
  }
  .new-btn.active {
    color: var(--brand);
    border-color: var(--brand);
  }
  .header-actions {
    display: flex;
    gap: var(--sp-xs);
  }
  .session-list {
    flex-shrink: 0;
    max-height: 200px;
    overflow-y: auto;
    border-bottom: 1px solid var(--border);
    padding: var(--sp-xs);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .session-empty {
    padding: var(--sp-sm);
    font-size: var(--fs-xs);
    color: var(--text-muted);
    text-align: center;
  }
  .session-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--sp-sm);
    width: 100%;
    padding: 6px var(--sp-sm);
    border: none;
    border-radius: var(--r-sm);
    background: transparent;
    color: var(--sidebar-text);
    cursor: pointer;
    text-align: left;
  }
  .session-item:hover {
    background: var(--sidebar-hover);
  }
  .session-item.current {
    background: rgba(29, 99, 237, 0.18);
    box-shadow: inset 2px 0 0 var(--brand);
  }
  .session-row {
    display: flex;
    align-items: stretch;
    gap: 2px;
  }
  .session-row .session-item {
    flex: 1;
    min-width: 0;
  }
  .resume-btn {
    flex-shrink: 0;
    font-size: var(--fs-xs);
    color: var(--brand);
    padding: 0 var(--sp-sm);
    border-radius: var(--r-sm);
    border: 1px solid var(--border);
    cursor: pointer;
  }
  .resume-btn:hover {
    border-color: var(--brand);
    background: var(--sidebar-hover);
  }
  .s-title {
    font-size: var(--fs-xs);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
  }
  .s-time {
    font-size: 10px;
    color: var(--text-muted);
    flex-shrink: 0;
  }
  .msg-list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: var(--sp-md);
    display: flex;
    flex-direction: column;
    gap: var(--sp-sm);
  }
  .empty {
    text-align: center;
    padding: var(--sp-xl) var(--sp-md);
    color: var(--text-secondary);
  }
  .empty-title {
    font-size: var(--fs-base);
    font-weight: var(--fw-sb);
    color: var(--text-primary);
    margin-bottom: var(--sp-xs);
  }
  .empty-desc {
    font-size: var(--fs-xs);
  }
  .row {
    display: flex;
    justify-content: flex-start;
  }
  .row.user {
    justify-content: flex-end;
  }
  .row.center {
    justify-content: center;
  }
  .bubble {
    max-width: 88%;
    padding: var(--sp-sm) var(--sp-md);
    border-radius: var(--r-lg);
    font-size: var(--fs-sm);
    line-height: var(--lh-normal);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .user-bubble {
    background: var(--brand);
    color: #fff;
  }
  .assistant-bubble {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .error-bubble {
    background: var(--danger-bg);
    color: var(--danger);
  }
  .cursor {
    display: inline-block;
    width: 7px;
    height: 14px;
    margin-left: 2px;
    vertical-align: text-bottom;
    background: var(--brand-ocean);
    opacity: 0;
  }
  .cursor.on {
    opacity: 1;
    animation: blink 1s step-end infinite;
  }
  @keyframes blink {
    50% {
      opacity: 0;
    }
  }
  .tool-card {
    border: 1px solid var(--border);
    border-radius: var(--r-md);
    background: var(--bg-card);
    padding: var(--sp-sm) var(--sp-sm);
    font-size: var(--fs-xs);
  }
  .tool-card.running {
    border-color: var(--brand);
  }
  .tool-name {
    color: var(--brand-ocean);
    font-weight: var(--fw-sb);
  }
  .tool-state {
    margin-left: var(--sp-sm);
    color: var(--text-muted);
  }
  .tool-payload {
    margin-top: var(--sp-xs);
    padding: var(--sp-xs) var(--sp-sm);
    background: var(--bg-page);
    border-radius: var(--r-sm);
    color: var(--text-secondary);
    max-height: 120px;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-all;
  }
  .sys {
    font-size: 11px;
    color: var(--text-muted);
  }
  .artifact-card {
    border: 1px solid var(--border);
    border-left: 3px solid var(--brand);
    border-radius: 6px;
    padding: var(--sp-sm) var(--sp-md);
    margin: var(--sp-xs) 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
    background: var(--bg-card);
  }
  .ac-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .ac-label {
    font-size: var(--fs-xs);
    font-weight: var(--fw-med);
    color: var(--brand);
  }
  .ac-state {
    font-size: var(--fs-xs);
    color: var(--text-muted);
  }
  .ac-state.final {
    color: var(--ok, #4ade80);
  }
  .ac-path {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ac-open {
    align-self: flex-start;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-xs);
    padding: 2px 10px;
    border-radius: 4px;
    cursor: pointer;
  }
  .ac-open:hover {
    color: var(--text-primary);
    border-color: var(--brand);
  }
  .approval-card {
    border: 1px solid var(--warning);
    border-radius: var(--r-md);
    background: var(--warning-bg);
    padding: var(--sp-sm);
    font-size: var(--fs-xs);
  }
  .ap-head {
    font-weight: var(--fw-sb);
    color: var(--warning);
  }
  .ap-cmd {
    margin: var(--sp-xs) 0;
    padding: var(--sp-xs) var(--sp-sm);
    background: var(--bg-page);
    border-radius: var(--r-sm);
    color: var(--text-secondary);
    white-space: pre-wrap;
    word-break: break-all;
  }
  .ap-foot {
    color: var(--text-muted);
  }
  .ap-alt {
    color: var(--text-secondary);
    margin-bottom: var(--sp-xs);
  }
  .ap-actions {
    display: flex;
    gap: var(--sp-sm);
    margin-top: var(--sp-xs);
  }
  .ap-btn {
    height: 26px;
    padding: 0 var(--sp-md);
    font-size: var(--fs-xs);
    font-weight: var(--fw-med);
    border-radius: var(--r-sm);
  }
  .ap-btn.approve {
    background: var(--brand);
    color: #fff;
  }
  .ap-btn.approve:hover {
    background: var(--brand-hover);
  }
  .ap-btn.reject {
    background: transparent;
    color: var(--danger);
    border: 1px solid var(--danger);
  }
  .ap-btn.reject:hover {
    background: var(--danger-bg);
  }
  .ap-ok {
    margin-top: var(--sp-xs);
    color: var(--success);
  }
  .ap-err {
    margin-top: var(--sp-xs);
    color: var(--danger);
  }
  .composer {
    position: relative;
    flex-shrink: 0;
    border-top: 1px solid rgba(255, 255, 255, 0.08);
    padding: var(--sp-sm);
    display: flex;
    flex-direction: column;
    gap: var(--sp-sm);
  }
  .mention-pop {
    position: absolute;
    bottom: calc(100% - 4px);
    left: var(--sp-sm);
    right: var(--sp-sm);
    max-height: 260px;
    overflow-y: auto;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--r-md);
    padding: var(--sp-xs);
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.45);
    z-index: 30;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .m-label {
    font-size: 10px;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 4px var(--sp-sm) 2px;
  }
  .m-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--sp-sm);
    width: 100%;
    padding: 4px var(--sp-sm);
    border: none;
    background: transparent;
    color: var(--sidebar-text);
    cursor: pointer;
    text-align: left;
    border-radius: var(--r-sm);
    font-size: var(--fs-xs);
  }
  .m-row:hover {
    background: var(--sidebar-hover);
  }
  .m-row.sel {
    background: rgba(29, 99, 237, 0.18);
    box-shadow: inset 2px 0 0 var(--brand);
  }
  .m-path {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .m-path .hit {
    color: var(--brand);
    font-weight: var(--fw-sb);
  }
  .m-kind {
    flex-shrink: 0;
    font-size: 10px;
    color: var(--text-muted);
  }
  .m-empty {
    padding: var(--sp-sm);
    text-align: center;
    color: var(--text-muted);
    font-size: var(--fs-xs);
  }
  .m-more {
    padding: 4px var(--sp-sm);
    color: var(--text-muted);
    font-size: 10px;
  }
  .chip-tray {
    display: flex;
    flex-wrap: wrap;
    gap: var(--sp-xs);
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    border-radius: 999px;
    padding: 2px 4px 2px 8px;
    font-size: var(--fs-xs);
    max-width: 100%;
  }
  .chip-kind {
    color: var(--brand);
    font-size: 10px;
    flex-shrink: 0;
  }
  .chip-ref {
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 180px;
  }
  .chip-x {
    border: none;
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    padding: 0 4px;
    font-size: 12px;
    line-height: 1;
  }
  .chip-x:hover {
    color: var(--danger);
  }
  .draft {
    width: 100%;
    resize: none;
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: var(--fs-sm);
    padding: var(--sp-sm);
    outline: none;
    transition: border-color var(--tr-fast);
  }
  .draft:focus {
    border-color: var(--brand);
  }
  .draft::placeholder {
    color: var(--text-muted);
  }
  .composer-actions {
    display: flex;
    justify-content: flex-end;
  }
  .btn {
    height: 30px;
    padding: 0 var(--sp-md);
    font-size: var(--fs-sm);
    font-weight: var(--fw-med);
    border-radius: var(--r-sm);
  }
  .btn.primary {
    background: var(--brand);
    color: #fff;
  }
  .btn.primary:hover {
    background: var(--brand-hover);
  }
  .btn.primary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .btn.danger {
    background: var(--danger);
    color: #fff;
  }
  .btn.danger:hover {
    filter: brightness(0.9);
  }
</style>
