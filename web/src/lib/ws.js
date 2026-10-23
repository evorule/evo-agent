// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 工作台 WS 客户端 — 复用 evo-agent G16 双向流协议
// (服务端实现见 src/api/ws_handler.rs:client→server snake_case,
//  server→client PascalCase AgentEvent 帧)
import { get } from 'svelte/store';
import {
  connStatus,
  sessionId,
  turnActive,
  lastEngineEventAt,
  stepCount,
  messages,
  pushMessage,
  updateMessage,
  refreshSessions,
  loadTranscriptInto,
  pushPanelEvent,
  refreshGovBadges,
  registerArtifact,
  loadArtifacts,
  clearArtifacts,
  openFile,
  fsEvents,
  refreshGitStatus,
  govS4Notify,
} from './stores.js';
import {
  refreshOrchestration,
  orchestrationToolCall,
  orchestrationToolResult,
} from './orchestration.js';
import { getTranscript } from './api.js';
import { assembleContext } from './mentions.js';
import { looksLikeDeadSession, buildRebuildBlock, lastUserTextOf } from './sessionRebuild.js';
import { push as pushNotification } from './notifications.js';
import { initAuditWatcher } from './audit.js';

// audit-one-click:会话创建/打开/切换即触发审计报告轻量拉取(徽章数据源),
// 轮次收尾再刷新一次(新终稿徽章灰→绿);成功/失败经 pushPanelEvent('gov')
// 留痕进审计 tab 事件流。
initAuditWatcher(sessionId, turnActive, (e) => pushPanelEvent('gov', e));

let ws = null;
let streamMsgId = null; // 当前轮流式 assistant 气泡
let lastToolMsgId = null; // 最近一个运行中工具气泡(按 name 匹配结果)
let toolStartedAt = 0; // 当前工具调用起始时刻(M2:ToolResult 时算耗时 tookMs)
// M2:工具 args/result 的客户端截断帽(原 safeJson 缺省 400)。编辑类 diff
// 需完整 old_string/new_string,「查看全部」需余量,提到 20000;其余调用点缺省不变。
const TOOL_PAYLOAD_CAP = 20000;
let pendingWrite = null; // S4:进行中的 file_write {path, content}(ToolResult 成功即登记产物)
let pendingNotice = null; // newSession 清屏后需补显的提示(SessionCreated 时补显)
let pendingResume = false; // 恢复意图待发旗标(连接就绪 onopen 时补发 resume 帧)
let pendingRebuild = null; // B17:重建待发态 {block, autoSend, userText}——消费点在 SessionCreated(autoSend)/sendMessage(前缀)

// B22:S4 定稿治理留痕——stores.markFinalized 首次定稿置 govS4Notify,
// 此处消费后发 s4_publish 帧(serve 侧入 gov 账本;摘要级 best-effort,
// WS 未连即丢弃)。先复位再发送防订阅回调重入;stores 不反导 ws.js,
// 信号桥是唯一通道。
govS4Notify.subscribe((n) => {
  if (!n) return;
  govS4Notify.set(null);
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify({ type: 's4_publish', path: n.path }));
  }
});

/**
 * 死会话自愈升级(B17 持久会话):引擎侧会话失效后不再裸新建丢上下文——
 * 取历史(REST transcript:活投影→本地快照两级,与 serve 同口径)合成
 * 重建回注块,新建会话后自动续发最后一条 user 消息(前缀回注历史块);
 * 历史不可得时降级为裸重发(至少不丢用户输入)。
 */
async function recoverFromDeadSession(rawError) {
  finishStream();
  turnActive.set(false);
  disconnect(); // 先摘 socket,防 onclose 退避重连用死 sid 与自愈重建竞态
  pushPanelEvent('gov', {
    label: 'SessionRebuilt',
    detail: `死会话自愈重建:${String(rawError).slice(0, 100)}`,
    level: 'warn',
  });
  const failedUserText = lastUserTextOf(get(messages));
  const deadSid = get(sessionId);
  let block = null;
  if (deadSid) {
    try {
      const res = await getTranscript(deadSid);
      block = buildRebuildBlock(res.messages || []);
    } catch {
      block = null; // 历史取回失败 → 降级为裸重发
    }
  }
  newSession(); // 清屏/清 artifacts/清 pendingRebuild + connect('new')
  pendingNotice = failedUserText
    ? `已自动重建会话——正在续发刚才的消息${block ? '(历史已回注)' : ''}。`
    : '原会话已失效，已自动新建会话。';
  if (failedUserText) {
    // 先于 SessionCreated 就位(newSession 入口清的是旧态,此为新建态)
    pendingRebuild = { block, autoSend: true, userText: failedUserText };
  }
}

