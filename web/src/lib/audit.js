// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 会话审计状态 store(audit-one-click)。
//
// 职责:
// 1. 会话级审计报告轻量拉取(/console-api 代理,/audit 无 content——单次
//    请求同时拿到 verified 与 entries,徽章层不再另发 verify 请求);
// 2. 徽章状态机纯函数(全分支可测,ChatSidebar/MemoryTab 共用);
// 3. M1b 顺序对齐映射:audit entries 中 fact_type==='IoResponse' 的条目
//    (entries 序=逻辑时间序=请求序)↔ UI kind==='assistant' 消息序
//    (无 tool_calls 的终稿消息)一一对应;react 中间轮不提交 io_response
//    (单 turn 单 request_id 收尾轮语义),故该对应成立。
//
// 硬承诺:audit 数据只进内存(store + 模块内 Map),禁写
// localStorage/sessionStorage;会话切换即换档,零落盘。

import { writable } from 'svelte/store';
import { getAuditReport } from './api.js';

/**
 * 会话审计状态。元素:
 * { status: 'idle'|'loading'|'ready'|'failed',
 *   supported: boolean        // 非数字 sid(本地路径会话)=false,无审计链
 *   verified: boolean|null,   // 整链哈希校验(ready 时有效)
 *   factCount: number|null,
 *   lastHash: string|null,
 *   ioEntries: Array,         // fact_type==='IoResponse' 条目(逻辑时间序,M1b 配对用)
 *   allEntries: Array         // 全量条目(轻量无 content;记忆 fact_id 直查用)
 * }
 */
export const auditState = writable(null);

/** 当前拉取目标(竞态守卫:旧会话响应到达时丢弃) */
let loadedFor = null;
let loadGen = 0;

/** 治理事件汇(可选;ws.js 接线为 pushPanelEvent('gov')——审计拉取
 *  成功/失败在审计 tab 留痕,排障有迹。经注入而非直连 stores.js,
 *  保持本模块依赖面只有 api.js) */
let govSink = null;

/** 拉取结果落账(成功/失败各一条;level: ''=正常 'warn'=失败或校验未过) */
function emitGov(detail, level = '') {
  try {
    govSink?.({ label: 'audit', detail, level });
  } catch {
    /* 留痕失败不阻断主流程 */
  }
}

/** sid 是否为 evorule 数字会话(本地路径会话为非数字串,无审计链) */
export function isAuditSupported(sid) {
  return typeof sid === 'string' && /^\d+$/.test(sid);
}

/** 拉取会话审计报告(轻量,无 content);失败静默降级(status='failed') */
export async function loadAudit(sid) {
  if (!isAuditSupported(sid)) {
    loadedFor = sid;
    auditState.set({
      status: 'ready',
      supported: false,
      verified: null,
      factCount: null,
      lastHash: null,
      ioEntries: [],
      allEntries: [],
    });
    return;
  }
  loadedFor = sid;
  const gen = ++loadGen;
  auditState.set({
    status: 'loading',
    supported: true,
    verified: null,
    factCount: null,
    lastHash: null,
    ioEntries: [],
    allEntries: [],
  });
  try {
    const res = await getAuditReport(sid, false);
    if (gen !== loadGen) return; // 已切会话,丢弃过期响应
    const entries = Array.isArray(res?.entries) ? res.entries : [];
    const verified = !!res?.verified;
    auditState.set({
      status: 'ready',
      supported: true,
      verified,
      factCount: typeof res?.fact_count === 'number' ? res.fact_count : entries.length,
      lastHash: typeof res?.last_hash === 'string' ? res.last_hash : null,
      ioEntries: entries.filter((e) => e?.fact_type === 'IoResponse'),
      allEntries: entries,
    });
    emitGov(
      `审计报告已加载(${entries.length} 条事实;${verified ? '整链哈希校验通过' : '链校验未通过（哈希链断裂）'})`,
      verified ? '' : 'warn'
    );
  } catch {
    if (gen !== loadGen) return;
    auditState.set({
      status: 'failed',
      supported: true,
      verified: null,
      factCount: null,
      lastHash: null,
      ioEntries: [],
      allEntries: [],
    });
    emitGov('审计报告拉取失败(徽章已静默隐藏;可重开会话重试)', 'warn');
  }
}

// ---- 徽章状态机(纯函数) ----

/**
 * 徽章状态。
 * @param state auditState 快照(null=未初始化)
 * @param hasMapping boolean 本条是否映射到链上事实
 * @returns 'hidden'|'red'|'green'|'gray'
 *  - 未初始化/拉取中/失败 → hidden(静默降级,不打扰对话)
 *  - 本地路径会话(不支持) → gray(无链上存证)
 *  - verified=false → red(链校验未通过)
 *  - 映射到事实 → green;verified 但无映射 → gray
 */
export function badgeStatus(state, hasMapping) {
  if (!state || state.status === 'loading' || state.status === 'failed') return 'hidden';
  if (state.supported === false) return 'gray';
  if (!state.verified) return 'red';
  return hasMapping ? 'green' : 'gray';
}

/**
 * M1b 顺序对齐:assistant 终稿消息 ↔ IoResponse entries 一一配对。
 * @param messages UI 消息数组(kind==='assistant' 的消息参与配对)
 * @param state auditState 快照(null/无 entries → 空 Map)
 * @returns Map<messageId, entry>
 */
export function mapAssistantToFacts(messages, state) {
  const map = new Map();
  if (!state || !Array.isArray(messages)) return map;
  const io = state.ioEntries || [];
  let k = 0;
  for (const m of messages) {
    if (m?.kind === 'assistant' && k < io.length) {
      map.set(m.id, io[k]);
      k++;
    }
  }
  return map;
}

/**
 * 记忆事件按 fact_id 直查(不走消息映射;MemoryTab 徽章用)。
 * @param state auditState 快照
 * @param factId number|null|undefined
 * @returns entry | null
 */
export function factForMemoryEvent(state, factId) {
  if (!state || factId == null) return null;
  const all = state.allEntries || [];
  return all.find((e) => e?.fact_id === factId) || null;
}

// ---- 会话 watcher 接线 ----

/**
 * 订阅 sessionId:会话创建/打开/切换即触发审计拉取(一处接线覆盖全部
 * 路径——newSession/openSession/恢复)。由 ws.js 模块初始化时调用一次;
 * 非数字 sid(本地路径会话)同样置档(supported=false 落灰),不发请求。
 * @param onGov 可选治理事件汇({label,detail,level})——ws.js 接线为
 *        pushPanelEvent('gov', e),审计拉取留痕进审计 tab 事件流
 */
export function initAuditWatcher(sessionIdStore, onGov = null) {
  govSink = onGov;
  sessionIdStore.subscribe((sid) => {
    if (sid && sid !== loadedFor) loadAudit(sid);
  });
}
