// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { writable, get } from 'svelte/store';
import {
  listSessions,
  listDir,
  readFile,
  writeFile,
  getAgentDef,
  getEvolutionSignals,
  getWorkbenchSettings,
  getTranscript,
  gitStatus as fetchGitStatus,
  gitDiff,
} from './api.js';
import { userLayerDoc } from './settings.js';

/** 连接状态: connecting | online | offline */
export const connStatus = writable('offline');

/** 断连横幅文案(null = 无横幅)。断连提示从消息转录(永久残留)改为
 *  连接状态翻转渲染——断连期间恒显示最新退避文案,重连成功/主动换线即清,
 *  零残留不跨次累积。 */
export const connBanner = writable(null);

/** 会话工作区纪元(批 I):工作区切换(任意入口)后自增,文件树订阅重建。
 *  文件面随请求头即刻重锚,此处只承担「树需要重建」的显式信号。 */
export const workspaceEpoch = writable(0);

/** 首页请求信号:活动栏「evo-agent 工作台」等入口点击后自增,
 *  EditorPane 订阅消费(切回首页态,不清已开 tab)。 */
export const homeRequested = writable(0);

/** 当前会话 id(null = 尚未创建) */
export const sessionId = writable(null);

/** 当前轮次执行中(控制发送/中断按钮与流式光标) */
export const turnActive = writable(false);

/** 扩展面板待审批数(活动栏「扩展」角标;F2) */
export const extensionsBadge = writable(0);

/** 最近一次引擎帧到达时刻(毫秒 epoch;ws 帧漏斗统一触碰)。
 *  心跳停滞提示消费:turn 活跃且久无引擎事件→前端琥珀提示(纯客户端
 *  判据,serve 心跳 2 窗中止的可见前兆) */
export const lastEngineEventAt = writable(Date.now());

/** 当前轮次步数(Step 事件累计) */
export const stepCount = writable(0);

/** 消息列表。元素: {id, kind, ...}
 *  kind: user | assistant | tool | info | error | approval */
export const messages = writable([]);

let seq = 0;
export function pushMessage(msg) {
  messages.update((arr) => [...arr, { id: ++seq, ...msg }]);
  return seq;
}
export function updateMessage(id, patch) {
  messages.update((arr) => arr.map((m) => (m.id === id ? { ...m, ...patch } : m)));
}

// ---- 会话列表(对话与历史阶段) ----

/** 会话列表(serve 本地索引,按最近活跃降序) */
export const sessions = writable([]);

/** 拉取会话列表;失败静默(列表是辅助面) */
export async function refreshSessions() {
  try {
    const res = await listSessions();
    sessions.set(res.sessions || []);
  } catch {
    /* 列表拉取失败不阻断 */
  }
}

/** MessageRecord(evorule 投影)→ UI 消息;带 tool_calls 的 assistant 记录
 *  返回数组:[叙述文字气泡(content 非空时,不丢引擎已落库的叙述)] + 调用卡。
 *  调用卡无结果时 result 不设键(undefined)——配合模板「无结果回退显示 args」,
 *  历史回灌不得出现字面量 "null" 渲染。reasoning(思考留痕)挂到 assistant
 *  气泡的 thinking 字段,历史思考卡(收起态)由此渲染。 */
export function recordToMessage(r) {
  if (r.role === 'user') {
    return { kind: 'user', text: r.content };
  }
  if (r.role === 'assistant') {
    const reasoning = typeof r.reasoning === 'string' && r.reasoning ? r.reasoning : undefined;
    let calls = [];
    if (r.tool_calls) {
      try {
        calls = Array.isArray(r.tool_calls) ? r.tool_calls : [r.tool_calls];
      } catch {
        calls = [];
      }
    }
    const cards = calls.map((c) => ({
      kind: 'tool',
      // 键形态兼容三源:引擎持久化(serde rename → tool_name/args)/
      // 内部名(name/arguments)/OpenAI wire 形状(function.name/.arguments)
      name: c?.tool_name ?? c?.name ?? c?.function?.name ?? 'tool',
      args: c?.args ?? c?.arguments ?? c?.function?.arguments ?? null,
      running: false,
    }));
    if (cards.length === 0) {
      return { kind: 'assistant', text: r.content, thinking: reasoning };
    }
    if (typeof r.content === 'string' && r.content.trim()) {
      return [{ kind: 'assistant', text: r.content, thinking: reasoning }, ...cards];
    }
    if (reasoning) {
      // 仅思考无叙述:思考卡单独成行(不丢)
      return [{ kind: 'assistant', text: '', thinking: reasoning }, ...cards];
    }
    return cards;
  }
  if (r.role === 'tool') {
    return {
      kind: 'tool',
      name: r.tool_name || 'tool',
      args: null,
      result: r.content,
      running: false,
    };
  }
  return null; // system 等不渲染
}

