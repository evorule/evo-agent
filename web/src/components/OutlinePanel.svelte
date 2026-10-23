<!-- SPDX-License-Identifier: AGPL-3.0-or-later
Copyright (C) 2026 EvoRule Project
This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
大纲面板(H3):Explorer 视图下方折叠段。符号收集走 lib/outline.js 直采
(JSON 本地扫描 / JS、TS 走 ts worker 导航树);点击符号跳转行列(复用
pendingReveal 定位信号,col 走 serve 搜索坐标的 0-based 约定)。 -->
<script>
  import { onDestroy } from 'svelte';
  import { get } from 'svelte/store';
  import { monaco } from '../lib/monaco-setup.js';
  import { activePath, openFile } from '../lib/stores.js';
  import { getModelForPath } from '../lib/modelRegistry.js';
  import { collectOutline, symbolGlyph } from '../lib/outline.js';

  let open = true;
  let symbols = [];
  let state = 'idle'; // idle | loading | ready | empty
  // 订阅句柄容器:Svelte 5 对 legacy 组件 async 函数的 await 点做作用域重建
  // ($.track_reactivity_loss),plain let 在恢复点值错乱(disposeContent 变
  // truthy 非函数,摘除即 TypeError)。const 绑定的对象属性读写不经重建,
  // 且句柄摘除收敛到同步 detachContent,async 面内不再直接读句柄。
  const sub = { content: null };
  let refreshSeq = 0; // 代际令牌:仅最新一次 refresh 有落座/存续裁决权
  let debounceTimer = null;

  function detachContent() {
    if (sub.content) {
      sub.content.dispose(); // monaco onDidChangeContent returns IDisposable, not a fn
      sub.content = null;
    }
  }

  $: refresh($activePath);

  async function refresh(path) {
    const gen = ++refreshSeq;
    detachContent();
    clearTimeout(debounceTimer);
    // 非文件 tab(设置页/虚拟路径/欢迎页)不采
    if (!path || path.startsWith('evo://') || path.startsWith('git-diff://')) {
      symbols = [];
      state = 'idle';
      return;
    }
    // model 由 EditorPane/GroupPane 打开 tab 后异步创建,activePath 先行——
    // 短重试等就绪(3×300ms);期间被抢代/切走即弃,仍无 model 回 idle(错误占位等)
    let model = getModelForPath(path);
    for (let i = 0; !model && i < 3; i++) {
      await new Promise((r) => setTimeout(r, 300));
      if (gen !== refreshSeq || get(activePath) !== path) return;
      model = getModelForPath(path);
    }
    if (!model) {
      symbols = [];
      state = 'idle';
      return;
    }
    state = 'loading';
    // 内容变化 → 去抖重采(编辑中大纲跟随)
    const d = model.onDidChangeContent(() => {
      clearTimeout(debounceTimer);
      debounceTimer = setTimeout(() => collect(path, model), 500);
    });
    if (gen !== refreshSeq) {
      d.dispose(); // superseded during await window: dispose now, never seat
      return;
    }
    sub.content = d;
    await collect(path, model);
  }

  async function collect(path, model) {
    try {
      const syms = await collectOutline(monaco, model);
      if (get(activePath) !== path) return; // 采集期间已切走:弃
      symbols = syms;
      state = syms.length ? 'ready' : 'empty';
    } catch {
      symbols = [];
      state = 'empty';
    }
  }

  function jump(s) {
    openFile($activePath, { line: s.line, col: s.col - 1 }); // pendingReveal col 为 0-based
  }

  onDestroy(() => {
    detachContent();
    clearTimeout(debounceTimer);
  });
</script>

<div class="outline">
  <button class="head" onclick={() => (open = !open)} aria-expanded={open}>
    <span class="chev" class:open>▸</span>
    大纲
  </button>
  {#if open}
    <div class="body">
      {#if state === 'idle'}
        <div class="hint">打开文件后显示大纲</div>
      {:else if state === 'loading'}
        <div class="hint">解析中…</div>
      {:else if state === 'empty'}
        <div class="hint">该语言无符号信息</div>
      {:else}
        {#each symbols as s, i (i)}
          <button
            class="sym"
            style="padding-left: {10 + s.depth * 12}px"
            title={`${s.name} · 行 ${s.line}`}
            onclick={() => jump(s)}
          >
            <span class="k {symbolGlyph(s.kind).cls}">{symbolGlyph(s.kind).glyph}</span>
            <span class="nm">{s.name}</span>
          </button>
        {/each}
      {/if}
    </div>
  {/if}
</div>

<style>
  .outline {
    flex-shrink: 0;
    border-top: 1px solid var(--border);
    max-height: 240px;
    display: flex;
    flex-direction: column;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 4px;
    width: 100%;
    padding: 5px 10px;
    background: transparent;
    border: none;
    color: var(--sidebar-text);
    font-size: var(--fs-2xs);
    font-weight: var(--fw-sb);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    cursor: pointer;
    text-align: left;
  }
  .head:hover {
    color: var(--sidebar-text-active);
  }
  .chev {
    display: inline-block;
    transition: transform 0.12s ease;
    color: var(--text-muted);
  }
  .chev.open {
    transform: rotate(90deg);
  }
  .body {
    overflow-y: auto;
    padding-bottom: 4px;
  }
  .hint {
    padding: 4px 10px 8px;
    font-size: var(--fs-xs);
    color: var(--text-muted);
  }
  .sym {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    padding: 2px 10px;
    background: transparent;
    border: none;
    color: var(--sidebar-text);
    font-size: var(--fs-xs);
    cursor: pointer;
    text-align: left;
    min-width: 0;
  }
  .sym:hover {
    background: var(--sidebar-hover);
    color: var(--sidebar-text-active);
  }
  .sym .nm {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .k {
    flex-shrink: 0;
    width: 14px;
    height: 14px;
    line-height: 14px;
    text-align: center;
    font-size: 9px;
    border-radius: 3px;
    background: var(--bg-active);
    color: var(--fg-dim);
  }
  .k.fn {
    color: #d2a8ff;
  }
  .k.class {
    color: #ffa657;
  }
  .k.key {
    color: #79c0ff;
  }
</style>
