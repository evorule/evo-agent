<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 全局搜索面板(B2):模式行(词法/语义/混合)+ 查询区(三 toggle + include/exclude)+
     折叠替换区(仅词法)+ 结果区(词法=文件组行项高亮;语义=chunk 粒度文件组+cosine 百分比;
     混合=文件粒度 RRF 排序+词法/语义命中徽标)+ 状态/截断/超时/错误区 + 历史下拉。
     语义/混合走 /api/index/search(mode 透传);语义面未启用降级标注→引导横幅。
     设置消费:search.* 4 键 + index.semantic.enabled(设置页 schema 自动渲染)。
     命令入口经 searchIntent 一次性信号接线。 -->
<script>
  import { onMount, onDestroy, tick } from 'svelte';
  import { searchIntent, openFile } from '../lib/stores.js';
  import { settingsState } from '../lib/settings.js';
  import { searchGrep, replaceFiles, indexSearch } from '../lib/api.js';
  import ReplacePreview from './ReplacePreview.svelte';

  const HISTORY_KEY = 'evo_search_history';
  const HISTORY_LIMIT = 10;

  let mode = 'lexical'; // lexical | semantic | hybrid
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
  let result = null; // 词法:grep_core 响应(groups/totalMatches/…)
  let semResult = null; // 语义/混合:/api/index/search 响应(results/semantic 标注)
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

  /** 模式切换:清两态结果(响应形态不同,陈旧渲染必错) */
  function setMode(m) {
    if (mode === m) return;
    mode = m;
    result = null;
    semResult = null;
    error = '';
    previewData = null;
    applyResult = null;
    historyOpen = false;
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
      if (mode === 'lexical') {
        result = await searchGrep(baseParams());
        semResult = null;
      } else {
        // 语义/混合:索引面 limit 封顶 100(服务端硬顶),与 search.maxResults 取小
        semResult = await indexSearch(query, Math.min(maxResults, 100), mode);
        result = null;
      }
      collapsed = new Set();
      saveHistory(query);
    } catch (e) {
      result = null;
      semResult = null;
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
    semResult = null;
    error = '';
    previewData = null;
    applyResult = null;
    historyOpen = false;
  }

  /** 语义结果按文件分组(服务端 cosine 降序扁平表 → B2 文件组形态) */
  function groupSemantics(results) {
    const map = new Map();
    for (const h of results || []) {
      let g = map.get(h.file);
      if (!g) {
        g = { path: h.file, hits: [] };
        map.set(h.file, g);
      }
      g.hits.push(h);
    }
    return [...map.values()];
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
    <button class="tg" class:on={mode === 'lexical'} title="符号/文本字面检索" onclick={() => setMode('lexical')}>词法</button>
    <button class="tg" class:on={mode === 'semantic'} title="向量语义检索(按含义近似匹配)" onclick={() => setMode('semantic')}>语义</button>
    <button class="tg" class:on={mode === 'hybrid'} title="词法+语义 RRF 融合排序" onclick={() => setMode('hybrid')}>混合</button>
  </div>

  {#if mode === 'lexical'}
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
  {:else if mode === 'lexical' && result}
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
  {:else if mode === 'semantic' && semResult}
    {#if semResult.semantic === 'disabled'}
      <div class="banner warn">
        语义面未启用:设置 → 工作台 →「语义面向量检索」开启后重启生效(首次启用下载约 24MB 模型)
      </div>
    {/if}
    <div class="status">
      {semResult.results.length} 个 chunk 命中 · {groupSemantics(semResult.results).length} 个文件
      <span class="muted">· cosine 降序</span>
    </div>
    <div class="results">
      {#each groupSemantics(semResult.results) as g (g.path)}
        <div class="group">
          <button class="group-head" title="折叠/展开" onclick={() => toggleGroup(g.path)}>
            <span class="chev">{collapsed.has(g.path) ? '▸' : '▾'}</span>
            <span class="fname">{g.path.split('/').pop()}</span>
            <span class="fpath">{g.path}</span>
            <span class="count">{g.hits.length}</span>
          </button>
          {#if !collapsed.has(g.path)}
            {#each g.hits as h}
              <button class="hit" onclick={() => openFile(g.path, { line: h.line_start })}>
                <span class="ln">{h.line_start}-{h.line_end}</span>
                <span class="preview">
                  {h.symbol || '(文件级)'} <span class="muted">{h.kind}</span>
                </span>
                <span class="score">{Math.round(h.score * 100)}%</span>
              </button>
            {/each}
          {/if}
        </div>
      {/each}
      {#if semResult.results.length === 0}
        <div class="empty muted">无匹配结果</div>
      {/if}
    </div>
  {:else if mode === 'hybrid' && semResult}
    {#if semResult.semantic === 'disabled'}
      <div class="banner warn">
        语义面未启用,已降级纯词法排序:设置 → 工作台 →「语义面向量检索」开启后重启生效
      </div>
    {/if}
    <div class="status">
      {semResult.results.length} 个文件
      <span class="muted">· RRF 融合排序(词法+语义)</span>
    </div>
    <div class="results">
      {#each semResult.results as h (h.file)}
        <button
          class="hit hybrid"
          onclick={() =>
            openFile(h.file, h.semantic_best ? { line: h.semantic_best.line_start } : {})}
        >
          <span class="fname">{h.file.split('/').pop()}</span>
          <span class="fpath">{h.file}</span>
          <span class="chips">
            {#if h.lexical_hits}
              <span class="chip">词 {h.lexical_hits}</span>
            {/if}
            {#if h.semantic_best}
              <span class="chip sem" title="最佳语义 chunk:{h.semantic_best.symbol} ({h.semantic_best.line_start}-{h.semantic_best.line_end})">
                语义 {h.semantic_best.symbol}
              </span>
            {/if}
          </span>
        </button>
      {/each}
      {#if semResult.results.length === 0}
        <div class="empty muted">无匹配结果</div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .search-panel {
    width: var(--sidebar-w, 260px); /* E2:divider 可调,根变量统一侧栏宽 */
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
  .hit .score {
    margin-left: auto;
    color: var(--text-muted);
    flex-shrink: 0;
  }
  .hit.hybrid {
    align-items: center;
  }
  .hit.hybrid .fpath {
    flex: 1;
  }
  .chips {
    display: flex;
    gap: 4px;
    flex-shrink: 0;
  }
  .chip {
    font-family: var(--font-mono);
    color: var(--text-secondary);
    background: var(--bg-hover);
    border-radius: var(--r-full);
    padding: 0 6px;
    max-width: 110px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .chip.sem {
    color: var(--brand-light);
    background: var(--brand-bg);
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