/** 调用卡/结果卡配对合并(transcript 投影收口):assistant.tool_calls 投影出的
 *  调用卡(有 args 无 result)与紧随的 tool 结果记录(有 result 无 args)按
 *  「工具名+顺序(FIFO)」合并为一卡(args+result 两段齐备,与实时一卡形态一致)。
 *  配不上的各自保留(孤立结果卡原样、未收结果调用卡 result 保持 undefined),
 *  不丢任何信息。非工具消息不参与配对(叙述气泡可夹在中间)。原地改写调用卡。 */
export function mergeToolPairs(entries) {
  const out = [];
  const pending = []; // 待收结果的调用卡,FIFO
  for (const m of entries) {
    if (m && m.kind === 'tool') {
      if (m.result == null && m.args != null) {
        pending.push(m);
        out.push(m);
        continue;
      }
      if (m.result != null) {
        const idx = pending.findIndex((c) => c.name === m.name);
        if (idx >= 0) {
          pending.splice(idx, 1)[0].result = m.result; // 合并进调用卡,结果卡丢弃
          continue;
        }
      }
    }
    out.push(m);
  }
  return out;
}

/** 历史消息回灌(清空当前消息,按投影顺序渲染;工具调用/结果配对合并) */
export function loadTranscriptInto(records) {
  transcriptSource = Array.isArray(records) ? [...records] : [];
  projectTranscript();
}

/** transcript 分页状态:更早侧还有未加载页(「加载更早消息」按钮数据源) */
export const transcriptHasMore = writable(false);

/** transcript 首拉页大小(超过部分走「加载更早消息」游标续拉) */
export const TRANSCRIPT_PAGE_SIZE = 200;

/** 已回灌的源记录(分页累积,重投影数据源) */
let transcriptSource = [];

/** 加载更早一页:按当前最旧 idx 取游标页,前插进源记录后整体重投影
 *  (重投影保证跨页的调用/结果配对仍成立);返回 has_more。 */
export async function loadOlderTranscript(sessionId) {
  const oldestIdx = transcriptSource.find((r) => typeof r.idx === 'number')?.idx;
  if (!sessionId || !Number.isFinite(oldestIdx)) return false;
  const res = await getTranscript(sessionId, {
    limit: TRANSCRIPT_PAGE_SIZE,
    beforeIdx: oldestIdx,
  });
  const older = Array.isArray(res.messages) ? res.messages : [];
  if (older.length === 0) {
    transcriptHasMore.set(false);
    return false;
  }
  transcriptSource = [...older, ...transcriptSource];
  projectTranscript();
  const hasMore = !!res.has_more;
  transcriptHasMore.set(hasMore);
  return hasMore;
}

/** 源记录 → 投影+配对合并+重编号,一次性落入 messages store */
function projectTranscript() {
  let id = 0;
  const out = [];
  for (const r of transcriptSource) {
    const m = recordToMessage(r);
    if (Array.isArray(m)) {
      for (const item of m) out.push({ id: ++id, ...item });
    } else if (m) {
      out.push({ id: ++id, ...m });
    }
  }
  messages.set(mergeToolPairs(out));
  if (out.length > 0) seq = Math.max(seq, id);
}

// ---- 编辑器 tabs(标准 IDE 基础阶段) ----

/** 打开的编辑器 tab 列表。元素:{path, name, content, dirty, error} */
export const tabs = writable([]);

/** 当前激活 tab 的文件路径(null = 无激活 tab,显示欢迎页) */
export const activePath = writable(null);

/** 当前焦点编辑器光标(F1 状态栏):{line, column, language}(monaco 坐标);
 *  null = 无编辑器焦点(欢迎页/设置页)。由 EditorPane/GroupPane 写入。 */
export const cursorPos = writable(null);

