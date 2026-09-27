<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 问题面板:两级树(文件组 → 诊断行:severity 图标+消息+source(code))。
     数据全部来自诊断单源 problems store(markerService 权威聚合,零双写);
     severity 三开关(错误/警告/提示+建议)+ 文本过滤(消息/来源/代码)+
     两类空态(无诊断 vs 过滤无结果)。过滤与分组展开态放模块级 store——
     面板折叠销毁组件后状态仍保持。行点击/回车经 openFile 定位信号跳转。 -->
<script context="module">
  import { writable } from 'svelte/store';
  // 模块级 UI 状态(跨组件销毁保持)
  const filters = writable({ errors: true, warnings: true, infos: true, text: '' });
  const collapsed = writable(new Set());
</script>

<script>
  import { openFile } from '../lib/stores.js';
  import { problems, markerToReveal, markerId } from '../lib/diagnostics.js';

  function toggleFilter(key) {
    filters.update((f) => ({ ...f, [key]: !f[key] }));
  }

  function toggleGroup(path) {
    collapsed.update((s) => {
      const n = new Set(s);
      if (n.has(path)) n.delete(path);
      else n.add(path);
      return n;
    });
  }

  function onTextInput(e) {
    const v = e.target.value;
    filters.update((f) => ({ ...f, text: v }));
  }

  /** 行跳转:marker 1-based 行列 → openFile 定位参数(col/endCol 0-based) */
  function reveal(path, m) {
    openFile(path, markerToReveal({ marker: m, path }));
  }

  const sevClass = (m) => (m.severity >= 8 ? 'err' : m.severity >= 4 ? 'warn' : m.severity >= 2 ? 'info' : 'hint');
  const sevName = (m) => (m.severity >= 8 ? '错误' : m.severity >= 4 ? '警告' : m.severity >= 2 ? '提示' : '建议');

  $: f = $filters;
  $: sevOk = (m) => (m.severity >= 8 ? f.errors : m.severity >= 4 ? f.warnings : f.infos);
  $: textOk = (m) => {
    const q = f.text.trim().toLowerCase();
    if (!q) return true;
    return (
      String(m.message || '').toLowerCase().includes(q) ||
      String(m.source || '').toLowerCase().includes(q) ||
      String(m.code ?? '').toLowerCase().includes(q)
    );
  };
  $: visibleGroups = $problems.groups
    .map((g) => ({
      path: g.path,
      visible: g.markers.filter((m) => sevOk(m) && textOk(m)),
      total: g.markers.length,
    }))
    .filter((g) => g.visible.length > 0);
  $: totalFiltered = visibleGroups.reduce((n, g) => n + g.visible.length, 0);
</script>

