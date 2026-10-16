// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// B25 编排可视化:委托谱系 store + 纯逻辑(实时增量 / REST 权威合并 / 树布局)
//
// 数据双通道:
// - 实时:ws.js 事件流里 delegate 工具的 ToolCall/ToolResult → 临时节点
//   (temp,`tmp:{sid}:{seq}`)即时入图与收色,零往返;
// - 权威:连接建立(SessionCreated)时拉 REST 树合并——journal 为权威谱系
//   (真实 child_session_id),已收色的临时节点被真身取代;仍在跑的临时节点
//   保留(其落账锚尚未写入 journal)。
//
// 本模块不 import ws.js(单向依赖:ws.js → 本模块 → api.js),防循环。

import { writable } from 'svelte/store';
import { getOrchestrationAgents, getOrchestrationTree } from './api.js';

/** 编排面板状态(当前会话谱系;换线即重建) */
export const orchestration = writable({
  sessionId: null,
  nodes: [],
  edges: [],
  agents: [],
  loading: false,
  error: null,
});

// ---------------------------------------------------------------------------
// 纯函数(vitest 覆盖面)
// ---------------------------------------------------------------------------

/**
 * WS ToolCall(delegate)→ 追加 running 临时节点。
 * 深度 = 父节点深度 + 1(父不在图内按 1,当前实现父即根)。
 */
export function applyDelegateCall(nodes, parentSid, args, seq) {
  const parent = nodes.find((n) => n.session_id === parentSid);
  const depth = parent ? parent.depth + 1 : 1;
  return [
    ...nodes,
    {
      session_id: `tmp:${parentSid}:${seq}`,
      agent_type: args?.agent_type || 'unknown',
      parent: parentSid,
      kind: 'delegate',
      depth,
      spawned_at: null,
      status: 'running',
      temp: true,
    },
  ];
}

/**
 * WS ToolResult(delegate)→ 按 agent_type + 栈顶(LIFO)匹配收色。
 * agentType 为空(错误结果不含 agent_type)时匹配最近一个在跑临时节点。
 */
export function applyDelegateResult(nodes, agentType, ok) {
  for (let i = nodes.length - 1; i >= 0; i--) {
    const n = nodes[i];
    if (n.temp && n.status === 'running' && (!agentType || n.agent_type === agentType)) {
      return nodes.map((m, j) =>
        j === i ? { ...m, status: ok ? 'done' : 'failed', temp: false } : m,
      );
    }
  }
  return nodes;
}

/**
 * REST 权威树合并:真身节点全量替换;prev 中仍在跑的临时节点保留追加
 * (其 journal 锚未落,REST 尚无真身;已收色临时节点由真身取代防重复)。
 */
export function mergeTree(prevNodes, tree) {
  const stillRunning = (prevNodes || []).filter((n) => n.temp && n.status === 'running');
  return { ...tree, nodes: [...(tree.nodes || []), ...stillRunning] };
}

/**
 * 会话列表谱系归属排序(PR-3):子会话紧跟其父后(深度优先,父在列者才
 * 归位;父不在列保持原序),输出项附带 lineage:{agent_type, kind, display}。
 * display 文案:delegate=「委托自 <agent_type>」;spawn 锚不带 agent_type
 * →「派生会话」。
 */
export function orderSessionsWithLineage(sessions, links) {
  const childMap = new Map();
  for (const l of links || []) childMap.set(l.child, l);
  const byParent = new Map();
  for (const s of sessions || []) {
    const l = childMap.get(s.session_id);
    if (l) {
      if (!byParent.has(l.parent)) byParent.set(l.parent, []);
      byParent.get(l.parent).push(s);
    }
  }
  const seen = new Set();
  const out = [];
  const pushWithChildren = (s) => {
    if (seen.has(s.session_id)) return;
    seen.add(s.session_id);
    const l = childMap.get(s.session_id) || null;
    out.push({
      ...s,
      lineage: l
        ? {
            agent_type: l.agent_type,
            kind: l.kind,
            display: l.kind === 'spawn' ? '派生会话' : `委托自 ${l.agent_type || '代理'}`,
          }
        : null,
    });
    for (const c of byParent.get(s.session_id) || []) pushWithChildren(c);
  };
  for (const s of sessions || []) pushWithChildren(s);
  return out;
}

