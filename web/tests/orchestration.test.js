// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// B25 编排可视化纯逻辑单测:delegate 实时增量/收色、REST 权威合并、
// 会话列表谱系归属排序、树布局几何。
import { describe, it, expect } from 'vitest';
import {
  applyDelegateCall,
  applyDelegateResult,
  mergeTree,
  orderSessionsWithLineage,
  layoutTree,
  edgePath,
} from '../src/lib/orchestration.js';

function restNode(sid, agentType, depth, status = 'done') {
  return {
    session_id: sid,
    agent_type: agentType,
    parent: 'root',
    kind: 'delegate',
    depth,
    spawned_at: 1000,
    status,
  };
}

describe('applyDelegateCall', () => {
  it('追加 running 临时节点,深度 = 父深度 + 1', () => {
    const nodes = [{ session_id: 'root', agent_type: null, parent: null, kind: null, depth: 0, status: 'unknown' }];
    const out = applyDelegateCall(nodes, 'root', { agent_type: 'researcher', task: 't' }, 7);
    expect(out).toHaveLength(2);
    const tmp = out[1];
    expect(tmp.session_id).toBe('tmp:root:7');
    expect(tmp.agent_type).toBe('researcher');
    expect(tmp.status).toBe('running');
    expect(tmp.temp).toBe(true);
    expect(tmp.depth).toBe(1);
    expect(tmp.parent).toBe('root');
  });

  it('父不在图内时深度回落 1', () => {
    const out = applyDelegateCall([], 'root', { agent_type: 'planner' }, 1);
    expect(out[0].depth).toBe(1);
  });

  it('args 缺 agent_type 回落 unknown', () => {
    const out = applyDelegateCall([], 'root', null, 2);
    expect(out[0].agent_type).toBe('unknown');
  });
});

describe('applyDelegateResult', () => {
  const running = () => applyDelegateCall([], 'root', { agent_type: 'researcher' }, 1);

  it('同 agent_type 收色 done,临时标记清除', () => {
    const out = applyDelegateResult(running(), 'researcher', true);
    expect(out[0].status).toBe('done');
    expect(out[0].temp).toBe(false);
  });

  it('ok=false 收色 failed', () => {
    const out = applyDelegateResult(running(), 'researcher', false);
    expect(out[0].status).toBe('failed');
  });

  it('错误结果无 agent_type 时匹配最近在跑临时节点', () => {
    let nodes = applyDelegateCall([], 'root', { agent_type: 'researcher' }, 1);
    nodes = applyDelegateCall(nodes, 'root', { agent_type: 'planner' }, 2);
    const out = applyDelegateResult(nodes, null, false);
    expect(out[1].status).toBe('failed');
    expect(out[0].status).toBe('running');
  });

  it('LIFO:同型多个在跑时收最后一个', () => {
    let nodes = applyDelegateCall([], 'root', { agent_type: 'researcher' }, 1);
    nodes = applyDelegateCall(nodes, 'root', { agent_type: 'researcher' }, 2);
    const out = applyDelegateResult(nodes, 'researcher', true);
    expect(out[0].status).toBe('running');
    expect(out[1].status).toBe('done');
  });

  it('无在跑临时节点时原样返回', () => {
    const nodes = [restNode('c1', 'researcher', 1)];
    expect(applyDelegateResult(nodes, 'researcher', true)).toBe(nodes);
  });
});