function wsUrl(sid) {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  let url = `${proto}://${location.host}/api/sessions/${sid || 'new'}/ws?agent_type=general`;
  // C 批「会话级 workspace」:localStorage 存有工作区绝对路径 → 连接级携带
  // (?workspace=,serve 面会话级能力边界根,读写同域)。改工作区在下次
  // 连接(新建会话/换线/重连)生效;缺省不携带=serve workdir 行为零变化。
  const workspace = localStorage.getItem('evo_workspace');
  if (workspace) url += `&workspace=${encodeURIComponent(workspace)}`;
  // auth 启用时走 ?token= fallback(与 SSE 同口径);token 由用户侧配置注入
  const token = localStorage.getItem('evo_agent_token');
  if (token) url += `&token=${encodeURIComponent(token)}`;
  return url;
}

// WS 非主动断开自动重连(指数退避 1s→2s→…→30s 封顶;连上即清零)
let reconnectAttempts = 0;
let reconnectTimer = null;
let currentSid = 'new'; // 最近连接目标(断连自动重连复用)
// 被动断连后在位旗标:下次 onopen 即「非计划重连」——广播 evo:reconnected
// 供对账面消费(可恢复列表刷新+中断回合横幅);主动换线/新建不置位
let reconnected = false;

function scheduleReconnect() {
  if (reconnectTimer) return;
  reconnected = true;
  const delay = Math.min(1000 * 2 ** reconnectAttempts, 30000);
  reconnectAttempts += 1;
  // 提示降噪:首次断连必提示,其后每 5 次重试提示一次(状态区恒可见连接态)
  if (reconnectAttempts === 1 || reconnectAttempts % 5 === 0) {
    const text = `连接已断开，${Math.round(delay / 1000)}s 后自动重连(第 ${reconnectAttempts} 次)…`;
    pushMessage({ kind: 'error', text });
    // F2 通知中心:断连同时入列(状态栏铃铛/右下 toast;首次 warn,后续 err 降噪档)
    pushNotification(reconnectAttempts === 1 ? 'warn' : 'err', '工作台连接断开', text);
  }
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect(currentSid);
  }, delay);
}

export function connect(sid) {
  disconnect();
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  currentSid = sid || 'new';
  connStatus.set('connecting');
  ws = new WebSocket(wsUrl(currentSid));

  ws.onopen = () => {
    connStatus.set('online');
    reconnectAttempts = 0; // 连上即清零,下次断连从 1s 重新退避
    // 恢复意图待发:resume 换线(openSession)后连接刚建立,此处补发恢复帧
    if (pendingResume) {
      pendingResume = false;
      sendResume();
    }
    // 非计划重连成功:广播对账事件(会话列表/可恢复列表消费方自行刷新)
    if (reconnected) {
      reconnected = false;
      try {
        window.dispatchEvent(
          new CustomEvent('evo:reconnected', { detail: { sid: currentSid } })
        );
      } catch {
        /* 非浏览器环境(测试)无 window,静默 */
      }
    }
  };
  ws.onclose = () => {
    connStatus.set('offline');
    turnActive.set(false);
    ws = null;
    // 非主动断开(disconnect 已置空 onclose,走到此处即被动断)
    // → 显式提示 + 指数退避自动重连;主动换线/newSession 的 connect 入口会清计时器
    scheduleReconnect();
  };
  ws.onerror = () => connStatus.set('offline');
  ws.onmessage = (ev) => {
    let frame;
    try {
      frame = JSON.parse(ev.data);
    } catch {
      return;
    }
    handleFrame(frame);
  };
}

export function disconnect() {
  if (ws) {
    const sock = ws;
    ws = null;
    try {
      sock.onclose = null;
      sock.close();
    } catch {
      /* already closing */
    }
  }
}

/**
 * 发送消息。mentions 可选(composer @引用元数据):发送前 assembleContext
 * 逐个抓取拼成 <evo-context> XML 追加在正文之后——WS 帧形状不变,扩展全部
 * 在 content 字符串内。单个 mention 失败仅记 dropped(随用户消息回显),
 * 不阻断发送。
 * @returns {Promise<{used: Array, dropped: Array} | null>} null=连接不可用
 */
