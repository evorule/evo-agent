<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 替换预览(B2):列表式 before/after 预览(非 Monaco diff——全文件 diff 归 B3)。
     per-file 折叠组 + 行内 diff(删除线红/新增绿,风格 token)+ 按文件勾选。
     「全部替换」「替换所选」经 onapply 回调由 SearchPanel 执行 apply:true;
     预览数据来自 apply=false 响应,应用时 serve 端重匹配(不信任快照)。 -->
<script>
  /** 预览数据:{preview:[{path, edits:[{line, before, after}]}], fileCount, matchCount} */
  export let preview;
  /** apply 执行中(按钮禁用) */
  export let busy = false;
  /** onapply(paths):paths=null 全部替换;paths=勾选文件路径数组 */
  export let onapply = null;
  export let onclose = null;

  let collapsed = new Set();
  /** path → 勾选状态(默认全选) */
  let selected = {};

  $: initSelected(preview);
  function initSelected(p) {
    const next = {};
    for (const f of p?.preview ?? []) next[f.path] = selected[f.path] !== false;
    selected = next;
  }

  $: checkedCount = Object.values(selected).filter(Boolean).length;

  function toggleSel(p) {
    selected = { ...selected, [p]: !selected[p] };
  }
  function toggleGroup(p) {
    const next = new Set(collapsed);
    if (next.has(p)) next.delete(p);
    else next.add(p);
    collapsed = next;
  }
  function applySelected() {
    if (onapply) onapply(Object.keys(selected).filter((k) => selected[k]));
  }
  function applyAll() {
    if (onapply) onapply(null);
  }
</script>

<div class="rp">
  <div class="rp-head">
    <span class="rp-title">替换预览</span>
    <span class="rp-meta">{preview.fileCount} 个文件 {preview.matchCount} 处</span>
    <span class="spacer"></span>
    <button class="rp-btn primary" disabled={busy} onclick={applyAll}>全部替换</button>
    <button class="rp-btn" disabled={busy || checkedCount === 0} onclick={applySelected}>
      替换所选({checkedCount})
    </button>
    <button class="rp-x" title="关闭预览" onclick={() => onclose && onclose()}>×</button>
  </div>
  <div class="rp-body">
    {#each preview.preview as f (f.path)}
      <div class="rp-group">
        <div class="rp-file">
          <input
            type="checkbox"
            checked={selected[f.path]}
            onchange={() => toggleSel(f.path)}
            title="勾选后可替换该文件"
          />
          <button class="chev" title="折叠/展开" onclick={() => toggleGroup(f.path)}>
            {collapsed.has(f.path) ? '▸' : '▾'}
          </button>
          <span class="fname">{f.path.split('/').pop()}</span>
          <span class="fpath">{f.path}</span>
          <span class="count">{f.edits.length}</span>
        </div>
        {#if !collapsed.has(f.path)}
          {#each f.edits as ed}
            <div class="rp-edit">
              <span class="ln">{ed.line}</span>
              <span class="diff"><del>{ed.before}</del><span class="arrow">→</span><ins>{ed.after}</ins></span>
            </div>
          {/each}
        {/if}
      </div>
    {/each}
  </div>
</div>

<style>
  .rp {
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    background: var(--bg-card);
    margin-bottom: var(--sp-sm);
  }
  .rp-head {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    padding: var(--sp-xs) var(--sp-sm);
    border-bottom: 1px solid var(--border);
  }
  .rp-title {
    font-size: var(--fs-xs);
    font-weight: var(--fw-med);
  }
  .rp-meta {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
  }
  .spacer {
    flex: 1;
  }
  .rp-btn {
    font-size: var(--fs-xs);
    padding: 2px 8px;
    border-radius: var(--r-sm);
    border: 1px solid var(--border-strong);
    color: var(--text-primary);
    background: var(--bg-card);
    transition: background var(--tr-fast), border-color var(--tr-fast);
  }
  .rp-btn:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .rp-btn.primary {
    background: var(--brand);
    border-color: var(--brand);
    color: #fff;
  }
  .rp-btn.primary:hover:not(:disabled) {
    background: var(--brand-hover);
  }
  .rp-btn:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .rp-x {
    font-size: var(--fs-lg);
    line-height: 1;
    color: var(--text-secondary);
    padding: 0 var(--sp-xs);
  }
  .rp-x:hover {
    color: var(--text-primary);
  }
  .rp-body {
    max-height: 40vh;
    overflow-y: auto;
  }
  .rp-group {
    border-bottom: 1px solid var(--border);
  }
  .rp-group:last-child {
    border-bottom: none;
  }
  .rp-file {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    padding: var(--sp-xs) var(--sp-sm);
  }
  .rp-file input {
    accent-color: var(--brand);
    flex-shrink: 0;
  }
  .chev {
    color: var(--text-muted);
    font-size: var(--fs-xs);
    width: 14px;
  }
  .fname {
    font-size: var(--fs-xs);
    color: var(--text-primary);
    flex-shrink: 0;
  }
  .fpath {
    font-size: var(--fs-xs);
    color: var(--text-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
    min-width: 0;
    direction: rtl;
    text-align: left;
  }
  .count {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    background: var(--bg-hover);
    border-radius: var(--r-full);
    padding: 0 6px;
    flex-shrink: 0;
  }
  .rp-edit {
    display: flex;
    gap: var(--sp-sm);
    padding: 1px var(--sp-sm) 1px var(--sp-md);
    font-family: var(--font-mono);
    font-size: var(--fs-xs);
  }
  .ln {
    color: var(--text-muted);
    min-width: 32px;
    text-align: right;
    flex-shrink: 0;
  }
  .diff {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  del {
    background: var(--danger-bg);
    color: var(--danger);
    text-decoration: line-through;
    padding: 0 1px;
  }
  ins {
    background: var(--success-bg);
    color: var(--success);
    text-decoration: none;
    padding: 0 1px;
  }
  .arrow {
    color: var(--text-muted);
    margin: 0 var(--sp-xs);
  }
</style>