/** 编辑器侧统一发布光标位(焦点编辑器的事件回调里调用,天然只反映当前焦点实例) */
export function publishCursor(editor) {
  const model = editor?.getModel?.();
  const pos = editor?.getPosition?.();
  if (!model || !pos) return;
  cursorPos.set({
    line: pos.lineNumber,
    column: pos.column,
    language: model.getLanguageId?.() || '',
  });
}

/** 按路径查找已打开的 tab(tabs 快照上) */
export function findTab(list, path) {
  return list.find((t) => t.path === path);
}

/** 打开文件:已开则激活;否则请求内容后开新 tab。
 *  reveal 可选 {line, col, endCol}(serve 搜索坐标约定):tab 打开后设
 *  pendingReveal 定位信号,由 EditorPane 消费。返回错误消息或 null */
export async function openFile(path, reveal = null) {
  const name = path.split('/').pop() || path;
  let existing = null;
  tabs.update((list) => {
    existing = findTab(list, path) || null;
    return list;
  });
  if (existing) {
    activePath.set(path);
    if (reveal) pendingReveal.set({ path, ...reveal });
    return null;
  }
  try {
    const res = await readFile(path);
    const content = res.content ?? '';
    tabs.update((list) => [...list, { path, name, content, dirty: false, error: null }]);
    activePath.set(path);
    if (reveal) pendingReveal.set({ path, ...reveal });
    return null;
  } catch (e) {
    const msg = String(e?.message || e);
    tabs.update((list) => [...list, { path, name, content: '', dirty: false, error: msg }]);
    activePath.set(path);
    return msg;
  }
}

/** 确保全局 tab 存在(不激活主组;分屏副组开文件用)。返回 tab 对象或错误占位 */
export async function ensureTabOpen(path) {
  const existing = get(tabs).find((t) => t.path === path);
  if (existing) return existing;
  const name = path.split('/').pop() || path;
  try {
    const res = await readFile(path);
    const content = res.content ?? '';
    let added = null;
    tabs.update((list) => {
      added = { path, name, content, dirty: false, error: null };
      return [...list, added];
    });
    return added;
  } catch (e) {
    let added = null;
    tabs.update((list) => {
      added = { path, name, content: '', dirty: false, error: String(e?.message || e) };
      return [...list, added];
    });
    return added;
  }
}

/** 关闭 tab:返回相邻 tab 路径(若关的是激活 tab) */
export function closeTab(path) {
  let next = null;
  tabs.update((list) => {
    const idx = list.findIndex((t) => t.path === path);
    const rest = list.filter((t) => t.path !== path);
    if (rest.length > 0) {
      next = rest[Math.min(idx, rest.length - 1)].path;
    }
    return rest;
  });
  activePath.update((cur) => (cur === path ? next : cur));
}

/** 文件系统事件批量(ws fs_events 帧投影;Explorer 订阅增量刷新)。
 *  值被消费后由订阅方置 null(一次性信号),形如
 *  {added:[], updated:[], removed:[], moved:[{from,to}]}(相对 workdir 路径) */
export const fsEvents = writable(null);

/** H2 文件树定位信号(面包屑/命令共用):{path}(相对 workdir 的文件或目录)。
 *  消费方 Explorer:切到文件树视图 + 逐级展开祖先 + 滚动定位+闪烁;消费即清 */
export const pendingTreeReveal = writable(null);

// ---- @上下文文件索引(B13;懒 BFS,fs_events 失效,2000 条上限) ----

/** 文件索引快照 [{path, name, isDir}](相对 workdir,路径升序)。
 *  懒构建:弹层打开时经 ensureFileIndex 触发;fs_events 触发后置陈旧,下次重建 */
export const fileIndex = writable([]);

/** 索引是否已构建过(至少完成一次 BFS) */
export const fileIndexReady = writable(false);

/** @上下文 mention 元数据(composer chip 托盘)。
 *  元素:{type, ref, insertAt, token}——type 见 mentions.js MENTION_GROUPS,
 *  ref 为实体标识(文件=相对路径),insertAt 为 draft 中 @token 起始偏移,
 *  token 为文本内 @token 原文(供 chip 删除与发送前对账) */
export const composerMentions = writable([]);

const FILE_INDEX_LIMIT = 2000;
// 高频/体积大的目录不遍历(目录条目本身保留在索引,可被 @ 搜到)
const FILE_INDEX_SKIP_DIRS = new Set([
  'node_modules',
  '.git',
  '.svn',
  '.hg',
  'dist',
  'build',
  'target',
  'out',
  '.next',
  '.nuxt',
  '.cache',
  '__pycache__',
  '.venv',
  'venv',
  'coverage',
  '.idea',
]);

