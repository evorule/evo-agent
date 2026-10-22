// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// audit 状态机单测(audit-one-click):
// 1. 徽章状态机全分支(hidden/red/green/gray×本地会话);
// 2. M1b 顺序对齐映射锚(assistant 终稿消息 ↔ IoResponse entries);
// 3. 拉取生命周期(成功/失败静默降级/竞态丢弃/非数字 sid 零请求)。
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';

vi.mock('../src/lib/api.js', () => ({
  getAuditReport: vi.fn(async () => ({ entries: [] })),
  getAuditVerify: vi.fn(async () => ({ verified: true })),
  getAuditCausalChain: vi.fn(async () => ({ chain: [] })),
}));

import {
  auditState,
  loadAudit,
  badgeStatus,
  mapAssistantToFacts,
  factForMemoryEvent,
  isAuditSupported,
  initAuditWatcher,
} from '../src/lib/audit.js';
import { getAuditReport } from '../src/lib/api.js';
import { writable } from 'svelte/store';

beforeEach(() => {
  auditState.set(null);
  vi.clearAllMocks();
});

// ---- 状态机 ----

describe('badgeStatus 全分支', () => {
  const ready = (over = {}) => ({
    status: 'ready',
    supported: true,
    verified: true,
    factCount: 3,
    lastHash: 'ab',
    ioEntries: [],
    allEntries: [],
    ...over,
  });

  it('null/加载中/失败 → hidden(静默降级)', () => {
    expect(badgeStatus(null, true)).toBe('hidden');
    expect(badgeStatus(ready({ status: 'loading' }), true)).toBe('hidden');
    expect(badgeStatus(ready({ status: 'failed', verified: null }), true)).toBe('hidden');
  });

  it('verified=false → red(链校验未通过,与映射无关)', () => {
    expect(badgeStatus(ready({ verified: false }), true)).toBe('red');
    expect(badgeStatus(ready({ verified: false }), false)).toBe('red');
  });

  it('verified 且映射到事实 → green', () => {
    expect(badgeStatus(ready(), true)).toBe('green');
  });

  it('verified 但无映射 → gray(本条无链上存证)', () => {
    expect(badgeStatus(ready(), false)).toBe('gray');
  });

  it('本地路径会话(supported=false) → gray,不受 verified 影响', () => {
    expect(badgeStatus(ready({ supported: false, verified: null }), false)).toBe('gray');
    expect(badgeStatus(ready({ supported: false, verified: null }), true)).toBe('gray');
  });
});

// ---- M1b 映射 ----

describe('mapAssistantToFacts(M1b 顺序对齐)', () => {
  const io = (fact_id) => ({ fact_id, fact_type: 'IoResponse', logical_time: fact_id });
  const state = (entries) => ({
    status: 'ready',
    supported: true,
    verified: true,
    ioEntries: entries,
    allEntries: entries,
  });

  it('assistant 终稿消息按序 ↔ IoResponse entries 按序', () => {
    const messages = [
      { id: 1, kind: 'user' },
      { id: 2, kind: 'assistant' }, // 终稿#1
      { id: 3, kind: 'tool' }, // 工具卡(不参与)
      { id: 4, kind: 'assistant' }, // 终稿#2
    ];
    const map = mapAssistantToFacts(messages, state([io(7), io(12)]));
    expect(map.get(2).fact_id).toBe(7);
    expect(map.get(4).fact_id).toBe(12);
    expect(map.size).toBe(2);
  });

  it('entries 多于消息 → 多余 entry 忽略;消息多于 entries → 溢出消息无映射', () => {
    const msgs = [
      { id: 1, kind: 'assistant' },
      { id: 2, kind: 'assistant' },
      { id: 3, kind: 'assistant' },
    ];
    const map1 = mapAssistantToFacts(msgs, state([io(7)]));
    expect(map1.get(1).fact_id).toBe(7);
    expect(map1.has(2)).toBe(false);
    const map2 = mapAssistantToFacts(msgs, state([io(7), io(9), io(11), io(13)]));
    expect(map2.get(3).fact_id).toBe(11);
  });

  it('state 为 null 或无 entries → 空 Map', () => {
    const msgs = [{ id: 1, kind: 'assistant' }];
    expect(mapAssistantToFacts(msgs, null).size).toBe(0);
    expect(mapAssistantToFacts(msgs, state([])).size).toBe(0);
  });
});

