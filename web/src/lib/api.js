// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 工作台文件 REST 面客户端(目录列表 / 读 / 写 / 目录浏览)
//
// 端点全部对应 serve 侧 file_api.rs;写走 file_write 同一实现(同沙箱)。
// auth 启用时以 Bearer header 接入(与 WS 的 ?token= 同一 token 来源)。
// 批 I 重锚:文件面 8 端点统一携带 X-Evo-Workspace 头(localStorage
// evo_workspace,与 WS ?workspace= 同源),serve 以该绝对路径为本请求沙箱
// 基准——文件树/编辑/搜索即刻随工作区;agent 能力边界仍由连接级参数承担。
// 头缺省 = serve workdir(严格零变化)。

export const WORKSPACE_STORAGE_KEY = 'evo_workspace';

function token() {
  return localStorage.getItem('evo_agent_token') || '';
}

function headers(json = false) {
  const h = {};
  const t = token();
  if (t) h['Authorization'] = `Bearer ${t}`;
  if (json) h['Content-Type'] = 'application/json';
  return h;
}

/** 文件面请求头 = 通用头 + 会话工作区重锚头(空工作区不携带 → workdir 根) */
function fileHeaders(json = false) {
  const h = headers(json);
  const w = (localStorage.getItem(WORKSPACE_STORAGE_KEY) || '').trim();
  if (w) h['X-Evo-Workspace'] = w;
  return h;
}

async function unwrap(resp) {
  if (!resp.ok) {
    let msg = `HTTP ${resp.status}`;
    // 先整读文本再试 JSON(JSON 失败后 body 已耗尽不能再 text());
    // serve 错误体可能是 JSON({error}) 也可能是纯文本(400 参数/正则错误说明)
    const raw = await resp.text().catch(() => '');
    if (raw) {
      try {
        const body = JSON.parse(raw);
        if (typeof body === 'string') msg = body;
        else if (body && typeof body.error === 'string') msg = body.error;
        else msg = raw;
      } catch {
        msg = raw;
      }
    }
    const err = new Error(msg);
    err.status = resp.status;
    throw err;
  }
  return resp.json();
}

/**
 * 幂等 GET 退避重试:仅网络级失败(fetch reject,serve 停机窗内连接拒绝即拒)
 * 重试,任何 HTTP 响应(含 4xx/5xx)不重试——错误语义归调用方。
 * 与 WS 重连节奏对齐(1s→2s→…封顶 30s);供断连恢复期的对账类拉取用
 * (会话列表/可恢复列表/transcript),普通用户动作型请求不走此通道。
 */
export async function getJsonRetry(path, tries = 6) {
  let delay = 1000;
  for (let i = 0; ; i++) {
    let resp;
    try {
      resp = await fetch(path, { headers: headers() });
    } catch (e) {
      if (i >= tries - 1) throw e;
      await new Promise((r) => setTimeout(r, delay));
      delay = Math.min(delay * 2, 30000);
      continue;
    }
    return unwrap(resp);
  }
}

/** 列目录(dir 相对有效根 = 工作区 || serve workdir;缺省 = 根) */
export function listDir(dir) {
  const q = dir ? `?dir=${encodeURIComponent(dir)}` : '';
  return fetch(`/api/files/list${q}`, { headers: fileHeaders() }).then(unwrap);
}

/** serve workdir 基准探测(刻意不带重锚头):工作区模式下供文件树取
 *  workdir 作 fs_events 事件换算基准(watcher 固定盯 serve workdir) */
export function listDirWorkdir() {
  return fetch('/api/files/list', { headers: headers() }).then(unwrap);
}

/**
 * 目录浏览(批 I 段 2;/api/fs/browse;选工作区专用,不携带重锚头):
 * path 缺省 → 文件系统根(Windows 盘符);显式绝对路径 → 子目录列表(只列目录)。
 * 响应 {path, parent, roots, entries:[{name,path}], error?}
 */
export function browseFs(path = '') {
  const q = path ? `?path=${encodeURIComponent(path)}` : '';
  return fetch(`/api/fs/browse${q}`, { headers: headers() }).then(unwrap);
}

/** 公告列表(批 J1 段 2;/api/notices;全局面,不携带重锚头)。
 *  响应 {notices:[{id,date,level,title,body_md,pinned,links:[{label,href}]}], error?}
 *  失败由调用方 fail-soft(公告卡整卡隐藏)。 */
