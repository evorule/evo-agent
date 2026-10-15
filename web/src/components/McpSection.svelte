<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- B16 设置面板 MCP 分区(C-2a):server 卡片列表(状态点三色/启停开关/重启钮)
     + 工具树(注册名/描述/alwaysAllow 勾选)+ JSON 配置编辑(env 不回显,
     省略字段 = 保留服务端现值)。未启用态(503)提示条,同 /api/index/* 惯例。 -->
<script>
  import { onMount } from 'svelte';
  import {
    mcpState,
    loadMcp,
    toggleServerDisabled,
    restartServer,
    toggleToolAllow,
    applyServersText,
    serversToText,
    statusKind,
    statusLabel,
    badgeCounts,
  } from '../lib/mcp.js';

  let editorOpen = false;
  let draft = '';
  let draftError = '';
  /** 动作进行中标记(按 server 名;该卡按钮禁用) */
  let busy = {};

  $: state = $mcpState;
  $: badge = badgeCounts(state.servers);

  function openEditor() {
    draft = serversToText(state.servers);
    draftError = '';
    editorOpen = true;
  }

  async function applyEditor() {
    const msg = await applyServersText(draft);
    if (msg) {
      draftError = msg;
      return;
    }
    draftError = '';
    editorOpen = false;
  }

  async function onToggle(s, ev) {
    const el = ev.target;
    busy = { ...busy, [s.name]: true };
    const ok = await toggleServerDisabled(s.name);
    if (!ok) el.checked = !el.checked; // 回滚到 store 权威值
    busy = { ...busy, [s.name]: false };
  }

  async function onRestart(s) {
    busy = { ...busy, [s.name]: true };
    await restartServer(s.name);
    busy = { ...busy, [s.name]: false };
  }

  async function onAllow(s, tool, ev) {
    const el = ev.target;
    busy = { ...busy, [s.name]: true };
    const ok = await toggleToolAllow(s.name, tool.server_tool);
    if (!ok) el.checked = !el.checked; // 回滚到 store 权威值
    busy = { ...busy, [s.name]: false };
  }

  onMount(() => {
    loadMcp();
  });
</script>

