// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect } from 'vitest';
import {
  STAGE_ORDER,
  QUIET_MS,
  STALL_MS,
  aggregateChains,
  chainStatus,
  replayFromManifest,
  manifestPathFor,
} from '../src/lib/pipeline-view.js';

/** 制作看板纯函数判据(设计档 §三 J1~J4):
 *  链聚合容错/段序归位/双窗判定/manifest 磁盘投影降级。 */

const chainEvents = (run, overrides = []) => [
  { seq: 1, phase: 'pipeline', run_id: run, stage: 'build', ok: true, ts: 1000, message: 'build ok' },
  { seq: 2, phase: 'pipeline', run_id: run, stage: 'qc', ok: true, ts: 1010, message: 'qc ok' },
  ...overrides,
];

describe('aggregateChains (J1 链聚合容错)', () => {
  it('聚合同 run_id 事件为单链', () => {
    const chains = aggregateChains(chainEvents('r1'));
    expect(chains).toHaveLength(1);
    expect(chains[0].run_id).toBe('r1');
    expect(chains[0].stages.build.ok).toBe(true);
  });

  it('空数组/坏元素/旧式事件(无 run_id)全部忽略不抛错', () => {
    expect(aggregateChains([])).toEqual([]);
    expect(aggregateChains(null)).toEqual([]);
    expect(aggregateChains([null, 42, 'x', { phase: 'plan' }, { run_id: '' }])).toEqual([]);
    // 旧式 build 面事件(无 run_id 无 phase=pipeline)不进链板
    expect(aggregateChains([{ seq: 1, phase: 'plan', message: 'x' }])).toEqual([]);
  });

  it('链序=首见顺序;同段位多条取最新', () => {
    const chains = aggregateChains([
      { phase: 'pipeline', run_id: 'b', stage: 'build', ok: true, ts: 1 },
      { phase: 'pipeline', run_id: 'a', stage: 'build', ok: true, ts: 2 },
      { phase: 'pipeline', run_id: 'b', stage: 'qc', ok: false, ts: 3 },
      { phase: 'pipeline', run_id: 'b', stage: 'qc', ok: true, ts: 4, message: 'retry pass' },
    ]);
    expect(chains.map((c) => c.run_id)).toEqual(['b', 'a']);
    expect(chains[0].stages.qc).toMatchObject({ ok: true, message: 'retry pass' });
  });

  it('chain_end 收链:ended+ok 落定', () => {
    const chains = aggregateChains(
      chainEvents('r1', [{ phase: 'pipeline', run_id: 'r1', stage: 'chain_end', ok: true, ts: 1100 }])
    );
    expect(chains[0].ended).toBe(true);
    expect(chains[0].ok).toBe(true);
  });
});

describe('chainStatus (J2 段序 + J3 双窗)', () => {
  it('段位按固定序呈现,乱序到达也归位(J2)', () => {
    const chains = aggregateChains([
      { phase: 'pipeline', run_id: 'r', stage: 'broadcast', ok: true, ts: 1 },
      { phase: 'pipeline', run_id: 'r', stage: 'script', ok: true, ts: 2 },
    ]);
    const st = chainStatus(chains[0], 2000);
    expect(st.stages.map((s) => s.stage)).toEqual(STAGE_ORDER);
    expect(st.stages[0].stage).toBe('script');
    expect(st.stages[4].stage).toBe('broadcast');
  });

  it('未收链三态迁移:5min 内 LIVE / 5~10min QUIET / >10min STALL(J3)', () => {
    const mk = (ts) => aggregateChains([
      { phase: 'pipeline', run_id: 'r', stage: 'build', ok: true, ts },
    ])[0];
    const now = 10_000_000;
    expect(chainStatus(mk((now - 1000) / 1000), now).state).toBe('LIVE');
    expect(chainStatus(mk((now - QUIET_MS - 1000) / 1000), now).state).toBe('QUIET');
    expect(chainStatus(mk((now - STALL_MS - 1000) / 1000), now).state).toBe('STALL');
  });

  it('无时间戳的链不误报 STALL(容错,显 LIVE 待事件)', () => {
    const c = aggregateChains([
      { phase: 'pipeline', run_id: 'r', stage: 'build', ok: true },
    ])[0];
    expect(chainStatus(c, Date.now()).state).toBe('LIVE');
  });

  it('收链终态:ok 链/失败链(LIVE/QUIET/STALL 不再适用)', () => {
    const ok = aggregateChains(
      chainEvents('r', [{ phase: 'pipeline', run_id: 'r', stage: 'chain_end', ok: true, ts: 5 }])
    )[0];
    const bad = aggregateChains(
      chainEvents('r', [{ phase: 'pipeline', run_id: 'r', stage: 'chain_end', ok: false, ts: 5 }])
    )[0];
    expect(chainStatus(ok, 10 ** 12)).toMatchObject({ state: 'ok', ended: true });
    expect(chainStatus(bad, 10 ** 12)).toMatchObject({ state: 'failed', ended: true });
  });
});

describe('replayFromManifest (J4 磁盘投影降级)', () => {
  it('正常 manifest → 时间轴', () => {
    const tl = replayFromManifest({
      shots: [
        { id: 's1', duration: 3.5, path: 'shot_s1.mp4' },
        { id: 's2', duration: 2.0, path: 'shot_s2.mp4' },
      ],
    });
    expect(tl).toHaveLength(2);
    expect(tl[0]).toMatchObject({ shot_id: 's1', duration: 3.5, path: 'shot_s1.mp4' });
  });

  it('无 shots 键 → null(显态「manifest 不可用」)', () => {
    expect(replayFromManifest({})).toBeNull();
    expect(replayFromManifest(null)).toBeNull();
    expect(replayFromManifest('x')).toBeNull();
  });

  it('坏段降级 null,好段照出(never-block)', () => {
    const tl = replayFromManifest({
      shots: [null, { id: 's2', duration: -1 }, { id: 's3', duration: 2, path: 'p.mp4' }, 42],
    });
    expect(tl).toHaveLength(4);
    expect(tl[0]).toBeNull();
    expect(tl[1]).toMatchObject({ shot_id: 's2', duration: null }); // 负 duration 降级
    expect(tl[2]).toMatchObject({ shot_id: 's3', duration: 2 });
    expect(tl[3]).toBeNull();
  });
});

describe('manifestPathFor', () => {
  it('项目根布局 → build/manifest.json', () => {
    expect(manifestPathFor('video_engine/projects/p/build.progress.ndjson')).toBe(
      'video_engine/projects/p/build/manifest.json'
    );
  });
  it('build/ 内布局不再追加一层', () => {
    expect(manifestPathFor('video_engine/projects/p/build/build.progress.ndjson')).toBe(
      'video_engine/projects/p/build/manifest.json'
    );
  });
  it('空输入 → null', () => {
    expect(manifestPathFor('')).toBeNull();
    expect(manifestPathFor(null)).toBeNull();
  });
});