export function fetchNotices() {
  return fetch('/api/notices', { headers: headers() }).then(unwrap);
}

/** 读文件(path 相对有效根) */
export function readFile(path) {
  return fetch(`/api/files/read?path=${encodeURIComponent(path)}`, {
    headers: fileHeaders(),
  }).then(unwrap);
}

/** 写文件(整体覆盖;serve 侧固定 overwrite=true + create_parents=true) */
export function writeFile(path, content) {
  return fetch('/api/files/write', {
    method: 'PUT',
    headers: fileHeaders(true),
    body: JSON.stringify({ path, content }),
  }).then(unwrap);
}

/** 创建文件/目录(kind: 'file'|'dir',缺省 file;serve 侧自动建父目录) */
export function createFile(path, kind = 'file') {
  return fetch('/api/files/create', {
    method: 'POST',
    headers: fileHeaders(true),
    body: JSON.stringify({ path, kind }),
  }).then(unwrap);
}

/** 对话附件上传(base64 → serve 解码落盘 .evo-agent/uploads/,返回 {name,path,size,mime}) */
export function uploadFile(name, dataBase64) {
  return fetch('/api/files/upload', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ name, data_base64: dataBase64 }),
  }).then(unwrap);
}

/** 移动/重命名(targetDir 必须已存在;newName 缺省保留原名) */
export function moveFile(path, targetDir, newName = null) {
  const body = { path, target_dir: targetDir };
  if (newName) body.new_name = newName;
  return fetch('/api/files/move', {
    method: 'POST',
    headers: fileHeaders(true),
    body: JSON.stringify(body),
  }).then(unwrap);
}

/** 删除(软删除进 .evo-trash 回收目录,可找回) */
export function deleteFile(path) {
  return fetch(`/api/files?path=${encodeURIComponent(path)}`, {
    method: 'DELETE',
    headers: fileHeaders(),
  }).then(unwrap);
}

/** 全局内容搜索(body 与 agent grep_files 工具参数同形) */
export function searchGrep(body) {
  return fetch('/api/files/search', {
    method: 'POST',
    headers: fileHeaders(true),
    body: JSON.stringify(body),
  }).then(unwrap);
}

/** 全局替换(apply=false 预览零写盘 / true 应用重匹配原子写) */
export function replaceFiles(body) {
  return fetch('/api/files/replace', {
    method: 'POST',
    headers: fileHeaders(true),
    body: JSON.stringify(body),
  }).then(unwrap);
}

/** 会话列表(本地索引,按最近活跃降序) */
export function listSessions() {
  return getJsonRetry('/api/sessions');
}

/** 可恢复 run 扫尾列表(崩溃会话,只读投影;恢复须显式触发,无自动续跑) */
export function listResumableRuns() {
  return getJsonRetry('/api/sessions/resumable');
}

/** 会话消息历史(活投影 source=live;TTL 回收后回落本地快照 source=snapshot)。
 *  opts 可选分页:{limit, beforeIdx}——serve 端 before_idx 游标取更早侧最新段,
 *  响应带 has_more;缺省不传=全量(向后兼容)。 */
export function getTranscript(sessionId, opts = {}) {
  const p = new URLSearchParams();
  if (Number.isFinite(opts.limit)) p.set('limit', String(opts.limit));
  if (Number.isFinite(opts.beforeIdx)) p.set('before_idx', String(opts.beforeIdx));
  const qs = p.toString();
  return getJsonRetry(
    `/api/sessions/${encodeURIComponent(sessionId)}/transcript${qs ? `?${qs}` : ''}`
  );
}

/** 工作台配置读取(快照保留期;缺省 3m) */
export function getWorkbenchConfig() {
  return fetch('/api/workbench/config', { headers: headers() }).then(unwrap);
}

/** 保存快照保留期(retention: 1d|1m|3m|6m|1y|forever) */
export function putWorkbenchConfig(retention) {
  return fetch('/api/workbench/config', {
    method: 'PUT',
    headers: headers(true),
    body: JSON.stringify({ retention }),
  }).then(unwrap);
}

/** 工作台设置:合并后全量 + 每键生效层标注 */
export function getWorkbenchSettings() {
  return fetch('/api/workbench/settings', { headers: headers() }).then(unwrap);
}

/** 写工作台设置单键(value=null 重置;键/值非法返回 400) */
export function putWorkbenchSetting(key, value) {
  return fetch('/api/workbench/settings', {
    method: 'PUT',
    headers: headers(true),
    body: JSON.stringify({ key, value }),
  }).then(unwrap);
}

