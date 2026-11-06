// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 审批会话记忆与工作区放行清单 store:三选一审批卡的「本会话内允许/本工作区
// 始终允许」两档记忆面 + 归一化前缀匹配 + 危险子命令守卫 + 回执文案纯函数。
//
// 边界(人在环红线):
// - 自动批准只作用于用户在本会话/本工作区内显式授权过的同类重复命令,
//   判定纯本地(零 LLM、零后端改动);每次自动批准仍走既有 G8 /approve 通道
//   提交,引擎侧提案生命周期与审计留痕不变。
// - 危险子命令(删除/移动/远端推送/网络外发/进程与凭据面)永不自动批准——
//   只响应显式点击,守卫优先于一切记忆与清单。
// - 工作区放行清单与后端分级判定面共用(读-改-写保全未知字段);后端
//   扩展 deny/tier/workspace_root 字段沿用同一路径与 deny 优先语义。

import { writable, get } from 'svelte/store';
import { readFile, writeFile } from './api.js';

/** 工作区放行清单路径(相对 workdir;与 .evo/approval-policy.toml 同域) */
export const ALLOWLIST_PATH = '.evo/approval-allowlist.json';

/** 未决审批卡聚合阈值:同屏未决卡超过此数聚合为一张汇总卡 */
export const AGGREGATE_THRESHOLD = 3;

// ---- 归一化与前缀(纯函数,vitest 锁) ----

/** 命令归一化:去首尾空白 + 内部空白折叠为单空格 + 统一小写。
 *  Windows 盘符与 PowerShell 命令名大小写不敏感;引号/路径分隔符级归一
 *  属持久化配置面职责,会话记忆只做等价命令复判。 */
export function normalizeCommand(cmd) {
  return String(cmd ?? '').trim().replace(/\s+/g, ' ').toLowerCase();
}

/** 归一化命令前缀:首个词元(如 find/python/git)。
 *  会话记忆与放行清单均按 (tool, prefix) 匹配——首个词元相同的同类命令
 *  免重复批准;参数级前缀由后端分级判定面接手。 */
export function commandPrefix(cmd) {
  const n = normalizeCommand(cmd);
  if (!n) return '';
  return n.split(' ')[0];
}

/** 危险子命令守卫(小写前缀表):命中者永不自动批准,仍可显式单次批准/拒绝。
 *  覆盖删除/移动/远端推送/网络外发/进程服务操纵/凭据与权限面——与红线
 *  清单同族;后端分级面接手后按其裁定扩展。 */
const AUTO_DENY_PREFIXES = [
  'rm ', 'rmdir', 'del ', 'erase ', 'remove-item', 'ri ', 'rd ',
  'mv ', 'move ', 'ren ', 'rename ',
  'git push', 'git reset', 'git clean', 'git rebase', 'git checkout', 'git restore',
  'git branch -d', 'git tag -d',
  'curl ', 'wget ', 'invoke-webrequest', 'iwr ', 'invoke-restmethod', 'irm ',
  'invoke-expression', 'iex ', 'start-process', 'start-job',
  'taskkill', 'stop-process', 'stop-service', 'restart-service',
  'reg ', 'regedit', 'schtasks', 'sc.exe', 'netsh', 'net user', 'net localgroup',
  'format ', 'diskpart', 'cipher /w',
  'ssh ', 'scp ', 'nc ', 'ncat ', 'telnet ',
  'chmod', 'chown', 'icacls', 'takeown',
];

/** 危险子命令守卫判定(纯函数):空命令按守卫处理(不自动批)。 */
export function isAutoDeny(cmd) {
  const n = normalizeCommand(cmd);
  if (!n) return true;
  return AUTO_DENY_PREFIXES.some((p) => n === p || n.startsWith(p));
}

// ---- 会话内放行记忆(内存态,新会话清空) ----

const sessionAllows = new Set(); // 键:`${tool}|${prefix}`

/** 登记会话内放行(用户点「本会话内允许」时调用) */
export function recordSessionAllow(tool, command) {
  const prefix = commandPrefix(command);
  if (tool && prefix) sessionAllows.add(`${tool}|${prefix}`);
}

/** 清空会话记忆(新建会话时调用;工作区清单不受影响) */
export function clearSessionMemory() {
  sessionAllows.clear();
}

// ---- 工作区放行清单(per-workspace 持久化) ----

/** 清单内存镜像:[{tool, prefix}] */
export const workspaceAllowlist = writable([]);

async function readAllowlistFromDisk() {
  const doc = await readAllowlistDoc();
  const allow = Array.isArray(doc?.allow)
    ? doc.allow.filter((e) => e && e.tool && e.prefix)
    : [];
  return allow;
}

