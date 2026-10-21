<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 首页工作区卡(批 I 段 3):无打开 tab 时取代欢迎编辑器区。
     工作区选择(目录浏览器 / 最近列表 / 恢复缺省,统一切换入口) +
     审批模式 toggle(严格/自由,联动批 B 两键;红线任何模式强制人工) +
     快速上手指引。零新增后端:全部消费既有 REST/设置通道。 -->
<script>
  import { onMount } from 'svelte';
  import {
    loadRecentWorkspaces,
    workspaceLabel,
    switchWorkspace,
  } from '../lib/workspace.js';
  import { workspaceEpoch } from '../lib/stores.js';
  import {
    APPROVAL_MODE_KEY,
    APPROVAL_EXEMPTION_KEY,
    approvalPresetWrites,
    approvalPresetView,
  } from '../lib/approval.js';
  import { settingsState, setSetting } from '../lib/settings.js';
  import { reconnectCurrent } from '../lib/ws.js';
  import { fetchNotices } from '../lib/api.js';
  import { sortNotices, sliceForDisplay, levelClass } from '../lib/notices.js';
  import { marked } from 'marked';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import DirectoryBrowser from './DirectoryBrowser.svelte';

  let ws = '';
  let recents = [];
  let browserOpen = false;
  let wsErr = '';
  let modeBusy = false;
  let modeErr = '';
  let confirmState = null; // {message, resolve}

  // ---- 公告卡(批 J1 段 2):/api/notices 单向只读发布面 ----
  // fail-soft:拉取失败/空表整卡隐藏(未发布态与现状零差异);
  // body_md 走 marked 渲染,口径同 EditorPane md 预览(内容源=发布者本机文件)。
  let notices = [];
  let noticeErr = '';
  let allNotices = false;

  $: visibleNotices = sliceForDisplay(notices, allNotices);

  function confirmViaDialog(message) {
    return new Promise((resolve) => {
      confirmState = { message, resolve };
    });
  }

  /** 统一切换入口:落库 + 关文件 tab + 纪元自增(树重建由 Explorer 订阅) */
  async function switchTo(v) {
    wsErr = '';
    const r = await switchWorkspace(v, confirmViaDialog);
    if (!r.ok && r.error) wsErr = r.error;
  }

  /** 审批模式 toggle(批 I 双模式 = 批 B 预设组合器):
   *  严格 = manual + 豁免复位;自由 = auto_policy + 豁免复位。
   *  红线豁免不在此开启——「完全访问」仅存在于对话侧栏四档选择器。 */
  async function setWorkbenchMode(m) {
    if (modeBusy) return;
    modeBusy = true;
    modeErr = '';
    try {
      const preset = m === 'strict' ? 'manual' : 'auto';
      for (const [k, v] of approvalPresetWrites(preset)) await setSetting(k, v);
    } catch (e) {
      modeErr = String(e?.message || e);
    } finally {
      modeBusy = false;
    }
  }

  // 当前工作区/最近列表随纪元刷新(任意入口切换后同步)
  let epochSeen = -1;
  $: if ($workspaceEpoch !== epochSeen) {
    epochSeen = $workspaceEpoch;
    ws = localStorage.getItem('evo_workspace') || '';
    recents = loadRecentWorkspaces();
  }

  $: approvalView = approvalPresetView(
    $settingsState.settings[APPROVAL_MODE_KEY],
    $settingsState.settings[APPROVAL_EXEMPTION_KEY],
  );
  $: modeActive = $settingsState.settings[APPROVAL_MODE_KEY] === 'auto_policy' ? 'free' : 'strict';

  function reconnectNow() {
    reconnectCurrent();
  }

  onMount(() => {
    epochSeen = -2; // 强制首次刷新(rune 块已随首帧执行过一次)
    ws = localStorage.getItem('evo_workspace') || '';
    recents = loadRecentWorkspaces();
    // 公告面:异步拉取,失败静默(整卡隐藏)
    fetchNotices()
      .then((r) => {
        notices = sortNotices(r?.notices || []);
        if (r?.error) noticeErr = r.error;
      })
      .catch(() => {});
  });
</script>