/** 工作台设置注册表(schema 单源下发;UI 渲染/校验/JSON 补全共用) */
export function getWorkbenchSettingsSchema() {
  return fetch('/api/workbench/settings/schema', { headers: headers() }).then(unwrap);
}

/** LSP 两语言门状态(可用性/版本/安装引导 + 工作区根) */
export function getLspStatus() {
  return fetch('/api/lsp/status', { headers: headers() }).then(unwrap);
}

/** DAP 白名单适配器清单(运行和调试板块;fail-closed:缺省空) */
export function getDapAdapters() {
  return fetch('/api/dap/adapters', { headers: headers() }).then(unwrap);
}

/** LLM 配置只读快照(脱敏;响应体不携带任何密钥内容) */
export function getLlmStatus() {
  return fetch('/admin/llm-status', { headers: headers() }).then(unwrap);
}

/** G1:serve 日志环形缓冲尾部({count, lines})(控制台日志 tab 2s 轮询) */
export function getAdminLogs(n = 200) {
  return fetch(`/admin/logs?n=${n}`, { headers: headers() }).then(unwrap);
}

/** G2:会话记忆事件清单({count, events}) */
export function listSessionEvents(sid, entity = null) {
  const q = entity ? `?entity=${encodeURIComponent(entity)}` : '';
  return fetch(`/api/sessions/${encodeURIComponent(sid)}/events${q}`, { headers: headers() }).then(unwrap);
}

/** G3:会话回放事件链({count, events[, source, message]};无参=全量) */
export function replaySession(sid) {
  return fetch(`/api/sessions/${encodeURIComponent(sid)}/replay`, { headers: headers() }).then(unwrap);
}

// ===== 会话检查点(手动存档+引擎版本回滚) =====

/** 引擎版本时间线({session_id, latest, versions:[{version, facts}]}) */
export function listSessionVersions(sid) {
  return fetch(`/api/sessions/${encodeURIComponent(sid)}/versions`, { headers: headers() }).then(unwrap);
}

/** 手动回滚引擎 fact 链到指定版本({requested_version, actual_version};活跃轮 409) */
export function rewindSession(sid, version) {
  return fetch(`/api/sessions/${encodeURIComponent(sid)}/rewind`, {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ version }),
  }).then(unwrap);
}

/** 手动检查点列表({checkpoints:[{cp_id,label,saved_at,count,engine_version}]}) */
export function listCheckpoints(sid) {
  return fetch(`/api/sessions/${encodeURIComponent(sid)}/checkpoints`, { headers: headers() }).then(unwrap);
}

/** 打点:transcript 快照+当前引擎版本锚定({cp_id,label,engine_version,count};活跃轮 409) */
export function createCheckpoint(sid, label) {
  return fetch(`/api/sessions/${encodeURIComponent(sid)}/checkpoints`, {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ label }),
  }).then(unwrap);
}

/** 恢复检查点:引擎回滚到打点锚定版本,返回打点时刻消息({restored_version,label,messages};活跃轮 409) */
export function restoreCheckpoint(sid, cpId) {
  return fetch(`/api/sessions/${encodeURIComponent(sid)}/checkpoints/${cpId}/restore`, {
    method: 'POST',
    headers: headers(),
  }).then(unwrap);
}

/** S5:会话模型清单(执行体白名单中继;{ready, models, current[, note]}) */
export function getLlmModels() {
  return fetch('/admin/llm-models', { headers: headers() }).then(unwrap);
}

/** S5:切换会话模型(执行体不可达/清单外拒绝;成功返回 {ok, status}) */
export function setLlmModel(model) {
  return fetch('/admin/llm-model', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ model }),
  }).then(unwrap);
}

/** 审批决定(G8 既有通道;approved=true 批准 / false 拒绝) */
export function approveProposal(agentType, sessionId, approved, proposalId) {
  return fetch(`/agents/${encodeURIComponent(agentType)}/approve`, {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({
      session_id: sessionId,
      approved,
      proposal_id: proposalId,
      reason: '工作台审批',
    }),
  }).then(unwrap);
}

/** 问答回传(批K 段7 人在环 ask_user;answer 数组:单选=1 项/多选=1..=N 项/自由文本=1 项) */
export function answerQuestion(agentType, sessionId, questionId, answer) {
  return fetch(`/agents/${encodeURIComponent(agentType)}/answer`, {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({
      session_id: sessionId,
      question_id: questionId,
      answer,
    }),
  }).then(unwrap);
}

