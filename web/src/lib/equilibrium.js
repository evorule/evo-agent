// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 均衡观测面(store+轮询+呈现映射,纯读态):
// 数据源 /api/equilibrium/status(serve 代理,evorule-server equilibrium 块);
// 固定 10s 慢轮询(不做 crisis 自适应加速,UI 是纯读观测者);
// 观测不可用=灰置(≠平衡,防假绿);着色/数据行映射为纯函数(可测)。

import { writable } from 'svelte/store';
import { fetchEquilibrium } from './api.js';

/** 轮询节流(固定 10s 慢节奏) */
export const EQUILIBRIUM_POLL_MS = 10000;

/**
 * 均衡观测 store:{available, data, detail, observedAt}
 * - available=false:观测不可用(503/网络失败/尚未首拉)——UI 灰置
 * - data:server equilibrium 块原样(字段随 server 只加不改演进)
 */
export const equilibrium = writable({ available: false, data: null, detail: '', observedAt: 0 });

let timer = null;

async function pollOnce() {
  const r = await fetchEquilibrium();
  if (r.ok) {
    equilibrium.set({
      available: true,
      data: r.equilibrium,
      detail: '',
      observedAt: r.observed_at || 0,
    });
  } else {
    equilibrium.set({ available: false, data: null, detail: r.detail, observedAt: 0 });
  }
}

/** 启动轮询(幂等;立即首拉一次再进 10s 节奏) */
export function startEquilibriumPolling() {
  if (timer) return;
  pollOnce();
  timer = setInterval(pollOnce, EQUILIBRIUM_POLL_MS);
}

/** 停止轮询(组件销毁时调用;store 保持最后状态不清零) */
export function stopEquilibriumPolling() {
  if (timer) {
    clearInterval(timer);
    timer = null;
  }
}

/**
 * 全局相位着色映射(纯函数):
 * absorbing→'balanced'(灰绿)/relaxing→'relaxing'(黄)/crisis→'crisis'(红);
 * 缺失/未知值→'unknown'(灰)——未知相位绝不映射成任何相位色(防假绿)。
 */
export function phaseColor(phase) {
  switch (phase) {
    case 'absorbing':
      return 'balanced';
    case 'relaxing':
      return 'relaxing';
    case 'crisis':
      return 'crisis';
    default:
      return 'unknown';
  }
}

/**
 * 三层相位行(纯函数;popover 主体):
 * capability/rules/data 固定序;缺失层以 unknown 呈现(不省略行——三层
 * 完整可见性优先,观测块早于三层复合的旧 server 也灰字呈现)。
 */
export function layerRows(data) {
  const lp = (data && data.layer_phases) || {};
  return ['capability', 'rules', 'data'].map((layer) => ({
    layer,
    phase: lp[layer] || 'unknown',
  }));
}

/**
 * 警量分账摘要行(纯函数):
 * [{label,value}];缺分账块(旧 server)返回空数组,popover 隐藏该段。
 * capability 独立账+Σ真域分账+全局总数,不变式 capability+Σdomains==total。
 */
export function attributionRows(data) {
  const att = data && data.alert_attribution;
  if (!att || typeof att !== 'object') return [];
  const domains = att.domains && typeof att.domains === 'object' ? att.domains : {};
  const domainKeys = Object.keys(domains).sort();
  const rows = [{ label: 'capability', value: att.capability ?? 0 }];
  for (const k of domainKeys) rows.push({ label: k, value: domains[k] ?? 0 });
  rows.push({ label: 'total', value: att.total ?? 0 });
  return rows;
}
