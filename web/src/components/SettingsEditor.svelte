<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 设置页(schema 驱动表单 + 命令面板式交互):
     搜索(支持 @modified 过滤令牌) / 分类分组 / 四类控件(改即存) /
     「已修改」指示(点击=重置回默认) / 快照保留期收编行(原工作台配置端点) /
     LLM 配置只读区(脱敏快照,编辑引导至启动配置文件)。
     serve 不可达 → 缓存回放 + 只读黄条。 -->
<script>
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import { settingsState, setSetting } from '../lib/settings.js';
  import { getWorkbenchConfig, putWorkbenchConfig, getLlmStatus } from '../lib/api.js';
  import { openSettingsJson } from '../lib/stores.js';

  let query = '';
  /** 数组控件展开态(键 ID) */
  let expandedKey = null;
  /** 数组控件 JSON 草稿与错误 */
  let arrayDraft = '';
  let arrayError = '';
  /** 每键保存错误(失败 toast 后回滚控件值;成功即清) */
  let keyErrors = {};

  // 快照保留期(「工作台」分类特殊行;端点与消费通道零改动)
  let retention = null;
  let retentionMsg = '';

  // LLM 配置只读快照(fail-soft:null = 不渲染该区)
  let llmStatus = null;

  $: degraded = $settingsState.degraded;
  $: entries = $settingsState.entries;
  $: sources = $settingsState.sources;
  $: settings = $settingsState.settings;

  // 搜索解析:'@modified' 令牌(大小写不敏感)+ 自由文本(键/描述)。
  // 过滤逻辑内联进分组推导(Svelte 响应式只追踪块内直接引用,函数内依赖不触发重算)
  $: modifiedOnly = /(^|\s)@modified(\s|$)/i.test(query);
  $: textQuery = query.replace(/(^|\s)@modified(\s|$)/gi, ' ').trim().toLowerCase();

  // 分类分组(保持 schema 下发顺序)
  $: categories = (() => {
    const map = new Map();
    for (const e of entries) {
      if (modifiedOnly && (sources[e.key] || 'default') === 'default') continue;
      if (textQuery) {
        const hit =
          e.key.toLowerCase().includes(textQuery) ||
          (e.description || '').toLowerCase().includes(textQuery);
        if (!hit) continue;
      }
      if (!map.has(e.category)) map.set(e.category, []);
      map.get(e.category).push(e);
    }
    return [...map.entries()];
  })();

  /** 已修改 = 显式设置过(user/workspace 层生效);点击重置回落默认/工作区值 */
  function isModified(key) {
    return (sources[key] || 'default') !== 'default';
  }

  function sourceLabel(key) {
    const s = sources[key] || 'default';
    if (s === 'user') return '用户';
    if (s === 'workspace') return '工作区';
    return '默认';
  }

  async function changeSetting(key, value, el, isBoolean = false) {
    keyErrors = { ...keyErrors, [key]: '' };
    try {
      await setSetting(key, value);
      if (expandedKey === key) {
        expandedKey = null;
        arrayError = '';
      }
    } catch (e) {
      keyErrors = { ...keyErrors, [key]: String(e?.message || e) };
      // 回滚控件到 store 权威值(store 未变,DOM 手动复位)
      if (el) {
        const v = get(settingsState).settings[key];
        if (isBoolean) el.checked = !!v;
        else el.value = v ?? '';
      }
    }
  }

  function openArrayEditor(entry) {
    expandedKey = entry.key;
    arrayError = '';
    arrayDraft = JSON.stringify(settings[entry.key] ?? [], null, 2);
  }

  async function applyArray(entry) {
    let parsed;
    try {
      parsed = JSON.parse(arrayDraft);
    } catch (e) {
      arrayError = `JSON 解析失败:${String(e?.message || e)}`;
      return;
    }
    if (!Array.isArray(parsed)) {
      arrayError = '内容必须是 JSON 数组';
      return;
    }
    await changeSetting(entry.key, parsed, null);
  }

  onMount(() => {
    getWorkbenchConfig()
      .then((c) => (retention = c.retention))
      .catch(() => {});
    getLlmStatus()
      .then((s) => (llmStatus = s))
      .catch(() => (llmStatus = null));
  });

  async function changeRetention(e) {
    const v = e.target.value;
    retentionMsg = '';
    try {
      const res = await putWorkbenchConfig(v);
      retention = res.retention;
      retentionMsg = '已保存';
    } catch (err) {
      retentionMsg = String(err?.message || err);
      e.target.value = retention ?? '3m';
    }
  }

  const RETENTION_OPTIONS = [
    ['1d', '1 天'],
    ['1m', '1 个月'],
    ['3m', '3 个月'],
    ['6m', '半年'],
    ['1y', '1 年'],
    ['forever', '长期保留'],
  ];
</script>

