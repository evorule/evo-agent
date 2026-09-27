<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 源代码管理视图(B3):双态更改列表 + 提交框。
     单状态源 $gitStatus(fs_events 防抖刷新 + 操作后即时刷新);
     提交走 serve GitOps(身份预检/hooks 分流在后端);
     行点击打开 git diff 虚拟 tab(pendingGitDiff 信号,EditorPane 消费)。 -->
<script>
  import { onMount } from 'svelte';
  import { gitStatus, refreshGitStatus, openGitDiff } from '../lib/stores.js';
  import { gitStage, gitUnstage, gitDiscard, gitCommit, gitIdentity } from '../lib/api.js';
  import ConfirmDialog from './ConfirmDialog.svelte';

  let message = '';
  let committing = false;
  let identityMissing = false;
  let identityHint = '';
  let notice = null; // {kind:'ok'|'err', text}(短暂 toast,命令面板同款底部浮层)
  let noticeTimer = null;

  let confirmState = null; // {paths:[..], message:'..'}(discard 二次确认)

  function flash(kind, text) {
    notice = { kind, text };
    if (noticeTimer) clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => (notice = null), 3200);
  }

  function errText(e) {
    return String(e?.message || e);
  }

  onMount(() => {
    // 身份预检(缺身份时提交被 serve 以 identity_missing 拦截,此处提前引导)
    gitIdentity()
      .then((id) => {
        identityMissing = !!id.identity_missing;
        identityHint = id.hint || '';
      })
      .catch(() => {});
    refreshGitStatus(0);
    return () => {
      if (noticeTimer) clearTimeout(noticeTimer);
    };
  });

  async function runStage(paths) {
    try {
      await gitStage(paths);
      refreshGitStatus(0);
    } catch (e) {
      flash('err', errText(e));
    }
  }

  async function runUnstage(paths) {
    try {
      await gitUnstage(paths);
      refreshGitStatus(0);
    } catch (e) {
      flash('err', errText(e));
    }
  }

  function askDiscard(paths) {
    const label =
      paths.length === 1
        ? String(paths[0]).endsWith('/')
          ? `确定丢弃未跟踪目录「${paths[0]}」?整个目录将被删除,不可恢复。`
          : `确定丢弃「${paths[0]}」的工作区更改?未跟踪文件将被直接删除,不可恢复。`
        : `确定丢弃 ${paths.length} 个条目的工作区更改?未跟踪文件将被直接删除,不可恢复。`;
    confirmState = { paths, message: label };
  }

  async function doDiscard() {
    const paths = confirmState?.paths || [];
    confirmState = null;
    try {
      await gitDiscard(paths);
      refreshGitStatus(0);
    } catch (e) {
      flash('err', errText(e));
    }
  }

  async function commit() {
    const msg = message.trim();
    if (!msg) {
      flash('err', '提交消息不能为空');
      return;
    }
    committing = true;
    try {
      const res = await gitCommit(msg);
      message = '';
      flash('ok', `已提交 ${(res.commit || '').slice(0, 7)}`);
      refreshGitStatus(0);
    } catch (e) {
      const m = errText(e);
      if (m.includes('identity_missing')) {
        identityMissing = true;
        flash('err', 'git 身份缺失:请先配置 user.name / user.email');
      } else {
        flash('err', m);
      }
    } finally {
      committing = false;
    }
  }

  function onCommitKey(e) {
    if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
      e.preventDefault();
      commit();
    }
  }

  /** 状态字母 → 语义色 class */
  function letterClass(s) {
    if (s === 'A' || s === 'U') return 'added';
    if (s === 'D') return 'deleted';
    if (s === 'R') return 'renamed';
    return 'modified';
  }

  /** 路径拆显示:文件名 + 目录部分(弱化) */
  function splitPath(p) {
    const idx = p.lastIndexOf('/');
    return idx >= 0 ? { name: p.slice(idx + 1), dir: p.slice(0, idx) } : { name: p, dir: '' };
  }
</script>

