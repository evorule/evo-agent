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
    try {
      const body = await resp.json();
      if (typeof body === 'string') msg = body;
      else if (body && typeof body.error === 'string') msg = body.error;
    } catch {
      /* 保留默认消息 */
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

/** 会话列表(本地索引,按最近活跃降序) */
export function listSessions() {
  return fetch('/api/sessions', { headers: headers() }).then(unwrap);
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
