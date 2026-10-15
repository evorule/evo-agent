// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 工作台文件 REST 面客户端(目录列表 / 读 / 写)
//
// 三个端点全部对应 serve 侧 file_api.rs;写走 file_write 同一实现(同沙箱)。
// auth 启用时以 Bearer header 接入(与 WS 的 ?token= 同一 token 来源)。

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

/** 列目录(dir 相对 workdir 根,缺省 = 根) */
export function listDir(dir) {
  const q = dir ? `?dir=${encodeURIComponent(dir)}` : '';
  return fetch(`/api/files/list${q}`, { headers: headers() }).then(unwrap);
}

/** 读文件(path 相对 workdir 根) */
export function readFile(path) {
  return fetch(`/api/files/read?path=${encodeURIComponent(path)}`, {
    headers: headers(),
  }).then(unwrap);
}

/** 写文件(整体覆盖;serve 侧固定 overwrite=true + create_parents=true) */
export function writeFile(path, content) {
  return fetch('/api/files/write', {
    method: 'PUT',
    headers: headers(true),
    body: JSON.stringify({ path, content }),
  }).then(unwrap);
}

/** 创建文件/目录(kind: 'file'|'dir',缺省 file;serve 侧自动建父目录) */
export function createFile(path, kind = 'file') {
  return fetch('/api/files/create', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify({ path, kind }),
  }).then(unwrap);
}

/** 移动/重命名(targetDir 必须已存在;newName 缺省保留原名) */
export function moveFile(path, targetDir, newName = null) {
  const body = { path, target_dir: targetDir };
  if (newName) body.new_name = newName;
  return fetch('/api/files/move', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify(body),
  }).then(unwrap);
}

/** 删除(软删除进 .evo-trash 回收目录,可找回) */
export function deleteFile(path) {
  return fetch(`/api/files?path=${encodeURIComponent(path)}`, {
    method: 'DELETE',
    headers: headers(),
  }).then(unwrap);
}

/** 全局内容搜索(body 与 agent grep_files 工具参数同形) */
export function searchGrep(body) {
  return fetch('/api/files/search', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify(body),
  }).then(unwrap);
}

/** 全局替换(apply=false 预览零写盘 / true 应用重匹配原子写) */
export function replaceFiles(body) {
  return fetch('/api/files/replace', {
    method: 'POST',
    headers: headers(true),
    body: JSON.stringify(body),
  }).then(unwrap);
}

/** 会话列表(本地索引,按最近活跃降序) */
export function listSessions() {
  return fetch('/api/sessions', { headers: headers() }).then(unwrap);
}

/** 可恢复 run 扫尾列表(崩溃会话,只读投影;恢复须显式触发,无自动续跑) */
export function listResumableRuns() {
  return fetch('/api/sessions/resumable', { headers: headers() }).then(unwrap);
}

/** 会话消息历史(活投影 source=live;TTL 回收后回落本地快照 source=snapshot) */
export function getTranscript(sessionId) {
  return fetch(`/api/sessions/${encodeURIComponent(sessionId)}/transcript`, {
    headers: headers(),
  }).then(unwrap);
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

/** LLM 配置只读快照(脱敏;响应体不携带任何密钥内容) */
export function getLlmStatus() {
  return fetch('/admin/llm-status', { headers: headers() }).then(unwrap);
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