// ---- 记忆直查 ----

describe('factForMemoryEvent', () => {
  it('按 fact_id 命中全量条目(非 IoResponse 也可查)', () => {
    const st = {
      allEntries: [
        { fact_id: 30002, fact_type: 'PayloadUpdate' },
        { fact_id: 7, fact_type: 'IoResponse' },
      ],
    };
    expect(factForMemoryEvent(st, 30002).fact_type).toBe('PayloadUpdate');
    expect(factForMemoryEvent(st, 7).fact_type).toBe('IoResponse');
    expect(factForMemoryEvent(st, 999)).toBeNull();
    expect(factForMemoryEvent(st, null)).toBeNull();
    expect(factForMemoryEvent(null, 7)).toBeNull();
  });
});

// ---- 拉取生命周期 ----

describe('loadAudit 生命周期', () => {
  it('数字 sid → 拉取成功置 ready+verified+ioEntries 过滤', async () => {
    getAuditReport.mockResolvedValueOnce({
      session_id: 2,
      fact_count: 20,
      verified: true,
      last_hash: 'deadbeef',
      entries: [
        { fact_id: 1, fact_type: 'IoRequest' },
        { fact_id: 7, fact_type: 'IoResponse' },
      ],
    });
    await loadAudit('2');
    const s = get(auditState);
    expect(s.status).toBe('ready');
    expect(s.verified).toBe(true);
    expect(s.factCount).toBe(20);
    expect(s.ioEntries.map((e) => e.fact_id)).toEqual([7]);
    expect(s.allEntries).toHaveLength(2);
    expect(getAuditReport).toHaveBeenCalledWith('2', false);
  });

  it('拉取失败 → failed(徽章层静默,不抛错)', async () => {
    getAuditReport.mockRejectedValueOnce(new Error('HTTP 503'));
    await loadAudit('3');
    expect(get(auditState).status).toBe('failed');
  });

  it('非数字 sid(本地路径会话) → supported=false 且零请求', async () => {
    expect(isAuditSupported('local-abc')).toBe(false);
    await loadAudit('local-abc');
    const s = get(auditState);
    expect(s.status).toBe('ready');
    expect(s.supported).toBe(false);
    expect(getAuditReport).not.toHaveBeenCalled();
  });

  it('isAuditSupported:纯数字串 true', () => {
    expect(isAuditSupported('2')).toBe(true);
    expect(isAuditSupported('12345')).toBe(true);
    expect(isAuditSupported(null)).toBe(false);
    expect(isAuditSupported(undefined)).toBe(false);
    expect(isAuditSupported('')).toBe(false);
    expect(isAuditSupported('12x')).toBe(false);
  });
});

// ---- watcher 接线 ----

describe('initAuditWatcher', () => {
  it('会话切换触发拉取;轮次收尾(true→false)轻量刷新', async () => {
    const sid = writable(null);
    const turnActive = writable(false);
    const gov = [];
    initAuditWatcher(sid, turnActive, (e) => gov.push(e));
    await vi.waitFor(() => {}); // 订阅回调同步执行,无需等待

    sid.set('9'); // 会话创建 → 拉取
    expect(getAuditReport).toHaveBeenCalledTimes(1);
    turnActive.set(true);
    turnActive.set(false); // 轮次收尾 → 刷新
    expect(getAuditReport).toHaveBeenCalledTimes(2);
    turnActive.set(false); // 非收尾沿(连续 false)不刷
    expect(getAuditReport).toHaveBeenCalledTimes(2);

    getAuditReport.mockRejectedValueOnce(new Error('x'));
    sid.set('10');
    await vi.waitFor(() => expect(gov.some((e) => e.level === 'warn')).toBe(true));
  });
});
