<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 审批策略分区(审批三档制 PR-4):条件文件编辑器(TOML 文本框+校验回显)
     + 按条件试判定(机械预检三色卡:红=强制人工/黄=评审判定/灰=降级·未预置)
     + 红线豁免覆盖开关(开启需确认,开启态标红警示条;关闭动作 serve 端
     warning 留痕)。判定逻辑全在 serve(approval_api.rs),本组件纯展示与编排。 -->
<script>
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import { settingsState, setSetting } from '../lib/settings.js';
  import {
    approvalState,
    loadApproval,
    readPolicyText,
    savePolicyText,
    runTrial,
    trialStatusView,
  } from '../lib/approval.js';

  // ---- 条件文件编辑器 ----
  let editorOpen = false;
  let draft = '';
  let draftLoading = false;
  let draftMsg = '';
  let draftError = false;

  // ---- 试判定表单 ----
  let trialTool = 'shell_exec';
  let trialArgs = '{\n  "command": "ls"\n}';
  let trialResult = null;
  let trialError = '';
  let trialing = false;

  // ---- 豁免覆盖开关 ----
  let confirmEnable = false;
  let overrideBusy = false;
  let overrideError = '';

  $: state = $approvalState;
  $: overrideOn = !!$settingsState.settings['approval.exemptionOverride'];
  $: overrideDegraded = $settingsState.degraded;
  $: groups = Object.entries(state.groups ?? {});
  $: trialView = trialResult ? trialStatusView(trialResult.status) : null;

  async function openEditor() {
    editorOpen = true;
    draftLoading = true;
    draftMsg = '';
    draft = await readPolicyText();
    draftLoading = false;
  }

  async function applyEditor() {
    draftMsg = '保存中…';
    draftError = false;
    const r = await savePolicyText(draft);
    if (!r.ok) {
      draftMsg = get(approvalState).error || '保存失败';
      draftError = true;
      return;
    }
    if (r.parseError) {
      draftMsg = `已落盘但校验失败(运行时将 fail-visible 降级人工):${r.parseError}`;
      draftError = true;
      return;
    }
    draftMsg = '已保存,校验通过';
    draftError = false;
  }

  async function onTrial() {
    trialing = true;
    trialError = '';
    let args = {};
    if (trialArgs.trim()) {
      try {
        args = JSON.parse(trialArgs);
      } catch (e) {
        trialError = `args JSON 解析失败:${String(e?.message || e)}`;
        trialing = false;
        return;
      }
    }
    const r = await runTrial(trialTool.trim(), args);
    if (!r.ok) {
      trialError = r.error;
      trialResult = null;
    } else {
      trialResult = r.result;
    }
    trialing = false;
  }

  function onOverrideToggle(ev) {
    const el = ev.target;
    overrideError = '';
    if (!el.checked) {
      // 关闭动作(恢复红线强制,安全向)直接落设置
      disableOverride(el);
      return;
    }
    // 开启动作(关闭红线强制):先回滚视觉态,出确认条
    el.checked = false;
    confirmEnable = true;
  }

  async function confirmOverrideEnable() {
    confirmEnable = false;
    overrideBusy = true;
    try {
      await setSetting('approval.exemptionOverride', true);
    } catch (e) {
      overrideError = String(e?.message || e);
    }
    overrideBusy = false;
  }

  async function disableOverride(el) {
    overrideBusy = true;
    try {
      await setSetting('approval.exemptionOverride', false);
    } catch (e) {
      overrideError = String(e?.message || e);
      if (el) el.checked = true; // 回滚到 store 权威值
    }
    overrideBusy = false;
  }

  onMount(() => {
    loadApproval();
  });
</script>

