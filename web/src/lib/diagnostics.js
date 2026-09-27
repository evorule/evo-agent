// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 诊断单源模块:Monaco markerService 为唯一权威(波浪线/问题面板/计数三端同源,
// 零双写,对齐 VS Code MarkerService 哲学)。
//
// 职责:
//  - owner 命名空间管理:setDiagnostics(path, owner, markers) 按 owner 全量替换,
//    互不覆盖。owner 规范:'json'(Monaco json worker 原生)/ 'markdown.lint'(自写轻校验)
//    / '<lsp-server-name>'(LSP 接入后沿用同款契约)
//  - model 生命周期:onDidCreateModel 回放缓存中未落地的诊断;onWillDisposeModel
//    随 model 清缓存(marker 本身随 dispose 自动消亡)
//  - 聚合:订阅 onDidChangeMarkers → 按 path 分组 + severity 计数 → problems store
//    (问题面板树与顶栏计数 chip 共同消费)
//  - F8 导航:全工作区/文件内循环游标(稳定游标 ID,标记重算后位置保持)
//
// 本模块零 monaco 静态依赖:initDiagnostics(monaco) 由编辑器宿主注入,
// 纯逻辑(uri 规约/排序/游标)可独立单测。
//
// ── B5 消费契约(v1 冻结,08 号 LSP 板块按此接入,勿在未升版前改动形状) ──
//
// 1. 生产者入口(仅此一对,其余导出为工作台内部消费):
//      setDiagnostics(path, owner, markers)   全量替换(非增量;同 owner 幂等覆盖)
//      clearDiagnostics(path, owner?)         owner 省略 = 清该 path 全部 owner
// 2. path 口径:工作区相对路径(注册表权威键,禁绝对路径/URL);设置等虚拟页
//    用 'evo://settings/user' 形态。重命名经 rebindPath 重绑,生产者无需感知。
// 3. owner 命名空间:'json' / 'markdown.lint' / '<lsp-server-name>'(接入方自报,
//    全局唯一,等价于 VS Code DiagnosticCollection 名)。各 owner 独立全量替换,
//    清理互不波及。
// 4. markers 元素形状(Monaco IMarkerData 子集,冻结):
//      { message: string,
//        severity: 1|2|4|8,          // Hint/Info/Warning/Error(取 SEVERITY 常量)
//        startLineNumber: number,    // 1-based
//        startColumn: number,        // 1-based
//        endLineNumber: number,
//        endColumn: number,
//        source?: string,            // 展示名,如 'rust-analyzer'
//        code?: string|number }      // 面板展示与 hover 链接预留
//    未列出字段一概不进契约(会被透传但不保证展示语义)。
// 5. 生命周期语义:文件未打开时仅入前端缓存,model 创建时自动回放;model dispose
//    时缓存与 marker 一并消亡(重开=生产者需重推)。v1 面板只呈现已打开文件,
//    与「无全仓扫描」边界一致——LSP 服务端如持诊断缓存,推送仍需以本 API 为准。
// 6. 聚合为单向流:setDiagnostics → marker 服务 → onDidChangeMarkers → problems
//    store。生产者不直接写 problems store。

import { writable } from 'svelte/store';

/** Monaco marker severity 数值(MarkerSeverity.Error/Warning/Info/Hint) */
export const SEVERITY = { Hint: 1, Info: 2, Warning: 4, Error: 8 };

const EMPTY_PROBLEMS = { groups: [], counts: { errors: 0, warnings: 0, infos: 0, hints: 0 } };

/** 聚合快照:{groups:[{path, markers, counts}], counts:{errors,warnings,infos,hints}} */
export const problems = writable(EMPTY_PROBLEMS);

// ---- 内部状态(initDiagnostics 建立,dispose 复位) ----

let monacoRef = null;
/** path → model(仅含符合 Uri 规约的正式文件 model;草稿/错误占位/git-diff 不入册) */
const modelRegistry = new Map();
/** path → Map(owner → markers[]):model 未创建期间的诊断缓存(创建时回放) */
const cache = new Map();

// ---- Uri 规约(基建:model 按文件路径稳定关联的前提) ----

