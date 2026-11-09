// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
/** 流水线链级视图派生面(制作看板数据层,纯函数零副作用)。
 *
 *  events/1.0 契约:链级事件 phase='pipeline',携带 run_id(链内单值)、
 *  stage(script|build|qc|gate|broadcast|chain_end)、ok、message、
 *  byte_offset。看板三柱(设计档):链级聚合/STALL 双窗显态/
 *  REPLAY 磁盘投影——全部从已落盘事件推导,UI 不直读引擎内部状态;
 *  坏输入一律降级渲染不抛错(never-block)。 */

/** 链内段位的固定呈现序(乱序到达也归位) */
export const STAGE_ORDER = ['script', 'build', 'qc', 'gate', 'broadcast'];

/** 双窗判定窗口(毫秒,Backlot 同值:LIVE 5min/STALL 10min) */
export const QUIET_MS = 5 * 60 * 1000;
export const STALL_MS = 10 * 60 * 1000;

/** 事件列表 → 有序链数组。无 run_id 的旧式事件被忽略(容错不抛错);
 *  链序=首见顺序;段位状态取链内该 stage 最新一条。 */
export function aggregateChains(events) {
  const chains = [];
  const byRun = new Map();
  for (const e of events || []) {
    if (!e || typeof e !== 'object') continue;
    const run = e.run_id;
    if (typeof run !== 'string' || !run.trim()) continue;
    if (e.phase !== 'pipeline') continue;
    let c = byRun.get(run);
    if (!c) {
      c = { run_id: run, stages: {}, lastEvent: e, ended: false, ok: false };
      byRun.set(run, c);
      chains.push(c);
    }
    const stage = typeof e.stage === 'string' ? e.stage : 'unknown';
    c.stages[stage] = { ok: e.ok === true, ts: e.ts, message: e.message };
    c.lastEvent = e;
    if (stage === 'chain_end') {
      c.ended = true;
      c.ok = e.ok === true;
    }
  }
  return chains;
}

/** 单链 → 展示态(固定段序+状态判定)。nowMs 可注入(测试确定性)。
 *  ended → ok|failed 终态;未收链按最新事件龄:LIVE/QUIET/STALL 双窗。 */
export function chainStatus(chain, nowMs = Date.now()) {
  if (!chain || typeof chain !== 'object') return null;
  const stages = STAGE_ORDER.map((s) => ({
    stage: s,
    ...(chain.stages[s] || { ok: false, ts: null, message: '' }),
  }));
  if (chain.ended) {
    return { stages, state: chain.ok ? 'ok' : 'failed', ended: true };
  }
  const ts = chain.lastEvent && typeof chain.lastEvent.ts === 'number'
    ? chain.lastEvent.ts
    : null;
  const age = ts != null ? Math.max(0, nowMs - ts * 1000) : 0;
  const state = age > STALL_MS ? 'STALL' : age > QUIET_MS ? 'QUIET' : 'LIVE';
  return { stages, state, ended: false };
}

/** manifest.json(构建产物指纹)→ 镜头回放时间轴。缺 shots/字段坏的段
 *  降级为 null 段(never-block:半写/未知形态不崩溃,其余段照出);
 *  无 shots 键 → null(调用方显态「manifest 不可用」)。 */
export function replayFromManifest(manifest) {
  if (!manifest || typeof manifest !== 'object') return null;
  const shots = manifest.shots;
  if (!Array.isArray(shots)) return null;
  return shots.map((s) => {
    if (!s || typeof s !== 'object') return null;
    const id = typeof s.id === 'string' ? s.id : null;
    const dur = typeof s.duration === 'number' && s.duration >= 0 ? s.duration : null;
    if (id == null && dur == null) return null;
    return { shot_id: id, duration: dur, path: typeof s.path === 'string' ? s.path : null };
  });
}

/** (ndjson 工作区路径, 链) → 该项目 manifest 的工作区路径(磁盘投影拉取锚)。
 *  归一规则同 progress-assets.js:ndjson 所在目录 + build/manifest.json。 */
export function manifestPathFor(ndjsonPath) {
  if (!ndjsonPath) return null;
  const nd = String(ndjsonPath).replace(/\\/g, '/');
  const dir = nd.slice(0, nd.lastIndexOf('/') + 1);
  if (!dir) return null;
  const base = dir.endsWith('build/') ? dir.slice(0, -'build/'.length) : dir;
  return `${base}build/manifest.json`;
}