describe('mergeTree', () => {
  it('真身替换全量,已收色临时节点被真身取代', () => {
    const prev = [
      restNode('c1', 'researcher', 1),
      { session_id: 'tmp:root:9', agent_type: 'planner', parent: 'root', kind: 'delegate', depth: 1, spawned_at: null, status: 'done', temp: false },
    ];
    const tree = { session_id: 'root', nodes: [restNode('c1', 'researcher', 1), restNode('c2', 'planner', 1)], edges: [] };
    const out = mergeTree(prev, tree);
    expect(out.nodes).toHaveLength(2);
    expect(out.nodes.map((n) => n.session_id)).toEqual(['c1', 'c2']);
  });

  it('仍在跑的临时节点保留(其 journal 锚未落,REST 尚无真身)', () => {
    const prev = [
      restNode('c1', 'researcher', 1),
      { session_id: 'tmp:root:9', agent_type: 'planner', parent: 'root', kind: 'delegate', depth: 1, spawned_at: null, status: 'running', temp: true },
    ];
    const tree = { session_id: 'root', nodes: [restNode('c1', 'researcher', 1)], edges: [] };
    const out = mergeTree(prev, tree);
    expect(out.nodes).toHaveLength(2);
    expect(out.nodes[1].temp).toBe(true);
    expect(out.nodes[1].status).toBe('running');
  });
});

describe('orderSessionsWithLineage', () => {
  const sessions = [
    { session_id: 'p1', title: '父' },
    { session_id: 'q1', title: '无关' },
    { session_id: 'c1', title: '子' },
  ];
  const links = [{ parent: 'p1', child: 'c1', agent_type: 'researcher', kind: 'delegate', depth: 1 }];

  it('子会话紧跟其父并携带 lineage 徽标文案', () => {
    const out = orderSessionsWithLineage(sessions, links);
    expect(out.map((s) => s.session_id)).toEqual(['p1', 'c1', 'q1']);
    expect(out[1].lineage).toEqual({ agent_type: 'researcher', kind: 'delegate', display: '委托自 researcher' });
    expect(out[0].lineage).toBeNull();
    expect(out[2].lineage).toBeNull();
  });

  it('spawn 锚不带 agent_type → 「派生会话」文案', () => {
    const out = orderSessionsWithLineage(sessions, [
      { parent: 'p1', child: 'c1', agent_type: '', kind: 'spawn', depth: 1 },
    ]);
    expect(out[1].lineage.display).toBe('派生会话');
  });

  it('父不在列表中:子保持原序且仍带徽标', () => {
    const out = orderSessionsWithLineage(sessions, [
      { parent: 'ghost', child: 'q1', agent_type: 'planner', kind: 'delegate', depth: 1 },
    ]);
    expect(out.map((s) => s.session_id)).toEqual(['p1', 'q1', 'c1']);
    expect(out[1].lineage.display).toBe('委托自 planner');
  });

  it('links 为空时原样平铺且 lineage 全 null', () => {
    const out = orderSessionsWithLineage(sessions, []);
    expect(out.map((s) => s.session_id)).toEqual(['p1', 'q1', 'c1']);
    expect(out.every((s) => s.lineage === null)).toBe(true);
  });
});

describe('layoutTree / edgePath', () => {
  it('列 = 深度,列内节点纵向均布不重叠', () => {
    const nodes = [
      { session_id: 'root', depth: 0 },
      restNode('c1', 'a', 1),
      restNode('c2', 'b', 1),
      restNode('g1', 'c', 2),
    ];
    const lay = layoutTree(nodes, { rowH: 58, nodeH: 44, colGap: 56, nodeW: 148, padX: 12, padY: 12 });
    expect(lay.placed).toHaveLength(4);
    const byId = new Map(lay.placed.map((p) => [p.node.session_id, p]));
    // 深度列 x 严格递增
    expect(byId.get('root').x).toBeLessThan(byId.get('c1').x);
    expect(byId.get('c1').x).toBeLessThan(byId.get('g1').x);
    // 同列两节点 y 不同且间距 = rowH
    const y1 = byId.get('c1').y;
    const y2 = byId.get('c2').y;
    expect(Math.abs(y2 - y1)).toBeCloseTo(58, 5);
    // 画布容纳所有节点
    expect(lay.width).toBeGreaterThan(148 * 3);
    expect(lay.height).toBeGreaterThan(58 * 2);
  });

  it('edgePath 从父右缘到子左缘', () => {
    const p = { x: 0, y: 0 };
    const c = { x: 200, y: 60 };
    const d = edgePath(p, c, 148, 44);
    expect(d.startsWith('M 148 22')).toBe(true);
    expect(d.includes('200 82')).toBe(true);
  });
});