<div class="settings">
  <div class="head">
    <div class="head-row">
      <input
        class="search"
        type="text"
        placeholder="搜索设置(支持 @modified 过滤已修改项)"
        bind:value={query}
      />
      <div class="json-links">
        <button class="btn" title="以 JSON 编辑用户层设置(保存即生效)" onclick={() => openSettingsJson('user')}>
          打开 settings.json
        </button>
        <button
          class="btn"
          title="编辑工作区层设置文件(.evo/settings.json,覆盖用户层)"
          onclick={() => openSettingsJson('workspace')}
        >
          工作区 settings.json
        </button>
      </div>
    </div>
    {#if degraded}
      <div class="degraded">服务不可达,当前显示缓存值(只读);恢复连接后自动解除</div>
    {/if}
  </div>

  <div class="body">
    {#if entries.length === 0}
      <div class="empty">
        {degraded ? '设置不可用:服务未连接且本地无缓存。' : '设置加载中…'}
      </div>
    {:else}
      {#each categories as [cat, items] (cat)}
        <section class="cat">
          <h3 class="cat-title">{cat}</h3>

          {#each items as entry (entry.key)}
            <div class="row" class:modified={isModified(entry.key)}>
              <div class="row-info">
                <div class="row-key mono">
                  {entry.key}
                  {#if isModified(entry.key)}
                    <button
                      class="reset"
                      title={`重置为默认值(当前生效层:${sourceLabel(entry.key)})`}
                      onclick={() => changeSetting(entry.key, null, null)}>已修改 · 重置</button
                    >
                  {/if}
                </div>
                <div class="row-desc">{entry.description}</div>
                {#if keyErrors[entry.key]}
                  <div class="row-err">{keyErrors[entry.key]}</div>
                {/if}
              </div>
              <div class="row-ctl" class:readonly={degraded}>
                {#if entry.type === 'boolean'}
                  <label class="chk">
                    <input
                      type="checkbox"
                      checked={!!settings[entry.key]}
                      disabled={degraded}
                      onchange={(e) => changeSetting(entry.key, e.target.checked, e.target, true)}
                    />
                    <span>{settings[entry.key] ? '开启' : '关闭'}</span>
                  </label>
                {:else if entry.type === 'enum'}
                  <select
                    value={settings[entry.key]}
                    disabled={degraded}
                    onchange={(e) => changeSetting(entry.key, e.target.value, e.target)}
                  >
                    {#each entry.enum_values || [] as opt (opt)}
                      <option value={opt}>{opt}</option>
                    {/each}
                  </select>
                {:else if entry.type === 'number'}
                  <input
                    class="num"
                    type="number"
                    value={settings[entry.key]}
                    min={entry.range ? entry.range[0] : undefined}
                    max={entry.range ? entry.range[1] : undefined}
                    disabled={degraded}
                    onchange={(e) => changeSetting(entry.key, Number(e.target.value), e.target)}
                  />
                {:else if entry.type === 'array'}
                  {#if expandedKey === entry.key}
                    <div class="array-editor">
                      <textarea
                        class="array-json mono"
                        rows="8"
                        bind:value={arrayDraft}
                        disabled={degraded}
                        spellcheck="false"
                      ></textarea>
                      {#if arrayError}<div class="row-err">{arrayError}</div>{/if}
                      <div class="array-actions">
                        <button class="btn primary" disabled={degraded} onclick={() => applyArray(entry)}>应用</button>
                        <button class="btn" onclick={() => (expandedKey = null)}>取消</button>
                      </div>
                    </div>
                  {:else}
                    <button class="btn" disabled={degraded} onclick={() => openArrayEditor(entry)}>
                      编辑列表({(settings[entry.key] || []).length} 项)
                    </button>
                  {/if}
                {:else}
                  <input
                    class="txt"
                    type="text"
                    value={settings[entry.key] ?? ''}
                    disabled={degraded}
                    onchange={(e) => changeSetting(entry.key, e.target.value, e.target)}
                  />
                {/if}
                <span class="src">生效层:{sourceLabel(entry.key)}</span>
              </div>
            </div>
          {/each}

          {#if cat === '工作台'}
            <div class="row">
              <div class="row-info">
                <div class="row-key">会话快照保留期</div>
                <div class="row-desc">超过保留期的会话快照由服务端清理任务回收</div>
                {#if retentionMsg}<div class="row-ok">{retentionMsg}</div>{/if}
              </div>
              <div class="row-ctl" class:readonly={degraded}>
                <select value={retention ?? '3m'} disabled={degraded} onchange={changeRetention}>
                  {#each RETENTION_OPTIONS as [v, label] (v)}
                    <option value={v}>{label}</option>
                  {/each}
                </select>
              </div>
            </div>
          {/if}
        </section>
      {/each}
    {/if}

    {#if llmStatus}
      <section class="cat llm">
        <h3 class="cat-title">AI 服务状态(只读)</h3>
        <div class="llm-grid">
          <span class="llm-k">状态</span>
          <span class="llm-v">{llmStatus.configured ? '已配置' : '未配置(功能降级运行)'}</span>
          <span class="llm-k">提供方</span>
          <span class="llm-v">{llmStatus.provider || '—'}</span>
          <span class="llm-k">模型</span>
          <span class="llm-v mono">{llmStatus.model || '—'}</span>
          <span class="llm-k">端点</span>
          <span class="llm-v mono">{llmStatus.api_base || '—'}</span>
          <span class="llm-k">密钥</span>
          <span class="llm-v">
            {llmStatus.api_key?.present
              ? `已配置(尾号 ${llmStatus.api_key.hint || '****'},来源 ${llmStatus.api_key.source || 'config'})`
              : '未配置'}
          </span>
        </div>
        <div class="llm-note">密钥不出现在本页;如需更换模型或密钥,请编辑启动配置文件后重启服务。</div>
      </section>
    {/if}
  </div>
</div>

<style>
  .settings {
    height: 100%;
    display: flex;
    flex-direction: column;
    background: var(--bg-card);
  }
  .head {
    flex-shrink: 0;
    padding: var(--sp-md);
    border-bottom: 1px solid var(--border);
  }
  .head-row {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
  }
  .json-links {
    display: flex;
    gap: var(--sp-xs);
    flex-shrink: 0;
  }
  .search {
    flex: 1;
    min-width: 0;
    box-sizing: border-box;
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: var(--fs-sm);
    padding: var(--sp-sm);
    outline: none;
  }
  .search:focus {
    border-color: var(--brand);
  }
  .search::placeholder {
    color: var(--text-muted);
  }
  .degraded {
    margin-top: var(--sp-sm);
    font-size: var(--fs-xs);
    color: var(--warning, #fbbf24);
    background: var(--warning-bg, rgba(251, 191, 36, 0.08));
    border: 1px solid var(--warning, #fbbf24);
    border-radius: var(--r-sm);
    padding: var(--sp-xs) var(--sp-sm);
  }
  .body {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: var(--sp-md);
  }
  .empty {
    color: var(--text-muted);
    font-size: var(--fs-sm);
    padding: var(--sp-xl) 0;
    text-align: center;
  }
  .cat {
    margin-bottom: var(--sp-lg);
  }
  .cat-title {
    font-size: var(--fs-sm);
    font-weight: var(--fw-sb);
    color: var(--brand);
    border-bottom: 1px solid var(--border);
    padding-bottom: var(--sp-xs);
    margin-bottom: var(--sp-sm);
  }
  .row {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--sp-md);
    padding: var(--sp-sm) var(--sp-sm);
    border-radius: var(--r-sm);
  }
  .row:hover {
    background: var(--bg-hover);
  }
  .row.modified {
    box-shadow: inset 2px 0 0 var(--brand);
  }
  .row-info {
    flex: 1;
    min-width: 0;
  }
  .row-key {
    font-size: var(--fs-xs);
    color: var(--text-primary);
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
  }
  .row-desc {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    margin-top: 2px;
  }
  .row-err {
    font-size: var(--fs-xs);
    color: var(--danger);
    margin-top: 2px;
  }
  .row-ok {
    font-size: var(--fs-xs);
    color: var(--success, #4ade80);
    margin-top: 2px;
  }
  .reset {
    font-size: 10px;
    border: 1px solid var(--brand);
    color: var(--brand);
    background: transparent;
    border-radius: 3px;
    padding: 0 6px;
    cursor: pointer;
    flex-shrink: 0;
  }
  .reset:hover {
    background: var(--brand);
    color: #fff;
  }
  .row-ctl {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 4px;
    flex-shrink: 0;
  }
  .row-ctl select,
  .row-ctl .num,
  .row-ctl .txt {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: var(--fs-xs);
    padding: 3px var(--sp-sm);
    outline: none;
    min-width: 140px;
  }
  .row-ctl select:focus,
  .row-ctl .num:focus,
  .row-ctl .txt:focus {
    border-color: var(--brand);
  }
  .row-ctl.readonly select,
  .row-ctl.readonly .num,
  .row-ctl.readonly .txt,
  .row-ctl.readonly .btn,
  .row-ctl.readonly .chk {
    opacity: 0.5;
  }
  .chk {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    cursor: pointer;
  }
  .src {
    font-size: 10px;
    color: var(--text-muted);
  }
  .array-editor {
    display: flex;
    flex-direction: column;
    gap: var(--sp-xs);
    align-items: flex-end;
  }
  .array-json {
    width: 360px;
    max-width: 46vw;
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-primary);
    font-size: var(--fs-xs);
    padding: var(--sp-xs);
    outline: none;
    resize: vertical;
  }
  .array-json:focus {
    border-color: var(--brand);
  }
  .array-actions {
    display: flex;
    gap: var(--sp-xs);
  }
  .btn {
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-xs);
    padding: 3px 12px;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .btn:hover {
    color: var(--text-primary);
    border-color: var(--brand);
  }
  .btn.primary {
    background: var(--brand);
    border-color: var(--brand);
    color: #fff;
  }
  .btn.primary:hover {
    background: var(--brand-hover);
  }
  .llm-grid {
    display: grid;
    grid-template-columns: 72px 1fr;
    gap: 4px var(--sp-md);
    font-size: var(--fs-xs);
    padding: var(--sp-sm);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
  }
  .llm-k {
    color: var(--text-muted);
  }
  .llm-v {
    color: var(--text-primary);
    word-break: break-all;
  }
  .llm-note {
    margin-top: var(--sp-xs);
    font-size: 10px;
    color: var(--text-muted);
  }
</style>