let indexBuilding = null;
let indexStale = false;

/** 构建/刷新文件索引(懒 BFS)。
 *  force=true 强制重建;无 force 时:在途复用,已就绪且未陈旧则直接返回快照 */
export function ensureFileIndex(force = false) {
  if (!force) {
    if (indexBuilding) return indexBuilding;
    if (!indexStale && get(fileIndexReady)) return Promise.resolve(get(fileIndex));
  }
  indexStale = false;
  const p = (async () => {
    try {
      const out = [];
      const visited = new Set(['']);
      const queue = [''];
      while (queue.length > 0 && out.length < FILE_INDEX_LIMIT) {
        const dir = queue.shift();
        let res;
        try {
          res = await listDir(dir || undefined);
        } catch {
          continue; // 目录读取失败(权限/竞态删除)跳过,不致命
        }
        for (const e of res.entries || []) {
          if (out.length >= FILE_INDEX_LIMIT) break;
          const path = dir ? `${dir}/${e.name}` : e.name;
          // size 保留:token 估算条的 @ 文件权重来源(file_list 缺省带字节数)
          out.push({
            path,
            name: e.name,
            isDir: e.kind === 'dir',
            size: Number.isFinite(e.size) ? e.size : null,
          });
          if (e.kind === 'dir' && !FILE_INDEX_SKIP_DIRS.has(e.name) && !visited.has(path)) {
            visited.add(path);
            queue.push(path);
          }
        }
      }
      out.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
      fileIndex.set(out);
      fileIndexReady.set(true);
      return out;
    } finally {
      if (indexBuilding === p) indexBuilding = null;
    }
  })();
  return p;
}

// 索引失效钩子:fs_events 一次性信号,多订阅方各自消费互不影响
// (Svelte store 同步广播,Explorer 等订阅方 set(null) 不吞掉本钩子的事件)
fsEvents.subscribe((ev) => {
  if (ev) indexStale = true;
});

// ---- Git 状态(B3;单状态源:SCM 视图与 Explorer 装饰共同订阅) ----

/** git 双态 status(null = 非 git 仓 / 未加载 / 拉取失败)。
 *  形如 {branch, dirty, staged:[{path,status}], changes:[{path,status}]}(serve GitStatus)。
 *  刷新触发:ws fs_events 帧(800ms 防抖)/ SCM 面操作后 / 启动加载 / 手动刷新 */
export const gitStatus = writable(null);

let gitRefreshTimer = null;
let gitRefreshInFlight = false;
let gitRefreshQueued = false;

/** 刷新 git status(默认 800ms 防抖,合并突发 fs 事件;in-flight 期间新触发
 *  合并为一次尾巴刷新,保证最终一致)。delay=0 供操作后立即刷新 */
export function refreshGitStatus(delay = 800) {
  if (gitRefreshTimer) clearTimeout(gitRefreshTimer);
  gitRefreshTimer = setTimeout(() => {
    gitRefreshTimer = null;
    if (gitRefreshInFlight) {
      gitRefreshQueued = true;
      return;
    }
    gitRefreshInFlight = true;
    fetchGitStatus()
      .then((st) => gitStatus.set(st))
      .catch(() => gitStatus.set(null)) // 非 git 仓/网络失败 → 无 git 面
      .finally(() => {
        gitRefreshInFlight = false;
        if (gitRefreshQueued) {
          gitRefreshQueued = false;
          refreshGitStatus(0);
        }
      });
  }, delay);
}

/** 打开 git diff 视图信号(一次性):ScmView 行点击 / git.openDiff 命令设置,
 *  EditorPane 消费(打开 kind:'git-diff' 特殊 tab)后置 null。值为相对路径 */
export const pendingGitDiff = writable(null);

/** 请求打开某文件的 git diff 虚拟 tab(git-diff://<path>) */
export function openGitDiff(path) {
  if (!path) return;
  pendingGitDiff.set(path);
}

const GIT_DIFF_SCHEME = 'git-diff://';

/** 打开 git diff 虚拟 tab(kind:'git-diff';快照 original/modified 随 tab 携带,
 *  EditorPane 构建只读 DiffEditor 模型)。失败以错误占位 tab 呈现。 */
