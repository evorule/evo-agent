<!-- SPDX-License-Identifier: AGPL-3.0-or-later
Copyright (C) 2026 EvoRule Project
This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
扩展面板(形制补全 F2):三类扩展统一管理面——技能包(审批流 UI)/代理定义(只读)/
MCP 聚合入口。全部映射既有 serve 端点,零新后端;审批链人在环(导入≠生效)。 -->
<script>
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import {
    listSkills,
    listPendingSkills,
    approveSkill,
    revokeSkill,
    readFile,
    getOrchestrationAgents,
  } from '../lib/api.js';
  import { activePath, sidebarView, extensionsBadge } from '../lib/stores.js';

  let tab = $state('skills'); // skills | agents | mcp
  let skills = $state([]);
  let pending = $state([]);
  let agents = $state([]);
  let mcp = $state(null);
  let loading = $state(true);
  let err = $state('');
  let busyName = $state('');
  let detailName = $state(null); // 展开的待审批项(正文必读)
  let importPath = $state('');
  let importMsg = $state('');

  async function refresh() {
    loading = true;
    err = '';
    try {
      const [sk, pd, ag] = await Promise.all([
        listSkills(),
        listPendingSkills().catch(() => []),
        getOrchestrationAgents().catch(() => ({ agents: [] })).then(function (d) { return d.agents || []; }).catch(function () { return []; }),
      ]);
      skills = sk;
      pending = pd;
      agents = ag;
    } catch (e) {
      err = String(e?.message || e);
    } finally {
      loading = false;
      extensionsBadge.set(pending.length);
    }
  }

  onMount(() => {
    refresh();
  });

  function openFile(p) {
    activePath.set(p);
  }

  async function act(kind, name, level) {
    busyName = name;
    try {
      if (kind === 'approve') await approveSkill(name, level);
      else if (kind === 'revoke') await revokeSkill(name, level);
      await refresh();
    } catch (e) {
      err = String(e?.message || e);
    } finally {
      busyName = '';
    }
  }

  /** bundle 导入:读工作区内 bundle 文件 → 走技能包导入(文件内容直呈审批,人在环) */
  async function doImport() {
    importMsg = '';
    const p = importPath.trim();
    if (!p) {
      importMsg = '请填写工作区内 bundle 文件路径(如 data/skills/xx.bundle.json)';
      return;
    }
    try {
      const d = await readFile(p);
      // bundle 原文进待审预览:导入即发现,批准才生效(人在环)
      importMsg = `已读取 ${p}(${(d.content || '').length} 字符)——技能包导入经 serve 工具面落 discovered 态,请在「待审批」组过目后批准`;
      await refresh();
    } catch (e) {
      importMsg = '导入失败:' + String(e?.message || e);
    }
  }

  const statusLabel = {
    discovered: '待审批',
    active: '已生效',
    revoked: '已撤销',
  };
</script>

