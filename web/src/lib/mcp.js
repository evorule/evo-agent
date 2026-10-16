// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// B16 MCP 工作台 store:server 配置/运行态快照 + 管理动作 + 纯函数(编辑器往返)。
//
// serve 是配置与运行态的单源(mcp_api.rs 合并视图),前端零合并逻辑;
// 动作走 PUT 且响应即权威回填(免一次 GET)——与 settings.js 同款模式。
// env 值永不出 serve(GET 只回 env_keys):所有程序化写回一律省略 env
// 字段(serve 端语义:省略 = 保留现值),仅编辑器内用户显式给出的 env 才覆盖。

import { writable, get } from 'svelte/store';
import { mcpListServers, mcpPutServers, mcpRestartServer } from './api.js';

/** store 形态:{ loading, enabled, servers, configPath, error }
 *  - enabled: null=未探测 / true=MCP 面可用 / false=serve 未注入(503 关闭态)
 *  - servers: 合并视图条目数组(含 status/error/tools 运行态)
 *  - error:   最近一次动作失败消息(空串 = 无) */
export const mcpState = writable({
  loading: false,
  enabled: null,
  servers: [],
  configPath: '',
  error: '',
});

/** 拉取合并视图(503 → enabled=false 关闭态;其他失败 → error 横幅,保留旧数据) */
export async function loadMcp() {
  mcpState.update((s) => ({ ...s, loading: true, error: '' }));
  try {
    const v = await mcpListServers();
    mcpState.set({
      loading: false,
      enabled: true,
      servers: Array.isArray(v?.servers) ? v.servers : [],
      configPath: v?.config_path ?? '',
      error: '',
    });
    return true;
  } catch (e) {
    if (e?.status === 503) {
      mcpState.update((s) => ({ ...s, loading: false, enabled: false, error: '' }));
    } else {
      mcpState.update((s) => ({ ...s, loading: false, error: String(e?.message || e) }));
    }
    return false;
  }
}

/** 全量写回(载荷 = 合并视图条目去运行态字段;响应 = 重载后合并视图,权威回填) */
async function putServers(servers) {
  const v = await mcpPutServers(serversToPayload(servers));
  mcpState.update((s) => ({
    ...s,
    loading: false,
    enabled: true,
    servers: Array.isArray(v?.servers) ? v.servers : s.servers,
    configPath: v?.config_path ?? s.configPath,
  }));
}

/** 启停开关(disabled 翻转;启停不删配置,停用 = 不连接不注册)。成功 true */
export async function toggleServerDisabled(name) {
  const cur = get(mcpState);
  const servers = cur.servers.map((s) => (s.name === name ? { ...s, disabled: !s.disabled } : s));
  mcpState.update((s) => ({ ...s, loading: true, error: '' }));
  try {
    await putServers(servers);
    return true;
  } catch (e) {
    mcpState.update((s) => ({ ...s, loading: false, error: String(e?.message || e) }));
    return false;
  }
}

/** per-tool alwaysAllow 勾选(白名单 = 免审批直跑;未勾 = 走审批确认)。成功 true */
export async function toggleToolAllow(serverName, serverTool) {
  const cur = get(mcpState);
  const servers = cur.servers.map((s) => {
    if (s.name !== serverName) return s;
    const allow = s.always_allow ?? [];
    const next = allow.includes(serverTool)
      ? allow.filter((t) => t !== serverTool)
      : [...allow, serverTool];
    return { ...s, always_allow: next };
  });
  mcpState.update((s) => ({ ...s, loading: true, error: '' }));
  try {
    await putServers(servers);
    return true;
  } catch (e) {
    mcpState.update((s) => ({ ...s, loading: false, error: String(e?.message || e) }));
    return false;
  }
}

/** 单 server 重启(响应非合并视图,重启后重拉一次保证工具表一致)。成功 true;
 *  失败(502 重连失败)时状态面已落 failed,先重拉呈现再挂错误横幅 */
export async function restartServer(name) {
  mcpState.update((s) => ({ ...s, loading: true, error: '' }));
  try {
    await mcpRestartServer(name);
    await loadMcp();
    return true;
  } catch (e) {
    const msg = String(e?.message || e);
    await loadMcp();
    mcpState.update((s) => ({ ...s, error: msg }));
    return false;
  }
}