/** path → model Uri 字符串:设置虚拟路径原样透传,其余挂 evofile:/// 前缀 */
export function uriSpecFor(path) {
  if (!path) return 'evofile:///';
  if (path.startsWith('evo://')) return path;
  return 'evofile:///' + String(path).replace(/^\//, '');
}

/** monaco Uri → path(规约逆变换);非规约 scheme 返回 null(不入诊断体系) */
export function pathFromUri(uri) {
  if (!uri || typeof uri.scheme !== 'string') return null;
  try {
    if (uri.scheme === 'evofile') return decodeURIComponent(uri.path.replace(/^\//, ''));
    if (uri.scheme === 'evo') return decodeURIComponent(uri.toString());
  } catch {
    return null;
  }
  return null;
}

// ---- 纯逻辑:排序 / 游标(单测直接覆盖) ----

function cmpStr(a, b) {
  return a < b ? -1 : a > b ? 1 : 0;
}

/** 组内排序:severity 降序 → 行 → 列 → 消息(确定性,供面板树行序) */
export function sortMarkers(markers) {
  return [...markers].sort((a, b) => {
    if ((b.severity || 0) !== (a.severity || 0)) return (b.severity || 0) - (a.severity || 0);
    if ((a.startLineNumber || 0) !== (b.startLineNumber || 0))
      return (a.startLineNumber || 0) - (b.startLineNumber || 0);
    if ((a.startColumn || 0) !== (b.startColumn || 0)) return (a.startColumn || 0) - (b.startColumn || 0);
    return cmpStr(String(a.message || ''), String(b.message || ''));
  });
}

/** 全工作区排序:severity 降序 → path → 行 → 列(F8 循环顺序) */
export function sortMarkerPairs(pairs) {
  return [...pairs].sort((a, b) => {
    if ((b.marker.severity || 0) !== (a.marker.severity || 0))
      return (b.marker.severity || 0) - (a.marker.severity || 0);
    if (a.path !== b.path) return cmpStr(a.path, b.path);
    if ((a.marker.startLineNumber || 0) !== (b.marker.startLineNumber || 0))
      return (a.marker.startLineNumber || 0) - (b.marker.startLineNumber || 0);
    if ((a.marker.startColumn || 0) !== (b.marker.startColumn || 0))
      return (a.marker.startColumn || 0) - (b.marker.startColumn || 0);
    return 0;
  });
}

/** 稳定游标 ID:path|owner|range|code(标记重算后按 ID 重定位,位置保持) */
export function markerId(pair) {
  const m = pair.marker;
  const range = `${m.startLineNumber ?? 0},${m.startColumn ?? 0},${m.endLineNumber ?? 0},${m.endColumn ?? 0}`;
  return `${pair.path}|${m.owner || ''}|${range}|${m.code ?? ''}`;
}

/** 游标推进:循环;当前 ID 失效(标记被修)时重置到端点;空集返回 null */
export function advanceCursor(sortedIds, currentId, backward = false) {
  if (!sortedIds.length) return null;
  if (currentId == null) return backward ? sortedIds.length - 1 : 0;
  const idx = sortedIds.indexOf(currentId);
  if (idx === -1) return backward ? sortedIds.length - 1 : 0;
  return (idx + (backward ? -1 : 1) + sortedIds.length) % sortedIds.length;
}

/** 诊断条目 → openFile 定位参数(pendingReveal 约定:line 1-based,col/endCol 0-based) */
export function markerToReveal(pair) {
  const m = pair.marker;
  return {
    line: Math.max(1, m.startLineNumber || 1),
    col: Math.max(0, (m.startColumn || 1) - 1),
    endCol: Math.max(0, (m.endColumn || m.startColumn || 1) - 1),
  };
}

/** severity 计数(阈值语义:≥8 错误/≥4 警告/≥2 提示/其余归 Hint) */
export function severityCounts(markers) {
  const c = { errors: 0, warnings: 0, infos: 0, hints: 0 };
  for (const m of markers) {
    const s = m.severity || 0;
    if (s >= SEVERITY.Error) c.errors++;
    else if (s >= SEVERITY.Warning) c.warnings++;
    else if (s >= SEVERITY.Info) c.infos++;
    else c.hints++;
  }
  return c;
}

// ---- 单源核心 ----

function recompute() {
  if (!monacoRef) return;
  const groups = [];
  const totals = { errors: 0, warnings: 0, infos: 0, hints: 0 };
  for (const [path, model] of modelRegistry) {
    const markers = monacoRef.editor.getModelMarkers({ resource: model.uri });
    if (!markers.length) continue;
    const counts = severityCounts(markers);
    totals.errors += counts.errors;
    totals.warnings += counts.warnings;
    totals.infos += counts.infos;
    totals.hints += counts.hints;
    groups.push({ path, markers: sortMarkers(markers), counts });
  }
  groups.sort((a, b) => cmpStr(a.path, b.path));
  problems.set({ groups, counts: totals });
}

/** model 创建时回放缓存(该 path 各 owner 的未落地诊断) */
function replay(path, model) {
  const owners = cache.get(path);
  if (!owners || !monacoRef) return;
  for (const [owner, markers] of owners) {
    if (markers.length) monacoRef.editor.setModelMarkers(model, owner, markers);
  }
}

function trackModel(model) {
  const path = pathFromUri(model.uri);
  if (!path) return;
  modelRegistry.set(path, model);
  replay(path, model);
}

/**
 * 初始化(编辑器宿主 onMount 调用一次;重复调用幂等返回空清理)。
 * 返回清理函数:摘除订阅 + 清注册表/缓存 + 复位 store。
 */
export function initDiagnostics(monaco) {
  if (monacoRef) return () => {};
  monacoRef = monaco;
  const d1 = monaco.editor.onDidCreateModel((m) => trackModel(m));
  const d2 = monaco.editor.onWillDisposeModel((m) => {
    const path = pathFromUri(m.uri);
    if (!path) return;
    for (const [p, reg] of modelRegistry) if (reg === m) modelRegistry.delete(p);
    cache.delete(path); // marker 随 model dispose 自动消亡;缓存同步清(重开=重新校验)
    recompute(); // 不依赖 marker 服务在 dispose 路径上发变更事件,主动收敛一次
  });
  const d3 = monaco.editor.onDidChangeMarkers(() => recompute());
  for (const m of monaco.editor.getModels()) trackModel(m); // 启动时序兜底:已存在的 model 补注册
  return () => {
    d1.dispose();
    d2.dispose();
    d3.dispose();
    modelRegistry.clear();
    cache.clear();
    monacoRef = null;
    problems.set(EMPTY_PROBLEMS);
  };
}

/**
 * 设置某 path 某 owner 的诊断(owner 命名空间全量替换,同 owner 重复调用幂等)。
 * LSP/校验器的统一消费面:有 model 即时下发(聚合经 onDidChangeMarkers 事件回流),
 * 无 model 仅入缓存待回放(v1 面板只呈现已打开文件)。
 * markers 元素形状(Monaco IMarkerData 子集,冻结为接入契约):
 *   { message, severity, startLineNumber, startColumn, endLineNumber, endColumn, source?, code? }
 *   severity 为 1/2/4/8(Hint/Info/Warning/Error);行列均 1-based。
 */
export function setDiagnostics(path, owner, markers) {
  if (!path || !owner) return;
  const list = Array.isArray(markers) ? markers.map((m) => ({ ...m })) : [];
  let owners = cache.get(path);
  if (!owners) {
    owners = new Map();
    cache.set(path, owners);
  }
  owners.set(owner, list);
  const model = monacoRef ? modelRegistry.get(path) : null;
  if (model) monacoRef.editor.setModelMarkers(model, owner, list);
}

/** 清诊断:owner 省略时清该 path 全部 owner(含缓存与 model 上实际挂载者) */
export function clearDiagnostics(path, owner = null) {
  const owners = cache.get(path);
  const model = monacoRef ? modelRegistry.get(path) : null;
  const toClear = new Set();
  if (owner) {
    if (owners) {
      owners.delete(owner);
      if (owners.size === 0) cache.delete(path);
    }
    toClear.add(owner);
  } else {
    cache.delete(path);
    if (owners) for (const o of owners.keys()) toClear.add(o);
  }
  if (model && monacoRef) {
    if (!owner) {
      // 兜底:清掉 model 上实际挂载但缓存缺失的 owner(如 json worker 原生标记)
      for (const m of monacoRef.editor.getModelMarkers({ resource: model.uri })) toClear.add(m.owner);
    }
    for (const o of toClear) monacoRef.editor.setModelMarkers(model, o, []);
  }
}

/** 重命名/移动 tab 时重绑 path 注册键(model 原对象保留,undo 栈不破) */
export function rebindPath(from, to) {
  if (!from || !to || from === to) return;
  const model = modelRegistry.get(from);
  if (model) {
    modelRegistry.delete(from);
    modelRegistry.set(to, model);
  }
  const owners = cache.get(from);
  if (owners) {
    cache.delete(from);
    cache.set(to, owners);
  }
  recompute();
}

/** 全工作区诊断收集:[{marker, path}](path 取注册表键,为重命名后的权威路径) */
export function collectMarkers() {
  const out = [];
  if (!monacoRef) return out;
  for (const [path, model] of modelRegistry) {
    for (const m of monacoRef.editor.getModelMarkers({ resource: model.uri })) out.push({ marker: m, path });
  }
  return out;
}

// ---- F8 导航 ----

let lastCursorId = null;

/** 重置游标(标记全集清空时由调用方触发) */
export function resetProblemCursor() {
  lastCursorId = null;
}

/**
 * 下一个诊断条目:F8 全工作区循环 / Shift+F8 反向 / Alt+F8 文件内循环。
 * 返回 {marker, path} 或 null(无诊断)。游标按稳定 ID 重定位,标记重算后位置保持。
 */
export function nextProblem({ activePath = null, inFile = false, backward = false } = {}) {
  let pairs = collectMarkers();
  if (inFile && activePath) pairs = pairs.filter((p) => p.path === activePath);
  const sorted = sortMarkerPairs(pairs);
  if (!sorted.length) {
    lastCursorId = null;
    return null;
  }
  const ids = sorted.map(markerId);
  const idx = advanceCursor(ids, lastCursorId, backward);
  lastCursorId = ids[idx];
  return sorted[idx];
}
