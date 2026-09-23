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
    } catch {
      /* 保留默认消息 */
    }
    throw new Error(msg);
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

/** 会话列表(本地索引,按最近活跃降序) */
export function listSessions() {
  return fetch('/api/sessions', { headers: headers() }).then(unwrap);
}

/** 会话消息历史(evorule facts 权威投影) */
export function getTranscript(sessionId) {
  return fetch(`/api/sessions/${encodeURIComponent(sessionId)}/transcript`, {
    headers: headers(),
  }).then(unwrap);
}