<div class="problems" data-zone="panel">
  <div class="toolbar">
    <button
      class="sev-toggle"
      class:on={f.errors}
      title="显示/隐藏错误"
      onclick={() => toggleFilter('errors')}
    >
      <span class="dot err"></span>错误
    </button>
    <button
      class="sev-toggle"
      class:on={f.warnings}
      title="显示/隐藏警告"
      onclick={() => toggleFilter('warnings')}
    >
      <span class="dot warn"></span>警告
    </button>
    <button
      class="sev-toggle"
      class:on={f.infos}
      title="显示/隐藏提示与建议"
      onclick={() => toggleFilter('infos')}
    >
      <span class="dot info"></span>提示
    </button>
    <input
      class="filter-input"
      type="text"
      placeholder="筛选(消息/来源/代码)"
      value={f.text}
      oninput={onTextInput}
    />
  </div>

  {#if $problems.groups.length === 0}
    <div class="empty">未在工作区检测到问题。打开 JSON / Markdown 文件即自动校验,诊断在此汇总。</div>
  {:else if totalFiltered === 0}
    <div class="empty">无匹配当前过滤条件的问题。</div>
  {:else}
    <div class="tree" role="tree">
      {#each visibleGroups as g (g.path)}
        <div class="group">
          <button
            class="group-head"
            role="treeitem"
            aria-expanded={!$collapsed.has(g.path)}
            title={g.path}
            onclick={() => toggleGroup(g.path)}
          >
            <span class="chev">{$collapsed.has(g.path) ? '▸' : '▾'}</span>
            <span class="path mono">{g.path}</span>
            <span class="badge">{g.visible.length}{g.visible.length !== g.total ? ` / ${g.total}` : ''}</span>
          </button>
          {#if !$collapsed.has(g.path)}
            {#each g.visible as m (markerId({ marker: m, path: g.path }))}
              <div
                class="row"
                role="treeitem"
                tabindex="0"
                title={`${sevName(m)}:${m.message}`}
                onclick={() => reveal(g.path, m)}
                onkeydown={(e) => e.key === 'Enter' && reveal(g.path, m)}
              >
                <span class="sev-icon {sevClass(m)}" aria-label={sevName(m)}>
                  {#if m.severity >= 8}
                    <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
                      <circle cx="12" cy="12" r="9" />
                      <path d="M15 9l-6 6M9 9l6 6" />
                    </svg>
                  {:else if m.severity >= 4}
                    <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
                      <path d="M12 3.5L21.5 20h-19L12 3.5z" />
                      <path d="M12 10v4.5" />
                      <path d="M12 17.6v.2" />
                    </svg>
                  {:else}
                    <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
                      <circle cx="12" cy="12" r="9" />
                      <path d="M12 11v5.5" />
                      <path d="M12 7.4v.2" />
                    </svg>
                  {/if}
                </span>
                <span class="msg">{m.message}</span>
                {#if m.source || m.code != null}
                  <span class="meta mono"
                    >{m.source || ''}{m.source && m.code != null ? ' ' : ''}{m.code != null ? `(${m.code})` : ''}</span
                  >
                {/if}
              </div>
            {/each}
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .problems {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-height: 0;
  }
  .toolbar {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    padding-bottom: var(--sp-xs);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .sev-toggle {
    display: flex;
    align-items: center;
    gap: 5px;
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    padding: 1px 6px;
    border-radius: var(--r-sm);
    border: none;
    background: transparent;
    opacity: 0.55;
  }
  .sev-toggle.on {
    opacity: 1;
  }
  .sev-toggle:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: var(--r-full, 50%);
  }
  .dot.err {
    background: var(--danger);
  }
  .dot.warn {
    background: var(--warning);
  }
  .dot.info {
    background: var(--brand-ocean);
  }
  .filter-input {
    margin-left: auto;
    width: 220px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: var(--fs-xs);
    padding: 2px 8px;
  }
  .filter-input:focus {
    outline: none;
    border-color: var(--brand);
  }
  .empty {
    color: var(--text-muted);
    padding: var(--sp-sm) 0;
  }
  .tree {
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .group-head {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    text-align: left;
    padding: 2px 4px;
    border-radius: var(--r-sm);
    background: transparent;
    border: none;
  }
  .group-head:hover {
    background: var(--bg-hover);
  }
  .chev {
    color: var(--text-muted);
    font-size: 10px;
    flex-shrink: 0;
  }
  .path {
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .badge {
    margin-left: auto;
    flex-shrink: 0;
    font-size: 10px;
    color: var(--text-secondary);
    background: var(--bg-hover);
    border-radius: var(--r-full, 999px);
    padding: 0 8px;
    line-height: 16px;
  }
  .row {
    display: flex;
    align-items: baseline;
    gap: var(--sp-sm);
    padding: 1px 4px 1px 22px;
    line-height: 1.5;
    cursor: pointer;
    border-radius: var(--r-sm);
  }
  .row:hover {
    background: var(--bg-hover);
  }
  .row:focus-visible {
    outline: 1px solid var(--brand);
  }
  .sev-icon {
    flex-shrink: 0;
    align-self: center;
    display: flex;
  }
  .sev-icon.err {
    color: var(--danger);
  }
  .sev-icon.warn {
    color: var(--warning);
  }
  .sev-icon.info {
    color: var(--brand-ocean);
  }
  .sev-icon.hint {
    color: var(--text-muted);
  }
  .msg {
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .meta {
    margin-left: auto;
    flex-shrink: 0;
    color: var(--text-muted);
    font-size: 10px;
  }
</style>