<section class="cat approval">
  <h3 class="cat-title">审批策略</h3>

  <div class="ovr">
    {#if overrideOn}
      <div class="ovr-warn" role="alert">
        红线豁免覆盖已开启:命中豁免清单的操作(删除文件 / 推送远端 / 数据外发 / 修改治理配置文件)不再强制人工审批,交由评审判定,判定留痕携带 exemption_overridden 标记。
      </div>
    {:else}
      <div class="ovr-safe">
        红线强制人工审批生效中:命中豁免清单的操作在任何审批档位下强制人工确认,评审不发生。
      </div>
    {/if}
    <div class="ovr-row">
      <label class="chk">
        <input
          type="checkbox"
          checked={overrideOn}
          disabled={overrideBusy || overrideDegraded}
          onchange={onOverrideToggle}
        />
        <span>开启豁免覆盖(关闭红线强制人工)</span>
      </label>
      {#if overrideError}<span class="ovr-err">{overrideError}</span>{/if}
    </div>
    {#if confirmEnable}
      <div class="ovr-confirm">
        确认关闭红线强制人工审批?开启后,删除文件 / 推送远端 / 数据外发 / 修改治理配置文件的操作将不再强制人工确认,由评审按条件自动判定。
        <button class="btn danger" onclick={confirmOverrideEnable} disabled={overrideBusy}>确认开启</button>
        <button class="btn" onclick={() => (confirmEnable = false)}>取消</button>
      </div>
    {/if}
  </div>

  <div class="pol-toolbar">
    <span class="badge">
      {#if state.parseError}
        条件文件无效(fail-visible)
      {:else if state.exists}
        条件文件已预置 · {groups.length} 个工具组 · 修正限额 {state.limits.max_corrections} · 自动操作配额
        {state.limits.max_actions_per_run}
      {:else}
        条件文件未预置(.evo/approval-policy.toml)——policy 档运行时自动回落人工
      {/if}
    </span>
    <span class="spacer"></span>
    <button class="btn" onclick={() => loadApproval()} disabled={state.loading}>刷新</button>
    <button class="btn primary" onclick={openEditor} disabled={state.loading}>编辑条件文件</button>
  </div>

  {#if state.parseError}
    <div class="pol-err">条件文件解析失败:{state.parseError}</div>
  {/if}
  {#if state.error}
    <div class="pol-err">{state.error}</div>
  {/if}

  {#if groups.length}
    <details class="groups">
      <summary>条件组({groups.length})</summary>
      {#each groups as [name, conds] (name)}
        <div class="group">
          <span class="mono g-name">{name}</span>
          <ul class="conds">
            {#each conds as c}
              <li>{c}</li>
            {/each}
          </ul>
        </div>
      {/each}
    </details>
  {/if}

  {#if editorOpen}
    <div class="editor">
      <div class="editor-hint">
        TOML 条件文件(.evo/approval-policy.toml):serve 侧封存读取,agent 上下文不可见。
        保存即整体覆盖;校验失败会显式标红——policy 档对无效文件 fail-visible 降级人工,绝不静默放行。
      </div>
      {#if draftLoading}
        <div class="editor-hint">读取中…</div>
      {:else}
        <textarea class="editor-toml mono" rows="14" bind:value={draft} spellcheck="false"></textarea>
        {#if draftMsg}<div class={draftError ? 'pol-err' : 'pol-ok'}>{draftMsg}</div>{/if}
        <div class="editor-actions">
          <button class="btn primary" onclick={applyEditor} disabled={state.loading}>保存并校验</button>
          <button class="btn" onclick={() => (editorOpen = false)}>取消</button>
        </div>
      {/if}
    </div>
  {/if}

  <div class="trial">
    <div class="trial-title">按条件试判定</div>
    <div class="editor-hint">
      机械预检(红线豁免 → 条件清单 → 限额),零 LLM 调用、不落账——预览「该操作在 policy 档下会走哪条路」。
    </div>
    <div class="trial-form">
      <input class="txt mono" type="text" placeholder="工具名,如 shell_exec" bind:value={trialTool} />
      <textarea
        class="args mono"
        rows="3"
        bind:value={trialArgs}
        spellcheck="false"
        placeholder={'操作参数 JSON,如 {"command": "ls"}'}
      ></textarea>
      <button class="btn primary" onclick={onTrial} disabled={trialing || !trialTool.trim()}>
        {trialing ? '判定中…' : '试判定'}
      </button>
    </div>
    {#if trialError}<div class="pol-err">{trialError}</div>{/if}
    {#if trialView}
      <div class="trial-card {trialView.kind}">
        <div class="t-head">
          <span class="t-label">{trialView.label}</span>
          {#if trialResult.red_line}<span class="t-rl mono">红线:{trialResult.red_line}</span>{/if}
        </div>
        <div class="t-note">{trialResult.note}</div>
        {#if (trialResult.conditions ?? []).length}
          <ul class="t-conds">
            {#each trialResult.conditions as c}
              <li>{c}</li>
            {/each}
          </ul>
        {/if}
      </div>
    {/if}
  </div>
</section>

<style>
  .ovr {
    margin-bottom: var(--sp-sm);
  }
  .ovr-warn {
    font-size: var(--fs-xs);
    color: var(--danger);
    background: var(--danger-bg, rgba(231, 76, 60, 0.12));
    border: 1px solid var(--danger);
    border-radius: var(--r-sm);
    padding: var(--sp-xs) var(--sp-sm);
    margin-bottom: var(--sp-xs);
  }
  .ovr-safe {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: var(--sp-xs) var(--sp-sm);
    margin-bottom: var(--sp-xs);
  }
  .ovr-row {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
  }
  .ovr-err {
    font-size: var(--fs-xs);
    color: var(--danger);
    word-break: break-all;
  }
  .ovr-confirm {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    flex-wrap: wrap;
    font-size: var(--fs-xs);
    color: var(--text-primary);
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: var(--sp-xs) var(--sp-sm);
    margin-top: var(--sp-xs);
  }
  .chk {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    cursor: pointer;
  }
  .pol-toolbar {
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
  .pol-err {
    font-size: var(--fs-xs);
    color: var(--danger);
    background: var(--danger-bg, rgba(231, 76, 60, 0.12));
    border: 1px solid var(--danger);
    border-radius: var(--r-sm);
    padding: var(--sp-xs) var(--sp-sm);
    margin-bottom: var(--sp-sm);
    word-break: break-all;
  }
  .pol-ok {
    font-size: var(--fs-xs);
    color: var(--success, #4ade80);
    margin-bottom: var(--sp-xs);
  }
  .groups {
    margin-bottom: var(--sp-sm);
  }
  .groups summary {
    font-size: var(--fs-xs);
    color: var(--brand);
    cursor: pointer;
    user-select: none;
  }
  .group {
    display: flex;
    gap: var(--sp-md);
    padding: var(--sp-xs) 0 var(--sp-xs) var(--sp-md);
  }
  .g-name {
    font-size: var(--fs-xs);
    color: var(--text-primary);
    flex-shrink: 0;
  }
  .conds {
    margin: 0;
    padding-left: var(--sp-lg);
    font-size: var(--fs-xs);
    color: var(--text-secondary);
  }
  .editor {
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    padding: var(--sp-sm);
    margin-bottom: var(--sp-sm);
  }
  .editor-hint {
    font-size: var(--fs-2xs);
    color: var(--text-muted);
    margin-bottom: var(--sp-xs);
  }
  .editor-toml,
  .args {
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
  .editor-toml:focus,
  .args:focus {
    border-color: var(--brand);
  }
  .editor-actions {
    display: flex;
    gap: var(--sp-xs);
    margin-top: var(--sp-xs);
  }
  .trial {
    margin-bottom: var(--sp-sm);
  }
  .trial-title {
    font-size: var(--fs-xs);
    font-weight: var(--fw-sb);
    color: var(--text-primary);
    margin-bottom: var(--sp-xs);
  }
  .trial-form {
    display: flex;
    flex-direction: column;
    gap: var(--sp-xs);
  }
  .txt {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: var(--fs-xs);
    padding: 3px var(--sp-sm);
    outline: none;
  }
  .txt:focus {
    border-color: var(--brand);
  }
  .trial-card {
    border: 1px solid var(--border);
    border-left-width: 3px;
    border-radius: var(--r-sm);
    padding: var(--sp-xs) var(--sp-sm);
    margin-top: var(--sp-xs);
    font-size: var(--fs-xs);
  }
  .trial-card.red {
    border-left-color: var(--danger);
  }
  .trial-card.amber {
    border-left-color: var(--warning, #fbbf24);
  }
  .trial-card.grey {
    border-left-color: var(--text-muted);
  }
  .t-head {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
  }
  .t-label {
    font-weight: var(--fw-sb);
    color: var(--text-primary);
  }
  .trial-card.red .t-label {
    color: var(--danger);
  }
  .trial-card.amber .t-label {
    color: var(--warning, #fbbf24);
  }
  .t-rl {
    font-size: var(--fs-2xs);
    color: var(--danger);
  }
  .t-note {
    color: var(--text-secondary);
    margin-top: 2px;
  }
  .t-conds {
    margin: var(--sp-xs) 0 0;
    padding-left: var(--sp-lg);
    color: var(--text-muted);
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
    align-self: flex-start;
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
    color: var(--text-inverse);
  }
  .btn.primary:hover:not(:disabled) {
    background: var(--brand-hover);
  }
  .btn.danger {
    background: var(--danger);
    border-color: var(--danger);
    color: var(--text-inverse);
  }
  .mono {
    font-family: var(--font-mono, monospace);
  }
</style>