export async function openGitDiffTab(path) {
  if (!path) return;
  const virtual = `${GIT_DIFF_SCHEME}${path}`;
  const name = `${(path.split('/').pop() || path)}(工作区)`;
  let existing = null;
  tabs.update((list) => {
    existing = list.find((t) => t.path === virtual) || null;
    return list;
  });
  if (existing) {
    activePath.set(virtual);
    return;
  }
  try {
    const d = await gitDiff(path);
    tabs.update((list) => [
      ...list,
      {
        path: virtual,
        kind: 'git-diff',
        gitPath: path,
        name,
        content: '',
        dirty: false,
        error: null,
        original: d.original ?? '',
        modified: d.modified ?? '',
        language: d.language || 'plaintext',
      },
    ]);
    activePath.set(virtual);
  } catch (e) {
    const msg = String(e?.message || e);
    tabs.update((list) => [
      ...list,
      { path: virtual, kind: 'git-diff', gitPath: path, name, content: '', dirty: false, error: msg },
    ]);
    activePath.set(virtual);
  }
}

/** 最近一次 tab 路径迁移信号 {from, to, seq}(EditorPane 订阅迁移按路径
 *  键缓存的 model,保 dirty 内容;先于 tabs 更新发出——订阅方迁移缓存的
 *  顺序必须早于 tabs-diff 清理,晚到会销毁含未保存内容的 model) */
export const lastRename = writable(null);

let renameSeq = 0;

/** 重命名/移动 tab 路径联动(tabs + activePath;dirty 内容保留在 model 中,
 *  由 lastRename 信号驱动 EditorPane 迁移缓存键) */
export function renameTabPath(from, to) {
  if (!from || from === to) return;
  const name = to.split('/').pop() || to;
  lastRename.set({ from, to, seq: ++renameSeq });
  tabs.update((list) => list.map((t) => (t.path === from ? { ...t, path: to, name } : t)));
  activePath.update((cur) => (cur === from ? to : cur));
}

/** 标记 tab 内容已变(dirty) */
export function markDirty(path, dirty) {
  tabs.update((list) => list.map((t) => (t.path === path ? { ...t, dirty } : t)));
}

/** 保存完成后回写基准内容与 dirty=false */
export function markSaved(path, content) {
  tabs.update((list) =>
    list.map((t) => (t.path === path ? { ...t, content, dirty: false, error: null } : t)),
  );
}

/** 保存文件(content 由调用方从编辑器 model 取)。返回错误消息或 null */
export async function saveTab(path, content) {
  try {
    await writeFile(path, content);
    markSaved(path, content);
    markFinalized(path); // S4:产物文件首次人工保存 = 定稿
    return null;
  } catch (e) {
    return String(e?.message || e);
  }
}

/** 设置页虚拟路径(特殊 tab,非文件;EditorPane 按 kind 渲染设置组件) */
export const SETTINGS_TAB_PATH = 'evo://settings';

/** 用户层设置 JSON 虚拟路径(编辑器 tab;内容=生效层为 user 的键值集) */
export const USER_SETTINGS_URI = 'evo://settings/user';

/** 工作区层设置 JSON 虚拟路径(编辑器 tab;内容=.evo/settings.json 直读直写) */
export const WORKSPACE_SETTINGS_URI = 'evo://settings/workspace';

/** 打开设置页(特殊 tab;已开则激活) */
export function openSettingsTab() {
  let existing = null;
  tabs.update((list) => {
    existing = findTab(list, SETTINGS_TAB_PATH) || null;
    return list;
  });
  if (existing) {
    activePath.set(SETTINGS_TAB_PATH);
    return;
  }
  tabs.update((list) => [
    ...list,
    { path: SETTINGS_TAB_PATH, kind: 'settings', name: '设置', content: '', dirty: false, error: null },
  ]);
  activePath.set(SETTINGS_TAB_PATH);
}

/**
 * 打开设置 JSON 文档(user=派生快照 / workspace=工作区文件;已开则激活,
 * 保留未保存编辑)。返回错误消息或 null(错误也以错误占位 tab 呈现)。
 */