export async function sendMessage(content, mentions = [], opts = {}) {
  if (!ws || ws.readyState !== WebSocket.OPEN) return null;
  // B17:快照重建待发态(autoSend=false)——本条消息即续聊新输入,wire
  // 正文前缀回注历史块(气泡仍显示原文),消费即清。autoSend=true 态归
  // SessionCreated 消费,此处不动。
  let rebuildBlock = null;
  if (pendingRebuild && !pendingRebuild.autoSend) {
    rebuildBlock = pendingRebuild.block;
    pendingRebuild = null;
  }
  let xml = '';
  let used = [];
  let dropped = [];
  if (Array.isArray(mentions) && mentions.length > 0) {
    try {
      const res = await assembleContext(mentions, {});
      xml = res.xml;
      used = res.used;
      dropped = res.dropped;
    } catch {
      xml = ''; // 组装异常按无上下文发送(正文不受影响)
    }
    // 治理留痕:同 type:ref 计次;带 dropped 数
    if (used.length > 0 || dropped.length > 0) {
      const counts = new Map();
      for (const u of used) counts.set(`${u.type}:${u.ref}`, (counts.get(`${u.type}:${u.ref}`) || 0) + 1);
      const detail =
        [...counts].map(([k, n]) => (n > 1 ? `${k}×${n}` : k)).join(',') +
        (dropped.length > 0 ? `(dropped=${dropped.length})` : '');
      pushPanelEvent('gov', { label: 'MentionSent', detail, level: 'info' });
    }
  }
  pushMessage({ kind: 'user', text: content, mentions: used, dropped });
  streamMsgId = pushMessage({ kind: 'assistant', text: '', streaming: true });
  stepCount.set(0);
  turnActive.set(true);
  // S3:对话模式 wire-only 提示块(气泡仍显示原文;仅强化先确认后变更的
  // man-in-loop 姿态,不触碰审批链语义)
  const modeHint =
    opts.mode === 'chat'
      ? '<mode>chat</mode>\n<mode-note>用户当前为对话模式:本条为咨询问答,仅分析/解释/给建议,不要执行文件修改、命令执行等变更性操作;若判断确需变更,先给出方案并等待用户确认。</mode-note>'
      : '';
  // S4:附件块(wire-only,气泡不渲染)——路径引用形态,agent 以文件工具读取;
  // 名称/路径均经服务端净化与时间戳命名,无 XML 注入面。
  // S4-b:图片类附件随消息多模态上行(images 旁路,服务端沙箱校验+魔数复检);
  // 非图片附件仍以文件形态提供路径引用。
  const atts = Array.isArray(opts.attachments)
    ? opts.attachments.filter((a) => a && a.path)
    : [];
  const images = atts
    .filter((a) => (a.mime || '').startsWith('image/'))
    .map((a) => ({ path: a.path, mime_type: a.mime }));
  const attachBlock =
    atts.length > 0
      ? `<attachments>\n${atts
          .map(
            (a) =>
              `<file name="${a.name}" path="${a.path}" size="${a.size}" mime="${a.mime || ''}"/>`
          )
          .join('\n')}\n<note>以上附件已保存于工作区相对路径,可用文件读取工具访问;图片类附件已随本消息以多模态形态上行(可直接视觉理解),文件本体仍可经路径用工具处理。</note>\n</attachments>`
      : '';
  // 技能激活块(wire-only,气泡不渲染)——会话框技能菜单选中的 active 技能,
  // 随本条消息装配;name/path 来自服务端 /api/skills 权威清单(仅 active 态
  // 可选,discovered 未经人工审批不得进入),agent 以文件工具读取 SKILL.md
  // 并遵循其纪律,直至用户明示停用。形态与 modeHint 同构,零协议改动。
  const skillBlock = opts.skill
    ? `<skill-activate name="${opts.skill.name}" path="${opts.skill.path}">\n<note>用户为本会话启用了上述技能:先用文件读取工具完整读取该 SKILL.md,再按其纪律执行本条消息及后续任务,直至用户明示停用或更换技能。</note>\n</skill-activate>`
    : '';
  // wire 正文 = [重建回注块] + [模式提示块] + [技能激活块] + 正文 + [附件块] + [@上下文 XML];气泡显示仍为原文
  const wireContent = `${rebuildBlock ? `${rebuildBlock}\n\n` : ''}${modeHint ? `${modeHint}\n\n` : ''}${skillBlock ? `${skillBlock}\n\n` : ''}${content}${attachBlock ? `\n\n${attachBlock}` : ''}${xml ? `\n\n${xml}` : ''}`;
  ws.send(
    JSON.stringify({
      type: 'message',
      content: wireContent,
      ...(images.length > 0 ? { images } : {}),
    })
  );
  return { used, dropped };
}

