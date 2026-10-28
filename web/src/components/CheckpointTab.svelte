<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 检查点 tab:用户手动存档+引擎版本回滚(人类持剑——所有写操作需显式
     点击,二次确认后执行;活跃轮由后端 409 拒绝,前端只呈消息)。
     打点=transcript 快照+当前引擎版本锚定;恢复=引擎回滚到打点锚定版本
     并取回打点时刻消息;回滚=引擎 fact 链版本回退。二者均不可自动撤销,
     恢复/回滚按钮采用两段式确认(首击变确认态,3 秒未再击自动回弹)。 -->
<script>
  import { sessionId } from '../lib/stores.js';
  import {
    listSessionVersions,
    rewindSession,
    listCheckpoints,
    createCheckpoint,
    restoreCheckpoint,
  } from '../lib/api.js';

  let checkpoints = $state([]);
  let versions = $state([]);
  let latest = $state(0);
  let loading = $state(false);
  let err = $state('');
  let note = $state('');
  let label = $state('');
  let busy = $state(false);
  // 两段式确认:待确认的操作 key('restore:{id}' / 'rewind:{version}'),3 秒回弹
  let armed = $state(null);
  let armTimer = $state(null);
  // 恢复结果摘要(最近一次恢复/回滚)
  let lastResult = $state(null);

  function disarm() {
    if (armTimer) clearTimeout(armTimer);
    armed = null;
    armTimer = null;
  }

  function arm(key) {
    disarm();
    armed = key;
    armTimer = setTimeout(disarm, 3000);
  }

  function fmtTime(ts) {
    if (!ts) return '—';
    const d = new Date(Number(ts) * 1000);
    const p = (n) => String(n).padStart(2, '0');
    return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
  }

  function defaultLabel() {
    const d = new Date();
    const p = (n) => String(n).padStart(2, '0');
    return `检查点 ${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
  }

  async function refresh() {
    if (!$sessionId) return;
    loading = true;
    err = '';
    try {
      const [cp, ver] = await Promise.all([listCheckpoints($sessionId), listSessionVersions($sessionId)]);
      checkpoints = cp.checkpoints || [];
      versions = ver.versions || [];
      latest = ver.latest || 0;
    } catch (e) {
      err = String(e?.message || e);
    } finally {
      loading = false;
    }
  }

  async function takeCheckpoint() {
    if (!$sessionId || busy) return;
    busy = true;
    err = '';
    note = '';
    try {
      const r = await createCheckpoint($sessionId, label.trim() || defaultLabel());
      note = `已打点「${r.label}」· 引擎版本 v${r.engine_version} · ${r.count} 条消息`;
      label = '';
      await refresh();
    } catch (e) {
      err = String(e?.message || e);
    } finally {
      busy = false;
    }
  }

  async function doRestore(cp) {
    if (busy) return;
    busy = true;
    err = '';
    note = '';
    try {
      const r = await restoreCheckpoint($sessionId, cp.cp_id);
      lastResult = {
        kind: 'restore',
        label: r.label,
        version: r.restored_version,
        count: (r.messages || []).length,
      };
      note = `已恢复「${r.label ?? cp.label}」→ 引擎版本 v${r.restored_version}(对话视图重新打开会话后同步)`;
      disarm();
      await refresh();
    } catch (e) {
      err = String(e?.message || e);
    } finally {
      busy = false;
    }
  }

  async function doRewind(v) {
    if (busy) return;
    busy = true;
    err = '';
    note = '';
    try {
      const r = await rewindSession($sessionId, v);
      lastResult = { kind: 'rewind', version: r.actual_version };
      note = `已回滚到引擎版本 v${r.actual_version}`;
      disarm();
      await refresh();
    } catch (e) {
      err = String(e?.message || e);
    } finally {
      busy = false;
    }
  }

  function onRestoreClick(cp) {
    const key = `restore:${cp.cp_id}`;
    if (armed === key) return doRestore(cp);
    arm(key);
  }

  function onRewindClick(v) {
    const key = `rewind:${v.version}`;
    if (armed === key) return doRewind(v);
    arm(key);
  }

  $effect(() => {
    // 会话切换即刷新;无会话保持空态引导
    if ($sessionId) refresh();
    return () => disarm();
  });
</script>

<div class="ckpt">
  <div class="bar">
    <input
      class="label-input"
      placeholder={defaultLabel()}
      bind:value={label}
      onkeydown={(e) => e.key === 'Enter' && takeCheckpoint()}
      disabled={busy || !$sessionId}
    />
    <button class="take" onClick={takeCheckpoint} disabled={busy || !$sessionId}>打点</button>
    <button class="refresh" onClick={refresh} disabled={busy || !$sessionId}>刷新</button>
  </div>
  {#if !$sessionId}
    <div class="empty">未选择会话——检查点属于具体会话,先在左侧选择或开始一个新会话。</div>
  {:else}
    {#if err}<div class="err">{err}</div>{/if}
    {#if note}<div class="note">{note}</div>{/if}

    <div class="section-title">检查点({checkpoints.length})</div>
    {#if checkpoints.length === 0}
      <div class="empty">暂无检查点——输入标签或直接「打点」,任意时点存档当前对话与引擎状态。</div>
    {:else}
      <ul class="list">
        {#each checkpoints as cp (cp.cp_id)}
          <li>
            <div class="row">
              <span class="label">{cp.label ?? '(未命名)'}</span>
              <span class="meta">{fmtTime(cp.saved_at)} · {cp.count} 条 · 引擎 v{cp.engine_version ?? '—'}</span>
              <button
                class="danger"
                class:armed={armed === `restore:${cp.cp_id}`}
                onClick={() => onRestoreClick(cp)}
                disabled={busy}
              >
                {armed === `restore:${cp.cp_id}` ? '确认恢复?' : '恢复'}
              </button>
            </div>
          </li>
        {/each}
      </ul>
    {/if}

    <div class="section-title">引擎版本时间线(最新 v{latest})</div>
    {#if versions.length === 0}
      <div class="empty">引擎账为空——回合开始后此处显示可回滚的版本链。</div>
    {:else}
      <ul class="list versions">
        {#each [...versions].reverse() as v (v.version)}
          <li>
            <div class="row">
              <span class="label">v{v.version}</span>
              <span class="meta">{v.facts} 条事实</span>
              {#if v.version !== latest}
                <button
                  class="danger"
                  class:armed={armed === `rewind:${v.version}`}
                  onClick={() => onRewindClick(v)}
                  disabled={busy}
                >
                  {armed === `rewind:${v.version}` ? '确认回滚?' : '回滚到此'}
                </button>
              {:else}
                <span class="meta current">当前</span>
              {/if}
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  {/if}
</div>

<style>
  .ckpt {
    padding: 10px 14px;
    font-size: 12px;
    overflow-y: auto;
    height: 100%;
    box-sizing: border-box;
  }
  .bar {
    display: flex;
    gap: 8px;
    margin-bottom: 10px;
  }
  .label-input {
    flex: 1;
    background: var(--bg-input, #1c1c1c);
    color: var(--fg, #ddd);
    border: 1px solid var(--border, #333);
    border-radius: 4px;
    padding: 5px 8px;
    font-size: 12px;
  }
  button {
    background: var(--bg-btn, #2a2a2a);
    color: var(--fg, #ddd);
    border: 1px solid var(--border, #3a3a3a);
    border-radius: 4px;
    padding: 4px 12px;
    font-size: 12px;
    cursor: pointer;
    white-space: nowrap;
  }
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  button.take {
    background: var(--accent, #b8860b);
    border-color: var(--accent, #b8860b);
    color: #111;
    font-weight: 600;
  }
  button.danger.armed {
    background: #8b2020;
    border-color: #c04040;
    color: #fff;
  }
  .section-title {
    margin: 12px 0 6px;
    font-weight: 600;
    color: var(--fg-dim, #999);
    font-size: 11px;
    letter-spacing: 0.04em;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .list li {
    border-bottom: 1px solid var(--border-dim, #262626);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 2px;
  }
  .label {
    min-width: 120px;
    color: var(--fg, #ddd);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .meta {
    color: var(--fg-dim, #888);
    flex: 1;
  }
  .meta.current {
    color: var(--accent, #b8860b);
  }
  .empty {
    color: var(--fg-dim, #888);
    padding: 8px 2px;
  }
  .err {
    color: #e07070;
    padding: 4px 0;
  }
  .note {
    color: #7ec97e;
    padding: 4px 0;
  }
</style>
