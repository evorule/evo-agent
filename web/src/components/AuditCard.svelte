<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 审计报告卡(audit-one-click):点徽章弹出的人话报告薄浮层。
     结构=结论 + 本条依据 + 时间线 + 下钻因果链抽屉(缺省收起,用户
     原话全文仅在展开时渲染) + 下栏路由按钮。
     懒加载:include_content 完整报告与因果链均开卡才拉,大会话不拖慢对话栏。
     措辞边界:锚点生产启用前不使用「不可篡改/防篡改/绝对可信」类表述;
     绿=整链哈希校验通过 / 红=链校验未通过(哈希链断裂) / 灰=本条无链上存证。 -->
<script>
  import { getAuditReport, getAuditCausalChain } from '../lib/api.js';
  import { openBottomPanel } from '../lib/panel.js';

  let { entry, sid, verified = null, factCount = null, onclose = null } = $props();

  let report = $state(null); // include_content 完整报告
  let contentErr = $state('');
  let chain = $state(null); // 因果链(根→当前)
  let chainErr = $state('');
  let drawerOpen = $state(false);

  const factId = $derived(entry?.fact_id ?? null);
  const contentJson = $derived.by(() => {
    if (!report || factId == null) return null;
    const hit = (report.entries || []).find((e) => e?.fact_id === factId);
    return hit?.content_json ?? null;
  });

  // 开卡才拉(懒加载):完整报告(含 content_json)+ 因果链
  $effect(() => {
    if (!sid || factId == null) return;
    report = null;
    chain = null;
    contentErr = '';
    chainErr = '';
    drawerOpen = false;
    getAuditReport(sid, true)
      .then((res) => (report = res))
      .catch((e) => (contentErr = String(e?.message || e)));
    getAuditCausalChain(sid, factId)
      .then((res) => (chain = Array.isArray(res?.chain) ? res.chain : []))
      .catch((e) => (chainErr = String(e?.message || e)));
  });

  /** IoResponse 回答全文(实证形态:result.content) */
  const responseText = $derived.by(() => {
    const c = contentJson;
    if (!c) return '';
    const t = c?.result?.content;
    return typeof t === 'string' ? t : '';
  });

  /** IoRequest 用户原话(params.messages 内 user 角色内容;仅抽屉展开渲染) */
  const userText = $derived.by(() => {
    const c = contentJson;
    if (!c) return '';
    const msgs = c?.params?.messages;
    if (!Array.isArray(msgs)) return '';
    const parts = msgs
      .map((m) => {
        if (m && typeof m === 'object' && typeof m.content === 'string') {
          if (m.role && m.role !== 'user') return '';
          return m.content;
        }
        return typeof m === 'string' ? m : '';
      })
      .filter(Boolean);
    return parts.join('\n');
  });

  /** 本条依据的紧凑描述(非 Io 类 fact 回退 JSON 摘要) */
  function basisText(cj) {
    if (!cj) return '';
    if (typeof cj?.result?.content === 'string') return cj.result.content;
    if (Array.isArray(cj?.params?.messages)) {
      const joined = userText;
      if (joined) return joined;
    }
    const s = JSON.stringify(cj);
    return s.length > 240 ? `${s.slice(0, 240)}…` : s;
  }

  function shortHash(h) {
    return typeof h === 'string' && h.length > 16 ? `${h.slice(0, 16)}…` : String(h ?? '—');
  }

  function close() {
    onclose?.();
  }

  function goPanel(tab) {
    openBottomPanel(tab);
    close();
  }

  function openConsoleAudit() {
    window.open(`/console/audit?session=${sid}`, '_blank', 'noopener,noreferrer');
  }

  function onKeydown(e) {
    if (e.key === 'Escape') close();
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="ac-overlay" onclick={close} role="presentation">
  <div
    class="ac-card"
    role="dialog"
    aria-label="审计报告"
    onclick={(e) => e.stopPropagation()}
  >
    <div class="ac-head">
      <span class="ac-title">审计报告 · #{factId ?? '—'}</span>
      <button class="ac-close" onclick={close} aria-label="关闭">×</button>
    </div>

    <!-- 结论 -->
    <div class="ac-verdict" class:ok={verified} class:bad={verified === false}>
      {verified === false
        ? '链校验未通过（哈希链断裂）'
        : verified
          ? '整链哈希校验通过'
          : '本条无链上存证'}
      {#if factCount != null}
        <span class="ac-sub mono">链上位置 t{entry?.logical_time ?? '—'} · 共 {factCount} 条事实</span>
      {/if}
    </div>

    <!-- 本条依据 -->
    <div class="ac-section">
      <div class="ac-label">本条依据</div>
      {#if factId == null}
        <div class="ac-dim">本条无链上存证。</div>
      {:else if contentErr}
        <div class="ac-err">报告内容拉取失败:{contentErr}</div>
      {:else if !report}
        <div class="ac-dim">正在加载链上原文…</div>
      {:else}
        {#if responseText}
          <pre class="ac-quote">{responseText}</pre>
        {:else}
          <pre class="ac-quote dim">{basisText(contentJson) || '（无内容载荷）'}</pre>
        {/if}
        <div class="ac-kv mono">
          <span class="k">类型</span><span class="v">{entry?.fact_type ?? '—'}</span>
          <span class="k">内容哈希</span><span class="v">{shortHash(entry?.content_hash)}</span>
          <span class="k">前驱哈希</span><span class="v">{shortHash(entry?.prev_hash)}</span>
        </div>
      {/if}
    </div>

    <!-- 时间线(轻量:仅 id/类型/逻辑时间) -->
    <div class="ac-section">
      <div class="ac-label">时间线(因果来源链,根 → 本条)</div>
      {#if chainErr}
        <div class="ac-err">因果链拉取失败:{chainErr}</div>
      {:else if !chain}
        <div class="ac-dim">正在加载时间线…</div>
      {:else if chain.length === 0}
        <div class="ac-dim">本条无因果来源(链根)。</div>
      {:else}
        <div class="ac-timeline">
          {#each chain as n, i (n.fact_id)}
            <div class="tl-node" class:sel={n.fact_id === factId}>
              <span class="tl-dot mono">{i === 0 ? '根' : '└'}</span>
              <span class="mono">#{n.fact_id}</span>
              <span class="tl-type">{n.fact_type}</span>
              <span class="tl-t mono">t{n.logical_time}</span>
            </div>
          {/each}
        </div>
      {/if}
    </div>

    <!-- 下钻抽屉(缺省收起;展开才渲染链上原文与用户原话全文) -->
    <div class="ac-section">
      <button class="ac-drawer-toggle" onclick={() => (drawerOpen = !drawerOpen)}>
        {drawerOpen ? '▾' : '▸'} 下钻因果链{drawerOpen ? '(含链上原文与用户原话全文)' : ''}
      </button>
      {#if drawerOpen}
        <div class="ac-drawer">
          {#if !report}
            <div class="ac-dim">报告未就绪。</div>
          {:else}
            {#each chain ?? [] as n (n.fact_id)}
              <div class="dr-node">
                <div class="dr-head mono">
                  #{n.fact_id} · {n.fact_type} · t{n.logical_time}
                  <span class="dr-hash">h={shortHash(n.content_hash)} ← {shortHash(n.prev_hash)}</span>
                </div>
                {#if n.fact_id === factId && responseText}
                  <div class="dr-cap">回答全文</div>
                  <pre class="ac-quote">{responseText}</pre>
                {:else}
                  {@const cj = (report.entries || []).find((e) => e?.fact_id === n.fact_id)?.content_json}
                  {#if cj}
                    {@const ut = Array.isArray(cj?.params?.messages)
                      ? cj.params.messages
                          .map((m) => (m && typeof m === 'object' ? (m.role === 'user' || !m.role ? m.content : '') : m))
                          .filter((t) => typeof t === 'string' && t)
                          .join('\n')
                      : ''}
                    {#if ut}
                      <div class="dr-cap">用户原话</div>
                      <pre class="ac-quote">{ut}</pre>
                    {:else}
                      <pre class="ac-quote dim">{basisText(cj)}</pre>
                    {/if}
                  {/if}
                {/if}
              </div>
            {/each}
          {/if}
        </div>
      {/if}
    </div>

    <!-- 路由:下栏容器为主出口,console 深链为次级出口 -->
    <div class="ac-actions">
      <button class="ac-btn" onclick={() => goPanel('audit')}>在审计面板查看</button>
      <button class="ac-btn" onclick={() => goPanel('timetravel')}>在时光机器中查看</button>
      <button class="ac-btn ghost" onclick={openConsoleAudit}>权威审计页 →</button>
    </div>
  </div>
</div>

<style>
  .ac-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.45);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .ac-card {
    width: min(560px, 92vw);
    max-height: 78vh;
    overflow-y: auto;
    background: var(--bg-card);
    border: 1px solid var(--border-strong);
    border-radius: 8px;
    padding: var(--sp-md);
    display: flex;
    flex-direction: column;
    gap: var(--sp-md);
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.4);
  }
  .ac-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .ac-title {
    font-weight: 600;
    color: var(--text-primary);
  }
  .ac-close {
    border: none;
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-lg);
    cursor: pointer;
    padding: 0 var(--sp-xs);
  }
  .ac-verdict {
    padding: var(--sp-sm) var(--sp-sm);
    border-radius: 6px;
    font-size: var(--fs-sm);
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--sp-sm);
    background: var(--success-bg);
    color: var(--success);
  }
  .ac-verdict.bad {
    background: var(--danger-bg);
    color: var(--danger);
  }
  .ac-verdict:not(.ok):not(.bad) {
    background: var(--bg-hover);
    color: var(--text-secondary);
  }
  .ac-sub {
    color: var(--text-muted);
    font-size: var(--fs-2xs);
  }
  .ac-section {
    display: flex;
    flex-direction: column;
    gap: var(--sp-xs);
  }
  .ac-label {
    font-size: var(--fs-2xs);
    text-transform: uppercase;
    letter-spacing: 0.4px;
    color: var(--text-muted);
  }
  .ac-quote {
    margin: 0;
    padding: var(--sp-sm);
    background: var(--bg-page);
    border: 1px solid var(--border);
    border-radius: 6px;
    font-size: var(--fs-xs);
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 180px;
    overflow-y: auto;
    color: var(--text-primary);
  }
  .ac-quote.dim {
    color: var(--text-secondary);
  }
  .ac-kv {
    display: flex;
    flex-wrap: wrap;
    gap: var(--sp-xs) var(--sp-md);
    font-size: var(--fs-2xs);
    color: var(--text-muted);
  }
  .ac-kv .k::after {
    content: ':';
  }
  .ac-timeline {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .tl-node {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    font-size: var(--fs-xs);
    padding: 2px var(--sp-xs);
    border-radius: 4px;
    color: var(--text-secondary);
  }
  .tl-node.sel {
    background: var(--bg-active);
    color: var(--text-primary);
  }
  .tl-dot {
    width: 18px;
    color: var(--text-muted);
    flex: none;
  }
  .tl-type {
    color: var(--text-secondary);
  }
  .tl-t {
    margin-left: auto;
    color: var(--text-muted);
    font-size: var(--fs-2xs);
  }
  .ac-drawer-toggle {
    align-self: flex-start;
    border: none;
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-xs);
    cursor: pointer;
    padding: 0;
  }
  .ac-drawer-toggle:hover {
    color: var(--text-primary);
  }
  .ac-drawer {
    display: flex;
    flex-direction: column;
    gap: var(--sp-sm);
    border-left: 2px solid var(--border);
    padding-left: var(--sp-sm);
  }
  .dr-head {
    font-size: var(--fs-2xs);
    color: var(--text-secondary);
    display: flex;
    flex-wrap: wrap;
    gap: var(--sp-xs) var(--sp-sm);
  }
  .dr-hash {
    color: var(--text-muted);
  }
  .dr-cap {
    font-size: var(--fs-2xs);
    color: var(--text-muted);
    margin-top: var(--sp-xs);
  }
  .ac-actions {
    display: flex;
    gap: var(--sp-sm);
    flex-wrap: wrap;
  }
  .ac-btn {
    padding: 5px var(--sp-md);
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background: var(--bg-hover);
    color: var(--text-primary);
    font-size: var(--fs-xs);
    cursor: pointer;
  }
  .ac-btn:hover {
    background: var(--bg-active);
  }
  .ac-btn.ghost {
    background: transparent;
    color: var(--text-secondary);
  }
  .ac-dim {
    font-size: var(--fs-xs);
    color: var(--text-muted);
  }
  .ac-err {
    font-size: var(--fs-xs);
    color: var(--danger);
  }
  .mono {
    font-family: var(--font-mono, monospace);
  }
</style>