export function interrupt() {
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify({ type: 'interrupt' }));
  }
}

/** 手动立即重连(E1 工作区切换「立即重连」消费):断开现有 socket 并按
 *  当前目标重建——wsUrl 读 localStorage evo_workspace,新连接即携带新工作区。
 *  connect 入口会清被动退避计时器,无重连竞态。 */
export function reconnectCurrent() {
  connect(currentSid);
}

/** 新建会话:断开当前连接并以 "new" 重连 */
export function newSession() {
  pendingResume = false;
  pendingRebuild = null; // 防串台:手动新建不吃旧重建态
  sessionId.set(null);
  localStorage.removeItem('evo_session_id');
  messages.set([]);
  clearArtifacts();
  connect('new');
}

/** 打开历史会话:断开 → 回灌消息 → 按数据源分流连接(继续对话) */
export async function openSession(sid) {
  if (!sid || sid === get(sessionId)) return;
  pendingResume = false;
  pendingRebuild = null; // 防串台:换线不吃旧重建态
  sessionId.set(sid);
  localStorage.setItem('evo_session_id', sid);
  loadArtifacts(sid); // S4:恢复该会话的产物留痕(草稿基线/定稿状态)
  disconnect();
  messages.set([]);
  connStatus.set('connecting');
  let targetSid = sid;
  try {
    const res = await getTranscript(sid);
    loadTranscriptInto(res.messages || []);
    if (res.source === 'snapshot') {
      // B17:快照源=引擎会话必死(30min 闲置 TTL 回收)——主动重建而非连
      // 死会话:历史回显保留,新引擎会话在下一发消息时前缀回注历史块,
      // 不再让用户首条消息撞 404
      pendingRebuild = {
        block: buildRebuildBlock(res.messages || []),
        autoSend: false,
        userText: null,
      };
      sessionId.set(null);
      localStorage.removeItem('evo_session_id');
      targetSid = 'new';
      pushMessage({
        kind: 'info',
        text: `已恢复历史会话(${res.count ?? 0} 条消息,本地快照副本——已就绪新引擎会话,继续对话即可)`,
      });
    } else {
      pushMessage({ kind: 'info', text: `已恢复历史会话(${res.count ?? 0} 条消息)` });
    }
  } catch (e) {
    pushMessage({
      kind: 'error',
      text: `历史恢复失败:${String(e?.message || e)}(已重连,可继续发消息)`,
    });
  }
  connect(targetSid);
}

/**
 * 恢复崩溃会话(C3 前端恢复接线):发 resume 帧——无新输入,服务端加载
 * 历史+悬挂工具处置+LLM 自然续完中断轮。挂起的人工审批不会被恢复(须
 * 重走审批),发送前 info 提示明示。
 * 目标非当前会话时先换线(openSession 回灌历史),连接就绪(onopen)补发。
 * @param {string} [sid] 目标会话 id;缺省=当前会话
 */
export async function resumeSession(sid) {
  if (sid && sid !== get(sessionId)) {
    await openSession(sid);
  }
  pendingResume = true;
  // 已就绪立即发(并消费旗标);未就绪(CONNECTING)由 onopen 补发
  sendResume();
}

function sendResume() {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  pendingResume = false;
  pushMessage({
    kind: 'info',
    text: '正在恢复会话——agent 将加载历史并续完中断的轮次;挂起的人工审批不会被恢复,须重走审批。',
  });
  streamMsgId = pushMessage({ kind: 'assistant', text: '', streaming: true });
  stepCount.set(0);
  turnActive.set(true);
  ws.send(JSON.stringify({ type: 'resume' }));
}