/** 谱系树布局:列 = 深度,列内节点纵向均布;返回节点坐标与连线几何。 */
export function layoutTree(nodes, opts = {}) {
  const colGap = opts.colGap ?? 56;
  const nodeW = opts.nodeW ?? 148;
  const nodeH = opts.nodeH ?? 44;
  const rowH = opts.rowH ?? 58;
  const padX = opts.padX ?? 12;
  const padY = opts.padY ?? 12;
  const byDepth = new Map();
  for (const n of nodes) {
    const d = n.depth || 0;
    if (!byDepth.has(d)) byDepth.set(d, []);
    byDepth.get(d).push(n);
  }
  const depths = [...byDepth.keys()].sort((a, b) => a - b);
  const maxRows = Math.max(1, ...[...byDepth.values()].map((c) => c.length));
  const width = padX * 2 + depths.length * nodeW + Math.max(0, depths.length - 1) * colGap;
  const height = padY * 2 + maxRows * rowH;
  const pos = new Map();
  for (const d of depths) {
    const col = byDepth.get(d);
    const x = padX + d * (nodeW + colGap);
    col.forEach((n, i) => {
      const cy = height / 2 + (i - (col.length - 1) / 2) * rowH;
      pos.set(n.session_id, { x, y: cy - nodeH / 2, cx: x + nodeW / 2, cy });
    });
  }
  const placed = nodes
    .filter((n) => pos.has(n.session_id))
    .map((n) => ({ node: n, ...pos.get(n.session_id) }));
  return { width, height, nodeW, nodeH, placed };
}

/** 连线几何:父右缘中点 → 子左缘中点,水平贝塞尔(圆滑 L 弯的等价平滑)。 */
export function edgePath(p, c, nodeW, nodeH) {
  const x1 = p.x + nodeW;
  const y1 = p.y + nodeH / 2;
  const x2 = c.x;
  const y2 = c.y + nodeH / 2;
  const mx = (x1 + x2) / 2;
  return `M ${x1} ${y1} C ${mx} ${y1}, ${mx} ${y2}, ${x2} ${y2}`;
}

// ---------------------------------------------------------------------------
// store 动作(ws.js 消费;过期响应按 sessionId 丢弃防换线串台)
// ---------------------------------------------------------------------------

let callSeq = 0;

/** 会话建立/重连:重建状态并拉权威树(设计判据 J1/J2 的 REST 兜底) */
export async function refreshOrchestration(sid) {
  if (!sid) {
    orchestration.set({ sessionId: null, nodes: [], edges: [], agents: [], loading: false, error: null });
    return;
  }
  orchestration.update((s) => ({
    ...s,
    sessionId: sid,
    loading: s.sessionId !== sid,
    error: null,
  }));
  try {
    const tree = await getOrchestrationTree(sid);
    orchestration.update((s) => {
      if (s.sessionId !== sid) return s; // 换线竞态:过期响应丢弃
      const merged = mergeTree(s.nodes, tree);
      return { ...s, nodes: merged.nodes, edges: merged.edges, loading: false };
    });
  } catch (e) {
    // 树拉取失败不阻断实时通道(临时节点照常入图);错误只挂状态
    orchestration.update((s) =>
      s.sessionId === sid ? { ...s, loading: false, error: String(e?.message || e) } : s,
    );
  }
}

/** WS ToolCall(name=delegate):追加 running 临时节点 */
export function orchestrationToolCall(sid, args) {
  if (!sid) return;
  callSeq += 1;
  orchestration.update((s) =>
    s.sessionId === sid ? { ...s, nodes: applyDelegateCall(s.nodes, sid, args, callSeq) } : s,
  );
}

/** WS ToolResult(name=delegate):按 agent_type 栈顶收色(ok/error) */
export function orchestrationToolResult(sid, result) {
  if (!sid) return;
  const isObj = result && typeof result === 'object';
  const ok = isObj ? result.status === 'ok' : false;
  const agentType = isObj ? result.agent_type || null : null;
  orchestration.update((s) =>
    s.sessionId === sid ? { ...s, nodes: applyDelegateResult(s.nodes, agentType, ok) } : s,
  );
}

/** 可委托 agent 清单(图例;面板挂载时拉一次,失败静默) */
export async function loadOrchestrationAgents() {
  try {
    const res = await getOrchestrationAgents();
    orchestration.update((s) => ({ ...s, agents: res.agents || [] }));
  } catch {
    /* 图例是辅助面,失败静默 */
  }
}
