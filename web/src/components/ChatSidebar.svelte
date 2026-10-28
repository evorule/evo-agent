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
    transcriptHasMore,
    loadOlderTranscript,
    openGuideTab,
  } from '../lib/stores.js';
  import { sendMessage, interrupt, newSession, openSession, resumeSession } from '../lib/ws.js';
  // Svelte 5 不深度代理原生 Set(add/delete 不触发重渲染,点击开合会失明),
  // 交互态集合必须用 SvelteSet
  import { SvelteSet } from 'svelte/reactivity';
  import { renderChatHtml, copyChatCode } from '../lib/chat-markdown.js';
  import { lineDiff, toolDiffPayload } from '../lib/chat-diff.js';
  import { approveProposal, answerQuestion, listResumableRuns, getOrchestrationLinks, uploadFile, getLlmModels, setLlmModel, llmModelsNoteFromError, listSkills } from '../lib/api.js';
  import { settingsState, setSetting } from '../lib/settings.js';
  import {
    APPROVAL_MODE_KEY,
    APPROVAL_EXEMPTION_KEY,
    FULL_ACCESS_CONFIRM,
    approvalPresetWrites,
    approvalPresetView,
  } from '../lib/approval.js';
  import { switchWorkspace } from '../lib/workspace.js';
  import DirectoryBrowser from './DirectoryBrowser.svelte';
  import { chatWidth } from '../lib/layout.js';
  import { orderSessionsWithLineage } from '../lib/orchestration.js';
  import { push as pushNotification } from '../lib/notifications.js';
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
  import AuditBadge from './AuditBadge.svelte';
  import AuditCard from './AuditCard.svelte';
  import { auditState, badgeStatus, mapAssistantToFacts } from '../lib/audit.js';

  // audit-one-click:assistant 终稿消息 ↔ IoResponse entries 顺序对齐(M1b);
  // 会话级审计状态由 ws.js initAuditWatcher 拉取,此处只做派生与徽章挂载。
  const auditBadgeMap = $derived(mapAssistantToFacts($messages, $auditState));
  let auditCardEntry = $state(null); // 报告卡当前条目(null=关)

  // Svelte 5:组件用了 $effect 等 rune 即进入 runes 模式,
  // 顶层状态必须 $state() 声明,普通 let 不具备响应性(历史面板/发送按钮会失效)
  let draft = $state('');
  let listEl = $state(null);
  let showHistory = $state(false);
  // 可恢复崩溃会话集合(扫尾端点投影;与历史索引交集后展示「恢复」入口)
  let resumableIds = $state(new Set());
  // 非计划重连对账后,当前会话可恢复时的中断回合横幅(存 sid;null=不展示)
  let reconnectBannerSid = $state(null);
  // B25 谱系归属(全局归属链投影):子会话在列表内缩进 + 「委托自」徽标
  let lineageLinks = $state([]);
  // C 批「会话级 workspace」:当前工作区(连接级 ?workspace= 携带;
  // localStorage 持久,改值在下次连接生效——新建会话/换线/重连)
  let workspace = $state(localStorage.getItem('evo_workspace') || '');

  /** 编辑会话工作区:prompt 输入绝对路径,空串清除。批 I:走统一切换入口
   *  (dirty 护栏 + 落库 + 关文件 tab + 纪元自增);文件面即刻重锚,
   *  agent 能力边界下次连接生效(新建会话/换线/重连)。 */
  let wsBrowserOpen = $state(false);

  /** 工作区编辑(体验优化项 5):弃裸 prompt,复用图形化目录浏览器(与首页
   *  同款);恢复缺省入口在首页工作区卡(「恢复缺省」按钮)。 */
  function editWorkspace() {
    wsBrowserOpen = true;
  }

  /** 工作区按钮显示名:末段路径(长路径收窄);缺省态显式标示 */
  function workspaceLabel() {
    if (!workspace) return '工作区:未选择';
    const parts = workspace.split(/[\\/]/).filter(Boolean);
    return `工作区:${parts[parts.length - 1] || workspace}`;
  }

  // E1:Explorer 侧工作区切换同步(自定义事件;防两侧按钮态漂移)
  $effect(() => {
    const sync = () => {
      workspace = localStorage.getItem('evo_workspace') || '';
    };
    window.addEventListener('evo:workspace-changed', sync);
    return () => window.removeEventListener('evo:workspace-changed', sync);
  });

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

  // ---- S3 构建/计划/对话模式(前端姿态+提示语;审批链语义不动) ----
  // 构建=默认,agent 执行任务;计划(交互批)=agent 只读探查后经 plan_submit
  // 提交计划,批准后按计划执行;对话=仅问答咨询,提示块引导先确认后变更。
  const MODE_KEY = 'evo_chat_mode';
  const MODES = ['build', 'plan', 'chat'];
  let chatMode = $state(
    MODES.includes(localStorage.getItem(MODE_KEY)) ? localStorage.getItem(MODE_KEY) : 'build',
  );

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
        pushNotification('info', '附件上传完成', `${res.path || f.name}`);
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
      modelNote = llmModelsNoteFromError(e?.message || e);
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
      pushNotification('err', '模型切换失败', modelErr);
    } finally {
      modelBusy = false;
    }
  }

  function onDocClickCloseModelMenu(e) {
    if (!modelMenuOpen) return;
    if (modelPickEl?.contains(e.target)) return;
    modelMenuOpen = false;
  }

  // ---- 审批档位选择器(批B):四档快捷预设;写键走既有设置通道,对新会话生效 ----
  // 静态路由层无放行权:仅设置键写入,决策端/红线闸/审计链零触碰;
  // 「完全访问」必经确认条,绝不静默开红线豁免。
  let approvalMenuOpen = $state(false);
  let approvalPickEl = $state(null);
  let approvalFullConfirm = $state(false);
  let approvalErr = $state('');
  let approvalBusy = $state(false);

  const APPROVAL_PRESETS = [
    { id: 'manual', label: '手动审批', desc: '每次人工确认;红线强制' },
    { id: 'auto', label: '自动审批', desc: '无人值守判定放行+留痕;红线仍强制人工' },
    { id: 'full', label: '完全访问', desc: '红线类操作交评审判定(需确认)' },
    { id: 'custom', label: '自定义配置', desc: '对照条件文件评审判定' },
  ];

  let approvalView = $derived(
    approvalPresetView(
      $settingsState.settings[APPROVAL_MODE_KEY],
      $settingsState.settings[APPROVAL_EXEMPTION_KEY],
    ),
  );

  function toggleApprovalMenu() {
    approvalMenuOpen = !approvalMenuOpen;
    approvalFullConfirm = false;
    if (approvalErr) approvalErr = '';
  }

  async function pickApprovalPreset(p) {
    if (approvalBusy) return;
    if (p === 'full' && !approvalFullConfirm) {
      approvalFullConfirm = true; // 先出确认条;再次点击确认才写入
      return;
    }
    approvalBusy = true;
    approvalErr = '';
    try {
      for (const [k, v] of approvalPresetWrites(p)) await setSetting(k, v);
      approvalMenuOpen = false;
      approvalFullConfirm = false;
    } catch (e) {
      approvalErr = String(e?.message || e);
    } finally {
      approvalBusy = false;
    }
  }

  function onDocClickCloseApprovalMenu(e) {
    if (!approvalMenuOpen) return;
    if (approvalPickEl?.contains(e.target)) return;
    approvalMenuOpen = false;
    approvalFullConfirm = false;
  }

  // ---- 技能选择器(对标 Trae 会话框技能入口;evorule 治理语义) ----
  // 仅列 active 态(GET /api/skills 服务端权威清单;discovered 须经扩展面板
  // 人工审批,导入≠生效——供应链闸口不因 UI 入口而旁路)。点选=激活,随
  // 下一条消息以 wire-only 技能块装配(服务端技能文件,agent 以文件工具
  // 读取 SKILL.md 并遵循),再点同项=停用;零协议改动,形态与 modeHint 同构。
  let skillMenuOpen = $state(false);
  let skillPickEl = $state(null);
  let skillList = $state([]);
  let skillLoaded = $state(false);
  let skillBusy = $state(false);
  let skillErr = $state('');
  let skillPendingCount = $state(0);
  let selectedSkill = $state(null); // {name, path, description}

  async function loadSkills() {
    if (skillBusy) return;
    skillBusy = true;
    skillErr = '';
    try {
      const all = await listSkills();
      skillList = all.filter((s) => s.status === 'active');
      skillPendingCount = all.filter((s) => s.status === 'discovered').length;
      skillLoaded = true;
      // 已选技能被撤销时静默清除(下一条消息不再装配)
      if (selectedSkill && !skillList.some((s) => s.name === selectedSkill.name)) {
        selectedSkill = null;
      }
    } catch (e) {
      skillErr = String(e?.message || e);
    } finally {
      skillBusy = false;
    }
  }

  function toggleSkillMenu() {
    skillMenuOpen = !skillMenuOpen;
    if (skillMenuOpen) loadSkills();
  }

  function pickSkill(s) {
    selectedSkill = selectedSkill?.name === s.name ? null : s;
    skillMenuOpen = false;
  }

  function onDocClickCloseSkillMenu(e) {
    if (!skillMenuOpen) return;
    if (skillPickEl?.contains(e.target)) return;
    skillMenuOpen = false;
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
    document.addEventListener('click', onDocClickCloseApprovalMenu);
    document.addEventListener('click', onDocClickCloseSkillMenu);
    window.addEventListener('evo:reconnected', onReconnected);
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
    document.removeEventListener('click', onDocClickCloseApprovalMenu);
    document.removeEventListener('click', onDocClickCloseSkillMenu);
    window.removeEventListener('evo:reconnected', onReconnected);
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

  /** 非计划重连对账(连接韧性):serve 归位后刷新可恢复列表,当前会话
   *  在其中 → 中断回合横幅。恢复仍走 askResume 显式确认——「人类持剑:
   *  只列不续」语义不变,横幅只是把入口带到眼前。 */
  async function onReconnected(ev) {
    reconnectBannerSid = null;
    await loadResumable();
    const sid = ev && ev.detail ? ev.detail.sid : null;
    if (sid && sid !== 'new' && resumableIds.has(sid)) {
      reconnectBannerSid = sid;
    }
  }

  /** 中断回合「继续」横幅(37 号项 1):会话切换/清单刷新后,若当前会话
   *  存在未完回合→把恢复入口以横幅带到眼前(启动壳即见,不必进历史翻找)。
   *  语义同上:只列不续,点击经 askResume 显式确认。 */
  async function refreshResumeBanner() {
    const sid = $sessionId;
    if (!sid || $turnActive) {
      return;
    }
    await loadResumable();
    if (resumableIds.has(sid)) {
      reconnectBannerSid = sid;
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

  // ---- 长会话渲染窗(DOM 上限防千条级会话卡顿;分页加载走游标续拉) ----
  const RENDER_WINDOW = 400;
  const RENDER_WINDOW_STEP = 200;
  let renderCount = $state(RENDER_WINDOW);
  let loadingOlder = $state(false);
  // 只渲染最近 renderCount 条;更早的经「显示更早消息」按钮扩窗
  const visibleMessages = $derived(
    $messages.length > renderCount ? $messages.slice($messages.length - renderCount) : $messages,
  );
  // 换线/新建即复位渲染窗(防旧会话窗口状态串台)
  $effect(() => {
    void $sessionId;
    renderCount = RENDER_WINDOW;
  });
  $effect(() => {
    // 换线/启动后:当前会话存在可恢复回合 → 顶部横幅(37 号项 1)
    void $sessionId;
    refreshResumeBanner();
  });

  // ---- 回合内过程容器(项目方令):思考与工具调用收进一个框 ----
  // 回合进行中=框内实时展开(左缘品牌色+「进行中」标);正文/回答气泡抵达
  // 即整框自动折叠成一行(约一行字高);点击头部展开,再点收起。开合状态
  // 按组键记忆,用户手动开合覆写默认。
  const expandedGroups = new SvelteSet();
  const turnLayout = $derived.by(() => {
    // 归组判定按「连续 agent 活动段」两遍扫描:段内最后一条带正文的
    // assistant 是回答(single 留框外),其余(tool/纯思考/带中间陈述的
    // 思考步)全进过程框。背景:MiniMax-M3 型推理模型每个思考步都带
    // 中间陈述文本,旧判定(仅无正文 assistant 算活动)把它们全部散开
    // 成 N 个「已深度思考」框(真机实锤=7 步 7 框撑爆对话区)。
    const out = [];
    let buf = null;
    const flushBuf = (closed) => {
      if (!buf) return;
      let answerIdx = -1;
      for (let i = buf.items.length - 1; i >= 0; i--) {
        const m = buf.items[i];
        if (m.kind === 'assistant' && (m.text || '').trim()) { answerIdx = i; break; }
      }
      const boxed = buf.items.filter((_, i) => i !== answerIdx);
      if (boxed.length) {
        out.push({ type: 'process', key: `p${boxed[0].id}`, items: boxed, closed });
      }
      if (answerIdx >= 0) {
        out.push({ type: 'single', key: `m${buf.items[answerIdx].id}`, m: buf.items[answerIdx] });
      }
      buf = null;
    };
    for (const m of visibleMessages) {
      if (m.kind === 'tool' || m.kind === 'assistant') {
        if (!buf) buf = { items: [] };
        buf.items.push(m);
      } else {
        flushBuf(true);
        out.push({ type: 'single', key: `m${m.id}`, m });
      }
    }
    // 末尾未闭合段=进行中的实时活动,保持框内展开(closed=false)
    flushBuf(false);
    return out;
  });
  function toggleGroup(key) {
    if (expandedGroups.has(key)) expandedGroups.delete(key);
    else expandedGroups.add(key);
  }
  async function onLoadOlder() {
    if (loadingOlder) return;
    loadingOlder = true;
    // 滚动锚定(37 号项 4):前插旧内容后把视口停在原阅读位,不跳顶
    const prevHeight = listEl?.scrollHeight ?? 0;
    try {
      if ($messages.length > renderCount) {
        // 仅扩渲染窗(消息已在内存)
        renderCount += RENDER_WINDOW_STEP;
      } else if ($sessionId) {
        // 拉更早一页(transcript 游标续拉,重投影配对)
        await loadOlderTranscript($sessionId);
        renderCount += RENDER_WINDOW_STEP;
      }
      requestAnimationFrame(() => {
        if (!listEl) return;
        listEl.scrollTop += listEl.scrollHeight - prevHeight;
      });
    } finally {
      loadingOlder = false;
    }
  }

  // 滚动体验:贴底跟滚;上翻阅读时不拽回,新增消息与流式增量计入浮条
  let stickBottom = $state(true);
  let freshCount = $state(0);
  let lastMsgLen = 0; // 普通变量游标:$effect 依赖面之外,防重复计数
  let lastTailLen = 0; // 末条消息文本长度游标(回答流式增量计数用)
  $effect(() => {
    const arr = $messages;
    const len = arr.length;
    const tail = arr.length ? (arr[arr.length - 1].text?.length || 0) : 0;
    if (listEl) {
      if (stickBottom) {
        requestAnimationFrame(() => listEl.scrollTo({ top: listEl.scrollHeight }));
      } else if (len > lastMsgLen) {
        freshCount += len - lastMsgLen;
      } else if (tail > lastTailLen) {
        // 末条气泡流式增长(回答正文)也算新内容——滚离时回答抵达必须有可见信号
        freshCount += 1;
      }
    }
    lastMsgLen = len;
    lastTailLen = tail;
  });

  /** 滚动位置对账:距底 <48px 视为贴底(恢复自动跟滚),否则交还用户阅读位 */
  function onListScroll() {
    const el = listEl;
    if (!el) return;
    if (el.scrollHeight - el.scrollTop - el.clientHeight < 48) {
      stickBottom = true;
      freshCount = 0;
    } else {
      stickBottom = false;
    }
  }

  /** 回到底部(浮条/发送时),恢复贴底跟滚 */
  function jumpToBottom() {
    stickBottom = true;
    freshCount = 0;
    listEl?.scrollTo({ top: listEl.scrollHeight });
  }

  // ---- 工具卡交互态(易失 UI 态,不入消息 store、不随会话持久化) ----
  // 必须用 SvelteSet:$state 对 Set 变更不触发重渲染(真机实锤——展开/查看
  // 全部点击失灵,卡片永远收起=「内容不显示」的交互层根因)
  const expandedTools = new SvelteSet();
  const fullResults = new SvelteSet();
  const TOOL_RESULT_CAP = 4000; // 展开态预览截断帽(「查看全部」解锁全文)

  function toggleTool(id) {
    if (expandedTools.has(id)) expandedTools.delete(id);
    else expandedTools.add(id);
  }

  function toggleFull(id) {
    fullResults.add(id);
  }

  // ---- 思考面板交互态(易失 UI 态不入消息 store、不随会话持久化) ----
  // 一律默认折叠(含流式中):流式默认展开的旧设计经项目方令废止——长回合下
  // 展开思考挤占视线、回答沉底;用户点击任意段展开,流式段展开时保持吸底跟滚
  const expandedThinking = new SvelteSet();
  // 流式思考体元素引用(同刻至多一条流式;$effect 吸底消费)
  let streamThinkingEl = null;

  function thinkingOpen(m) {
    return expandedThinking.has(m.id);
  }

  function toggleThinking(m) {
    if (expandedThinking.has(m.id)) expandedThinking.delete(m.id);
    else expandedThinking.add(m.id);
  }

  /** 思考耗时文案:非流式取 thinkingEndedAt 固化值;无起始时间返回空 */
  function thinkingDuration(m) {
    if (!m.thinkingStartedAt) return '';
    const end = m.thinkingEndedAt || Date.now();
    return formatTook(end - m.thinkingStartedAt);
  }

  // 流式思考吸底:messages 任何更新(思考/正文增量)即把流式中的思考体
  // 滚到底部(仿终端跟随语义);非流式展开体 data-streaming='0' 不滚动
  $effect(() => {
    void $messages;
    if (streamThinkingEl && streamThinkingEl.dataset.streaming === '1') {
      streamThinkingEl.scrollTop = streamThinkingEl.scrollHeight;
    }
  });

  /** 耗时文案:<1s 毫秒,否则一位小数秒 */
  function formatTook(ms) {
    return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
  }

  /** 编辑类/整文件写 → 行级 diff 行序列;不可 diff(非编辑类/超帽/args 缺失/
   *  file_write 无草稿基线)→ null 降级。file_write 基线取产物链草稿快照
   *  (同路径首写无基线 → null,写入内容经动作段 args 展示)。 */
  function toolDiffRows(m) {
    const baseline = m.name === 'file_write' ? fileWriteBaseline(m) : null;
    const p = toolDiffPayload(m, baseline);
    return p ? lineDiff(p.oldText, p.newText) : null;
  }

  /** file_write 的旧文基线:产物登记(draftContent)按 args.path 查找;形态
   *  兼容 JSON 字符串/对象两种 args 载体 */
  function fileWriteBaseline(m) {
    let a = m.args;
    if (typeof a === 'string') {
      try {
        a = JSON.parse(a);
      } catch {
        return null;
      }
    }
    const p = a && typeof a === 'object' ? a.path : null;
    if (!p) return null;
    const hit = $artifacts.find((x) => x.path === p);
    return hit && typeof hit.draftContent === 'string' ? hit.draftContent : null;
  }

  /** 动作段文本:实时卡为 JSON 字符串,历史卡为对象(配对合并投影),统一文本化 */
  function argsText(m) {
    if (typeof m.args === 'string') return m.args;
    try {
      return JSON.stringify(m.args, null, 2);
    } catch {
      return String(m.args);
    }
  }

  /** 消息级复制(assistant/error 气泡与工具卡共用);按钮瞬时「已复制」反馈 */
  async function copyPlainText(text, btn) {
    try {
      await navigator.clipboard.writeText(String(text));
      if (btn) {
        const old = btn.textContent;
        btn.textContent = '已复制';
        btn.disabled = true;
        setTimeout(() => {
          btn.textContent = old;
          btn.disabled = false;
        }, 1200);
      }
    } catch {
      /* 剪贴板不可用静默(与代码块复制同口径) */
    }
  }

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
    jumpToBottom(); // 用户主动发言:必然关心最新流,强制恢复贴底
    sendMessage(text, kept, { mode: chatMode, attachments: okAtts, skill: selectedSkill });
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

  // 代码块复制(事件委托:{@html} 注入节点无 Svelte 事件绑定)
  function onMdCopyClick(e) {
    const btn = e.target.closest?.('.md-copy');
    if (btn?.dataset?.mdCopy) copyChatCode(btn.dataset.mdCopy, btn);
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

  // 批K 段7 人在环问答:回答走新增 POST /answer 通道(二阶段校验在工具侧,
  // 非法回答 fail-visible 回喂,agent 自行澄清续跑;AskUserResult 帧收卡)
  function decideAnswer(m, answer) {
    if (m.answerState === 'sending' || m.answerState === 'sent') return;
    if (!$sessionId) {
      updateMessage(m.id, { answerState: 'error', errText: '会话未建立,无法提交回答' });
      return;
    }
    const clean = (Array.isArray(answer) ? answer : [])
      .map((a) => String(a ?? '').trim())
      .filter(Boolean);
    if (clean.length === 0) {
      updateMessage(m.id, { answerState: 'error', errText: '回答不能为空' });
      return;
    }
    updateMessage(m.id, { answerState: 'sending', errText: null });
    answerQuestion('general', $sessionId, m.questionId, clean)
      .then(() => updateMessage(m.id, { answerState: 'sent' }))
      .catch((err) => {
        const status = err?.status;
        const text =
          status === 404
            ? '回答提交失败:问题已失效(超时或已收场)'
            : status === 400
              ? `回答被拒:${String(err?.message || err)}`
              : `提交失败:${String(err?.message || err)}`;
        updateMessage(m.id, { answerState: 'error', errText: text });
      });
  }

  // 多选勾选切换(草稿存消息对象;提交时整体送二阶段校验)
  function toggleDraftSel(m, opt) {
    const sel = new Set(m.draftSel || []);
    if (sel.has(opt)) sel.delete(opt);
    else sel.add(opt);
    updateMessage(m.id, { draftSel: [...sel] });
  }

  // ---- 计划卡决策(交互批 计划模式):批准=build 模式按计划执行;
  // 拒绝=留在计划模式,agent 重新探查并提交修订计划。决策注记走
  // opts.wireBlock(wire-only,气泡显示短决策文本) ----
  function decidePlan(m, approved) {
    if (m.decision || $turnActive) return;
    updateMessage(m.id, { decision: approved ? 'approved' : 'rejected' });
    if (approved) {
      const stepsText = (m.steps || [])
        .map((s, i) => `${i + 1}. ${s.title}${s.detail ? ` — ${s.detail}` : ''}`)
        .join('\n');
      setMode('build');
      sendMessage(
        '[已批准计划,请按计划执行]',
        [],
        {
          mode: 'build',
          wireBlock:
            `<plan-approved id="${m.planId || ''}">\n<plan-title>${m.planTitle || ''}</plan-title>\n<steps>\n${stepsText}\n</steps>` +
            (m.notes ? `\n<notes>${m.notes}</notes>` : '') +
            '\n<note>用户已批准上述计划:请严格按步骤顺序执行;执行中如确需偏离计划,先说明原因再继续。</note>\n</plan-approved>',
        },
      );
    } else {
      sendMessage(
        '[已拒绝该计划,请修订后重新提交]',
        [],
        {
          mode: 'plan',
          wireBlock:
            `<plan-rejected id="${m.planId || ''}">\n<note>用户拒绝了当前计划:请重新只读探查(如需)并提交修订后的计划(仍走 plan_submit);不要在未获批准前执行任何变更。</note>\n</plan-rejected>`,
        },
      );
    }
  }
</script>

<aside class="chat" class:history-open={showHistory} data-zone="chat" style:width={`${$chatWidth}px`}>
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
      <button class="new-btn" onclick={openGuideTab} title="系统性使用说明:工作区边界/审批模式/回复阅读/常见问题">指南</button>
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
              onclick={() => {
                openSession(s.session_id);
                showHistory = false; // 选中即收起:内容独占阅读位(37 号项 3)
              }}
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

  <div class="msg-list" bind:this={listEl} onscroll={onListScroll}>
    {#if reconnectBannerSid}
      <div class="reconnect-banner" role="status">
        <span class="rb-text">连接已恢复——上一回合被打断。</span>
        <button
          class="resume-btn"
          onclick={() => {
            const sid = reconnectBannerSid;
            reconnectBannerSid = null;
            askResume(sid);
          }}
          title="恢复中断的轮次(挂起审批不恢复,须重走审批)"
        >恢复</button>
        <button
          class="rb-dismiss"
          onclick={() => (reconnectBannerSid = null)}
          title="不再提示"
        >✕</button>
      </div>
    {/if}
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
    {#if !workspace}
      <button
        type="button"
        class="ws-hint"
        onclick={editWorkspace}
        title="选择后 agent 即可在该文件夹内读写"
      >
        ⚠ 未选择工作文件夹——agent 目前只能访问程序自带目录。点击选择你的工作文件夹
      </button>
    {/if}
    {#if $messages.length > renderCount || $transcriptHasMore}
      <button
        type="button"
        class="load-older"
        onclick={onLoadOlder}
        disabled={loadingOlder}
      >
        {#if loadingOlder}加载中…{:else}
          {#if $messages.length > renderCount}显示更早消息(还有 {$messages.length - renderCount} 条未渲染){:else}加载更早消息{/if}
        {/if}
      </button>
    {/if}
    {#snippet renderMsg(m)}
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
          <div class="assistant-col">
            {#if m.thinking}
              <div class="thinking-card" class:streaming={m.thinkingStreaming} class:expanded={thinkingOpen(m)}>
                <div
                  class="thinking-head"
                  role="button"
                  tabindex="0"
                  onclick={() => toggleThinking(m)}
                  onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && toggleThinking(m)}
                >
                  <span class="tool-chev">▸</span>
                  <span class="thinking-label">
                    {m.thinkingStreaming
                      ? '深度思考中…'
                      : m.thinkingStartedAt
                        ? `已深度思考(用时 ${thinkingDuration(m)})`
                        : '已深度思考'}
                  </span>
                </div>
                {#if thinkingOpen(m)}
                  <div
                    class="thinking-body mono"
                    data-streaming={m.thinkingStreaming ? '1' : '0'}
                    bind:this={streamThinkingEl}
                  >{m.thinking}</div>
                {/if}
              </div>
            {/if}
            {#if m.text || m.streaming}
              <!-- 纯思考步(无正文且非流式)不渲染气泡:答案升格样式会让
                   空气泡变成醒目空盒(真机实锤=思考线下的成排空框);
                   流式中未出字的段只留紧凑光标线 -->
              <div class="bubble assistant-bubble md-body" class:pending={!m.text && m.streaming} onclick={onMdCopyClick}>
                {@html renderChatHtml(m.text, m.id)}<span class="cursor" class:on={m.streaming}></span>
                {#if m.text}
                  <button
                    type="button"
                    class="msg-copy"
                    title="复制全文"
                    onclick={(e) => {
                      e.stopPropagation();
                      copyPlainText(m.text, e.currentTarget);
                    }}
                  >复制</button>
                {/if}
                {#if m.text || m.streaming}
                  <span class="audit-chip">
                    <AuditBadge
                      status={badgeStatus($auditState, auditBadgeMap.has(m.id))}
                      clickable={auditBadgeMap.has(m.id) && $auditState?.verified === true}
                      onclick={() => (auditCardEntry = auditBadgeMap.get(m.id))}
                    />
                  </span>
                {/if}
              </div>
            {/if}
          </div>
        </div>
      {:else if m.kind === 'tool'}
        <div class="tool-card" class:running={m.running} class:open={expandedTools.has(m.id)}>
          <div
            class="tool-head"
            role="button"
            tabindex="0"
            onclick={() => toggleTool(m.id)}
            onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && toggleTool(m.id)}
          >
            <span class="tool-chev">▸</span>
            <span class="tool-name mono">{m.name}</span>
            <span class="tool-state">
              {m.running
                ? '执行中…'
                : m.tookMs !== undefined
                  ? `完成·${formatTook(m.tookMs)}`
                  : '完成'}
            </span>
          </div>
          {#if expandedTools.has(m.id)}
            <div class="tool-body">
              {#if m.args != null}
                <div class="tb-label">
                  <span>动作</span>
                  {#if m.argsMeta}
                    <span class="tb-trunc">已截断（原 {m.argsMeta.originalLen} 字符）</span>
                  {/if}
                </div>
                {@const rows = toolDiffRows(m)}
                {#if rows}
                  <div class="diff-box mono">
                    {#each rows as r}
                      <div class="diff-row {r.t}">
                        <span class="diff-sign">{r.t === 'add' ? '+' : r.t === 'del' ? '-' : ' '}</span>
                        <span class="diff-text">{r.s}</span>
                      </div>
                    {/each}
                  </div>
                {:else}
                  <pre class="tool-payload mono">{argsText(m)}</pre>
                {/if}
              {/if}
              {#if m.result != null}
                <div class="tb-label">
                  <span>结果</span>
                  {#if m.resultMeta}
                    <span class="tb-trunc">已截断（原 {m.resultMeta.originalLen} 字符）</span>
                  {/if}
                </div>
                {@const text = String(m.result)}
                {#if text.includes('sandbox boundary')}
                  <div class="boundary-hint">
                    ⚠ 该目标在工作区边界之外。调整:点顶部「工作区」按钮扩大范围后新建会话;或把文件放进当前工作区。
                  </div>
                {/if}
                {#if text.length > TOOL_RESULT_CAP && !fullResults.has(m.id)}
                  <pre class="tool-payload mono">{text.slice(0, TOOL_RESULT_CAP)}</pre>
                  <button
                    type="button"
                    class="tool-full"
                    onclick={() => toggleFull(m.id)}
                  >查看全部(共 {text.length} 字符)</button>
                {:else}
                  <pre class="tool-payload mono">{text}</pre>
                {/if}
                <button
                  type="button"
                  class="tool-copy"
                  onclick={(e) => copyPlainText(text, e.currentTarget)}
                >复制</button>
              {/if}
              {#if m.args == null && m.result == null}
                <pre class="tool-payload mono">(无参数)</pre>
              {/if}
            </div>
          {/if}
        </div>
      {:else if m.kind === 'info'}
        <div class="row center"><span class="sys mono">{m.text}</span></div>
      {:else if m.kind === 'error'}
        <div class="row">
          <div class="bubble error-bubble">
            {m.text}
            <button
              type="button"
              class="msg-copy"
              title="复制全文"
              onclick={(e) => copyPlainText(m.text, e.currentTarget)}
            >复制</button>
          </div>
        </div>
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
      {:else if m.kind === 'ask_user'}
        <div class="ask-card">
          <div class="ap-head">
            Agent 提问 · 等待回答{m.timeoutSecs ? `(超时 ${m.timeoutSecs}s)` : ''}
          </div>
          <div class="ask-q">{m.question}</div>
          {#if m.result}
            {#if m.result.answered}
              <div class="ap-ok">已回答,agent 正在继续…</div>
            {:else if m.result.timedOut}
              <div class="ap-err">无人回答(超时),agent 将自行决策续跑</div>
            {:else}
              <div class="ap-err">回答通道已关闭,agent 将自行决策续跑</div>
            {/if}
          {:else if m.answerState === 'sent'}
            <div class="ap-ok">回答已提交,等待 agent 继续…</div>
          {:else if (m.options || []).length > 0}
            <div class="ask-opts">
              {#each m.options as opt}
                {#if m.multiSelect}
                  <button
                    class="ask-opt"
                    class:sel={(m.draftSel || []).includes(opt)}
                    disabled={m.answerState === 'sending'}
                    onclick={() => toggleDraftSel(m, opt)}
                  >{opt}</button>
                {:else}
                  <button
                    class="ask-opt"
                    disabled={m.answerState === 'sending'}
                    onclick={() => decideAnswer(m, [opt])}
                  >{opt}</button>
                {/if}
              {/each}
            </div>
            {#if m.multiSelect}
              <div class="ap-actions">
                <button
                  class="ap-btn approve"
                  disabled={m.answerState === 'sending' || !(m.draftSel || []).length}
                  onclick={() => decideAnswer(m, m.draftSel || [])}
                >提交所选({(m.draftSel || []).length})</button>
              </div>
            {/if}
          {:else}
            <input
              class="ask-input"
              placeholder="输入你的回答,回车发送…"
              disabled={m.answerState === 'sending'}
              value={m.draftText || ''}
              oninput={(e) => updateMessage(m.id, { draftText: e.target.value })}
              onkeydown={(e) => {
                if (e.key === 'Enter' && !e.shiftKey) {
                  e.preventDefault();
                  decideAnswer(m, [m.draftText]);
                }
              }}
            />
          {/if}
          {#if m.answerState === 'sending'}
            <div class="ap-foot">回答提交中…</div>
          {/if}
          {#if m.answerState === 'error' && m.errText}
            <div class="ap-err">{m.errText}</div>
          {/if}
        </div>
      {:else if m.kind === 'plan_card'}
        <div class="plan-card">
          <div class="ap-head">执行计划 · 等待批准{m.planTitle ? ` · ${m.planTitle}` : ''}</div>
          <ol class="plan-steps">
            {#each m.steps as s}
              <li>
                <span class="plan-step-title">{s.title}</span>
                {#if s.detail}<span class="plan-step-detail">{s.detail}</span>{/if}
              </li>
            {/each}
          </ol>
          {#if m.notes}<div class="plan-notes">备注:{m.notes}</div>{/if}
          {#if m.decision === 'approved'}
            <div class="ap-ok">已批准,agent 按计划执行(已切回构建模式)</div>
          {:else if m.decision === 'rejected'}
            <div class="ap-err">已拒绝,agent 将重新探查并提交修订计划</div>
          {:else}
            <div class="ap-actions">
              <button
                class="ap-btn approve"
                disabled={$turnActive}
                onclick={() => decidePlan(m, true)}
                title="批准后 agent 按此计划执行"
              >批准执行</button>
              <button
                class="ap-btn reject"
                disabled={$turnActive}
                onclick={() => decidePlan(m, false)}
                title="拒绝后 agent 重新探查并提交修订计划"
              >拒绝</button>
            </div>
          {/if}
        </div>
      {/if}
    {/snippet}

    {#each turnLayout as g (g.key)}
      {#if g.type === 'process'}
        {@const open = g.closed ? expandedGroups.has(g.key) : !expandedGroups.has(g.key)}
        <div class="process-box" class:live={!g.closed}>
          <button type="button" class="process-head" onclick={() => toggleGroup(g.key)}>
            <span class="tool-chev" class:rot={open}>▸</span>
            <span>过程（{g.items.length} 步 · 思考与工具调用）</span>
            {#if !g.closed}<span class="process-live">· 进行中</span>{/if}
          </button>
          {#if open}
            <div class="process-body">
              {#each g.items as m (m.id)}{@render renderMsg(m)}{/each}
            </div>
          {/if}
        </div>
      {:else}
        {@render renderMsg(g.m)}
      {/if}
    {/each}

    {#if !stickBottom && freshCount > 0}
      <button type="button" class="fresh-pill" onclick={jumpToBottom} title="跳到底部查看最新回复">
        ↓ 查看最新回复（{freshCount} 条新消息）
      </button>
    {/if}

    {#if auditCardEntry}
      <AuditCard
        entry={auditCardEntry}
        sid={$sessionId}
        verified={$auditState?.verified ?? null}
        factCount={$auditState?.factCount ?? null}
        onclose={() => (auditCardEntry = null)}
      />
    {/if}
  </div>

  {#if wsBrowserOpen}
    <DirectoryBrowser
      title="选择会话工作区(agent 读写以此目录为边界;恢复缺省请用首页工作区卡)"
      initialPath={workspace}
      on:select={async (e) => {
        wsBrowserOpen = false;
        const r = await switchWorkspace(e.detail.path, (m) => Promise.resolve(window.confirm(m)));
        if (!r.ok && r.error) alert(r.error);
      }}
      on:close={() => (wsBrowserOpen = false)}
    />
  {/if}

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
          : chatMode === 'plan'
            ? '计划模式:描述目标,agent 只读探查后出计划待你批准'
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
            class:active={chatMode === 'plan'}
            onclick={() => setMode('plan')}
            disabled={$turnActive}
            title="计划模式:agent 只读探查后提交计划,批准后才执行(适合大改动前先看方案)"
          >
            计划
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
        <div class="model-pick" bind:this={skillPickEl}>
          <button
            class="tb-btn model-btn"
            class:active={skillMenuOpen || selectedSkill}
            onclick={toggleSkillMenu}
            disabled={$turnActive}
            title="技能(active 技能选用;随下一条消息生效;发现/审批在扩展面板)"
          >
            <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
              <path
                d="M9 1.5 3.5 9H8l-1 5.5L12.5 7H8Z"
                stroke="currentColor"
                stroke-width="1.3"
                fill="none"
                stroke-linejoin="round"
              />
            </svg>
            <span class="model-name">{selectedSkill ? selectedSkill.name : '技能'}</span>
          </button>
          {#if skillMenuOpen}
            <div class="model-menu" role="listbox" aria-label="技能">
              {#if skillErr}<div class="model-menu-note err">{skillErr}</div>{/if}
              {#if skillBusy && !skillLoaded}
                <div class="model-menu-note">技能清单加载中…</div>
              {:else if skillList.length === 0}
                <div class="model-menu-note">暂无生效技能 — 扩展面板导入并审批后可选</div>
              {:else}
                {#each skillList as s (s.name)}
                  <button
                    class="model-item"
                    class:active={selectedSkill?.name === s.name}
                    role="option"
                    aria-selected={selectedSkill?.name === s.name}
                    title={s.description}
                    onclick={() => pickSkill(s)}
                    disabled={skillBusy}
                  >
                    <span class="mono">{s.name}</span>
                    {#if selectedSkill?.name === s.name}<span class="model-check">✓</span>{/if}
                  </button>
                {/each}
              {/if}
              {#if skillPendingCount > 0}
                <div class="model-menu-note">待审技能 {skillPendingCount} 项(扩展面板处理)</div>
              {/if}
              <div class="ap-note">技能随下一条消息生效;再点已选项即停用;审批/撤销在扩展面板</div>
            </div>
          {/if}
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
        <div class="approval-pick" bind:this={approvalPickEl}>
          <button
            class="tb-btn approval-btn"
            class:active={approvalMenuOpen}
            onclick={toggleApprovalMenu}
            title="审批档位(对新会话生效;agent 定义显式指定档位时以其为准)"
          >
            <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
              <path
                d="M8 1.5 13 3.6v4.1c0 3.4-2.1 5.9-5 7-2.9-1.1-5-3.6-5-7V3.6Z"
                stroke="currentColor"
                stroke-width="1.3"
                fill="none"
                stroke-linejoin="round"
              />
            </svg>
            <span class="model-name" class:exempt={approvalView.exempt}>{approvalView.label}</span>
            {#if approvalView.exempt}<span class="exempt-dot" title="红线豁免覆盖开启中"></span>{/if}
          </button>
          {#if approvalMenuOpen}
            <div class="model-menu" role="listbox" aria-label="审批档位">
              {#if approvalErr}<div class="model-menu-note err">{approvalErr}</div>{/if}
              {#each APPROVAL_PRESETS as p (p.id)}
                <button
                  class="model-item"
                  class:active={approvalView.preset === p.id}
                  role="option"
                  aria-selected={approvalView.preset === p.id}
                  onclick={() => pickApprovalPreset(p.id)}
                  disabled={approvalBusy}
                >
                  <span class="ap-label">
                    {p.label}
                    {#if p.id === 'full' && approvalView.preset === 'full'}<span class="model-check">✓</span>{/if}
                  </span>
                  <span class="ap-desc">{p.desc}</span>
                </button>
              {/each}
              <div class="ap-note">档位对新会话生效;agent 定义显式指定档位时以其为准</div>
              {#if approvalFullConfirm}
                <div class="ap-confirm" role="alert">
                  {FULL_ACCESS_CONFIRM}
                  <div class="ap-confirm-row">
                    <button class="ap-confirm-btn danger" onclick={() => pickApprovalPreset('full')} disabled={approvalBusy}>
                      确认开启
                    </button>
                    <button class="ap-confirm-btn" onclick={() => (approvalFullConfirm = false)}>取消</button>
                  </div>
                </div>
              {/if}
              <button class="model-item ap-manage" onclick={openSettingsTab}>
                管理条件文件与豁免 →
              </button>
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
    /* 宽度由宿主经 inline style 绑定 chatWidth(300–560px,持久化 evo_chat_w) */
    position: relative; /* 历史浮层的定位锚 */
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
    border-bottom: 1px solid var(--border-muted);
    flex-shrink: 0;
  }
  .title {
    font-size: var(--fs-2xs);
    font-weight: var(--fw-sb);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--text-muted);
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
    /* 互斥布局:历史打开时浮层独占对话栏,不挤压对话内容;关闭即恢复 */
    position: absolute;
    inset: 0;
    z-index: 25;
    background: var(--bg-card);
    overflow-y: auto;
    padding: var(--sp-xs);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  /* 历史打开时隐藏对话流与输入框(互斥) */
  .chat.history-open .msg-list,
  .chat.history-open .composer {
    display: none;
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
    background: var(--bg-active);
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
    font-size: var(--fs-2xs);
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
    font-size: var(--fs-2xs);
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
  .reconnect-banner {
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
    padding: 6px 10px;
    border: 1px solid var(--border-warn, #b8860b);
    border-radius: 6px;
    background: var(--bg-warn-soft, rgba(184, 134, 11, 0.12));
    font-size: var(--fs-xs);
  }
  .reconnect-banner .rb-text {
    flex: 1;
  }
  .reconnect-banner .rb-dismiss {
    border: none;
    background: none;
    color: var(--fg-muted, #888);
    cursor: pointer;
    padding: 0 2px;
    font-size: var(--fs-xs);
  }
  .reconnect-banner .rb-dismiss:hover {
    color: var(--fg, inherit);
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
    position: relative;
    /* 节奏:气泡行(回合边界/答案)前留大间隙,与紧凑的过程行拉开层级 */
    margin-top: 12px;
  }
  /* 审计徽标=气泡内右上常驻圆点,与「复制」按钮同行(复制 hover 显于其左) */
  .audit-chip {
    position: absolute;
    top: 5px;
    right: 36px;
    display: inline-flex;
  }
  .row.user {
    justify-content: flex-end;
  }
  .row.center {
    justify-content: center;
    margin-top: 4px;
  }
  .bubble {
    max-width: 88%;
    padding: var(--sp-sm) var(--sp-md);
    border-radius: var(--r-lg);
    font-size: var(--fs-md);
    line-height: var(--lh-normal);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .user-bubble {
    background: var(--brand);
    color: var(--text-inverse);
  }
  /* 答案=主角:实底卡+品牌色左标线,与低对比的过程行拉开视觉层级;
     宽度与栏左右等距(右侧不再预留区分位) */
  .assistant-bubble {
    background: var(--bg-card);
    border-left: 2px solid var(--brand);
    border-radius: var(--r-md);
    color: var(--text-primary);
    padding: var(--sp-md);
    max-width: 100%;
  }
  /* 流式中未出字的段:只留紧凑光标线,不占醒目空盒 */
  .assistant-bubble.pending {
    background: transparent;
    border-left-color: transparent;
    padding: 2px var(--sp-md);
  }
  /* 思考面板:assistant 气泡上方的深度思考折叠卡——流式自动展开淡色 mono
     限高吸底,结束后折叠条固化耗时;交互态与工具卡同构 */
  .assistant-col {
    display: flex;
    flex-direction: column;
    gap: 4px;
    /* 回复气泡与栏左右等距(项目方令:右侧留宽无必要,宽列缩短长回复) */
    max-width: 100%;
    flex: 1 1 auto;
    min-width: 0;
  }
  /* 思考=安静脚手架行:去盒、缩进、低对比紧凑;仅展开体与流式段保留视觉标记 */
  .thinking-card {
    border: none;
    border-left: 2px solid transparent;
    border-radius: var(--r-sm);
    background: transparent;
    font-size: 11px;
    margin-left: 10px;
    overflow: hidden;
  }
  .thinking-card.streaming {
    border-left-color: var(--brand);
  }
  .thinking-head {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    padding: 2px 6px;
    cursor: pointer;
    user-select: none;
  }
  .thinking-head:hover {
    background: var(--bg-hover);
  }
  .thinking-card.expanded .tool-chev {
    transform: rotate(90deg);
  }
  .thinking-label {
    color: var(--text-muted);
  }
  .thinking-body {
    margin: 0 0 var(--sp-xs) 6px;
    padding: var(--sp-xs) var(--sp-sm);
    background: var(--bg-page);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    color: var(--text-secondary);
    max-height: 180px;
    overflow-y: auto;
    white-space: pre-wrap;
    word-break: break-word;
  }
  /* markdown 正文(注入 DOM 不带 Svelte 作用域属性,后代一律 :global);
     配色全部走排版 v4.0 令牌,hljs 主题色在 app.css 按主题切换 */
  .md-body :global(p) {
    margin: 0 0 8px;
  }
  .md-body :global(p:last-child) {
    margin-bottom: 0;
  }
  .md-body :global(h1),
  .md-body :global(h2),
  .md-body :global(h3),
  .md-body :global(h4),
  .md-body :global(h5),
  .md-body :global(h6) {
    margin: 14px 0 8px;
    line-height: 1.35;
    font-weight: 600;
  }
  .md-body :global(h1):first-child,
  .md-body :global(h2):first-child,
  .md-body :global(h3):first-child,
  .md-body :global(p:first-child) {
    margin-top: 0;
  }
  .md-body :global(h1) {
    font-size: 1.25em;
  }
  .md-body :global(h2) {
    font-size: 1.15em;
  }
  .md-body :global(h3) {
    font-size: 1.05em;
  }
  .md-body :global(h4),
  .md-body :global(h5),
  .md-body :global(h6) {
    font-size: 1em;
  }
  .md-body :global(ul),
  .md-body :global(ol) {
    margin: 4px 0 8px;
    padding-left: 22px;
  }
  .md-body :global(li) {
    margin: 2px 0;
  }
  .md-body :global(blockquote) {
    margin: 6px 0;
    padding: 2px 12px;
    border-left: 3px solid var(--border-strong);
    color: var(--text-secondary);
  }
  .md-body :global(a) {
    color: var(--brand-ocean);
    text-decoration: none;
  }
  .md-body :global(a:hover) {
    text-decoration: underline;
  }
  .md-body :global(hr) {
    border: none;
    border-top: 1px solid var(--border);
    margin: 12px 0;
  }
  .md-body :global(table) {
    display: block;
    max-width: 100%;
    overflow-x: auto;
    border-collapse: collapse;
    margin: 8px 0;
    font-size: 0.92em;
  }
  .md-body :global(th),
  .md-body :global(td) {
    border: 1px solid var(--border);
    padding: 4px 10px;
    text-align: left;
  }
  .md-body :global(th) {
    background: var(--bg-card);
  }
  .md-body :global(code:not(.hljs)) {
    background: var(--bg-card);
    border: 1px solid var(--border-muted);
    border-radius: 4px;
    padding: 1px 5px;
    font-family: var(--font-mono);
    font-size: 0.92em;
  }
  .md-body :global(.md-code) {
    margin: 8px 0;
    border: 1px solid var(--border-muted);
    border-radius: 6px;
    overflow: hidden;
    background: var(--bg-card);
  }
  .md-body :global(.md-code-head) {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 3px 10px;
    border-bottom: 1px solid var(--border-muted);
    background: var(--bg-hover);
  }
  .md-body :global(.md-code-lang) {
    font-family: var(--font-mono);
    font-size: 11px;
    color: var(--text-secondary);
    text-transform: uppercase;
  }
  .md-body :global(.md-copy) {
    background: none;
    border: none;
    padding: 0;
    font-size: 11px;
    color: var(--text-secondary);
    cursor: pointer;
  }
  .md-body :global(.md-copy:hover) {
    color: var(--text-primary);
  }
  .md-body :global(.md-code pre) {
    margin: 0;
    padding: 10px 12px;
    overflow-x: auto;
  }
  .md-body :global(.hljs) {
    font-family: var(--font-mono);
    font-size: 12px;
    line-height: 1.55;
    background: transparent;
    padding: 0;
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
  /* 工具卡=安静脚手架行:与思考行同制——去盒缩进低对比,展开体才成盒 */
  .tool-card {
    border: none;
    border-left: 2px solid transparent;
    border-radius: var(--r-sm);
    background: transparent;
    font-size: 11px;
    margin-left: 10px;
    overflow: hidden;
  }
  .tool-card.running {
    border-left-color: var(--brand);
  }
  .tool-head {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    padding: 2px 6px;
    cursor: pointer;
    user-select: none;
  }
  .tool-head:hover {
    background: var(--bg-hover);
  }
  .tool-chev {
    display: inline-block;
    color: var(--text-muted);
    font-size: 10px;
    transition: transform var(--tr-fast);
  }
  .tool-card.open .tool-chev {
    transform: rotate(90deg);
  }
  .tool-name {
    color: var(--text-secondary);
    font-weight: var(--fw-sb);
  }
  .tool-state {
    margin-left: auto;
    color: var(--text-muted);
  }
  .tool-body {
    margin: 0 0 var(--sp-xs) 6px;
    padding: var(--sp-xs) var(--sp-sm);
    background: var(--bg-page);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    display: flex;
    flex-direction: column;
    gap: var(--sp-xs);
  }
  /* 撞工作区边界的卡内引导条(体验优化项 6) */
  .boundary-hint {
    margin: 4px 0;
    padding: 6px 8px;
    font-size: 11px;
    color: var(--warning);
    background: rgba(210, 153, 34, 0.12);
    border: 1px solid var(--warning);
    border-radius: var(--r-sm);
  }
  /* 回合内过程容器(项目方令):思考与工具调用收进一个框——进行中框内实时
     展开(左缘品牌色),回答抵达整框自动折叠成一行,点击头部开合 */
  .process-box {
    margin: 4px 0 4px 10px;
    border: 1px solid var(--border);
    border-radius: var(--r-md);
    background: transparent;
    overflow: hidden;
  }
  .process-box.live {
    border-left: 2px solid var(--brand);
  }
  .process-head {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    width: 100%;
    padding: 4px 8px;
    font-size: 11px;
    color: var(--text-muted);
    background: transparent;
    border: none;
    cursor: pointer;
    text-align: left;
    user-select: none;
  }
  .process-head:hover {
    background: var(--bg-hover);
    color: var(--text-secondary);
  }
  .process-head .tool-chev {
    display: inline-block;
    font-size: 10px;
    transition: transform var(--tr-fast);
  }
  .process-head .tool-chev.rot {
    transform: rotate(90deg);
  }
  .process-live {
    color: var(--brand);
  }
  .process-body {
    padding: 2px 0 4px;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .process-body .row {
    margin-top: 0;
  }
  /* 未选择工作区的引导条(术语白话化批):缺省目录对用户是黑盒,显式引导选择 */
  .ws-hint {
    margin: 0 0 var(--sp-xs);
    padding: 7px 10px;
    font-size: 11px;
    text-align: left;
    color: var(--warning);
    background: rgba(210, 153, 34, 0.12);
    border: 1px solid var(--warning);
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .ws-hint:hover {
    background: rgba(210, 153, 34, 0.22);
  }
  .process-body .tool-card,
  .process-body .thinking-card {
    margin-left: 2px;
  }
  /* 动作/结果两段式小节标题与截断徽标 */
  .tb-label {
    display: flex;
    align-items: center;
    gap: var(--sp-xs);
    font-size: var(--fs-xs);
    color: var(--text-muted);
    margin-top: 2px;
  }
  .tb-trunc {
    color: var(--warning);
  }
  /* 渲染窗/分页的「更早」入口(列表顶部) */
  .load-older {
    margin: var(--sp-xs) auto 0;
    display: block;
    padding: 4px 14px;
    font-size: var(--fs-xs);
    color: var(--text-secondary);
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 999px;
    cursor: pointer;
  }
  .load-older:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .load-older:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .tool-payload {
    margin: 0;
    padding: var(--sp-xs) var(--sp-sm);
    background: var(--bg-page);
    border-radius: var(--r-sm);
    color: var(--text-secondary);
    overflow-x: auto;
    white-space: pre-wrap;
    word-break: break-all;
  }
  .tool-full {
    align-self: flex-start;
    background: none;
    border: none;
    padding: 0;
    font-size: var(--fs-xs);
    color: var(--brand-ocean);
    cursor: pointer;
  }
  .tool-full:hover {
    text-decoration: underline;
  }
  .tool-copy {
    align-self: flex-end;
    background: none;
    border: 1px solid var(--border-muted);
    border-radius: var(--r-sm);
    padding: 1px 8px;
    font-size: 11px;
    color: var(--text-secondary);
    cursor: pointer;
  }
  .tool-copy:hover {
    color: var(--text-primary);
    border-color: var(--border-strong);
  }
  /* M2:edit_file 行级 diff 视图(红删绿增,双主题走语义令牌) */
  .diff-box {
    border: 1px solid var(--border-muted);
    border-radius: var(--r-sm);
    overflow: hidden;
    font-size: 12px;
    line-height: var(--lh-code);
  }
  .diff-row {
    display: flex;
    gap: 6px;
    padding: 0 var(--sp-xs);
    white-space: pre-wrap;
    word-break: break-all;
  }
  .diff-sign {
    flex: 0 0 8px;
    user-select: none;
  }
  .diff-text {
    flex: 1;
  }
  .diff-row.add {
    background: var(--success-bg);
    color: var(--success);
  }
  .diff-row.del {
    background: var(--danger-bg);
    color: var(--danger);
  }
  .diff-row.ctx {
    color: var(--text-secondary);
  }
  /* M2:消息级复制钮(assistant/error 气泡右上角,hover 显现) */
  .bubble {
    position: relative;
  }
  .msg-copy {
    position: absolute;
    top: 2px;
    right: 2px;
    padding: 1px 6px;
    font-size: 11px;
    color: var(--text-secondary);
    background: var(--bg-card);
    border: 1px solid var(--border-muted);
    border-radius: var(--r-sm);
    opacity: 0;
    transition: opacity var(--tr-fast);
    cursor: pointer;
  }
  .bubble:hover .msg-copy,
  .msg-copy:focus-visible {
    opacity: 1;
  }
  .msg-copy:hover {
    color: var(--text-primary);
    border-color: var(--border-strong);
  }
  /* M2:新消息浮条(msg-list 内 sticky 底部居中) */
  .fresh-pill {
    position: sticky;
    bottom: var(--sp-sm);
    align-self: center;
    z-index: 2;
    padding: 4px 12px;
    border: 1px solid var(--border-strong);
    border-radius: var(--r-full);
    background: var(--bg-card);
    color: var(--text-primary);
    font-size: var(--fs-xs);
    box-shadow: var(--sh-md);
    cursor: pointer;
  }
  .fresh-pill:hover {
    border-color: var(--brand-ocean);
    color: var(--brand-ocean);
  }
  .sys {
    font-size: var(--fs-2xs);
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
    color: var(--text-inverse);
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
  /* 批K 段7 人在环问答卡(形态同构审批卡;选项按钮+自由文本输入) */
  .ask-card {
    border: 1px solid var(--brand);
    border-radius: var(--r-md);
    background: var(--bg-page);
    padding: var(--sp-sm);
    font-size: var(--fs-xs);
  }
  /* 交互批 计划模式计划卡(形态同构问答卡;步骤列表+批准/拒绝两键) */
  .plan-card {
    border: 1px solid var(--brand);
    border-radius: var(--r-md);
    background: var(--bg-page);
    padding: var(--sp-sm);
    font-size: var(--fs-xs);
  }
  .plan-card .ap-head {
    color: var(--brand);
  }
  .plan-steps {
    margin: var(--sp-xs) 0;
    padding-left: 1.4em;
    display: flex;
    flex-direction: column;
    gap: var(--sp-2xs, 2px);
    color: var(--text-primary);
  }
  .plan-step-title {
    font-weight: var(--fw-med);
  }
  .plan-step-detail {
    display: block;
    color: var(--text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .plan-notes {
    margin-bottom: var(--sp-xs);
    color: var(--text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .ask-card .ap-head {
    color: var(--brand);
  }
  .ask-q {
    margin: var(--sp-xs) 0;
    color: var(--text-primary);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .ask-opts {
    display: flex;
    flex-direction: column;
    gap: var(--sp-xs);
    margin-bottom: var(--sp-xs);
  }
  .ask-opt {
    text-align: left;
    padding: var(--sp-xs) var(--sp-sm);
    font-size: var(--fs-xs);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    background: var(--bg-card, transparent);
    color: var(--text-secondary);
    word-break: break-word;
  }
  .ask-opt:hover:not(:disabled) {
    border-color: var(--brand);
    color: var(--text-primary);
  }
  .ask-opt.sel {
    border-color: var(--brand);
    background: var(--brand-bg, var(--bg-page));
    color: var(--text-primary);
  }
  .ask-opt:disabled {
    opacity: 0.6;
  }
  .ask-input {
    width: 100%;
    height: 28px;
    padding: 0 var(--sp-sm);
    font-size: var(--fs-xs);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    background: var(--bg-page);
    color: var(--text-primary);
  }
  .ask-input:focus {
    outline: none;
    border-color: var(--brand);
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
    font-size: var(--fs-2xs);
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
    background: var(--bg-active);
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
    font-size: var(--fs-2xs);
    color: var(--text-muted);
  }
  .m-warn {
    flex-shrink: 0;
    font-size: var(--fs-2xs);
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
    font-size: var(--fs-2xs);
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
    font-size: var(--fs-2xs);
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
    font-size: var(--fs-xs);
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
    font-size: var(--fs-2xs);
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
    color: var(--text-inverse);
  }
  /* S5 模型选择器(Trae 对齐):mode 段右侧下拉,白名单正源=执行体 */
  .model-pick {
    position: relative;
    flex-shrink: 0;
  }
  /* 审批档位选择器(批B):与 model-pick 同款定位;豁免开启态红警示 */
  .approval-pick {
    position: relative;
    flex-shrink: 0;
  }
  .approval-btn .model-name.exempt {
    color: var(--danger);
  }
  .exempt-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--danger);
    flex-shrink: 0;
  }
  .approval-pick .model-item {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
  }
  .ap-label {
    font-size: var(--fs-xs);
    color: var(--text-primary);
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .ap-desc {
    font-size: var(--fs-2xs);
    color: var(--text-muted);
  }
  .ap-note {
    font-size: var(--fs-2xs);
    color: var(--text-muted);
    padding: 4px 10px 6px;
    border-top: 1px solid var(--border);
    margin-top: 2px;
  }
  .ap-confirm {
    font-size: var(--fs-xs);
    color: var(--text-primary);
    background: var(--danger-bg, rgba(231, 76, 60, 0.12));
    border: 1px solid var(--danger);
    border-radius: var(--r-sm);
    padding: 6px 8px;
    margin: 0 6px 6px;
  }
  .ap-confirm-row {
    display: flex;
    gap: 6px;
    margin-top: 6px;
  }
  .ap-confirm-btn {
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-xs);
    padding: 2px 10px;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .ap-confirm-btn.danger {
    background: var(--danger);
    border-color: var(--danger);
    color: var(--text-inverse);
  }
  .ap-confirm-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .ap-manage {
    border-top: 1px solid var(--border);
    border-radius: 0 0 var(--r-sm) var(--r-sm);
    color: var(--brand);
    width: 100%;
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
    font-size: var(--fs-xs);
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
    font-size: var(--fs-2xs);
    color: var(--brand);
  }
  .model-menu-note {
    padding: 8px 10px;
    font-size: var(--fs-xs);
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
    border-color: var(--danger);
    color: var(--danger);
  }
  .tb-btn {
    border: none;
    background: transparent;
    color: var(--text-secondary);
    font-size: var(--fs-2xs);
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
    color: var(--text-inverse);
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
    font-size: var(--fs-2xs);
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
    font-size: var(--fs-2xs);
    color: var(--danger);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .heartbeat-stall {
    font-size: var(--fs-2xs);
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
    font-size: var(--fs-2xs);
    color: var(--text-muted);
  }
  .used-chip {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 1px 6px;
    font-size: var(--fs-2xs);
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
