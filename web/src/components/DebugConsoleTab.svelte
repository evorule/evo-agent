<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 调试控制台(底部面板 tab):程序输出(output 事件)+ repl evaluate。
     数据面=lib/dap/index.js(debugConsole store + debugEvaluate 动作)。 -->
<script>
  import { debugConsole, debugEvaluate, debugState, DBG_STATES } from '../lib/dap/index.js';

  let bodyEl = null;
  let expr = '';
  let evalError = '';

  $: active = [DBG_STATES.RUNNING, DBG_STATES.STOPPED].includes($debugState.state);

  // 追行贴底
  $: if (bodyEl) {
    $debugConsole;
    requestAnimationFrame(() => bodyEl && (bodyEl.scrollTop = bodyEl.scrollHeight));
  }

  async function submit() {
    const e = expr.trim();
    if (!e || !active) return;
    expr = '';
    evalError = '';
    try {
      await debugEvaluate(e);
    } catch (err) {
      evalError = String(err?.message || err);
    }
  }

  function streamTag(s) {
    return s === 'stderr' ? 'ERR' : s === 'repl' ? 'REPL' : s === 'system' ? 'SYS' : 'OUT';
  }
</script>

<div class="dc-wrap">
  <div class="dc-body" bind:this={bodyEl}>
    {#if $debugConsole.length === 0}
      <div class="dc-empty">暂无调试输出。启动调试会话后,程序输出(output 事件)在此呈现;stopped 期可在下方输入表达式求值。</div>
    {:else}
      {#each $debugConsole as line, i (i)}
        <div class="dc-row mono {line.stream}">
          <span class="dc-tag">{streamTag(line.stream)}</span>
          <span class="dc-text">{line.text}</span>
        </div>
      {/each}
    {/if}
  </div>
  {#if evalError}
    <div class="dc-err mono">{evalError}</div>
  {/if}
  <form
    class="dc-repl"
    onsubmit={(e) => {
      e.preventDefault();
      submit();
    }}
  >
    <span class="dc-prompt">›</span>
    <input
      class="dc-input mono"
      placeholder={active ? '输入表达式求值(evaluate)' : '调试会话未活动'}
      bind:value={expr}
      disabled={!active}
      aria-label="调试控制台表达式"
    />
  </form>
</div>

<style>
  .dc-wrap {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
  }
  .dc-body {
    flex: 1;
    overflow-y: auto;
    padding: var(--sp-xs) var(--sp-md);
    min-height: 0;
  }
  .dc-empty {
    color: var(--text-muted);
    font-size: var(--fs-xs);
    padding: var(--sp-sm) 0;
  }
  .dc-row {
    display: flex;
    gap: var(--sp-sm);
    font-size: var(--fs-xs);
    line-height: 1.55;
    padding: 1px 0;
  }
  .dc-tag {
    color: var(--text-muted);
    flex: none;
    width: 36px;
  }
  .dc-row.stderr .dc-tag { color: var(--danger); }
  .dc-row.stderr .dc-text { color: var(--danger); }
  .dc-row.repl .dc-tag,
  .dc-row.repl .dc-text { color: var(--brand-light); }
  .dc-row.system .dc-text { color: var(--text-muted); }
  .dc-text {
    color: var(--text-primary);
    white-space: pre-wrap;
    word-break: break-all;
  }
  .dc-err {
    color: var(--danger);
    font-size: var(--fs-xs);
    padding: 2px var(--sp-md);
  }
  .dc-repl {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    border-top: 1px solid var(--border);
    padding: var(--sp-xs) var(--sp-md);
  }
  .dc-prompt {
    color: var(--brand-light);
    font-weight: 600;
  }
  .dc-input {
    flex: 1;
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: var(--fs-xs);
    padding: 4px 8px;
  }
  .dc-input:focus { outline: 1px solid var(--brand); }
  .dc-input:disabled { opacity: 0.5; }
  .mono { font-family: var(--font-mono); }
</style>