export async function openSettingsJson(mode = 'user') {
  const path = mode === 'workspace' ? WORKSPACE_SETTINGS_URI : USER_SETTINGS_URI;
  const name = mode === 'workspace' ? 'settings.json (工作区)' : 'settings.json';
  let existing = null;
  tabs.update((list) => {
    existing = findTab(list, path) || null;
    return list;
  });
  if (existing) {
    activePath.set(path);
    return null;
  }
  let content = '';
  try {
    if (mode === 'workspace') {
      try {
        const res = await readFile('.evo/settings.json');
        content = res.content ?? '{}';
      } catch {
        content = '{}'; // 工作区层文件尚不存在,保存即创建(serve 侧自动建父目录)
      }
    } else {
      const res = await getWorkbenchSettings();
      content =
        JSON.stringify(
          userLayerDoc({ settings: res?.settings ?? {}, sources: res?.sources ?? {} }),
          null,
          2,
        ) + '\n';
    }
  } catch (e) {
    const msg = String(e?.message || e);
    tabs.update((list) => [...list, { path, name, content: '', dirty: false, error: msg }]);
    activePath.set(path);
    return msg;
  }
  tabs.update((list) => [...list, { path, name, content, dirty: false, error: null }]);
  activePath.set(path);
  return null;
}

// ---- 产物协作编辑阶段(S4):agent 草稿 → 人定稿 ----
//
// 语义边界(立项设计阶段澄清,项目方默认接受):「agent 草稿 vs 人定稿」为工作台
// 层面的对比留痕(localStorage 展示层持久化记录),不入 agent 会话审计链。

/** 当前会话产物登记。元素:
 *  {path, name, draftContent, draftAt, bytes, finalizedAt}
 *  - draftContent: agent file_write 写入时的内容快照(diff 草稿基线)
 *  - draftAt: agent 写入时间戳
 *  - finalizedAt: 人首次保存(Ctrl+S)定稿时间戳;null = 仍是草稿 */
export const artifacts = writable([]);

const ARTIFACT_KEY = 'evo_artifacts';
const ARTIFACT_PERSIST_LIMIT = 2 * 1024 * 1024; // localStorage 持久化总量上限,超限只留内存

function persistArtifacts(list, sid) {
  try {
    if (!sid) return;
    const all = JSON.parse(localStorage.getItem(ARTIFACT_KEY) || '{}');
    all[sid] = list;
    const total = Object.values(all).reduce((n, arr) => n + JSON.stringify(arr).length, 0);
    if (total > ARTIFACT_PERSIST_LIMIT) return; // 超限静默跳过(留痕是尽力而为的展示层记录)
    localStorage.setItem(ARTIFACT_KEY, JSON.stringify(all));
  } catch {
    /* 存储不可用(隐私模式等)时留痕只存活于内存 */
  }
}

/** 载入某会话的产物留痕(切换/恢复会话时调用) */
export function loadArtifacts(sid) {
  try {
    const all = JSON.parse(localStorage.getItem(ARTIFACT_KEY) || '{}');
    artifacts.set(Array.isArray(all[sid]) ? all[sid] : []);
  } catch {
    artifacts.set([]);
  }
}

/** 清空当前产物视图(新建会话时调用;已持久化的留痕不动) */
export function clearArtifacts() {
  artifacts.set([]);
}

/** 登记产物(agent file_write 成功时调用);同路径重复写入 = 刷新草稿基线 */
export function registerArtifact(path, draftContent, sid) {
  const name = path.split('/').pop() || path;
  artifacts.update((list) => {
    const entry = {
      path,
      name,
      draftContent: draftContent || '',
      draftAt: Date.now(),
      bytes: (draftContent || '').length,
      finalizedAt: null,
    };
    const idx = list.findIndex((a) => a.path === path);
    const next = [...list];
    if (idx >= 0) next[idx] = entry;
    else next.push(entry);
    persistArtifacts(next, sid);
    return next;
  });
}

/** 定稿标记(保存成功后调用;仅对已登记产物生效,幂等)。
 *  返回是否发生首次定稿(true = 本次完成定稿;false = 已定稿或非登记产物)。
 *  首次定稿置 govS4Notify 信号——ws.js 消费后发 s4_publish 帧入 gov 账本
 *  (B22 治理留痕,摘要级 best-effort,WS 未连即丢弃不重试) */
export function markFinalized(path) {
  const sid = get(sessionId);
  const isFirst = get(artifacts).some((a) => a.path === path && a.finalizedAt === null);
  if (!isFirst) return false;
  artifacts.update((list) => {
    if (!list.some((a) => a.path === path && a.finalizedAt === null)) return list;
    const next = list.map((a) => (a.path === path && a.finalizedAt === null ? { ...a, finalizedAt: Date.now() } : a));
    persistArtifacts(next, sid);
    return next;
  });
  govS4Notify.set({ path });
  return true;
}