<div class="home" data-zone="editor">
  <div class="card">
    <div class="brand">
      <h1>evo-agent 工作台</h1>
      <p class="sub">以标准编程 IDE 为底盘,叠加 evorule 治理能力(agent 会话 / 审批 / 审计 / 规则)。</p>
    </div>

    <section class="sec notices" data-testid="notices-card">
      <div class="sec-head">
        <span class="sec-title">公告</span>
        {#if notices.length > 2}
          <button class="btn ghost sm" onclick={() => (allNotices = !allNotices)}>
            {allNotices ? '收起' : `全部展开(${notices.length})`}
          </button>
        {/if}
      </div>
      {#each visibleNotices as n (n.id)}
        <article class="notice">
          <div class="n-head">
            <span class="badge {levelClass(n.level)}">{n.level}</span>
            {#if n.pinned}<span class="pin" title="置顶">▲</span>{/if}
            <b class="n-title">{n.title}</b>
            <span class="n-date mono">{n.date}</span>
          </div>
          <div class="n-body md">{@html marked.parse(n.body_md || '', { async: false })}</div>
          {#if n.links?.length}
            <div class="n-links">
              {#each n.links as l (l.href)}
                <a class="chip" href={l.href} target="_blank" rel="noopener">{l.label}</a>
              {/each}
            </div>
          {/if}
        </article>
      {/each}
      {#if noticeErr}<p class="hint">{noticeErr}</p>{/if}
    </section>

    <section class="sec">
      <div class="sec-head">
        <span class="sec-title">工作区</span>
        <span class="ws-cur mono" title={ws || 'serve workdir'}>{ws || '缺省(serve workdir)'}</span>
      </div>
      <div class="btn-row">
        <button class="btn" onclick={() => (browserOpen = true)}>浏览目录…</button>
        <button class="btn" disabled={!ws} onclick={() => switchTo('')} title="恢复为 serve workdir 根">
          恢复缺省
        </button>
        <button class="btn ghost" onclick={reconnectNow} title="以当前工作区重建 agent 连接(能力边界生效)">
          立即重连
        </button>
      </div>
      {#if recents.length}
        <div class="recents">
          <span class="hint">最近:</span>
          {#each recents as r (r)}
            <button class="chip mono" title={r} onclick={() => switchTo(r)}>
              {workspaceLabel(r) || r}
            </button>
          {/each}
        </div>
      {/if}
      <p class="hint">
        文件树/编辑/搜索即刻随工作区重锚;agent 能力边界对新连接生效(新建会话/换线/重连)。
      </p>
      {#if wsErr}<p class="err">{wsErr}</p>{/if}
    </section>

    <section class="sec">
      <div class="sec-head">
        <span class="sec-title">审批模式</span>
        <span class="mode-cur" title={approvalView.exempt ? '红线豁免开启(完全访问)' : ''}>
          {approvalView.label}
        </span>
      </div>
      <div class="toggle" role="radiogroup" aria-label="审批模式切换">
        <button
          class="opt"
          class:on={modeActive === 'strict'}
          disabled={modeBusy}
          role="radio"
          aria-checked={modeActive === 'strict'}
          onclick={() => setWorkbenchMode('strict')}
        >
          严格
        </button>
        <button
          class="opt"
          class:on={modeActive === 'free'}
          disabled={modeBusy}
          role="radio"
          aria-checked={modeActive === 'free'}
          onclick={() => setWorkbenchMode('free')}
        >
          自由
        </button>
      </div>
      <p class="hint">
        {modeActive === 'strict'
          ? '严格:每次工具调用先经人工确认,适合不熟悉的项目与高危目录。'
          : '自由:工具调用自动评审判定放行并留痕,适合可信项目的高效迭代。'}
      </p>
      <p class="hint red">任何模式下,红线类操作(删除文件 / 推送远端 / 数据外发 / 修改治理配置文件)始终强制人工确认。</p>
      {#if modeErr}<p class="err">{modeErr}</p>{/if}
    </section>

    <section class="sec guide">
      <div class="sec-title">快速上手</div>
      <ul>
        <li>左侧文件树浏览当前工作区,点击文件即可编辑;Ctrl+S 保存落盘(与 agent 写文件同一安全通道)</li>
        <li>右侧对话侧栏与 general agent 对话;agent 写文件后产物自动打开,可切差异视图对比后定稿</li>
        <li>切换工作区后,文件面即刻以新工作区为根;agent 能力边界在「立即重连」后生效</li>
      </ul>
    </section>
  </div>
</div>

{#if browserOpen}
  <DirectoryBrowser
    title="选择工作区目录"
    initialPath={ws}
    on:select={(e) => {
      browserOpen = false;
      switchTo(e.detail.path);
    }}
    on:close={() => (browserOpen = false)}
  />
{/if}

<ConfirmDialog
  open={!!confirmState}
  title="确认"
  message={confirmState?.message || ''}
  confirmText="继续"
  on:confirm={() => {
    const c = confirmState;
    confirmState = null;
    c?.resolve(true);
  }}
  on:cancel={() => {
    const c = confirmState;
    confirmState = null;
    c?.resolve(false);
  }}
/>

<style>
  .home {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    justify-content: center;
    padding: 32px 16px;
    background: var(--bg-card);
  }
  .card {
    width: 620px;
    max-width: 100%;
    align-self: flex-start;
  }
  .brand h1 {
    font-size: 20px;
    font-weight: var(--fw-sb);
    color: var(--text-primary);
    margin: 0 0 6px;
  }
  .sub {
    font-size: var(--fs-sm);
    color: var(--text-secondary);
    margin: 0 0 20px;
    line-height: var(--lh-normal);
  }
  .sec {
    border: 1px solid var(--border);
    border-radius: var(--r-md);
    padding: 14px 16px;
    margin-bottom: 14px;
    background: var(--bg-header, rgba(255, 255, 255, 0.02));
  }
  .sec-head {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 10px;
  }
  .sec-title {
    font-size: var(--fs-sm);
    font-weight: var(--fw-sb);
    color: var(--text-primary);
  }
  .ws-cur {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 380px;
  }
  .mode-cur {
    font-size: var(--fs-xs);
    color: var(--brand);
    font-weight: var(--fw-sb);
  }
  .btn-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .btn {
    border: 1px solid var(--border-strong);
    border-radius: var(--r-sm);
    background: transparent;
    color: var(--text-primary);
    font-size: var(--fs-sm);
    padding: 5px 14px;
    cursor: pointer;
  }
  .btn:hover:not(:disabled) {
    border-color: var(--brand);
    color: var(--brand);
  }
  .btn:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .btn.ghost {
    border-color: transparent;
    color: var(--brand);
  }
  .recents {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
    margin-top: 10px;
  }
  .chip {
    border: 1px solid var(--border);
    border-radius: 999px;
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-xs);
    padding: 2px 10px;
    cursor: pointer;
    max-width: 150px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .chip:hover {
    border-color: var(--brand);
    color: var(--brand);
  }
  .toggle {
    display: inline-flex;
    border: 1px solid var(--border-strong);
    border-radius: var(--r-sm);
    overflow: hidden;
    margin-bottom: 10px;
  }
  .opt {
    border: none;
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-sm);
    padding: 6px 22px;
    cursor: pointer;
  }
  .opt + .opt {
    border-left: 1px solid var(--border-strong);
  }
  .opt.on {
    background: var(--brand);
    color: #fff;
    font-weight: var(--fw-sb);
  }
  .opt:disabled {
    cursor: default;
    opacity: 0.7;
  }
  .hint {
    font-size: var(--fs-xs);
    color: var(--text-muted);
    line-height: var(--lh-normal);
    margin: 8px 0 0;
  }
  .hint.red {
    color: var(--warning, #d29922);
  }
  .err {
    font-size: var(--fs-xs);
    color: #f87171;
    margin: 8px 0 0;
    word-break: break-all;
  }
  .guide ul {
    margin: 6px 0 0;
    padding-left: 18px;
    font-size: var(--fs-sm);
    color: var(--text-secondary);
    line-height: var(--lh-normal);
  }
  .guide li + li {
    margin-top: 4px;
  }
  /* ---- 公告卡(批 J1 段 2) ---- */
  .btn.sm {
    font-size: var(--fs-xs);
    padding: 2px 10px;
  }
  .notice {
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: 10px 12px;
    margin-bottom: 8px;
    background: var(--bg-card);
  }
  .notice:last-of-type {
    margin-bottom: 0;
  }
  .n-head {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .badge {
    flex: none;
    font-size: var(--fs-xs);
    border-radius: 4px;
    padding: 1px 8px;
    font-weight: var(--fw-sb);
  }
  .badge.lv-info {
    color: var(--text-secondary);
    border: 1px solid var(--border-strong);
  }
  .badge.lv-warn {
    color: var(--warning, #d29922);
    border: 1px solid var(--warning, #d29922);
  }
  .badge.lv-feature {
    color: var(--brand);
    border: 1px solid var(--brand);
  }
  .pin {
    flex: none;
    color: var(--brand);
    font-size: var(--fs-xs);
  }
  .n-title {
    font-size: var(--fs-sm);
    color: var(--text-primary);
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .n-date {
    flex: none;
    margin-left: auto;
    font-size: var(--fs-xs);
    color: var(--text-muted);
  }
  .n-body {
    margin-top: 6px;
    font-size: var(--fs-sm);
    color: var(--text-secondary);
    line-height: var(--lh-normal);
    overflow-wrap: anywhere;
  }
  .n-body :global(p) {
    margin: 0 0 6px;
  }
  .n-body :global(p:last-child) {
    margin-bottom: 0;
  }
  .n-body :global(img) {
    max-width: 100%;
    border-radius: var(--r-sm);
  }
  .n-body :global(code) {
    background: var(--bg-header, rgba(255, 255, 255, 0.06));
    border-radius: 4px;
    padding: 1px 5px;
    font-size: var(--fs-xs);
  }
  .n-links {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    margin-top: 8px;
  }
  .n-links .chip {
    display: inline-block;
    border: 1px solid var(--border);
    border-radius: 999px;
    color: var(--brand);
    font-size: var(--fs-xs);
    padding: 2px 10px;
    text-decoration: none;
  }
  .n-links .chip:hover {
    border-color: var(--brand);
  }
</style>
