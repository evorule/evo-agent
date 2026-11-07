<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<script>
  import { parseJson, summarize, buildTree } from '../lib/json-structured.js';

  /** JSON 结构化视图:解析 tab 内存 content(零网络),渲染摘要卡+折叠树。
   *  scene/1.0 契约识别出时附场景键卡;解析失败显式报错不静默。
   *  opt-in toggle 形态(同 F4 预览钮),JSON 本体保持可编辑。
   *  树渲染走「扁平化行+折叠集」而非模板递归(Svelte 5 snippet 递归
   *  在组件挂载路径抛 TypeError,已实测——扁平化是确定性渲染面)。 */
  let { content = '' } = $props();

  const parsed = $derived(parseJson(content));
  const summary = $derived(parsed.ok ? summarize(parsed.value) : null);
  const tree = $derived(parsed.ok ? buildTree(parsed.value) : null);

  let collapsed = $state({}); // 树节点折叠态(按稳定键)

  /** 深度优先展平;命中折叠键的子树剪枝(节点自身保留,子级剔除) */
  function flatten(node, key, depth, collapsed, out) {
    out.push({ key, depth, ...node });
    if (node.children && !collapsed[key]) {
      for (const child of node.children) {
        flatten(child, `${key}/${child.name}`, depth + 1, collapsed, out);
      }
    }
  }

  const rows = $derived.by(() => {
    if (!tree) return [];
    const out = [];
    flatten(tree, 'root', 0, collapsed, out);
    return out;
  });

  function toggleNode(key) {
    collapsed[key] = !collapsed[key];
    collapsed = { ...collapsed };
  }
</script>

<div class="sv-root">
  {#if !parsed.ok}
    <div class="sv-error">JSON 解析失败:{parsed.error}</div>
  {:else}
    <div class="sv-cards">
      {#each summary.cards as c}
        <div class="sv-card">
          <span class="sv-label">{c.label}</span>
          <span class="sv-value">{c.value}</span>
        </div>
      {/each}
    </div>
    <div class="sv-tree">
      {#each rows as row (row.key)}
        <div class="sv-node" style:margin-left="{row.depth * 14}px">
          {#if row.children}
            <button
              class="sv-toggle"
              onclick={() => toggleNode(row.key)}
              title={collapsed[row.key] ? '展开' : '折叠'}>{collapsed[row.key] ? '▸' : '▾'}</button
            >
          {:else}
            <span class="sv-leaf-dot">·</span>
          {/if}
          <span class="sv-name">{row.name}</span>
          <span class="sv-type">{row.type}</span>
          {#if !row.children}<span class="sv-preview">{row.preview}</span>{/if}
          {#if row.truncated}<span class="sv-trunc">…已截断</span>{/if}
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .sv-root {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    overflow: auto;
    padding: 12px 16px;
    background: var(--bg-page, #141414);
    color: var(--fg, #ccc);
    font-size: 12px;
  }
  .sv-error {
    color: #f14c4c;
    padding: 12px;
  }
  .sv-cards {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 12px;
  }
  .sv-card {
    display: flex;
    gap: 6px;
    align-items: center;
    padding: 4px 10px;
    border: 1px solid var(--border, #333);
    border-radius: 6px;
    background: var(--bg-card, #1d1d1d);
  }
  .sv-label {
    color: var(--fg-dim, #888);
  }
  .sv-value {
    font-weight: 600;
  }
  .sv-tree {
    font-family: var(--mono, monospace);
  }
  .sv-node {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 1px 0;
  }
  .sv-toggle {
    background: none;
    border: none;
    color: var(--fg-dim, #888);
    cursor: pointer;
    width: 16px;
    padding: 0;
    text-align: center;
  }
  .sv-leaf-dot {
    color: var(--fg-dim, #555);
    width: 16px;
    text-align: center;
  }
  .sv-name {
    color: #9cdcfe;
  }
  .sv-type {
    color: var(--fg-dim, #666);
    font-size: 11px;
  }
  .sv-preview {
    color: #ce9178;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 60%;
  }
  .sv-trunc {
    color: #d7ba7d;
    font-size: 11px;
  }
</style>
