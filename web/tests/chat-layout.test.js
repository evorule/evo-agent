// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 对话流回合布局测试:连续活动段归组/回答提取/审批回执入框/相邻同工具
// 同结局回执折叠/「审批结果:」兜底信息条入框/段级 steps 与 tookMs 合计。
import { describe, it, expect } from 'vitest';
import { buildTurnLayout, collapseReceiptRuns, RECEIPT_AGG_THRESHOLD } from '../src/lib/chat-layout.js';

const receipt = (id, toolName, decision = 'sent', sentApproved = true) => ({
  id,
  kind: 'approval_receipt',
  toolName,
  decision,
  sentApproved,
});

describe('buildTurnLayout 归组', () => {
  it('连续 tool/assistant 归入过程段,段末带正文 assistant 留框外作答案', () => {
    const groups = buildTurnLayout([
      { id: 1, kind: 'user', text: 'hi' },
      { id: 2, kind: 'tool', name: 'a', args: '{}' },
      { id: 3, kind: 'assistant', text: '中间陈述' },
      { id: 4, kind: 'tool', name: 'b', args: '{}', tookMs: 1200 },
      { id: 5, kind: 'assistant', text: '最终回答' },
    ]);
    const types = groups.map((g) => g.type);
    expect(types).toEqual(['single', 'process', 'single']);
    expect(groups[0].m.kind).toBe('user');
    expect(groups[1].items.map((m) => m.id)).toEqual([2, 3, 4]);
    // 末尾未闭合段 closed=false(进行中);组件层按轮次收尾改判闭合,库不掺轮态
    expect(groups[1].closed).toBe(false);
    expect(groups[2].m.id).toBe(5);
    // 段级 meta:步数与工具耗时合计;答案气泡 meta 同源
    expect(groups[1].steps).toBe(3);
    expect(groups[1].tookMs).toBe(1200);
    expect(groups[2].meta).toEqual({ steps: 3, tookMs: 1200 });
  });

  it('后续非框消息闭合过程段(closed=true)', () => {
    const groups = buildTurnLayout([
      { id: 1, kind: 'tool', name: 'a', args: '{}' },
      { id: 2, kind: 'assistant', text: '回答' },
      { id: 3, kind: 'user', text: '下一轮' },
    ]);
    expect(groups.map((g) => g.type)).toEqual(['process', 'single', 'single']);
    expect(groups[0].closed).toBe(true);
  });

  it('审批回执与「审批结果:」信息条入过程段;其他信息条独立成组', () => {
    const groups = buildTurnLayout([
      { id: 1, kind: 'tool', name: 'a', args: '{}' },
      receipt(2, 'a'),
      { id: 3, kind: 'info', text: '审批结果:已批准(工具 a)' },
      { id: 4, kind: 'info', text: '已完成上下文压缩(…)' },
    ]);
    expect(groups).toHaveLength(2);
    expect(groups[0].type).toBe('process');
    expect(groups[0].items.map((m) => m.id)).toEqual([1, 2, 3]);
    expect(groups[1].m.id).toBe(4);
  });

  it('待批卡/用户消息切断活动段', () => {
    const groups = buildTurnLayout([
      { id: 1, kind: 'tool', name: 'a', args: '{}' },
      { id: 2, kind: 'approval', toolName: 'a', proposalId: 'p1' },
      { id: 3, kind: 'tool', name: 'a', args: '{}' },
    ]);
    expect(groups.map((g) => g.type)).toEqual(['process', 'single', 'process']);
  });

  it('末尾未闭合段 closed=false(进行中恒展开)', () => {
    const groups = buildTurnLayout([{ id: 1, kind: 'tool', name: 'a', args: '{}' }]);
    expect(groups[0].closed).toBe(false);
  });
});

describe('相邻同工具同结局回执折叠', () => {
  it(`≥${RECEIPT_AGG_THRESHOLD} 条收敛为 receipt_agg,工具或结局不同即断开`, () => {
    const items = [
      receipt(1, 'python_exec'),
      receipt(2, 'python_exec'),
      { id: 3, kind: 'tool', name: 'python_exec', args: '{}' },
      receipt(4, 'python_exec'),
      receipt(5, 'python_exec'),
      receipt(6, 'python_exec', 'sent', false),
      receipt(7, 'python_exec', 'sent', false),
      receipt(8, 'python_exec', 'sent', false),
    ];
    const out = collapseReceiptRuns(items);
    // 低于阈值的 run 原样保留消息对象;达阈值的 run 收敛为 agg
    expect(out).toHaveLength(6); // [r1, r2, tool卡, r4, r5, agg(拒绝×3)]
    expect(out[0].id).toBe(1);
    expect(out[2].kind).toBe('tool');
    expect(out[5].kind).toBe('receipt_agg');
    expect(out[5].toolName).toBe('python_exec');
    expect(out[5].count).toBe(3);
    expect(out[5].outcome).toBe('deny');
    expect(out[5].items.map((r) => r.id)).toEqual([6, 7, 8]);
  });

  it('超时/失效同属 bad 一族可折叠;结局不同即断开', () => {
    const out = collapseReceiptRuns([
      receipt(1, 'a', 'timeout'),
      receipt(2, 'a', 'expired'),
      receipt(3, 'a', 'timeout'),
    ]);
    expect(out).toHaveLength(1);
    expect(out[0].outcome).toBe('bad');
    expect(out[0].count).toBe(3);
    // ok 回执夹在前面:不同结局断开 run
    const mixed = collapseReceiptRuns([receipt(4, 'a'), receipt(5, 'a', 'timeout')]);
    expect(mixed).toHaveLength(2);
  });

  it('buildTurnLayout 输出的过程段内折叠形态正确', () => {
    const groups = buildTurnLayout([
      { id: 1, kind: 'tool', name: 'python_exec', args: '{}' },
      receipt(2, 'python_exec'),
      receipt(3, 'python_exec'),
      receipt(4, 'python_exec'),
      receipt(5, 'python_exec'),
      { id: 6, kind: 'assistant', text: 'done' },
    ]);
    expect(groups).toHaveLength(2);
    const items = groups[0].items;
    expect(items).toHaveLength(2); // tool卡 + agg(4条)
    expect(items[1].kind).toBe('receipt_agg');
    expect(items[1].count).toBe(4);
    expect(groups[0].steps).toBe(5);
  });
});
