<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 全局搜索面板(B2):查询区(三 toggle + include/exclude)+ 折叠替换区 +
     结果树(文件组折叠,行项 <mark> 高亮,点击定位到行列)+ 状态/截断/超时/错误区 +
     历史下拉(localStorage 最近 10 条)。设置消费:search.maxResults/smartCase/
     useIgnoreFiles/excludeGlobs 4 键。命令入口经 searchIntent 一次性信号接线。 -->
<script>
  import { onMount, onDestroy, tick } from 'svelte';
  import { searchIntent, openFile } from '../lib/stores.js';
  import { settingsState } from '../lib/settings.js';
  import { searchGrep, replaceFiles } from '../lib/api.js';
  import ReplacePreview from './ReplacePreview.svelte';

  const HISTORY_KEY = 'evo_search_history';
  const HISTORY_LIMIT = 10;

  let query = '';
  let isRegex = false; // .* 正则
  let caseSensitive = false; // Aa 区分大小写
  let wholeWord = false; // ab 全字匹配
  let includeGlob = '';
  let excludeGlob = '';
  let replaceOpen = false;
  let replacement = '';
  let searching = false;
  let error = '';
  let result = null;
  let collapsed = new Set();
  let history = [];
  let historyOpen = false;
  let previewData = null;
  let previewParams = null;
  let applyBusy = false;
  let applyResult = null;
  let queryEl;
  let replaceEl;

  // ---- 设置消费(search.* 4 键,缺省回落出厂默认) ----
  $: maxResults =
    typeof $settingsState.settings['search.maxResults'] === 'number'
      ? $settingsState.settings['search.maxResults']
      : 1000;
  $: smartCase = $settingsState.settings['search.smartCase'] != null
    ? !!$settingsState.settings['search.smartCase']
    : true;
  $: useIgnore = $settingsState.settings['search.useIgnoreFiles'] != null
    ? !!$settingsState.settings['search.useIgnoreFiles']
    : true;
  $: defaultExcludes = Array.isArray($settingsState.settings['search.excludeGlobs'])
    ? $settingsState.settings['search.excludeGlobs']
    : [];

  // ---- 历史(localStorage,失败静默降级纯内存) ----
  function loadHistory() {
    try {
      const arr = JSON.parse(localStorage.getItem(HISTORY_KEY) || '[]');
      history = Array.isArray(arr) ? arr.slice(0, HISTORY_LIMIT) : [];
    } catch {
      history = [];
    }
  }
  function saveHistory(q) {
    history = [q, ...history.filter((x) => x !== q)].slice(0, HISTORY_LIMIT);
    try {
      localStorage.setItem(HISTORY_KEY, JSON.stringify(history));
    } catch {
      /* 隐私模式等:仅保留内存历史 */
    }
  }

  // ---- 搜索/替换 ----
  function parseGlobs(s) {
    return s
      .split(',')
      .map((x) => x.trim())
      .filter(Boolean);
  }

  function baseParams() {
    // 智能大小写:强制开关(Aa)优先;否则 smartCase 开且查询含大写才敏感(VS Code 同款)
    const effectiveCase = caseSensitive || (smartCase && /[A-Z]/.test(query));
    return {
      query,
      isRegex,
      caseSensitive: effectiveCase,
      wholeWord,
      includeGlobs: parseGlobs(includeGlob),
      excludeGlobs: [...defaultExcludes, ...parseGlobs(excludeGlob)],
      useIgnoreFiles: useIgnore,
      maxResults,
    };
  }

  async function doSearch() {
    if (!query.trim() || searching) return;
    historyOpen = false;
    searching = true;
    error = '';
    previewData = null;
    // 注意:不清 applyResult——apply 成功后的汇总横幅靠自动重搜刷新结果树,
    // 若在此清空则横幅一闪即逝(05-实施日志 PR5 修正记录);横幅由预览/关闭钮清除
    try {
      result = await searchGrep(baseParams());
      collapsed = new Set();
      saveHistory(query);
    } catch (e) {
      result = null;
      error = String(e?.message || e);
    } finally {
      searching = false;
    }
  }

  async function doPreview() {
    if (!query.trim() || searching) return;
    searching = true;
    error = '';
    applyResult = null;
    try {
      previewParams = baseParams();
      previewData = await replaceFiles({ ...previewParams, replacement, apply: false });
    } catch (e) {
      previewData = null;
      error = String(e?.message || e);
    } finally {
      searching = false;
    }
  }

  /** apply 执行(ReplacePreview 回调;paths=null 全部,数组=按所选文件)。
   *  serve 端重匹配不信任预览快照;成功后关预览并自动重搜刷新结果树 */
  async function doApply(paths) {
    if (applyBusy) return;
    applyBusy = true;
    error = '';
    try {
      const body = { ...previewParams, replacement, apply: true };
      if (paths) body.paths = paths;
      applyResult = await replaceFiles(body);
      previewData = null;
      if (query.trim()) doSearch();
    } catch (e) {
      error = String(e?.message || e);
    } finally {
      applyBusy = false;
    }
  }

  function clearAll() {
    query = '';
    result = null;
    error = '';
    previewData = null;
    applyResult = null;
    historyOpen = false;
  }

  function toggleGroup(p) {
    const next = new Set(collapsed);
    if (next.has(p)) next.delete(p);
    else next.add(p);
    collapsed = next;
  }

  /** 行内高亮切分:col/endCol 为 UTF-8 char 偏移,JS 用码点数组对齐(中文/emoji 安全) */
  function seg(preview, col, endCol) {
    const chars = Array.from(preview ?? '');
    const a = Math.max(0, Math.min(col ?? 0, chars.length));
    const b = Math.max(a, Math.min(endCol ?? col ?? 0, chars.length));
    return {
      before: chars.slice(0, a).join(''),
      mark: chars.slice(a, b).join(''),
      after: chars.slice(b).join(''),
    };
  }

  function hitClick(g, h) {
    openFile(g.path, { line: h.line, col: h.col, endCol: h.endCol });
  }

  // 命令入口信号消费(一次性;show 聚焦 / replace 展开替换区 / clear 清空)
  const unsubIntent = searchIntent.subscribe(async (v) => {
    if (!v) return;
    searchIntent.set(null);
    if (v === 'show') {
      await tick();
      queryEl?.focus();
      queryEl?.select();
    } else if (v === 'replace') {
      replaceOpen = true;
      await tick();
      replaceEl?.focus();
    } else if (v === 'clear') {
      clearAll();
    }
  });

  onMount(loadHistory);
  onDestroy(() => unsubIntent());