function handleFrame(f) {
  // 引擎活性触碰:任何帧到达即刷新(心跳停滞提示的判据源)
  lastEngineEventAt.set(Date.now());
  switch (f.type) {
    case 'SessionCreated': {
      sessionId.set(f.session_id);
      localStorage.setItem('evo_session_id', f.session_id);
      pushMessage({
        kind: 'info',
        text: `会话已建立${f.memory_enabled ? '(记忆已启用)' : ''}`,
      });
      if (pendingNotice) {
        pushMessage({ kind: 'info', text: pendingNotice });
        pendingNotice = null;
      }
      if (pendingRebuild) {
        const rb = pendingRebuild;
        pendingRebuild = null; // 消费即清
        if (rb.autoSend && rb.userText) {
          // 死会话自愈自动续发:重推 user 气泡+assistant 流,wire 正文前缀
          // 回注历史块(气泡显示仍为原文)
          pushMessage({ kind: 'user', text: rb.userText });
          streamMsgId = pushMessage({ kind: 'assistant', text: '', streaming: true });
          stepCount.set(0);
          turnActive.set(true);
          ws.send(
            JSON.stringify({
              type: 'message',
              content: rb.block ? `${rb.block}\n\n${rb.userText}` : rb.userText,
            }),
          );
        }
      }
      refreshSessions();
      pushPanelEvent('sys', { label: 'SessionCreated', detail: `会话 ${f.session_id} 已建立` });
      refreshGovBadges(f.session_id);
      // B25 编排可视化:连接建立即拉 journal 权威谱系树(REST 兜底/历史合并)
      refreshOrchestration(f.session_id);
      break;
    }
    case 'Step':
      stepCount.set(f.step);
      break;
    case 'LlmDelta':
      if (streamMsgId !== null) {
        appendToStream(f.text);
      }
      break;
    case 'ToolCall': {
      toolStartedAt = Date.now();
      lastToolMsgId = pushMessage({
        kind: 'tool',
        name: f.name,
        args: safeJson(f.args, TOOL_PAYLOAD_CAP),
        running: true,
        startedAt: toolStartedAt,
      });
      // S4:记下 file_write 的目标与内容(草稿基线),待成功结果确认
      if (f.name === 'file_write' && f.args && typeof f.args.path === 'string') {
        pendingWrite = {
          path: f.args.path,
          content: typeof f.args.content === 'string' ? f.args.content : '',
        };
      }
      pushPanelEvent('gov', { label: 'ToolCall', detail: f.name, level: 'info' });
      // B25 编排可视化:delegate 调用即入图(临时 running 节点,零往返)
      if (f.name === 'delegate') {
        orchestrationToolCall(get(sessionId), f.args);
      }
      break;
    }
    case 'ToolResult': {
      if (lastToolMsgId !== null) {
        const patch = { running: false, result: safeJson(f.result, TOOL_PAYLOAD_CAP) };
        if (toolStartedAt) patch.tookMs = Date.now() - toolStartedAt;
        updateMessage(lastToolMsgId, patch);
        lastToolMsgId = null;
        toolStartedAt = 0;
      }
      // S4:file_write 成功(结果为对象含 path)→ 登记产物 + 编辑器自动打开 + 产物卡
      if (f.name === 'file_write' && pendingWrite && f.result && typeof f.result === 'object') {
        const sid = get(sessionId);
        registerArtifact(pendingWrite.path, pendingWrite.content, sid);
        openFile(pendingWrite.path);
        pushMessage({
          kind: 'artifact',
          path: pendingWrite.path,
          bytes: pendingWrite.content.length,
        });
        pendingWrite = null;
      }
      pushPanelEvent('gov', { label: 'ToolResult', detail: f.name, level: 'ok' });
      // B25 编排可视化:delegate 结果收色(ok→done / error→failed,agent_type
      // 栈顶匹配;错误结果不含 agent_type 时匹配最近在跑临时节点)
      if (f.name === 'delegate') {
        orchestrationToolResult(get(sessionId), f.result);
      }
      break;
    }
    case 'LlmDone':
      // 流式文本即最终内容;LlmDone 仅标记收尾
      break;
    case 'Done': {
      finishStream();
      if (f.success === false && f.error) {
        if (looksLikeDeadSession(String(f.error))) {
          recoverFromDeadSession(f.error);
          break;
        }
        pushMessage({ kind: 'error', text: String(f.error) });
        pushPanelEvent('gov', { label: 'TurnFailed', detail: String(f.error), level: 'error' });
      } else {
        pushPanelEvent('sys', { label: 'Done', detail: `本轮完成(step ${f.steps ?? '?'})` });
      }
      refreshGovBadges(get(sessionId));
      turnActive.set(false);
      break;
    }
    case 'Error':
      if (looksLikeDeadSession(String(f.error))) {
        recoverFromDeadSession(f.error);
        break;
      }
      finishStream();
      pushMessage({ kind: 'error', text: String(f.error) });
      pushPanelEvent('gov', { label: 'Error', detail: String(f.error), level: 'error' });
      turnActive.set(false);
      break;
    case 'Info':
      pushMessage({ kind: 'info', text: String(f.message) });
      pushPanelEvent('sys', { label: 'Info', detail: String(f.message) });
      break;
    case 'fs_events':
      // 文件系统事件批量(相对 workdir);Explorer 订阅消费,一次性信号。
      // git 装饰/SCM 面不订阅一次性信号(防被 Explorer 抢走),此处直接
      // 触发防抖刷新(B3;800ms 合并突发事件)
      fsEvents.set(f.events || { added: [], updated: [], removed: [], moved: [] });
      refreshGitStatus(800);
      break;
    case 'ApprovalRequired':
      // 治理叠加:审批卡按钮走既有 G8 /approve 通道
      pushMessage({
        kind: 'approval',
        toolName: f.tool_name,
        command: f.command,
        risk: f.risk,
        alternative: f.alternative,
        proposalId: f.proposal_id,
      });
      pushPanelEvent('gov', {
        label: 'ApprovalRequired',
        detail: `${f.tool_name}(risk ${f.risk ?? '?'}) 待审批`,
        level: 'warn',
      });
      // F2 通知中心:审批请求弹 toast+角标+1(在聊天气泡之外的全局入口)
      pushNotification(
        'warn',
        `审批请求:${f.tool_name}`,
        `${f.command || ''}(风险 ${f.risk ?? '?'})等待人工确认`,
      );
      break;
    case 'ApprovalResult':
      pushMessage({
        kind: 'info',
        text: `审批结果:${f.approved ? '已批准' : '已拒绝'}(工具 ${f.tool_name})`,
      });
      pushPanelEvent('gov', {
        label: 'ApprovalResult',
        detail: `${f.tool_name} ${f.approved ? '已批准' : `已拒绝${f.auto_rejected ? '(超时自动)' : ''}`}`,
        level: f.approved ? 'ok' : 'warn',
      });
      pushNotification(
        f.approved ? 'info' : 'warn',
        `审批${f.approved ? '已批准' : '已拒绝'}`,
        `工具 ${f.tool_name}`,
      );
      break;
    case 'AskUser':
      // 批K 段7 人在环问答卡:点选/输入走新增 POST /answer 通道(二阶段
      // 校验在工具侧,非法回答 fail-visible 回喂,agent 自行澄清续跑)
      pushMessage({
        kind: 'ask_user',
        questionId: f.question_id,
        question: f.question,
        options: f.options || [],
        multiSelect: !!f.multi_select,
        timeoutSecs: f.timeout_secs,
        answerState: null, // null | 'sending' | 'sent' | 'error'
        errText: null,
        draftSel: [], // 多选勾选项
        draftText: '', // 自由文本草稿
        result: null, // {answered, timedOut} — AskUserResult 帧到达后回填
      });
      pushPanelEvent('gov', {
        label: 'AskUser',
        detail: `等待用户回答${f.timeout_secs ? `(超时 ${f.timeout_secs}s)` : ''}`,
        level: 'warn',
      });
      pushNotification('warn', 'Agent 提问等待回答', String(f.question || '').slice(0, 80));
      break;
    case 'AskUserResult':
      // 按 question_id 收卡置态(与开启帧配对;超时/关通道 = 无人回答)
      messages.update((arr) =>
        arr.map((m) =>
          m.kind === 'ask_user' && m.questionId === f.question_id
            ? { ...m, result: { answered: !!f.answered, timedOut: !!f.timed_out } }
            : m,
        ),
      );
      pushPanelEvent('gov', {
        label: 'AskUserResult',
        detail: `问答收场:${f.answered ? '已回答' : f.timed_out ? '无人回答(超时)' : '通道关闭'}`,
        level: f.answered ? 'ok' : 'warn',
      });
      break;
    default:
      break;
  }
}

function appendToStream(text) {
  messages.update((arr) =>
    arr.map((m) => (m.id === streamMsgId ? { ...m, text: (m.text || '') + text } : m)),
  );
}

function finishStream() {
  if (streamMsgId !== null) {
    updateMessage(streamMsgId, { streaming: false });
    // 空流式气泡(如纯工具轮)移除占位
    messages.update((arr) => arr.filter((m) => !(m.id === streamMsgId && !m.text)));
    streamMsgId = null;
  }
}

function safeJson(v, cap = 400) {
  try {
    const s = JSON.stringify(v);
    return s && s.length > cap ? s.slice(0, cap) + '…' : s;
  } catch {
    return String(v);
  }
}

// 页面加载即恢复上次会话(重连续用;历史回显在对话与历史阶段接入)
export function reconnectFromStorage() {
  const sid = localStorage.getItem('evo_session_id');
  connect(sid || 'new');
}