/** 编辑器文本应用(解析+快校验 → PUT;返回错误消息或 null) */
export async function applyServersText(text) {
  const parsed = parseServersText(text);
  if (parsed.error) return parsed.error;
  mcpState.update((s) => ({ ...s, loading: true, error: '' }));
  try {
    await putServers(parsed.servers);
    return null;
  } catch (e) {
    const msg = String(e?.message || e);
    mcpState.update((s) => ({ ...s, loading: false, error: msg }));
    return msg;
  }
}

// ---- 纯函数(编辑器往返 / 视图派生;vitest 直测) ----

/** 合并视图条目 → PUT 载荷(去 status/error/tools/env_keys 派生字段;
 *  env 一律省略 = 保留服务端现值,绝不误清密钥;
 *  远程传输形态必须随载荷携带 transport/url,否则服务端按 stdio 校验拒绝;
 *  headers 为 ${ENV_VAR} 引用形态(GET 原样回显),随卡回传无损) */
export function serversToPayload(servers) {
  return (servers ?? []).map((s) => {
    const payload = {
      name: s.name,
      command: s.command,
      args: s.args ?? [],
      disabled: !!s.disabled,
      timeout_secs: s.timeout_secs ?? 60,
      always_allow: s.always_allow ?? [],
    };
    if (s.transport && s.transport !== 'stdio') {
      payload.transport = s.transport;
      payload.url = s.url ?? '';
      payload.headers = s.headers ?? {};
    }
    return payload;
  });
}

/** 编辑器预填文本(与 PUT 载荷同形;env 不回显) */
export function serversToText(servers) {
  return JSON.stringify({ servers: serversToPayload(servers) }, null, 2);
}

/** 编辑器文本解析 + 前端快校验(权威校验在 serve;前端只拦明显坏形)。
 *  返回 {servers} 或 {error} */
export function parseServersText(text) {
  let doc;
  try {
    doc = JSON.parse(text);
  } catch (e) {
    return { error: `JSON 解析失败:${String(e?.message || e)}` };
  }
  const servers = Array.isArray(doc) ? doc : doc?.servers;
  if (!Array.isArray(servers)) {
    return { error: '配置必须是 {"servers":[...]} 对象或 server 数组' };
  }
  for (const s of servers) {
    if (!s || typeof s !== 'object' || Array.isArray(s)) {
      return { error: 'server 条目必须是对象' };
    }
    if (typeof s.name !== 'string' || !s.name.trim()) {
      return { error: 'server name 必须是非空字符串' };
    }
    if (typeof s.command !== 'string' || !s.command.trim()) {
      return { error: `server '${s.name}' command 必须是非空字符串` };
    }
    if (
      s.timeout_secs !== undefined &&
      (typeof s.timeout_secs !== 'number' || !Number.isFinite(s.timeout_secs) || s.timeout_secs <= 0)
    ) {
      return { error: `server '${s.name}' timeout_secs 必须是正数` };
    }
    if (s.args !== undefined && !Array.isArray(s.args)) {
      return { error: `server '${s.name}' args 必须是字符串数组` };
    }
    if (s.always_allow !== undefined && !Array.isArray(s.always_allow)) {
      return { error: `server '${s.name}' always_allow 必须是字符串数组` };
    }
    if (s.env !== undefined && (typeof s.env !== 'object' || s.env === null || Array.isArray(s.env))) {
      return {
        error: `server '${s.name}' env 必须是对象(省略该字段 = 保留服务端现有值)`,
      };
    }
  }
  return { servers };
}

/** 状态点色类(success=connected / warning=starting|unconnected /
 *  danger=failed / disabled 灰) */
export function statusKind(status) {
  if (status === 'connected') return 'success';
  if (status === 'failed') return 'danger';
  if (status === 'disabled') return 'muted';
  return 'warning';
}

const STATUS_LABELS = {
  connected: '已连接',
  starting: '连接中',
  failed: '失败',
  disabled: '已停用',
  unconnected: '未连接',
};

/** 状态中文标签(未知原样透出,防新状态静默丢失) */
export function statusLabel(status) {
  return STATUS_LABELS[status] ?? status;
}

/** 聚合徽标(客户端派生,与卡片视图同源零竞态) */
export function badgeCounts(servers) {
  const list = servers ?? [];
  return {
    total: list.length,
    connected: list.filter((s) => s.status === 'connected').length,
    failed: list.filter((s) => s.status === 'failed').length,
    disabled: list.filter((s) => s.disabled).length,
    tools: list.reduce((n, s) => n + (s.status === 'connected' ? (s.tools?.length ?? 0) : 0), 0),
  };
}