/** agent 定义(白名单徽标数据源:tools 列表) */
export function getAgentDef(agentType = 'general') {
  return fetch(`/agents/${encodeURIComponent(agentType)}`, { headers: headers() }).then(unwrap);
}

/** 会话进化信号(信号徽标数据源;只读代理 evorule-server 聚合端点) */
export function getEvolutionSignals(sessionId) {
  return fetch(`/api/sessions/${encodeURIComponent(sessionId)}/evolution-signals`, {
    headers: headers(),
  }).then(unwrap);
}

// ===== Git REST 面(B3;7 端点薄委托 serve GitOps,错误体 {error, hint?}) =====

/** 双态 status(暂存的更改 index vs HEAD + 更改 workdir vs index 含 untracked) */
export function gitStatus() {
  return fetch('/api/git/status', { headers: headers() }).then(unwrap);
}

/** 单文件两版全文 diff(HEAD vs 工作区;untracked original 为空串) */
export function gitDiff(path) {
  return fetch(`/api/git/diff?path=${encodeURIComponent(path)}`, {
    headers: headers(),
  }).then(unwrap);
}

/** 暂存(paths 数组;尾 `/` 折叠目录) */
export function gitStage(paths) {
  return fetch('/api/git/stage', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ paths }),
  }).then(unwrap);
}

/** 取消暂存(paths 数组) */
export function gitUnstage(paths) {
  return fetch('/api/git/unstage', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ paths }),
  }).then(unwrap);
}

/** 丢弃工作区变更(危险;untracked 直接删除) */
export function gitDiscard(paths) {
  return fetch('/api/git/discard', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ paths }),
  }).then(unwrap);
}

/** 提交(全量暂存后提交;身份缺失返回结构化 identity_missing + hint) */
export function gitCommit(message) {
  return fetch('/api/git/commit', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ message }),
  }).then(unwrap);
}

/** git 身份快照({identity_missing, name, email}) */
export function gitIdentity() {
  return fetch('/api/git/identity', { headers: headers() }).then(unwrap);
}

/** B15 AI commit 生成(staged diff → LLM → 提交消息;无 staged/开关关闭 400,LLM 失败 502;生成只读不提交) */
export function generateCommitMessage() {
  return fetch('/api/git/commit-message/generate', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({}),
  }).then(unwrap);
}

/** B14 语义索引:检索(q 必填,limit 缺省 20 封顶 100;mode=lexical|semantic|hybrid,缺省 lexical;semantic 未启用降级响应带 semantic:"disabled" 标注) */
export function indexSearch(q, limit = 20, mode = 'lexical') {
  const p = new URLSearchParams({ q, limit: String(limit) });
  if (mode && mode !== 'lexical') p.set('mode', mode);
  return fetch(`/api/index/search?${p}`, { headers: headers() }).then(unwrap);
}

/** B14 语义索引:就绪度与计数({files, symbols, refs, building, ready, ...}) */
export function indexStatus() {
  return fetch('/api/index/status', { headers: headers() }).then(unwrap);
}

/** B14 语义索引:全量重建(同步;在建 409) */
export function indexRebuild() {
  return fetch('/api/index/rebuild', { method: 'POST', headers: headers() }).then(unwrap);
}

// ===== MCP 工作台(B16;四端点对应 serve mcp_api.rs,未启用态 503) =====

/** MCP server 配置+运行态合并视图({servers,config_path};env 只回键名不回值防泄露) */
export function mcpListServers() {
  return fetch('/api/mcp/servers', { headers: headers() }).then(unwrap);
}

/**
 * MCP 配置全量写回并重载(校验 400 / 成功 200 回重载后合并视图)。
 * env 省略 = 保留服务端现有值,显式给出(含空对象)= 覆盖——前端拿不到
 * env 值,所有程序化写回一律省略该字段。
 */
export function mcpPutServers(servers) {
  return fetch('/api/mcp/servers', {
    method: 'PUT',
    headers: headers(true),
    body: JSON.stringify({ servers }),
  }).then(unwrap);
}

/** 单 server 断连重连重注册(404 未知名 / 502 重连失败) */
export function mcpRestartServer(name) {
  return fetch(`/api/mcp/servers/${encodeURIComponent(name)}/restart`, {
    method: 'POST',
    headers: headers(),
  }).then(unwrap);
}