// ---- 命令基础设施:视图显隐 + 命令面板状态 ----

/** 文件树显隐(Ctrl+B;display 切换保留组件状态,不销毁重建) */
export const explorerVisible = writable(true);

/** 活动栏侧面板视图:explorer = 文件树 / search = 全局搜索(B2)。
 *  ActivityBar 高亮与 App.svelte 侧面板分支共同订阅 */
export const sidebarView = writable('explorer');

/** 定位跳转信号(一次性):openFile(path, reveal) 打开后设置,
 *  EditorPane 消费(revealLineInCenter+setSelection+focus)后置 null。
 *  元素:{path, line, col, endCol};line 1-based,col/endCol 0-based char 偏移
 *  (serve 端搜索坐标约定;Monaco 消费时 +1)。问题面板/输出跳转直接复用 */
export const pendingReveal = writable(null);

/** 搜索面板意图信号(一次性):命令入口(Ctrl+Shift+F/H、清除)→ SearchPanel 消费。
 *  值:'show' 聚焦查询框 / 'replace' 展开替换区并聚焦 / 'clear' 清空查询与结果 */
export const searchIntent = writable(null);

/** 对话侧栏显隐(Ctrl+Alt+C) */
export const chatVisible = writable(true);

/** 底部面板开合(Ctrl+J;自 BottomPanel 组件内状态提升,命令层可切换) */
export const panelVisible = writable(false);

/** 命令面板开合(Ctrl+Shift+P / Ctrl+P) */
export const paletteOpen = writable(false);

/** 命令面板模式:commands = 命令(Ctrl+Shift+P);files = 文件/会话(Ctrl+P) */
export const paletteMode = writable('commands');

/** 打开命令面板(统一入口:置模式再开) */
export function openPalette(mode = 'commands') {
  paletteMode.set(mode);
  paletteOpen.set(true);
}

/** 关闭命令面板 */
export function closePalette() {
  paletteOpen.set(false);
}

// ---- 治理叠加阶段:事件流水(底部面板)+ 治理徽标 ----

const EVENT_CAP = 500; // 环形缓冲上限,防长会话内存膨胀

function pushCapped(arr, entry) {
  const next = [...arr, entry];
  return next.length > EVENT_CAP ? next.slice(next.length - EVENT_CAP) : next;
}

/** 系统事件流水(输出 tab):SessionCreated/Step/Info/Error/Done 等非内容帧 */
export const sysEvents = writable([]);

/** 治理事件流(审计 tab):工具调用/审批/错误等治理可见事件 */
export const govEvents = writable([]);

/** B15:AI 生成提交消息意图(命令面板触发;自增计数,ScmView 消费后触发生成) */
export const scmGenerateIntent = writable(0);

/** B22:S4 定稿治理留痕信号(一次性):markFinalized 首次定稿产物时置
 *  {path},ws.js 订阅后发 s4_publish 帧入 gov 账本(单向依赖——stores
 *  不反导 ws.js,经信号桥接;消费后复位 null) */
export const govS4Notify = writable(null);

let evSeq = 0;
/** 底部面板事件入口(kind: sys | gov;label/level 决定渲染样式) */
export function pushPanelEvent(kind, entry) {
  const e = { id: ++evSeq, time: Date.now(), ...entry };
  if (kind === 'gov') {
    govEvents.update((arr) => pushCapped(arr, e));
  } else {
    sysEvents.update((arr) => pushCapped(arr, e));
  }
}

/** agent 工具白名单(白名单徽标;null = 未加载) */
export const toolWhitelist = writable(null);

/** 当前会话违规信号数(信号徽标;null = 未加载/不可用) */
export const signalCount = writable(null);

/** 加载治理徽标数据(白名单 + 信号;均 fail-soft,失败置 null 显示「—」) */
export function refreshGovBadges(sid) {
  getAgentDef('general')
    .then((def) => toolWhitelist.set(Array.isArray(def.tools) ? def.tools : []))
    .catch(() => toolWhitelist.set(null));
  if (sid) {
    getEvolutionSignals(sid)
      .then((s) => signalCount.set(typeof s.total_violations === 'number' ? s.total_violations : null))
      .catch(() => signalCount.set(null));
  } else {
    signalCount.set(null);
  }
}
