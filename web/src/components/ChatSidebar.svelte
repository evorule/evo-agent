<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 对话侧栏:与 general agent 的真实流式会话(G16 WS 协议) -->
<script>
  import {
    connStatus,
    sessionId,
    turnActive,
    messages,
  } from '../lib/stores.js';
  import { sendMessage, interrupt, newSession } from '../lib/ws.js';

  let draft = '';
  let listEl;

  // 消息变化时滚动到底部
  $effect(() => {
    $messages;
    if (listEl) {
      requestAnimationFrame(() => listEl.scrollTo({ top: listEl.scrollHeight }));
    }
  });

  function submit() {
    const text = draft.trim();
    if (!text || $turnActive) return;
    draft = '';
    sendMessage(text);
  }

  function onKeydown(e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  }
</script>

<aside class="chat">
  <div class="chat-header">
    <span class="title">对话</span>
    <button class="new-btn" onclick={newSession} title="断开并新建会话">新建会话</button>
  </div>

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
      {:else if m.kind === 'approval'}
        <div class="approval-card">
          <div class="ap-head">需要审批 · {m.toolName}</div>
          <pre class="ap-cmd mono">{m.command}</pre>
          <div class="ap-foot">风险等级:{m.risk} · 审批交互在治理叠加阶段接入(超时将自动拒绝)</div>
        </div>
      {/if}
    {/each}
  </div>

  <div class="composer">
    <textarea
      class="draft"
      rows="2"
      placeholder={$turnActive ? 'agent 执行中…(可中断)' : '输入任务,Enter 发送,Shift+Enter 换行'}
      bind:value={draft}
      onkeydown={onKeydown}
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
  .composer {
    flex-shrink: 0;
    border-top: 1px solid rgba(255, 255, 255, 0.08);
    padding: var(--sp-sm);
    display: flex;
    flex-direction: column;
    gap: var(--sp-sm);
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