/** MCP 聚合徽标({total,connected,failed,disabled,tools}) */
export function mcpStatus() {
  return fetch('/api/mcp/status', { headers: headers() }).then(unwrap);
}

/**
 * 审批策略条件文件状态(批次一 PR-4)。
 * {exists, parse_error, groups, limits, override}——exists=false 是合法态
 * (未预置条件,policy 档运行时降级 manual);parse_error 非空 = 文件无效。
 */
export function getApprovalPolicy() {
  return fetch('/api/approval-policy', { headers: headers() }).then(unwrap);
}

/** 试判定(机械预检,零 LLM):{tool, args} → {status, red_line, conditions, note, limits, override} */
export function trialApproval(tool, args) {
  return fetch('/api/approval-policy/trial', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ tool, args }),
  }).then(unwrap);
}

// ===== 编排可视化(B25;端点对应 serve orchestration_api.rs,只读 journal 投影) =====

/** 会话委托谱系树(session_id 必填;journal 为权威谱系,子状态由尾部推断) */
export function getOrchestrationTree(sessionId) {
  return fetch(`/api/orchestration/sessions?session_id=${encodeURIComponent(sessionId)}`, {
    headers: headers(),
  }).then(unwrap);
}

/** 全局子会话归属链(会话列表「委托自」徽标数据源;links: [{parent,child,agent_type,kind,depth}]) */
export function getOrchestrationLinks() {
  return fetch('/api/orchestration/sessions', { headers: headers() }).then(unwrap);
}

/** 可委托 agent 清单(图例;agents: [{agent_type,description,can_delegate}]) */
export function getOrchestrationAgents() {
  return fetch('/api/orchestration/agents', { headers: headers() }).then(unwrap);
}

/** 技能包全清单(三态;F2 扩展面板) */
export function listSkills() {
  return fetch('/api/skills', { headers: headers() })
    .then(unwrap)
    .then((d) => d.skills ?? []);
}

/** 待审批技能(discovered 项+SKILL.md 全文预览,审批前必读) */
export function listPendingSkills() {
  return fetch('/api/skills/pending', { headers: headers() })
    .then(unwrap)
    .then((d) => d.pending ?? []);
}

/** 批准技能(blake3 落账→active) */
export function approveSkill(name, level) {
  return fetch('/api/skills/approve', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ name, level }),
  }).then(unwrap);
}

/** 撤销技能(状态置 revoked) */
export function revokeSkill(name, level) {
  return fetch('/api/skills/revoke', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ name, level }),
  }).then(unwrap);
}

/** skill-pack bundle 导入(解包落盘→discovered 待审批;导入≠生效) */
export function importSkillBundle(path) {
  return fetch('/api/skills/import', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ path }),
  }).then(unwrap);
}

// ===== 会话审计面(audit-one-click;经 /console-api 同源代理直达 evorule-server) =====
// serve 侧 console_proxy 无路径白名单全量转发,鉴权由 serve 自身对上注入;
// 本地路径会话(非数字 sid)不产生审计链,调用方须先按 sid 形态分流。
// 响应形态(与 server audit_archive.rs 同形注释锚定):
// /audit → {session_id, fact_count, last_hash, verified, unhashed_records,
//           entries:[{fact_id, fact_type, logical_time, content_hash,
//                     prev_hash, cause?, content_json?(include_content)}]}

/** 会话审计报告(entries 序=逻辑时间序;include_content 时每条附 content_json) */
export function getAuditReport(sid, includeContent = false) {
  const q = includeContent ? '?include_content=true' : '';
  return fetch(
    `/console-api/api/sessions/${encodeURIComponent(sid)}/audit${q}`,
    { headers: headers() }
  ).then(unwrap);
}

/** 整链哈希校验({verified, session_id, fact_count?, last_hash?}) */
export function getAuditVerify(sid) {
  return fetch(
    `/console-api/api/sessions/${encodeURIComponent(sid)}/audit/verify`,
    { headers: headers() }
  ).then(unwrap);
}

/** 因果链({session_id, fact_id, chain_length, chain:[CausalLink]}) */
export function getAuditCausalChain(sid, factId) {
  return fetch(
    `/console-api/api/sessions/${encodeURIComponent(sid)}/audit/causal/${encodeURIComponent(factId)}`,
    { headers: headers() }
  ).then(unwrap);
}