<div class="scm" data-zone="scm">
  <div class="panel-title">
    源代码管理
    {#if $gitStatus}
      <span class="branch mono" title="当前分支">{$gitStatus.branch || '(无分支)'}</span>
    {/if}
    <span class="spacer" />
    <button class="action-btn" title="刷新 git 状态" onclick={() => refreshGitStatus(0)}>
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
        <path d="M20 12a8 8 0 1 1-2.34-5.66M20 4v4h-4" />
      </svg>
    </button>
  </div>

  {#if !$gitStatus}
    <div class="empty">当前工作目录不是 git 仓库,源代码管理不可用。</div>
  {:else}
    {#if identityMissing}
      <div class="identity-warn" title={identityHint}>
        git 身份未配置,提交会被拒绝:请先运行 git config 设置 user.name 与 user.email。
      </div>
    {/if}

    <div class="commit-box">
      <textarea
        rows="2"
        placeholder="提交消息(Ctrl+Enter 提交)"
        bind:value={message}
        onkeydown={onCommitKey}
      />
      <button
        class="commit-btn"
        disabled={committing || !message.trim()}
        title="提交全部更改(先全量暂存)"
        onclick={commit}
      >
        {committing ? '提交中…' : '提交'}
      </button>
    </div>

    {#if $gitStatus.staged.length === 0 && $gitStatus.changes.length === 0}
      <div class="empty">工作区干净,没有待提交的更改。</div>
    {/if}

    {#if $gitStatus.staged.length > 0}
      <div class="group-header">
        <span>暂存的更改</span>
        <span class="count">{$gitStatus.staged.length}</span>
        <span class="spacer" />
        <button
          class="action-btn"
          title="全部取消暂存"
          onclick={() => runUnstage($gitStatus.staged.map((e) => e.path))}
        >
          <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
            <path d="M5 12h14" />
          </svg>
        </button>
      </div>
      {#each $gitStatus.staged as e (e.path)}
        <div class="row" role="button" tabindex="0" onclick={() => openGitDiff(e.path)}
          onkeydown={(ev) => (ev.key === 'Enter' || ev.key === ' ') && openGitDiff(e.path)}>
          <span class="name mono" title={e.path}>{splitPath(e.path).name}</span>
          <span class="dir mono">{splitPath(e.path).dir}</span>
          <span class="spacer" />
          <button class="action-btn" title="取消暂存" onclick={(ev) => { ev.stopPropagation(); runUnstage([e.path]); }}>
            <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
              <path d="M5 12h14" />
            </svg>
          </button>
          <span class="badge {letterClass(e.status)}">{e.status}</span>
        </div>
      {/each}
    {/if}

    {#if $gitStatus.changes.length > 0}
      <div class="group-header">
        <span>更改</span>
        <span class="count">{$gitStatus.changes.length}</span>
        <span class="spacer" />
        <button
          class="action-btn"
          title="全部暂存"
          onclick={() => runStage($gitStatus.changes.map((e) => e.path))}
        >
          <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
            <path d="M12 5v14M5 12h14" />
          </svg>
        </button>
        <button
          class="action-btn"
          title="全部丢弃更改"
          onclick={() => askDiscard($gitStatus.changes.map((e) => e.path))}
        >
          <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
            <path d="M4 9a8 8 0 1 1 2.34 5.66M4 13V9h4" />
          </svg>
        </button>
      </div>
      {#each $gitStatus.changes as e (e.path)}
        <div class="row" role="button" tabindex="0" onclick={() => openGitDiff(e.path)}
          onkeydown={(ev) => (ev.key === 'Enter' || ev.key === ' ') && openGitDiff(e.path)}>
          <span class="name mono" title={e.path}>{splitPath(e.path).name}</span>
          <span class="dir mono">{splitPath(e.path).dir}</span>
          <span class="spacer" />
          <button class="action-btn" title="暂存" onclick={(ev) => { ev.stopPropagation(); runStage([e.path]); }}>
            <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
              <path d="M12 5v14M5 12h14" />
            </svg>
          </button>
          <button
            class="action-btn"
            title={String(e.path).endsWith('/') ? '丢弃(删除整个目录)' : '丢弃工作区更改'}
            onclick={(ev) => { ev.stopPropagation(); askDiscard([e.path]); }}
          >
            <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
              <path d="M4 9a8 8 0 1 1 2.34 5.66M4 13V9h4" />
            </svg>
          </button>
          <span class="badge {letterClass(e.status)}">{e.status}</span>
        </div>
      {/each}
    {/if}
  {/if}
</div>

{#if notice}
  <div class="scm-toast {notice.kind}" role="alert">{notice.text}</div>
{/if}

<ConfirmDialog
  open={!!confirmState}
  title="丢弃更改"
  message={confirmState?.message || ''}
  confirmText="丢弃"
  on:confirm={doDiscard}
  on:cancel={() => (confirmState = null)}
/>

<style>
  .scm {
    width: 240px;
    flex-shrink: 0;
    background: var(--sidebar-bg);
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    overflow-y: auto;
  }
  .panel-title {
    font-size: 11px;
    font-weight: var(--fw-sb);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: rgba(255, 255, 255, 0.4);
    padding: var(--sp-sm) var(--sp-md);
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
  }
  .branch {
    font-size: 11px;
    color: var(--sidebar-text);
    text-transform: none;
    letter-spacing: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 120px;
  }
  .spacer {
    flex: 1;
  }
  .action-btn {
    width: 22px;
    height: 22px;
    border-radius: var(--r-sm);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--sidebar-text);
    flex-shrink: 0;
    transition: background var(--tr-fast), color var(--tr-fast);
  }
  .action-btn:hover {
    color: var(--sidebar-text-active);
    background: var(--sidebar-hover);
  }

  .empty {
    padding: var(--sp-md);
    color: var(--text-muted);
    font-size: var(--fs-xs);
  }

  .identity-warn {
    margin: 0 var(--sp-sm) var(--sp-sm);
    padding: var(--sp-xs) var(--sp-sm);
    border: 1px solid var(--warning);
    border-radius: var(--r-sm);
    background: var(--warning-bg);
    color: var(--warning);
    font-size: var(--fs-xs);
  }

  .commit-box {
    padding: 0 var(--sp-sm) var(--sp-sm);
    display: flex;
    flex-direction: column;
    gap: var(--sp-xs);
  }
  .commit-box textarea {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: var(--fs-xs);
    font-family: inherit;
    padding: var(--sp-xs) var(--sp-sm);
    resize: vertical;
  }
  .commit-box textarea:focus {
    outline: 1px solid var(--brand);
  }
  .commit-btn {
    background: var(--brand);
    color: #fff;
    border-radius: var(--r-sm);
    font-size: var(--fs-xs);
    padding: var(--sp-xs) 0;
    transition: opacity var(--tr-fast);
  }
  .commit-btn:disabled {
    opacity: 0.45;
    cursor: default;
  }

  .group-header {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    padding: var(--sp-xs) var(--sp-md);
    font-size: 11px;
    font-weight: var(--fw-sb);
    color: var(--sidebar-text);
    text-transform: uppercase;
    letter-spacing: 0.4px;
  }
  .count {
    background: var(--sidebar-hover);
    border-radius: 8px;
    min-width: 16px;
    text-align: center;
    font-size: 10px;
    padding: 0 4px;
  }

  .row {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    padding: 3px var(--sp-md) 3px var(--sp-lg);
    font-size: var(--fs-xs);
    color: var(--sidebar-text);
    cursor: pointer;
    transition: background var(--tr-fast);
  }
  .row:hover {
    background: var(--sidebar-hover);
  }
  .row .name {
    color: var(--sidebar-text-active);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .row .dir {
    color: var(--text-muted);
    font-size: 10px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 70px;
    direction: rtl; /* 长路径优先展示靠文件名的尾段 */
  }
  .row .action-btn {
    opacity: 0;
  }
  .row:hover .action-btn {
    opacity: 1;
  }

  .badge {
    font-family: var(--font-mono);
    font-size: 10px;
    font-weight: var(--fw-sb);
    width: 14px;
    text-align: center;
    flex-shrink: 0;
  }
  .badge.added {
    color: var(--git-added);
  }
  .badge.modified {
    color: var(--git-modified);
  }
  .badge.deleted {
    color: var(--git-deleted);
  }
  .badge.renamed {
    color: var(--git-renamed);
  }

  .scm-toast {
    position: fixed;
    left: 50%;
    bottom: 40px;
    transform: translateX(-50%);
    background: var(--bg-card);
    border: 1px solid var(--border-strong);
    border-radius: var(--r-md);
    color: var(--text-primary);
    font-size: var(--fs-xs);
    padding: var(--sp-xs) var(--sp-md);
    box-shadow: var(--sh-modal);
    z-index: 301;
  }
  .scm-toast.ok {
    border-color: var(--success);
    color: var(--success);
  }
  .scm-toast.err {
    border-color: var(--danger);
    color: var(--danger);
  }
</style>
