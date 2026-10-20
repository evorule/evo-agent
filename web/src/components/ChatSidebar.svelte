<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 对话侧栏:与 general agent 的真实流式会话(G16 WS 协议) -->
<script>
  import {
    connStatus,
    sessionId,
    turnActive,
    lastEngineEventAt,
    messages,
    sessions,
    refreshSessions,
    updateMessage,
    artifacts,
    openFile,
    composerMentions,
    activePath,
    openSettingsTab,
  } from '../lib/stores.js';
  import { sendMessage, interrupt, newSession, openSession, resumeSession } from '../lib/ws.js';
  import { approveProposal, listResumableRuns, getOrchestrationLinks, uploadFile, getLlmModels, setLlmModel } from '../lib/api.js';
  import { orderSessionsWithLineage } from '../lib/orchestration.js';
  import { onMount, onDestroy, tick } from 'svelte';
  import { registerCommand, unregisterCommand } from '../lib/commands.js';
  import { collectCandidates, MENTION_GROUP_LABELS, estimateComposerTokens, tokenLevel } from '../lib/mentions.js';
  import { highlightSegments } from '../lib/fuzzy.js';
  import { registerMentionProviders, reconcileMentions } from '../lib/mentionProviders.js';
  import {
    registerMentionSourceProviders,
    getActiveSelection,
    activeTerminalRef,
    problemsTotal,
  } from '../lib/mentionSources.js';

  // Svelte 5:组件用了 $effect 等 rune 即进入 runes 模式,
  // 顶层状态必须 $state() 声明,普通 let 不具备响应性(历史面板/发送按钮会失效)
  let draft = $state('');
  let listEl = $state(null);
  let showHistory = $state(false);
  // 可恢复崩溃会话集合(扫尾端点投影;与历史索引交集后展示「恢复」入口)
  let resumableIds = $state(new Set());
  // B25 谱系归属(全局归属链投影):子会话在列表内缩进 + 「委托自」徽标
  let lineageLinks = $state([]);
  // C 批「会话级 workspace」:当前工作区(连接级 ?workspace= 携带;
  // localStorage 持久,改值在下次连接生效——新建会话/换线/重连)
  let workspace = $state(localStorage.getItem('evo_workspace') || '');

  /** 编辑会话工作区:prompt 输入绝对路径,空串清除。立即持久化;对当前
   *  WS 连接不回溯生效(升级期已定型),提示条明示下次连接生效。 */
  function editWorkspace() {
    const next = prompt(
      '会话工作区(绝对路径,agent 读写以此目录为边界):\n留空恢复缺省(serve workdir)',
      workspace,
    );
    if (next === null) return; // 取消
    const v = next.trim();
    if (v && !/^[a-zA-Z]:[\\/]/.test(v) && !v.startsWith('\\\\') && !v.startsWith('/')) {
      alert('需要绝对路径(如 D:\\projects\\demo)');
      return;
    }
    workspace = v;
    if (v) localStorage.setItem('evo_workspace', v);
    else localStorage.removeItem('evo_workspace');
  }

  /** 工作区按钮显示名:末段路径(长路径收窄);缺省态显式标示 */
  function workspaceLabel() {
    if (!workspace) return '工作区:缺省';
    const parts = workspace.split(/[\\/]/).filter(Boolean);
    return `工作区:${parts[parts.length - 1] || workspace}`;
  }

  // ---- S6 输入历史召回 + 草稿持久化(对齐主流 IDE 输入框) ----
  // 空输入 ↑ 逐条召回已发送历史(0=最新),↓ 回退,至 -1 还原在途草稿;
  // 未发送草稿(含引用 chip)localStorage 持久,刷新不丢。
  const DRAFT_KEY = 'evo_chat_draft';
  const MENTIONS_KEY = 'evo_chat_mentions';
  const HISTORY_KEY = 'evo_chat_history';
  const HISTORY_MAX = 50;

  function loadDraftHistory() {
    try {
      const raw = JSON.parse(localStorage.getItem(HISTORY_KEY) || '[]');
      return Array.isArray(raw)
        ? raw.filter((x) => typeof x === 'string').slice(0, HISTORY_MAX)
        : [];
    } catch {
      return [];
    }
  }

  let draftHistory = $state(loadDraftHistory());
  let histIdx = $state(-1); // -1=草稿态;>=0=历史召回游标
  let recallBase = ''; // 召回前的在途草稿暂存(非响应式)

  function persistDraft() {
    if (draft) {
      localStorage.setItem(DRAFT_KEY, draft);
      try {
        localStorage.setItem(
          MENTIONS_KEY,
          JSON.stringify(
            $composerMentions.map(({ type, ref, token }) => ({ type, ref, token })),
          ),
        );
      } catch {
        // 存储满等异常 fail-soft:草稿正文仍已持久
      }
    } else {
      localStorage.removeItem(DRAFT_KEY);
      localStorage.removeItem(MENTIONS_KEY);
    }
  }

  function onInput() {
    if (histIdx !== -1) histIdx = -1;
    persistDraft();
    detectMention();
  }

  // S1 自动增高:高度随内容伸缩,超过上限(约 8 行)内部滚动
  const DRAFT_MAX_H = 176;
  $effect(() => {
    draft;
    const el = draftEl;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(el.scrollHeight, DRAFT_MAX_H)}px`;
  });

  // ---- S3 构建/对话模式(前端姿态+提示语;审批链语义不动) ----
  // 构建=默认,agent 执行任务;对话=仅问答咨询,wire 附提示块引导先确认后变更。
  const MODE_KEY = 'evo_chat_mode';
  let chatMode = $state(localStorage.getItem(MODE_KEY) === 'chat' ? 'chat' : 'build');

  function setMode(m) {
    if ($turnActive || chatMode === m) return;
    chatMode = m;
    localStorage.setItem(MODE_KEY, m);
  }

  // ---- S4 附件上传(图片/文件;落盘 .evo-agent/uploads,agent 同沙箱可读) ----
  // 附件不入草稿持久化(刷新后需重选);发送仅携带已上传成功件
  const ATT_MAX_BYTES = 10 * 1024 * 1024;
  let fileInputEl = $state(null);
  let attachments = $state([]);
  let uploadSeq = 0;

  function pickFiles() {
    if ($turnActive) return;
    fileInputEl?.click();
  }

  function fileToBase64(file) {
    return new Promise((resolve, reject) => {
      const r = new FileReader();
      r.onload = () => {
        const s = String(r.result);
        resolve(s.slice(s.indexOf(',') + 1));
      };
      r.onerror = () => reject(new Error('读取失败'));
      r.readAsDataURL(file);
    });
  }

  async function addAttachments(files) {
    for (const f of files) {
      if (!f) continue;
      uploadSeq += 1;
      const id = uploadSeq;
      if (f.size > ATT_MAX_BYTES) {
        attachments = [...attachments, { id, name: f.name, error: '超过 10MB 上限' }];
        continue;
      }
      attachments = [...attachments, { id, name: f.name, uploading: true }];
      try {
        const data = await fileToBase64(f);
        const res = await uploadFile(f.name, data);
        attachments = attachments.map((a) =>
          a.id === id ? { id, name: res.name || f.name, path: res.path, size: res.size, mime: res.mime } : a
        );
      } catch (e) {
        attachments = attachments.map((a) =>
          a.id === id ? { id, name: f.name, error: String(e?.message || e) } : a
        );
      }
    }
  }

  function onFilesChosen(e) {
    const files = [...(e.target?.files || [])];
    e.target.value = '';
    if (files.length > 0) addAttachments(files);
  }

  function removeAttachment(id) {
    attachments = attachments.filter((a) => a.id !== id);
  }

  function onPasteFiles(e) {
    const files = [...(e.clipboardData?.files || [])];
    if (files.length > 0) {
      e.preventDefault();
      addAttachments(files);
    }
  }

  // ---- S5 模型选择器(会话模型运行期热切换;白名单正源=执行体,serve 中继) ----
  // 选择即人工显式配置态,不涉审批链;清单外/执行体不可达由 serve 拒切。
  let modelMenuOpen = $state(false);
  let currentModel = $state('');
  let modelList = $state([]);
  let modelReady = $state(false);
  let modelNote = $state('');
  let modelErr = $state('');
  let modelBusy = $state(false);
  let modelPickEl = $state(null);

  async function refreshModels() {
    try {
      const s = await getLlmModels();
      modelList = Array.isArray(s.models) ? s.models : [];
      modelReady = !!s.ready;
      modelNote = s.note || '';
      if (s.current) currentModel = s.current;
      if (modelErr) modelErr = '';
    } catch (e) {
      modelReady = false;
      modelNote = String(e?.message || e);
    }
  }

  function toggleModelMenu() {
    modelMenuOpen = !modelMenuOpen;
    if (modelMenuOpen) refreshModels();
  }

  async function pickModel(m) {
    if (modelBusy || m === currentModel) {
      modelMenuOpen = false;
      return;
    }
    modelBusy = true;
    modelErr = '';
    try {
      const r = await setLlmModel(m);
      currentModel = r?.status?.model || m;
      modelMenuOpen = false;
    } catch (e) {
      modelErr = String(e?.message || e);
    } finally {
      modelBusy = false;
    }
  }

  function onDocClickCloseModelMenu(e) {
    if (!modelMenuOpen) return;
    if (modelPickEl?.contains(e.target)) return;
    modelMenuOpen = false;
  }

  onMount(() => {
    draft = localStorage.getItem(DRAFT_KEY) || '';
    try {
      const saved = JSON.parse(localStorage.getItem(MENTIONS_KEY) || '[]');
      const chips = [];
      for (const m of Array.isArray(saved) ? saved : []) {
        if (!m || typeof m.token !== 'string' || typeof m.ref !== 'string') continue;
        const at = draft.indexOf(m.token);
        if (at < 0) continue;
        chips.push({ type: m.type, ref: m.ref, insertAt: at, token: m.token });
      }
      if (chips.length > 0) composerMentions.set(chips);
    } catch {
      // 损坏存档 fail-soft:仅恢复正文
    }
    registerMentionProviders();
    registerMentionSourceProviders();
    registerCommand({
      id: 'chat.addContext',
      title: '聊天:插入 @ 上下文引用',
      category: '聊天',
      run: openMentionPopup,
    });
    refreshSessions();
    loadResumable();
    loadLineage();
    // S5:模型选择器初值(当前生效模型;清单懒加载,点开时再拉)
    refreshModels();
    document.addEventListener('click', onDocClickCloseModelMenu);
    // 心跳停滞提示的秒级心跳(纯客户端:turn 活跃且久无引擎事件→琥珀提示;
    // serve 侧心跳 2 窗中止,此处为其可见前兆)
    stallTicker = setInterval(() => {
      nowTick = Date.now();
    }, 1000);
  });

  onDestroy(() => {
    unregisterCommand('chat.addContext');
    if (stallTicker) clearInterval(stallTicker);
    document.removeEventListener('click', onDocClickCloseModelMenu);
  });

  /** 引擎事件停滞秒数(turn 活跃面;<0 = 事件仍新鲜) */
  const stallSeconds = $derived(
    $turnActive ? Math.floor((nowTick - $lastEngineEventAt) / 1000) : 0
  );
  const engineStalled = $derived(stallSeconds >= 10);

  /** 命令入口:聚焦输入框并打开 @ 弹层(空查询=快捷项视角) */
  function openMentionPopup() {
    const el = draftEl;
    if (!el) return;
    el.focus();
    openMention('', el.selectionStart ?? draft.length);
  }

  async function loadResumable() {
    try {
      const res = await listResumableRuns();
      resumableIds = new Set((res.resumable || []).map((r) => r.session_id));
    } catch {
      // fail-soft:扫尾端点不可用时不展示恢复入口(历史继续对话不受影响)
      resumableIds = new Set();
    }
  }

  /** B25 谱系归属链加载(fail-soft:端点不可用 = 列表原样平铺) */
  async function loadLineage() {
    try {
      const res = await getOrchestrationLinks();
      lineageLinks = res.links || [];
    } catch {
      lineageLinks = [];
    }
  }

  /** 列表渲染序:子会话紧跟其父(缩进/徽标按 lineage 标注) */
  const orderedSessions = $derived(orderSessionsWithLineage($sessions, lineageLinks));

  function fmtTime(unix) {
    if (!unix) return '';
    const d = new Date(unix * 1000);
    const pad = (n) => String(n).padStart(2, '0');
    return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }

  function toggleHistory() {
    showHistory = !showHistory;
    if (showHistory) {
      refreshSessions();
      loadResumable();
      loadLineage();
    }
  }

  // 恢复前确认(提示条):挂起审批不恢复的明示是恢复入口的硬性文案
  const RESUME_DISCLOSURE =
    'agent 将加载会话历史并续完中断的轮次;挂起的人工审批不会被恢复,须重走审批。';
  function askResume(sid) {
    const target = sid ? `会话 ${sid}` : '当前会话';
    if (!confirm(`恢复${target}?${RESUME_DISCLOSURE}`)) return;
    resumeSession(sid);
  }

  /** 审计深链:新开 console 审计页并定位到该会话(同源 /console/audit) */
  function openAuditDeepLink(sid) {
    window.open(`/console/audit?session=${sid}`, '_blank', 'noopener,noreferrer');
  }

  // 消息变化时滚动到底部
  $effect(() => {
    $messages;
    if (listEl) {
      requestAnimationFrame(() => listEl.scrollTo({ top: listEl.scrollHeight }));
    }
  });

  // ---- @上下文 mention(B13)----
  // 触发:光标前缀形如 (^|\s)@<query>,300ms 防抖后汇总 provider 候选;
  // 键盘:↑↓ 导航 / Enter 选中 / Esc 关闭;选中即插 @token + 登记元数据(chip)

  let draftEl = $state(null);
  let popupEl = $state(null);
  // 弹层状态:{query, insertAt, groups:[{type,label,items:[{...,idx}]}], truncated, sel}
  let mp = $state(null);
  let mpTimer = null;
  let mpSeq = 0;

  // 心跳停滞提示(秒级 ticker + 停滞判据)
  let stallTicker = null;
  let nowTick = $state(Date.now());

  // 键盘导航与 Enter 只走可用候选;置灰行(状态不可得)仅展示
  const flatCandidates = $derived(mp ? mp.enabled : []);
  const totalRendered = $derived(
    mp ? mp.groups.reduce((n, g) => n + g.items.length, 0) : 0,
  );

  // 选中行滚入可视区
  $effect(() => {
    if (!mp) return;
    mp.sel;
    popupEl?.querySelector('.m-row.sel')?.scrollIntoView({ block: 'nearest' });
  });

  function detectMention() {
    const el = draftEl;
    if (!el) return;
    const pos = el.selectionStart ?? draft.length;
    const m = draft.slice(0, pos).match(/(^|\s)@([^\s@]*)$/);
    if (!m) {
      closeMention();
      return;
    }
    openMention(m[2], pos - m[2].length - 1);
  }

  function openMention(query, insertAt) {
    const seq = ++mpSeq;
    if (mpTimer) clearTimeout(mpTimer);
    mpTimer = setTimeout(async () => {
      try {
        const res = await collectCandidates(query, {});
        if (seq !== mpSeq) return; // 过期响应丢弃
        // 快捷项仅空查询注入(设计口径:空查询显示快捷项;输入中让位检索结果)
        const groups = query ? res.groups : withQuickItems(res.groups);
        let idx = 0;
        const enabled = [];
        for (const g of groups) {
          g.items = g.items.map((it) => {
            const row = { ...it, idx: idx++ };
            if (!row.disabled) enabled.push(row);
            return row;
          });
        }
        mp = { query, insertAt, groups, truncated: res.truncated, sel: 0, enabled };
      } catch {
        mp = null;
      }
    }, 300);
  }

  function closeMention() {
    if (mpTimer) {
      clearTimeout(mpTimer);
      mpTimer = null;
    }
    mpSeq++;
    mp = null;
  }

  // 空查询快捷项:当前文件/当前选区/当前终端/全部问题(状态不可得时置灰)
  function withQuickItems(groups) {
    const put = (item) => {
      const g0 = groups.find((g) => g.type === item.type);
      if (g0) g0.items.unshift(item);
      else
        groups.push({
          type: item.type,
          label: MENTION_GROUP_LABELS[item.type] || item.type,
          items: [item],
        });
    };
    const ap = $activePath;
    put({
      type: 'file',
      ref: ap || 'file',
      label: ap ? ap.split('/').pop() || ap : '文件',
      detail: ap ? `${ap}(当前文件)` : '无打开文件',
      positions: [],
      disabled: !ap,
    });
    const sel = getActiveSelection();
    put({
      type: 'selection',
      ref: sel ? `${sel.path}:${sel.startLine}-${sel.endLine}` : 'selection',
      label: '选区',
      detail: sel ? `${sel.path} 选区 ${sel.endLine - sel.startLine + 1} 行` : '无选区',
      positions: [],
      disabled: !sel,
    });
    const tid = activeTerminalRef();
    put({
      type: 'terminal',
      ref: tid || 'terminal',
      label: '终端',
      detail: tid ? `${tid}(当前终端)` : '无活动终端',
      positions: [],
      disabled: !tid,
    });
    const pn = problemsTotal();
    put({
      type: 'problems',
      ref: pn > 0 ? '*' : 'problems',
      label: '问题',
      detail: pn > 0 ? `全部 ${pn} 条(工作区)` : '无诊断',
      positions: [],
      disabled: pn === 0,
    });
    return groups;
  }

  async function pickCandidate(c) {
    if (!mp || c.disabled) return;
    const token = `@${c.label}`;
    const start = mp.insertAt;
    const cursor = draftEl?.selectionStart ?? draft.length;
    draft = draft.slice(0, start) + token + ' ' + draft.slice(cursor);
    composerMentions.update((arr) => [
      ...arr,
      { type: c.type, ref: c.ref, insertAt: start, token, size: c.size ?? null },
    ]);
    closeMention();
    const caret = start + token.length + 1;
    await tick();
    if (draftEl) {
      draftEl.focus();
      draftEl.selectionStart = draftEl.selectionEnd = caret;
    }
  }

  // chip 删除:同步移除 draft 中的 @token(insertAt 漂移时全局找首处兜底)
  function removeMention(i) {
    const m = $composerMentions[i];
    if (!m) return;
    let d = draft;
    if (m.token) {
      if (d.startsWith(m.token, m.insertAt)) {
        d = d.slice(0, m.insertAt) + d.slice(m.insertAt + m.token.length);
      } else {
        const at = d.indexOf(m.token);
        if (at >= 0) d = d.slice(0, at) + d.slice(at + m.token.length);
      }
    }
    draft = d;
    composerMentions.update((arr) => arr.filter((_, idx) => idx !== i));
  }

  async function submit() {
    const text = draft.trim();
    if (!text || $turnActive) return;
    // 先清输入(防 await 窗口内重复提交),再对账,最后交 ws.js 组装发送
    draft = '';
    closeMention();
    // S6:入历史(去重置顶,封顶 50),清草稿持久化
    draftHistory = [text, ...draftHistory.filter((h) => h !== text)].slice(0, HISTORY_MAX);
    try {
      localStorage.setItem(HISTORY_KEY, JSON.stringify(draftHistory));
    } catch {
      // fail-soft:历史不可持久不影响本次发送
    }
    localStorage.removeItem(DRAFT_KEY);
    localStorage.removeItem(MENTIONS_KEY);
    histIdx = -1;
    const { kept } = await reconcileMentions(text, $composerMentions);
    composerMentions.set([]);
    const okAtts = attachments.filter((a) => a.path);
    attachments = attachments.filter((a) => !a.path);
    sendMessage(text, kept, { mode: chatMode, attachments: okAtts });
  }

  function onKeydown(e) {
    if (mp) {
      const n = flatCandidates.length;
      if (e.key === 'ArrowDown' && n > 0) {
        e.preventDefault();
        mp.sel = (mp.sel + 1) % n;
        return;
      }
      if (e.key === 'ArrowUp' && n > 0) {
        e.preventDefault();
        mp.sel = (mp.sel - 1 + n) % n;
        return;
      }
      if (e.key === 'Enter') {
        e.preventDefault();
        if (n > 0) pickCandidate(flatCandidates[mp.sel]);
        else closeMention();
        return;
      }
      if (e.key === 'Escape') {
        e.preventDefault();
        closeMention();
        return;
      }
    }
    // S6 历史召回:空草稿 ↑ 进历史,召回态 ↑ 更早 / ↓ 回新(至 -1 还原在途草稿)
    if (!mp && draftHistory.length > 0 && (draft === '' || histIdx >= 0)) {
      if (e.key === 'ArrowUp' && !e.shiftKey && !e.ctrlKey && !e.altKey) {
        e.preventDefault();
        if (histIdx === -1) {
          recallBase = draft;
          histIdx = 0;
        } else {
          histIdx = Math.min(histIdx + 1, draftHistory.length - 1);
        }
        draft = draftHistory[histIdx];
        return;
      }
      if (e.key === 'ArrowDown' && !e.shiftKey && histIdx >= 0) {
        e.preventDefault();
        histIdx -= 1;
        draft = histIdx === -1 ? recallBase : draftHistory[histIdx];
        return;
      }
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  }

  // 治理叠加:审批决定走既有 G8 /approve 通道(60s 超时自动拒绝)
  function decideApproval(m, approved) {
    if (!m.proposalId || m.decision === 'sending' || m.decision === 'sent') return;
    if (!$sessionId) {
      updateMessage(m.id, { decision: 'error', errText: '会话未建立,无法提交审批' });
      return;
    }
    updateMessage(m.id, { decision: 'sending', errText: null });
    approveProposal('general', $sessionId, approved, m.proposalId)
      .then(() => updateMessage(m.id, { decision: 'sent', sentApproved: approved }))
      .catch((err) => {
        const status = err?.status;
        const text =
          status === 404
            ? '审批已失效(60 秒超时自动拒绝,或该会话无待审批项)'
            : status === 400
              ? `审批被拒:${String(err?.message || err)}`
              : `提交失败:${String(err?.message || err)}`;
        updateMessage(m.id, { decision: 'error', errText: text });
      });
  }
</script>

<aside class="chat" data-zone="chat">
  <div class="chat-header">
    <span class="title">对话</span>
    <div class="header-actions">
      <button
        class="new-btn ws-btn"
        class:active={!!workspace}
        onclick={editWorkspace}
        title={`会话工作区:${workspace || '缺省(serve workdir)'}\n点击编辑,下次连接(新建会话/换线)生效`}
      >{workspaceLabel()}</button>
      <button class="new-btn" class:active={showHistory} onclick={toggleHistory} title="历史会话">历史</button>
      {#if $sessionId}
        <button class="new-btn" onclick={() => askResume()} title="恢复当前崩溃会话(加载历史并续完中断轮)">恢复</button>
      {/if}
      <button class="new-btn" onclick={newSession} title="断开并新建会话">新建会话</button>
    </div>
  </div>

  {#if showHistory}
    <div class="session-list">
      {#if $sessions.length === 0}
        <div class="session-empty">暂无历史会话</div>
      {:else}
        {#each orderedSessions as s (s.session_id)}
          <div class="session-row" class:lineage={!!s.lineage}>
            <button
              class="session-item"
              class:current={s.session_id === $sessionId}
              onclick={() => openSession(s.session_id)}
              title={s.lineage ? `${s.lineage.display} · ${s.session_id}` : s.session_id}
            >
              <span class="s-title">
                {#if s.lineage}<span class="s-branch">└</span>{/if}
                {s.title || '(无标题)'}
              </span>
              <span class="s-time mono">{fmtTime(s.last_active)}</span>
            </button>
            {#if s.lineage}
              <span class="lineage-badge" title={s.lineage.display}>{s.lineage.display}</span>
            {/if}
            {#if resumableIds.has(s.session_id)}
              <button
                class="resume-btn"
                onclick={() => askResume(s.session_id)}
                title="恢复崩溃会话(挂起审批不恢复,须重走审批)"
              >恢复</button>
            {/if}
            <button
              class="resume-btn audit-btn"
              onclick={() => openAuditDeepLink(s.session_id)}
              title="在 console 审计页打开该会话"
            >审计</button>
          </div>
        {/each}
      {/if}
    </div>
  {/if}

  <div class="msg-list" bind:this={listEl}>
    {#if $messages.length === 0}
      <div class="empty">
        <div class="empty-title">开始与 agent 对话</div>
        <div class="empty-desc">
          {$connStatus === 'online'
            ? '直接输入任务,agent 会调用工具并流式回复。'
            : $connStatus === 'connecting'
              ? '正在建立连接…'
              : '连接已断开,请确认 evo-agent serve 正在运行。'}
        </div>
      </div>
    {/if}
    {#each $messages as m (m.id)}
      {#if m.kind === 'user'}
        <div class="row user">
          <div class="user-col">
            <div class="bubble user-bubble">{m.text}</div>
            {#if (m.mentions && m.mentions.length > 0) || (m.dropped && m.dropped.length > 0)}
              <div class="used-refs">
                <span class="ur-label">引用</span>
                {#each m.mentions as u}
                  <span class="used-chip" title={u.summary}>
                    <span class="ur-kind">{MENTION_GROUP_LABELS[u.type] || u.type}</span>
                    <span class="ur-ref mono">{u.ref}</span>
                  </span>
                {/each}
                {#each m.dropped as d}
                  <span
                    class="used-chip dropped"
                    title="发送时未能取得该引用:{d.reason}"
                  >
                    <span class="ur-kind">{MENTION_GROUP_LABELS[d.type] || d.type}</span>
                    <span class="ur-ref mono">{d.ref}</span>
                    <span class="ur-note">未带上</span>
                  </span>
                {/each}
              </div>
            {/if}
          </div>
        </div>
      {:else if m.kind === 'assistant'}
        <div class="row">
          <div class="bubble assistant-bubble">
            {m.text}<span class="cursor" class:on={m.streaming}></span>
          </div>
        </div>
      {:else if m.kind === 'tool'}
        <div class="tool-card" class:running={m.running}>
          <span class="tool-name mono">{m.name}</span>
          <span class="tool-state">{m.running ? '执行中…' : '完成'}</span>
          {#if m.result !== undefined}
            <pre class="tool-payload mono">{m.result}</pre>
          {/if}
        </div>
      {:else if m.kind === 'info'}
        <div class="row center"><span class="sys mono">{m.text}</span></div>
      {:else if m.kind === 'error'}
        <div class="row"><div class="bubble error-bubble">{m.text}</div></div>
      {:else if m.kind === 'artifact'}
        <div class="artifact-card">
          <div class="ac-head">
            <span class="ac-label">agent 产物</span>
            <span
              class="ac-state"
              class:final={$artifacts.some((a) => a.path === m.path && a.finalizedAt)}
            >
              {$artifacts.some((a) => a.path === m.path && a.finalizedAt) ? '已定稿' : '草稿'}
            </span>
          </div>
          <div class="ac-path mono" title={m.path}>{m.path}({m.bytes} B)</div>
          <button class="ac-open" onclick={() => openFile(m.path)}>在编辑器打开</button>
        </div>
      {:else if m.kind === 'approval'}
        <div class="approval-card">
          <div class="ap-head">需要审批 · {m.toolName}</div>
          <pre class="ap-cmd mono">{m.command}</pre>
          {#if m.alternative}
            <div class="ap-alt">替代方案:{m.alternative}</div>
          {/if}
          <div class="ap-foot">风险等级:{m.risk}</div>
          {#if !m.proposalId}
            <div class="ap-err">缺少提案 ID,无法在工作台审批(请到会话台处理)</div>
          {:else if m.decision === 'sending'}
            <div class="ap-foot">审批提交中…</div>
          {:else if m.decision === 'sent'}
            <div class="ap-ok">已提交{m.sentApproved ? '批准' : '拒绝'},等待 agent 执行结果…</div>
          {:else}
            <div class="ap-actions">
              <button class="ap-btn approve" onclick={() => decideApproval(m, true)}>批准</button>
              <button class="ap-btn reject" onclick={() => decideApproval(m, false)}>拒绝</button>
            </div>
            {#if m.decision === 'error' && m.errText}
              <div class="ap-err">{m.errText}</div>
            {/if}
          {/if}
        </div>
      {/if}
    {/each}
  </div>

  <div class="composer">
    {#if mp}
      <div class="mention-pop" bind:this={popupEl}>
        {#each mp.groups as g (g.type)}
          <div class="m-label">{g.label}</div>
          {#each g.items as c (c.idx)}
            <button
              class="m-row"
              class:sel={mp.sel === c.idx}
              class:dis={c.disabled}
              onmousedown={(e) => e.preventDefault()}
              onclick={() => pickCandidate(c)}
            >
              <span class="m-path mono">
                {#each highlightSegments(c.detail || c.ref, c.positions) as seg}
                  <span class:hit={seg.hit}>{seg.text}</span>
                {/each}
              </span>
              {#if c.warn}
                <span class="m-warn" title="疑似敏感文件(密钥/凭据),引用前请自行确认">⚠</span>
              {/if}
              <span class="m-kind">{MENTION_GROUP_LABELS[c.type] || c.type}</span>
            </button>
          {/each}
        {/each}
        {#if totalRendered === 0}
          <div class="m-empty">无匹配候选</div>
        {/if}
        {#if mp.truncated}
          <div class="m-more">候选较多,仅显示前 50——继续输入可缩小范围</div>
        {/if}
      </div>
    {/if}
    {#if attachments.length > 0}
      <div class="chip-tray">
        {#each attachments as a (a.id)}
          <span
            class="chip"
            class:chip-err={a.error}
            title={a.error || `${a.name} · ${((a.size || 0) / 1024).toFixed(1)}KB · ${a.path || '上传中'}`}
          >
            <span class="chip-kind">附件</span>
            <span class="chip-ref mono">
              {a.uploading ? '上传中…' : a.error ? `失败:${a.error}` : a.name}
            </span>
            <button class="chip-x" title="移除该附件" onclick={() => removeAttachment(a.id)}>×</button>
          </span>
        {/each}
      </div>
    {/if}
    {#if $composerMentions.length > 0}
      <div class="chip-tray">
        {#each $composerMentions as cm, i}
          <span class="chip">
            <span class="chip-kind">{MENTION_GROUP_LABELS[cm.type] || cm.type}</span>
            <span class="chip-ref mono" title={cm.ref}>{cm.ref}</span>
            <button class="chip-x" title="移除该引用" onclick={() => removeMention(i)}>×</button>
          </span>
        {/each}
      </div>
    {/if}
    <textarea
      class="draft"
      rows="1"
      placeholder={$turnActive
        ? 'agent 执行中…(可中断)'
        : chatMode === 'chat'
          ? '对话模式:提问与讨论,Enter 发送'
          : '输入任务,@ 引用文件/上下文,Enter 发送'}
      bind:value={draft}
      bind:this={draftEl}
      oninput={onInput}
      onkeydown={onKeydown}
      onblur={closeMention}
      onpaste={onPasteFiles}
      disabled={$connStatus !== 'online'}
    ></textarea>
    <input type="file" multiple bind:this={fileInputEl} onchange={onFilesChosen} hidden />
    <div class="composer-toolbar">
      <div class="tb-left">
        <div class="mode-seg" role="group" aria-label="会话模式">
          <button
            class:active={chatMode === 'build'}
            onclick={() => setMode('build')}
            disabled={$turnActive}
            title="构建模式:agent 执行任务(默认)"
          >
            构建
          </button>
          <button
            class:active={chatMode === 'chat'}
            onclick={() => setMode('chat')}
            disabled={$turnActive}
            title="对话模式:仅问答咨询,不做变更"
          >
            对话
          </button>
        </div>
        <div class="model-pick" bind:this={modelPickEl}>
          <button
            class="tb-btn model-btn"
            class:active={modelMenuOpen}
            onclick={toggleModelMenu}
            disabled={$turnActive}
            title="会话模型(点击切换;准入白名单由执行体执法)"
          >
            <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
              <path
                d="M8 1.8 14 5v6L8 14.2 2 11V5Z"
                stroke="currentColor"
                stroke-width="1.3"
                fill="none"
                stroke-linejoin="round"
              />
            </svg>
            <span class="model-name">{currentModel || '模型'}</span>
          </button>
          {#if modelMenuOpen}
            <div class="model-menu" role="listbox" aria-label="会话模型">
              {#if modelErr}<div class="model-menu-note err">{modelErr}</div>{/if}
              {#if !modelReady}
                <div class="model-menu-note">{modelNote || '模型清单不可用'}</div>
              {:else}
                {#each modelList as m (m)}
                  <button
                    class="model-item"
                    class:active={m === currentModel}
                    role="option"
                    aria-selected={m === currentModel}
                    onclick={() => pickModel(m)}
                    disabled={modelBusy}
                  >
                    <span class="mono">{m}</span>
                    {#if m === currentModel}<span class="model-check">✓</span>{/if}
                  </button>
                {/each}
              {/if}
              {#if modelBusy}<div class="model-menu-note">切换中…</div>{/if}
            </div>
          {/if}
        </div>
        <button class="tb-btn" onclick={openMentionPopup} title="插入 @ 上下文引用">@</button>
        <button class="tb-btn" onclick={pickFiles} title="上传图片/附件(存入工作区 .evo-agent/uploads,agent 可读)">
          <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true">
            <path
              d="M8 3.2v9.6M3.2 8h9.6"
              stroke="currentColor"
              stroke-width="1.5"
              fill="none"
              stroke-linecap="round"
            />
          </svg>
        </button>
        <button class="tb-btn" onclick={openSettingsTab} title="MCP 服务管理(打开设置)">MCP</button>
      </div>
      <div class="tb-right">
        <span class="token-est {tokenLevel(estimateComposerTokens(draft, $composerMentions))}">
          ≈ {estimateComposerTokens(draft, $composerMentions)} tokens{#if $composerMentions.some((m) => m.type === 'file')}&nbsp;·&nbsp;含引用{/if}
        </span>
        {#if tokenLevel(estimateComposerTokens(draft, $composerMentions)) === 'over'}
          <span class="token-est-hint">引用体积较大，可删除部分引用 chip</span>
        {/if}
        {#if $turnActive}
          <button class="send-btn stop" onclick={interrupt} title="中断 agent 执行">
            <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
              <rect x="3.5" y="3.5" width="9" height="9" rx="1.5" fill="currentColor" />
            </svg>
          </button>
        {:else}
          <button
            class="send-btn"
            onclick={submit}
            disabled={!draft.trim() || $connStatus !== 'online'}
            title="发送 (Enter)"
          >
            <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
              <path
                d="M8 13V3.5M3.8 7.7 8 3.5l4.2 4.2"
                stroke="currentColor"
                stroke-width="1.7"
                fill="none"
                stroke-linecap="round"
                stroke-linejoin="round"
              />
            </svg>
          </button>
        {/if}
      </div>
    </div>
    {#if engineStalled}
      <div class="heartbeat-stall" role="status">
        ⚠ 引擎已 {stallSeconds}s 无事件返回——长程工具或引擎停滞;持续无响应可中断后重试
      </div>
    {/if}
  </div>
</aside>

<style>
  .chat {
    width: 380px;
    flex-shrink: 0;
    background: var(--sidebar-bg);
    border-left: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .chat-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--sp-sm) var(--sp-md);
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
    flex-shrink: 0;
  }
  .title {
    font-size: 11px;
    font-weight: var(--fw-sb);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: rgba(255, 255, 255, 0.4);
  }
  .new-btn {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    padding: 3px var(--sp-sm);
    border-radius: var(--r-sm);
    border: 1px solid var(--border);
  }
  .new-btn:hover {
    color: var(--text-primary);
    background: var(--sidebar-hover);
  }
  .new-btn.active {
    color: var(--brand);
    border-color: var(--brand);
  }
  /* 会话工作区按钮:缺省灰、在场品牌色;长目录名收窄省略 */
  .ws-btn {
    max-width: 130px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .header-actions {
    display: flex;
    gap: var(--sp-xs);
  }
  .session-list {
    flex-shrink: 0;
    max-height: 200px;
    overflow-y: auto;
    border-bottom: 1px solid var(--border);
    padding: var(--sp-xs);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .session-empty {
    padding: var(--sp-sm);
    font-size: var(--fs-xs);
    color: var(--text-muted);
    text-align: center;
  }
  .session-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--sp-sm);
    width: 100%;
    padding: 6px var(--sp-sm);
    border: none;
    border-radius: var(--r-sm);
    background: transparent;
    color: var(--sidebar-text);
    cursor: pointer;
    text-align: left;
  }
  .session-item:hover {
    background: var(--sidebar-hover);
  }
  .session-item.current {
    background: rgba(29, 99, 237, 0.18);
    box-shadow: inset 2px 0 0 var(--brand);
  }
  .session-row {
    display: flex;
    align-items: stretch;
    gap: 2px;
  }
  .session-row .session-item {
    flex: 1;
    min-width: 0;
  }
  .resume-btn {
    flex-shrink: 0;
    font-size: var(--fs-xs);
    color: var(--brand);
    padding: 0 var(--sp-sm);
    border-radius: var(--r-sm);
    border: 1px solid var(--border);
    cursor: pointer;
  }
  .resume-btn:hover {
    border-color: var(--brand);
    background: var(--sidebar-hover);
  }
  .s-title {
    font-size: var(--fs-xs);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
  }
  .s-time {
    font-size: 10px;
    color: var(--text-muted);
    flex-shrink: 0;
  }
  /* B25 谱系归属:缩进分支符 + 「委托自/派生会话」徽标 */
  .s-branch {
    color: var(--brand);
    margin-right: 4px;
    flex-shrink: 0;
  }
  .lineage-badge {
    flex-shrink: 0;
    font-size: 10px;
    color: var(--brand);
    border: 1px solid var(--brand);
    border-radius: 999px;
    padding: 0 6px;
    line-height: 16px;
    white-space: nowrap;
  }
  .msg-list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: var(--sp-md);
    display: flex;
    flex-direction: column;
    gap: var(--sp-sm);
  }
  .empty {
    text-align: center;
    padding: var(--sp-xl) var(--sp-md);
    color: var(--text-secondary);
  }
  .empty-title {
    font-size: var(--fs-base);
    font-weight: var(--fw-sb);
    color: var(--text-primary);
    margin-bottom: var(--sp-xs);
  }
  .empty-desc {
    font-size: var(--fs-xs);
  }
  .row {
    display: flex;
    justify-content: flex-start;
  }
  .row.user {
    justify-content: flex-end;
  }
  .row.center {
    justify-content: center;
  }
  .bubble {
    max-width: 88%;
    padding: var(--sp-sm) var(--sp-md);
    border-radius: var(--r-lg);
    font-size: var(--fs-sm);
    line-height: var(--lh-normal);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .user-bubble {
    background: var(--brand);
    color: #fff;
  }
  .assistant-bubble {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .error-bubble {
    background: var(--danger-bg);
    color: var(--danger);
  }
  .cursor {
    display: inline-block;
    width: 7px;
    height: 14px;
    margin-left: 2px;
    vertical-align: text-bottom;
    background: var(--brand-ocean);
    opacity: 0;
  }
  .cursor.on {
    opacity: 1;
    animation: blink 1s step-end infinite;
  }
  @keyframes blink {
    50% {
      opacity: 0;
    }
  }
  .tool-card {
    border: 1px solid var(--border);
    border-radius: var(--r-md);
    background: var(--bg-card);
    padding: var(--sp-sm) var(--sp-sm);
    font-size: var(--fs-xs);
  }
  .tool-card.running {
    border-color: var(--brand);
  }
  .tool-name {
    color: var(--brand-ocean);
    font-weight: var(--fw-sb);
  }
  .tool-state {
    margin-left: var(--sp-sm);
    color: var(--text-muted);
  }
  .tool-payload {
    margin-top: var(--sp-xs);
    padding: var(--sp-xs) var(--sp-sm);
    background: var(--bg-page);
    border-radius: var(--r-sm);
    color: var(--text-secondary);
    max-height: 120px;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-all;
  }
  .sys {
    font-size: 11px;
    color: var(--text-muted);
  }
  .artifact-card {
    border: 1px solid var(--border);
    border-left: 3px solid var(--brand);
    border-radius: 6px;
    padding: var(--sp-sm) var(--sp-md);
    margin: var(--sp-xs) 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
    background: var(--bg-card);
  }
  .ac-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .ac-label {
    font-size: var(--fs-xs);
    font-weight: var(--fw-med);
    color: var(--brand);
  }
  .ac-state {
    font-size: var(--fs-xs);
    color: var(--text-muted);
  }
  .ac-state.final {
    color: var(--ok, #4ade80);
  }
  .ac-path {
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ac-open {
    align-self: flex-start;
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-xs);
    padding: 2px 10px;
    border-radius: 4px;
    cursor: pointer;
  }
  .ac-open:hover {
    color: var(--text-primary);
    border-color: var(--brand);
  }
  .approval-card {
    border: 1px solid var(--warning);
    border-radius: var(--r-md);
    background: var(--warning-bg);
    padding: var(--sp-sm);
    font-size: var(--fs-xs);
  }
  .ap-head {
    font-weight: var(--fw-sb);
    color: var(--warning);
  }
  .ap-cmd {
    margin: var(--sp-xs) 0;
    padding: var(--sp-xs) var(--sp-sm);
    background: var(--bg-page);
    border-radius: var(--r-sm);
    color: var(--text-secondary);
    white-space: pre-wrap;
    word-break: break-all;
  }
  .ap-foot {
    color: var(--text-muted);
  }
  .ap-alt {
    color: var(--text-secondary);
    margin-bottom: var(--sp-xs);
  }
  .ap-actions {
    display: flex;
    gap: var(--sp-sm);
    margin-top: var(--sp-xs);
  }
  .ap-btn {
    height: 26px;
    padding: 0 var(--sp-md);
    font-size: var(--fs-xs);
    font-weight: var(--fw-med);
    border-radius: var(--r-sm);
  }
  .ap-btn.approve {
    background: var(--brand);
    color: #fff;
  }
  .ap-btn.approve:hover {
    background: var(--brand-hover);
  }
  .ap-btn.reject {
    background: transparent;
    color: var(--danger);
    border: 1px solid var(--danger);
  }
  .ap-btn.reject:hover {
    background: var(--danger-bg);
  }
  .ap-ok {
    margin-top: var(--sp-xs);
    color: var(--success);
  }
  .ap-err {
    margin-top: var(--sp-xs);
    color: var(--danger);
  }
  /* S1 一体化输入容器(主流 IDE 对齐):chips/输入区/工具行同处一张圆角卡片 */
  .composer {
    position: relative;
    flex-shrink: 0;
    margin: var(--sp-sm);
    padding: var(--sp-sm);
    border: 1px solid var(--border);
    border-radius: var(--r-lg);
    background: var(--bg-input);
    display: flex;
    flex-direction: column;
    gap: var(--sp-xs);
    transition: border-color var(--tr-fast);
  }
  .composer:focus-within {
    border-color: var(--brand);
  }
  .mention-pop {
    position: absolute;
    bottom: calc(100% + 6px);
    left: 0;
    right: 0;
    max-height: 260px;
    overflow-y: auto;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--r-md);
    padding: var(--sp-xs);
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.45);
    z-index: 30;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .m-label {
    font-size: 10px;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 4px var(--sp-sm) 2px;
  }
  .m-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--sp-sm);
    width: 100%;
    padding: 4px var(--sp-sm);
    border: none;
    background: transparent;
    color: var(--sidebar-text);
    cursor: pointer;
    text-align: left;
    border-radius: var(--r-sm);
    font-size: var(--fs-xs);
  }
  .m-row:hover {
    background: var(--sidebar-hover);
  }
  .m-row.sel {
    background: rgba(29, 99, 237, 0.18);
    box-shadow: inset 2px 0 0 var(--brand);
  }
  .m-row.dis,
  .m-row.dis:hover {
    color: var(--text-muted);
    opacity: 0.55;
    cursor: default;
    background: transparent;
  }
  .m-path {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .m-path .hit {
    color: var(--brand);
    font-weight: var(--fw-sb);
  }
  .m-kind {
    flex-shrink: 0;
    font-size: 10px;
    color: var(--text-muted);
  }
  .m-warn {
    flex-shrink: 0;
    font-size: 11px;
    color: var(--warning);
  }
  .m-empty {
    padding: var(--sp-sm);
    text-align: center;
    color: var(--text-muted);
    font-size: var(--fs-xs);
  }
  .m-more {
    padding: 4px var(--sp-sm);
    color: var(--text-muted);
    font-size: 10px;
  }
  .chip-tray {
    display: flex;
    flex-wrap: wrap;
    gap: var(--sp-xs);
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    border: 1px solid var(--border);
    background: var(--bg-card);
    border-radius: 999px;
    padding: 2px 4px 2px 8px;
    font-size: var(--fs-xs);
    max-width: 100%;
  }
  .chip-kind {
    color: var(--brand);
    font-size: 10px;
    flex-shrink: 0;
  }
  .chip-ref {
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 180px;
  }
  .chip-x {
    border: none;
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    padding: 0 4px;
    font-size: 12px;
    line-height: 1;
  }
  .chip-x:hover {
    color: var(--danger);
  }
  /* S1 输入区:融入容器(无独立边框),自动增高,超限内部滚动 */
  .draft {
    width: 100%;
    resize: none;
    background: transparent;
    border: none;
    color: var(--text-primary);
    font-size: var(--fs-sm);
    font-family: inherit;
    line-height: 1.5;
    padding: 2px 0;
    outline: none;
    overflow-y: auto;
    min-height: 44px;
    max-height: 176px;
  }
  .draft:disabled {
    color: var(--text-muted);
    cursor: not-allowed;
  }
  .draft::placeholder {
    color: var(--text-muted);
  }
  /* S2 底部工具行(主流 IDE 对齐):左=上下文/附件/MCP,右=token/发送圆钮 */
  .composer-toolbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--sp-sm);
  }
  .tb-left {
    display: flex;
    align-items: center;
    gap: 2px;
    min-width: 0;
  }
  /* S3 模式段控件(Trae 对齐):构建/对话二态切换 */
  .mode-seg {
    display: inline-flex;
    align-items: center;
    border: 1px solid var(--border);
    border-radius: 999px;
    overflow: hidden;
    margin-right: 4px;
    flex-shrink: 0;
  }
  .mode-seg button {
    border: none;
    background: transparent;
    color: var(--text-secondary);
    font-size: 11px;
    font-weight: var(--fw-med);
    line-height: 1;
    padding: 4px 8px;
    cursor: pointer;
  }
  .mode-seg button + button {
    border-left: 1px solid var(--border);
  }
  .mode-seg button.active {
    background: var(--brand);
    color: #fff;
  }
  /* S5 模型选择器(Trae 对齐):mode 段右侧下拉,白名单正源=执行体 */
  .model-pick {
    position: relative;
    flex-shrink: 0;
  }
  .model-btn {
    max-width: 150px;
  }
  .model-btn .model-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 110px;
  }
  .model-menu {
    position: absolute;
    bottom: calc(100% + 6px);
    left: 0;
    min-width: 210px;
    max-height: 240px;
    overflow-y: auto;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    box-shadow: var(--sh-modal);
    z-index: 60;
    padding: 4px;
  }
  .model-item {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 6px 10px;
    border: none;
    background: transparent;
    color: var(--text-primary);
    font-size: 12px;
    cursor: pointer;
    border-radius: var(--r-sm);
    text-align: left;
  }
  .model-item:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .model-item.active {
    color: var(--brand);
  }
  .model-item:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .model-check {
    font-size: 11px;
    color: var(--brand);
  }
  .model-menu-note {
    padding: 8px 10px;
    font-size: 12px;
    color: var(--text-secondary);
    white-space: normal;
  }
  .model-menu-note.err {
    color: var(--danger);
  }
  .mode-seg button:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  /* S4 附件 chip 错误态 */
  .chip-err {
    border-color: #e5484d;
    color: #e5484d;
  }
  .tb-btn {
    border: none;
    background: transparent;
    color: var(--text-secondary);
    font-size: 11px;
    font-weight: var(--fw-med);
    line-height: 1;
    padding: 4px 7px;
    border-radius: var(--r-sm);
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    gap: 3px;
  }
  .tb-btn:hover:not(:disabled) {
    background: var(--sidebar-hover);
    color: var(--text-primary);
  }
  .tb-btn:disabled {
    opacity: 0.38;
    cursor: not-allowed;
  }
  .tb-right {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .send-btn {
    width: 28px;
    height: 28px;
    padding: 0;
    border: none;
    border-radius: 999px;
    background: var(--brand);
    color: #fff;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    cursor: pointer;
    flex-shrink: 0;
    transition: background var(--tr-fast);
  }
  .send-btn:hover:not(:disabled) {
    background: var(--brand-hover);
  }
  .send-btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  .send-btn.stop {
    background: var(--danger);
  }
  .token-est {
    font-size: 10px;
    color: var(--text-muted);
    white-space: nowrap;
  }
  .token-est.warn {
    color: var(--warning);
  }
  .token-est.over {
    color: var(--danger);
  }
  .token-est-hint {
    font-size: 10px;
    color: var(--danger);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .heartbeat-stall {
    font-size: 11px;
    color: var(--warning);
    padding: 4px 0 0;
    border-top: 1px solid var(--border);
  }
  .user-col {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 4px;
    max-width: 88%;
  }
  .used-refs {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    align-items: center;
    gap: 4px;
  }
  .ur-label {
    font-size: 10px;
    color: var(--text-muted);
  }
  .used-chip {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 1px 6px;
    font-size: 10px;
    max-width: 220px;
  }
  .used-chip.dropped {
    opacity: 0.7;
    border-style: dashed;
  }
  .ur-kind {
    color: var(--brand);
    flex-shrink: 0;
  }
  .ur-ref {
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ur-note {
    color: var(--danger);
    flex-shrink: 0;
  }
</style>