/** 盘上清单原文读取(读-改-写保真基底;文件不存在/解析失败/通道错误 → null)。
 *  批②起后端分级判定面同写此文件(deny/tier/workspace_root 字段),
 *  前端追加放行项时必须整文档原样回写,不得覆盖为 {version,allow} 二件。 */
async function readAllowlistDoc() {
  try {
    const res = await readFile(ALLOWLIST_PATH);
    return JSON.parse(res?.content ?? '{}');
  } catch {
    return null;
  }
}

/** 拉取工作区放行清单(会话建立/工作区切换时调用;失败静默置空) */
export async function loadAllowlist() {
  const allow = await readAllowlistFromDisk();
  workspaceAllowlist.set(allow);
  return allow;
}

/** 追加工作区放行项并落盘(读-改-写保真:deny/tier/workspace_root 等未知
 *  字段原样回写;同项幂等;失败抛错由调用方留痕) */
export async function addWorkspaceAllow(tool, command) {
  const prefix = commandPrefix(command);
  if (!tool || !prefix) throw new Error('放行项不完整:工具与前缀均不可为空');
  const doc = await readAllowlistDoc();
  const current = Array.isArray(doc?.allow)
    ? doc.allow.filter((e) => e && e.tool && e.prefix)
    : [];
  if (current.some((e) => e.tool === tool && e.prefix === prefix)) {
    workspaceAllowlist.set(current);
    return current;
  }
  const next = [...current, { tool, prefix }];
  await writeFile(
    ALLOWLIST_PATH,
    JSON.stringify({ ...doc, version: 1, allow: next }, null, 2) + '\n',
  );
  workspaceAllowlist.set(next);
  return next;
}

// ---- 自动批准判定 ----

/** 自动批准判定(纯函数版,清单由调用方注入):返回
 *  'deny'(守卫命中,必问) | 'session' | 'workspace' | 'ask'(必问)。 */
export function autoApprovalVerdict(tool, command, allowEntries) {
  if (isAutoDeny(command)) return 'deny';
  const prefix = commandPrefix(command);
  if (!prefix) return 'deny';
  if (sessionAllows.has(`${tool}|${prefix}`)) return 'session';
  if ((allowEntries ?? []).some((e) => e.tool === tool && e.prefix === prefix)) {
    return 'workspace';
  }
  return 'ask';
}

/** 自动批准判定(实况版:用当前工作区清单内存镜像) */
export function evaluateAutoApproval(tool, command) {
  return autoApprovalVerdict(tool, command, get(workspaceAllowlist));
}

// ---- 回执与聚合文案(纯函数,vitest 锁) ----

/** 审批决策回执行文案(决策即离场:原位一行,不可展开成卡) */
export function approvalReceiptText(m) {
  const tool = m?.toolName || 'tool';
  const cmd = String(m?.command ?? '').trim();
  const cmdShow = cmd.length > 60 ? `${cmd.slice(0, 57)}…` : cmd;
  if (m?.decision === 'sending') return `${tool} · ${cmdShow} · 提交中…`;
  if (m?.decision === 'error') return `${tool} · ${cmdShow} · ${m.errText || '提交失败'}`;
  if (m?.decision === 'timeout') return `${tool} · ${cmdShow} · 已超时自动拒绝`;
  if (m?.decision === 'expired') return `${tool} · ${cmdShow} · 已随轮次结束失效`;
  if (m?.sentApproved === false) return `${tool} · ${cmdShow} · 已拒绝`;
  const tail =
    m?.scope === 'auto-session'
      ? '已自动批准(本会话内允许)'
      : m?.scope === 'auto-workspace'
        ? '已自动批准(本工作区始终允许)'
        : m?.scope === 'session'
          ? '已批准(本会话内允许)'
          : m?.scope === 'workspace'
            ? '已批准(本工作区始终允许)'
            : m?.scope === 'workspace-allowlist'
              ? '已批准(工作区清单)'
              : m?.scope === 'in-workspace'
                ? '已批准(工作区内)'
                : '已批准';
  return `${tool} · ${cmdShow} · ${tail}`;
}

/** 汇总卡摘要(按工具名计数,如「4×python_exec shell_exec」) */
export function aggregateSummary(items) {
  const counts = {};
  for (const it of items ?? []) {
    const t = it?.toolName || 'tool';
    counts[t] = (counts[t] || 0) + 1;
  }
  return Object.entries(counts)
    .map(([t, n]) => (n > 1 ? `${n}×${t}` : t))
    .join(' ');
}