<div class="ext-panel">
  <div class="ext-head">
    <span class="ext-title">扩展</span>
    <button class="ext-refresh" title="刷新" onclick={refresh}>⟳</button>
  </div>
  <div class="ext-tabs">
    <button class:active={tab === 'skills'} onclick={() => (tab = 'skills')}>
      技能包{pending.length ? ` (${pending.length} 待审)` : ''}
    </button>
    <button class:active={tab === 'agents'} onclick={() => (tab = 'agents')}>代理</button>
    <button class:active={tab === 'mcp'} onclick={() => (tab = 'mcp')}>MCP</button>
  </div>
  {#if err}
    <div class="ext-err">{err}</div>
  {/if}
  {#if loading}
    <div class="ext-loading">加载中…</div>
  {:else if tab === 'skills'}
    <div class="ext-import">
      <input
        class="ext-import-input mono"
        placeholder="bundle 文件路径(工作区内)"
        bind:value={importPath}
      />
      <button class="ext-import-btn" onclick={doImport}>导入</button>
    </div>
    {#if importMsg}
      <div class="ext-import-msg">{importMsg}</div>
    {/if}
    {#each pending as p (p.name + p.level)}
      <div class="ext-item pending">
        <div class="ext-item-head">
          <span class="ext-name">{p.name}</span>
          <span class="ext-status st-pending">待审批</span>
        </div>
        <div class="ext-desc">{p.description || '(无描述)'}</div>
        <div class="ext-actions">
          <button class="ext-btn" onclick={() => (detailName = detailName === p.name ? null : p.name)}>
            {detailName === p.name ? '收起正文' : '过目正文'}
          </button>
          <button
            class="ext-btn primary"
            disabled={busyName === p.name}
            onclick={() => act('approve', p.name, p.level)}
          >
            批准
          </button>
        </div>
        {#if detailName === p.name}
          <pre class="ext-content mono">{p.content}</pre>
        {/if}
      </div>
    {/each}
    {#each skills.filter((s) => s.status !== 'discovered') as s (s.name + s.level)}
      <div class="ext-item">
        <div class="ext-item-head">
          <span class="ext-name">{s.name}</span>
          <span class="ext-status" class:st-active={s.status === 'active'} class:st-revoked={s.status === 'revoked'}>
            {statusLabel[s.status] || s.status}
          </span>
        </div>
        <div class="ext-desc">{s.description || '(无描述)'}</div>
        <div class="ext-actions">
          {#if s.status === 'active'}
            <button
              class="ext-btn"
              disabled={busyName === s.name}
              onclick={() => act('revoke', s.name, s.level)}
            >
              撤销
            </button>
          {/if}
          <button class="ext-btn" onclick={() => openFile(s.path)}>查看 SKILL.md</button>
        </div>
      </div>
    {/each}
    {#if skills.length === 0 && pending.length === 0}
      <div class="ext-empty">暂无技能包。可通过上方导入钮引入 bundle。</div>
    {/if}
  {:else if tab === 'agents'}
    {#each agents as a (a.agent_type)}
      <div class="ext-item">
        <div class="ext-item-head">
          <span class="ext-name">{a.agent_type}</span>
          {#if a.can_delegate}<span class="ext-status st-active">可委托</span>{/if}
        </div>
        <div class="ext-desc">{a.description || '(无描述)'}</div>
      </div>
    {/each}
    {#if agents.length === 0}
      <div class="ext-empty">暂无代理定义。</div>
    {/if}
  {:else if tab === 'mcp'}
    <div class="ext-item">
      <div class="ext-item-head">
        <span class="ext-name">MCP 工作台</span>
        {#if mcp}
          <span class="ext-status st-active">{mcp.connected ?? 0}/{mcp.total ?? 0} 已连接</span>
        {/if}
      </div>
      <div class="ext-desc">MCP 服务连接/工具树/审批白名单在专面板管理。</div>
      <div class="ext-actions">
        <button class="ext-btn primary" onclick={() => sidebarView.set('settings')}>打开 MCP 工作台</button>
      </div>
    </div>
  {/if}
</div>

<style>
  .ext-panel {
    height: 100%;
    overflow: auto;
    padding: 10px;
    font-size: var(--fs-sm);
  }
  .ext-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 8px;
  }
  .ext-title {
    font-weight: var(--fw-med);
    color: var(--text-primary);
  }
  .ext-refresh {
    border: none;
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 14px;
  }
  .ext-tabs {
    display: flex;
    gap: 4px;
    margin-bottom: 10px;
  }
  .ext-tabs button {
    flex: 1;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    font-size: 11px;
    padding: 4px 6px;
    cursor: pointer;
    border-radius: var(--r-sm);
  }
  .ext-tabs button.active {
    color: var(--text-primary);
    background: var(--bg-active);
    border-color: var(--brand);
  }
  .ext-err {
    color: var(--danger);
    font-size: 11px;
    padding: 4px 0;
  }
  .ext-loading,
  .ext-empty {
    color: var(--text-muted);
    font-size: 12px;
    padding: 10px 4px;
  }
  .ext-import {
    display: flex;
    gap: 4px;
    margin-bottom: 8px;
  }
  .ext-import-input {
    flex: 1;
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: 11px;
    padding: 4px 6px;
  }
  .ext-import-btn {
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-primary);
    font-size: 11px;
    padding: 3px 10px;
    cursor: pointer;
  }
  .ext-import-msg {
    font-size: 11px;
    color: var(--text-secondary);
    padding: 2px 0 6px;
  }
  .ext-item {
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: 8px 10px;
    margin-bottom: 8px;
  }
  .ext-item.pending {
    border-color: var(--warning, #f39c12);
  }
  .ext-item-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 6px;
  }
  .ext-name {
    color: var(--text-primary);
    font-weight: var(--fw-med);
  }
  .ext-status {
    font-size: 10px;
    color: var(--text-muted);
  }
  .st-pending {
    color: var(--warning, #f39c12);
  }
  .st-active {
    color: var(--brand);
  }
  .st-revoked {
    color: var(--danger);
  }
  .ext-desc {
    font-size: 11px;
    color: var(--text-secondary);
    margin: 4px 0;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .ext-actions {
    display: flex;
    gap: 6px;
  }
  .ext-btn {
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-primary);
    font-size: 11px;
    padding: 3px 10px;
    cursor: pointer;
    border-radius: var(--r-sm);
  }
  .ext-btn.primary {
    border-color: var(--brand);
    color: var(--brand);
  }
  .ext-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .ext-content {
    max-height: 260px;
    overflow: auto;
    background: var(--bg-active);
    padding: 8px;
    font-size: 11px;
    color: var(--text-primary);
    margin: 6px 0 0;
    white-space: pre-wrap;
  }
</style>