<section class="cat mcp">
  <h3 class="cat-title">MCP 服务</h3>

  {#if state.enabled === false}
    <div class="mcp-off">
      未启用:serve 启动时未注入 MCP 管理器(无 mcp 配置或 agentTools.mcp=false)。
      在启动配置中启用后重启服务,即可在此管理 MCP server 与工具。
    </div>
  {:else}
    {#if state.error}<div class="mcp-err">{state.error}</div>{/if}

    <div class="mcp-toolbar">
      <span class="badge">
        共 {badge.total} · 已连接 {badge.connected} · 失败 {badge.failed} · 已停用 {badge.disabled}
        · 工具 {badge.tools}
      </span>
      <span class="spacer"></span>
      <button class="btn" onclick={() => loadMcp()} disabled={state.loading}>刷新</button>
      <button class="btn primary" onclick={openEditor} disabled={state.loading}>编辑配置</button>
    </div>

    {#if state.loading && state.servers.length === 0}
      <div class="mcp-empty">加载中…</div>
    {:else if state.servers.length === 0}
      <div class="mcp-empty">尚无 MCP server 配置;点击「编辑配置」添加,保存即写回并重载生效。</div>
    {/if}

    {#each state.servers as s (s.name)}
      <div class="mcp-card" class:off={s.disabled}>
        <div class="card-head">
          <span class="dot {statusKind(s.status)}" title={statusLabel(s.status)}></span>
          <span class="card-name mono">{s.name}</span>
          <span class="card-status">{statusLabel(s.status)}</span>
          <span class="spacer"></span>
          {#if !s.disabled}
            <button class="btn" onclick={() => onRestart(s)} disabled={busy[s.name] || state.loading}>
              {busy[s.name] ? '重启中…' : '重启'}
            </button>
          {/if}
          <label class="chk" title="停用后不连接不注册,配置保留">
            <input
              type="checkbox"
              checked={!s.disabled}
              disabled={busy[s.name] || state.loading}
              onchange={(e) => onToggle(s, e)}
            />
            <span>启用</span>
          </label>
        </div>
        <div class="card-cmd mono">
          {s.command}{(s.args ?? []).length ? ` ${s.args.join(' ')}` : ''} · 超时 {s.timeout_secs}s
        </div>
        {#if (s.env_keys ?? []).length}
          <div class="card-env">env:{s.env_keys.join(', ')}(值不回显)</div>
        {/if}
        {#if s.error}
          <div class="card-fail">{s.error}</div>
        {/if}
        {#if (s.tools ?? []).length}
          <details class="tools">
            <summary>工具({s.tools.length})</summary>
            <div class="tool-list">
              {#each s.tools as t (t.name)}
                <div class="tool-row">
                  <label class="chk">
                    <input
                      type="checkbox"
                      checked={t.allowed}
                      disabled={busy[s.name] || state.loading}
                      onchange={(e) => onAllow(s, t, e)}
                    />
                    <span class="mono tool-name">{t.server_tool}</span>
                  </label>
                  <span class="tool-desc">{t.description}</span>
                </div>
              {/each}
            </div>
            <div class="tool-note">勾选 = 该工具免审批直跑(alwaysAllow);不勾 = 调用时走审批确认。</div>
          </details>
        {:else if !s.disabled && s.status === 'connected'}
          <div class="tool-note">该 server 未暴露任何工具。</div>
        {/if}
      </div>
    {/each}

    {#if editorOpen}
      <div class="editor">
        <div class="editor-hint">
          全量写回并重载全部 server。env 值不回显(防泄露):省略 env 字段 = 保留服务端现有值,
          显式给出(含空对象)= 覆盖。
        </div>
        <textarea class="editor-json mono" rows="12" bind:value={draft} spellcheck="false"></textarea>
        {#if draftError}<div class="mcp-err">{draftError}</div>{/if}
        <div class="editor-actions">
          <button class="btn primary" onclick={applyEditor} disabled={state.loading}>应用</button>
          <button class="btn" onclick={() => (editorOpen = false)}>取消</button>
        </div>
      </div>
    {/if}
  {/if}
</section>

<style>
  .mcp-off {
    font-size: var(--fs-xs);
    color: var(--text-muted);
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: var(--sp-sm);
  }
  .mcp-toolbar {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    margin-bottom: var(--sp-sm);
  }
  .spacer {
    flex: 1;
  }
  .badge {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
  }
  .mcp-empty {
    font-size: var(--fs-xs);
    color: var(--text-muted);
    padding: var(--sp-sm) 0;
  }
  .mcp-err {
    font-size: var(--fs-xs);
    color: var(--danger);
    background: var(--danger-bg, rgba(231, 76, 60, 0.12));
    border: 1px solid var(--danger);
    border-radius: var(--r-sm);
    padding: var(--sp-xs) var(--sp-sm);
    margin-bottom: var(--sp-sm);
    word-break: break-all;
  }
  .mcp-card {
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: var(--sp-sm);
    margin-bottom: var(--sp-sm);
  }
  .mcp-card:hover {
    background: var(--bg-hover);
  }
  .mcp-card.off {
    opacity: 0.6;
  }
  .card-head {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .dot.success {
    background: var(--success);
  }
  .dot.warning {
    background: var(--warning);
  }
  .dot.danger {
    background: var(--danger);
  }
  .dot.muted {
    background: var(--text-muted);
  }
  .card-name {
    font-size: var(--fs-sm);
    color: var(--text-primary);
    font-weight: var(--fw-sb);
  }
  .card-status {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
  }
  .card-cmd {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    margin-top: var(--sp-xs);
    word-break: break-all;
  }
  .card-env {
    font-size: 10px;
    color: var(--text-muted);
    margin-top: 2px;
  }
  .card-fail {
    font-size: var(--fs-xs);
    color: var(--danger);
    margin-top: var(--sp-xs);
    word-break: break-all;
  }
  .tools {
    margin-top: var(--sp-xs);
  }
  .tools summary {
    font-size: var(--fs-xs);
    color: var(--brand);
    cursor: pointer;
    user-select: none;
  }
  .tool-list {
    margin-top: var(--sp-xs);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .tool-row {
    display: flex;
    align-items: baseline;
    gap: var(--sp-md);
    padding-left: var(--sp-md);
  }
  .tool-name {
    font-size: var(--fs-xs);
    color: var(--text-primary);
  }
  .tool-desc {
    font-size: var(--fs-xs);
    color: var(--text-muted);
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .tool-note {
    font-size: 10px;
    color: var(--text-muted);
    margin-top: var(--sp-xs);
  }
  .chk {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    cursor: pointer;
    flex-shrink: 0;
  }
  .editor {
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: var(--sp-sm);
    margin-top: var(--sp-sm);
  }
  .editor-hint {
    font-size: 10px;
    color: var(--text-muted);
    margin-bottom: var(--sp-xs);
  }
  .editor-json {
    width: 100%;
    box-sizing: border-box;
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: var(--fs-xs);
    padding: var(--sp-xs);
    outline: none;
    resize: vertical;
  }
  .editor-json:focus {
    border-color: var(--brand);
  }
  .editor-actions {
    display: flex;
    gap: var(--sp-xs);
    margin-top: var(--sp-xs);
  }
  .btn {
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-xs);
    padding: 3px 12px;
    border-radius: var(--r-sm);
    cursor: pointer;
    flex-shrink: 0;
  }
  .btn:hover:not(:disabled) {
    color: var(--text-primary);
    border-color: var(--brand);
  }
  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .btn.primary {
    background: var(--brand);
    border-color: var(--brand);
    color: #fff;
  }
  .btn.primary:hover:not(:disabled) {
    background: var(--brand-hover);
  }
</style>