</script>

<div class="search-panel">
  <div class="panel-head">搜索</div>

  <div class="query-row">
    <input
      class="q-input"
      bind:this={queryEl}
      bind:value={query}
      placeholder="搜索"
      onkeydown={(e) => e.key === 'Enter' && doSearch()}
    />
    <button
      class="hist-btn"
      title="搜索历史"
      onclick={() => (historyOpen = !historyOpen)}
    >
      ⌄
    </button>
    {#if historyOpen && history.length}
      <div class="hist-list">
        {#each history as h (h)}
          <button class="hist-item" onclick={() => { query = h; historyOpen = false; doSearch(); }}>
            {h}
          </button>
        {/each}
      </div>
    {/if}
  </div>

  <div class="toggles">
    <button class="tg" class:on={caseSensitive} title="区分大小写" onclick={() => (caseSensitive = !caseSensitive)}>Aa</button>
    <button class="tg" class:on={wholeWord} title="全字匹配" onclick={() => (wholeWord = !wholeWord)}>ab</button>
    <button class="tg" class:on={isRegex} title="使用正则表达式" onclick={() => (isRegex = !isRegex)}>.*</button>
  </div>

  <input class="glob-input" bind:value={includeGlob} placeholder="包含(如 *.rs, src/**)" />
  <input class="glob-input" bind:value={excludeGlob} placeholder="排除(逗号分隔,叠加设置排除集)" />

  <button class="replace-toggle" onclick={() => (replaceOpen = !replaceOpen)}>
    <span class="chev">{replaceOpen ? '▾' : '▸'}</span> 替换
  </button>
  {#if replaceOpen}
    <div class="replace-row">
      <input
        class="q-input"
        bind:this={replaceEl}
        bind:value={replacement}
        placeholder="替换为(支持 $1、$&#123;name&#125;)"
        onkeydown={(e) => e.key === 'Enter' && doPreview()}
      />
      <button class="mini-btn" disabled={!query.trim() || searching} onclick={doPreview}>
        预览替换
      </button>
    </div>
  {/if}

  {#if error}
    <div class="banner err" role="alert">{error}</div>
  {/if}

  {#if applyResult}
    <div class="banner ok">
      <span>
        已替换 {applyResult.appliedFiles} 个文件 {applyResult.appliedMatches} 处
        {#if applyResult.failed?.length}
          ,{applyResult.failed.length} 个文件失败
        {/if}
      </span>
      {#if applyResult.failed?.length}
        <div class="failed">
          {#each applyResult.failed as f}
            <div class="failed-line">⚠ {f.path}: {f.reason}</div>
          {/each}
        </div>
      {/if}
      <button class="dismiss" title="关闭提示" onclick={() => (applyResult = null)}>×</button>
    </div>
  {/if}

  {#if previewData}
    <ReplacePreview
      preview={previewData}
      busy={applyBusy}
      onapply={doApply}
      onclose={() => (previewData = null)}
    />
  {/if}

  {#if searching}
    <div class="status muted">搜索中…</div>
  {:else if result}
    <div class="status">
      {result.fileCount} 个文件 {result.totalMatches} 处匹配
      <span class="muted">· 已扫 {result.searchedFiles} · {result.elapsedMs}ms</span>
    </div>
    {#if result.truncated}
      <div class="banner warn">已达上限 {maxResults},结果已截断——可提高 search.maxResults 或缩小范围</div>
    {/if}
    {#if result.timedOut}
      <div class="banner warn">搜索超时(30s),结果不完整</div>
    {/if}
    <div class="results">
      {#each result.groups as g (g.path)}
        <div class="group">
          <button class="group-head" title="折叠/展开" onclick={() => toggleGroup(g.path)}>
            <span class="chev">{collapsed.has(g.path) ? '▸' : '▾'}</span>
            <span class="fname">{g.path.split('/').pop()}</span>
            <span class="fpath">{g.path}</span>
            <span class="count">{g.hits.length}</span>
          </button>
          {#if !collapsed.has(g.path)}
            {#each g.hits as h}
              {@const s = seg(h.preview, h.col, h.endCol)}
              <button class="hit" onclick={() => hitClick(g, h)}>
                <span class="ln">{h.line}</span>
                <span class="preview">{s.before}<mark>{s.mark}</mark>{s.after}</span>
              </button>
            {/each}
          {/if}
        </div>
      {/each}
      {#if result.groups.length === 0}
        <div class="empty muted">无匹配结果</div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .search-panel {
    width: 260px;
    flex-shrink: 0;
    background: var(--sidebar-bg);
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .panel-head {
    padding: var(--sp-sm) var(--sp-md);
    font-size: var(--fs-xs);
    font-weight: var(--fw-sb);
    color: var(--sidebar-text);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    flex-shrink: 0;
  }
  .query-row {
    position: relative;
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    padding: 0 var(--sp-sm);
    flex-shrink: 0;
  }
  .q-input {
    flex: 1;
    min-width: 0;
    background: var(--bg-input);
    border: 1px solid var(--border-strong);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: var(--fs-sm);
    padding: 5px var(--sp-sm);
  }
  .q-input:focus {
    outline: none;
    border-color: var(--brand);
  }
  .hist-btn {
    color: var(--text-muted);
    font-size: var(--fs-sm);
    padding: 2px var(--sp-xs);
    flex-shrink: 0;
  }
  .hist-btn:hover {
    color: var(--text-primary);
  }
  .hist-list {
    position: absolute;
    top: 100%;
    left: var(--sp-sm);
    right: var(--sp-sm);
    z-index: 30;
    background: var(--bg-card);
    border: 1px solid var(--border-strong);
    border-radius: var(--r-sm);
    box-shadow: var(--sh-modal);
    max-height: 40vh;
    overflow-y: auto;
  }
  .hist-item {
    display: block;
    width: 100%;
    text-align: left;
    font-size: var(--fs-xs);
    font-family: var(--font-mono);
    color: var(--text-primary);
    padding: var(--sp-xs) var(--sp-sm);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .hist-item:hover {
    background: var(--bg-hover);
  }
  .toggles {
    display: flex;
    gap: var(--sp-xs);
    padding: var(--sp-xs) var(--sp-sm);
    flex-shrink: 0;
  }
  .tg {
    min-width: 28px;
    height: 22px;
    font-size: var(--fs-xs);
    font-family: var(--font-mono);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    transition: all var(--tr-fast);
  }
  .tg:hover {
    color: var(--text-primary);
    background: var(--bg-hover);
  }
  .tg.on {
    color: #fff;
    background: var(--brand);
    border-color: var(--brand);
  }
  .glob-input {
    margin: 0 var(--sp-sm) var(--sp-xs);
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: var(--fs-xs);
    padding: 3px var(--sp-sm);
  }
  .glob-input:focus {
    outline: none;
    border-color: var(--brand);
  }
  .replace-toggle {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    padding: var(--sp-xs) var(--sp-sm);
    font-size: var(--fs-xs);
    color: var(--sidebar-text);
    text-align: left;
    flex-shrink: 0;
  }
  .replace-toggle:hover {
    color: var(--text-primary);
  }
  .chev {
    color: var(--text-muted);
    width: 12px;
    flex-shrink: 0;
  }
  .replace-row {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    padding: 0 var(--sp-sm) var(--sp-xs);
    flex-shrink: 0;
  }
  .mini-btn {
    font-size: var(--fs-xs);
    padding: 4px var(--sp-sm);
    border-radius: var(--r-sm);
    background: var(--brand);
    color: #fff;
    flex-shrink: 0;
  }
  .mini-btn:hover:not(:disabled) {
    background: var(--brand-hover);
  }
  .mini-btn:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .banner {
    margin: 0 var(--sp-sm) var(--sp-xs);
    padding: var(--sp-xs) var(--sp-sm);
    border-radius: var(--r-sm);
    font-size: var(--fs-xs);
    position: relative;
    word-break: break-all;
  }
  .banner.err {
    background: var(--danger-bg);
    color: var(--danger);
    border: 1px solid rgba(231, 76, 60, 0.35);
  }
  .banner.ok {
    background: var(--success-bg);
    color: var(--success);
    border: 1px solid rgba(46, 204, 113, 0.35);
  }
  .banner.warn {
    background: var(--warning-bg);
    color: var(--warning);
    border: 1px solid rgba(243, 156, 18, 0.35);
  }
  .dismiss {
    position: absolute;
    top: 0;
    right: var(--sp-xs);
    color: inherit;
    font-size: var(--fs-sm);
    line-height: 1;
  }
  .failed {
    margin-top: var(--sp-xs);
  }
  .failed-line {
    color: var(--warning);
    word-break: break-all;
  }
  .status {
    padding: var(--sp-xs) var(--sp-sm);
    font-size: var(--fs-xs);
    color: var(--text-primary);
    flex-shrink: 0;
  }
  .muted {
    color: var(--text-muted);
  }
  .results {
    flex: 1;
    overflow-y: auto;
    min-height: 0;
    padding-bottom: var(--sp-sm);
  }
  .group-head {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    width: 100%;
    padding: var(--sp-xs) var(--sp-sm);
    text-align: left;
  }
  .group-head:hover {
    background: var(--sidebar-hover);
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
  .hit {
    display: flex;
    gap: var(--sp-sm);
    width: 100%;
    padding: 1px var(--sp-sm) 1px var(--sp-md);
    font-family: var(--font-mono);
    font-size: var(--fs-xs);
    text-align: left;
  }
  .hit:hover {
    background: var(--sidebar-hover);
  }
  .hit .ln {
    color: var(--text-muted);
    min-width: 32px;
    text-align: right;
    flex-shrink: 0;
  }
  .hit .preview {
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  mark {
    background: var(--brand-bg);
    color: var(--brand-light);
    border-radius: 2px;
    padding: 0 1px;
  }
  .empty {
    padding: var(--sp-md);
    font-size: var(--fs-xs);
    text-align: center;
  }
</style>
