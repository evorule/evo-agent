// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { get } from 'svelte/store';
import {
  problems,
  initDiagnostics,
  setDiagnostics,
  clearDiagnostics,
  rebindPath,
  collectMarkers,
  nextProblem,
  resetProblemCursor,
  uriSpecFor,
  pathFromUri,
  sortMarkers,
  sortMarkerPairs,
  markerId,
  advanceCursor,
  markerToReveal,
  severityCounts,
  SEVERITY,
} from '../src/lib/diagnostics.js';

// ---- fake monaco(仅模拟诊断面;零静态 monaco 依赖使本文件可在 node 环境跑) ----

function makeFakeMonaco() {
  const models = new Set();
  const markersByModel = new Map(); // model → marker[]
  const subs = { create: [], dispose: [], change: [] };
  const sub = (arr) => (cb) => {
    arr.push(cb);
    return { dispose: () => arr.splice(arr.indexOf(cb), 1) };
  };
  const editor = {
    onDidCreateModel: sub(subs.create),
    onWillDisposeModel: sub(subs.dispose),
    onDidChangeMarkers: sub(subs.change),
    getModels: () => [...models],
    getModelMarkers: ({ resource } = {}) => {
      for (const [m, list] of markersByModel) if (m.uri === resource) return list;
      return [];
    },
    setModelMarkers: (model, owner, list) => {
      const rest = (markersByModel.get(model) || []).filter((x) => x.owner !== owner);
      const next = rest.concat(list.map((x) => ({ ...x, owner, resource: model.uri })));
      markersByModel.set(model, next);
      subs.change.forEach((f) => f([model.uri]));
    },
    // 测试辅助:model 创建(触发 onDidCreateModel)/销毁(触发 onWillDisposeModel)
    createTestModel: (spec) => {
      const model = { uri: { scheme: spec.scheme || 'evofile', path: spec.path || '/', toString: spec.toString } };
      models.add(model);
      subs.create.forEach((f) => f(model));
      return model;
    },
    disposeTestModel: (model) => {
      models.delete(model);
      markersByModel.delete(model);
      subs.dispose.forEach((f) => f(model));
    },
  };
  return { editor, markersByModel };
}

function mk(over = {}) {
  return {
    message: over.message || '问题',
    severity: over.severity ?? SEVERITY.Warning,
    startLineNumber: over.line ?? 1,
    startColumn: over.col ?? 1,
    endLineNumber: over.line ?? 1,
    endColumn: over.endCol ?? (over.col ?? 1) + 1,
    source: over.source,
    code: over.code,
    owner: over.owner,
  };
}

let fake;
let disposeDiag = null;

beforeEach(() => {
  fake = makeFakeMonaco();
});
afterEach(() => {
  if (disposeDiag) disposeDiag();
  disposeDiag = null;
});

// ---- 纯逻辑 ----

describe('Uri 规约', () => {
  it('相对路径挂 evofile:/// 前缀;设置虚拟路径透传', () => {
    expect(uriSpecFor('notes/todo.md')).toBe('evofile:///notes/todo.md');
    expect(uriSpecFor('D:/proj/a.json')).toBe('evofile:///D:/proj/a.json');
    expect(uriSpecFor('evo://settings/user')).toBe('evo://settings/user');
  });

  it('pathFromUri 规约逆变换 + 编码还原', () => {
    expect(pathFromUri({ scheme: 'evofile', path: '/notes/todo.md' })).toBe('notes/todo.md');
    expect(pathFromUri({ scheme: 'evofile', path: '/D:/proj/a.json' })).toBe('D:/proj/a.json');
    expect(pathFromUri({ scheme: 'evofile', path: '/a%20b.md' })).toBe('a b.md');
    expect(
      pathFromUri({ scheme: 'evo', path: '/user', toString: () => 'evo://settings/user' }),
    ).toBe('evo://settings/user');
    expect(pathFromUri({ scheme: 'inmemory', path: '/x' })).toBeNull();
    expect(pathFromUri(null)).toBeNull();
  });
});

describe('排序与游标', () => {
  it('sortMarkers:severity 降序 → 行 → 列 → 消息', () => {
    const out = sortMarkers([
      mk({ severity: SEVERITY.Hint, line: 9 }),
      mk({ severity: SEVERITY.Error, line: 5, col: 2 }),
      mk({ severity: SEVERITY.Error, line: 5, col: 1 }),
      mk({ severity: SEVERITY.Warning, line: 1 }),
    ]);
    expect(out.map((m) => m.severity)).toEqual([8, 8, 4, 1]);
    expect(out[0].startColumn).toBe(1);
    expect(out[2].startLineNumber).toBe(1);
  });

  it('sortMarkerPairs:path 为第二排序键(severity 降序在先)', () => {
    const out = sortMarkerPairs([
      { marker: mk({ severity: SEVERITY.Error }), path: 'b.md' },
      { marker: mk({ severity: SEVERITY.Error }), path: 'a.md' },
      { marker: mk({ severity: SEVERITY.Hint }), path: 'a.md' },
    ]);
    expect(out.map((p) => p.path)).toEqual(['a.md', 'b.md', 'a.md']);
    expect(out[0].marker.severity).toBe(SEVERITY.Error);
  });

  it('markerId:path|owner|range|code 稳定可区分', () => {
    const a = markerId({ marker: mk({ line: 3, col: 2, code: 'E1', owner: 'x' }), path: 'a.md' });
    const b = markerId({ marker: mk({ line: 3, col: 2, code: 'E2', owner: 'x' }), path: 'a.md' });
    const c = markerId({ marker: mk({ line: 3, col: 2, code: 'E1', owner: 'y' }), path: 'a.md' });
    expect(a).not.toBe(b);
    expect(a).not.toBe(c);
    expect(markerId({ marker: mk({ line: 3, col: 2, code: 'E1', owner: 'x' }), path: 'a.md' })).toBe(a);
  });

  it('advanceCursor:空集/起点/循环/反向/失效重置', () => {
    const ids = ['i1', 'i2', 'i3'];
    expect(advanceCursor([], 'i1')).toBeNull();
    expect(advanceCursor(ids, null)).toBe(0);
    expect(advanceCursor(ids, null, true)).toBe(2);
    expect(advanceCursor(ids, 'i1')).toBe(1);
    expect(advanceCursor(ids, 'i3')).toBe(0); // 正向循环
    expect(advanceCursor(ids, 'i1', true)).toBe(2); // 反向循环
    expect(advanceCursor(ids, 'gone')).toBe(0); // 失效重置到端点
    expect(advanceCursor(ids, 'gone', true)).toBe(2);
  });

  it('markerToReveal:marker 1-based → pendingReveal 0-based col', () => {
    expect(markerToReveal({ marker: mk({ line: 5, col: 3, endCol: 7 }), path: 'a.md' })).toEqual({
      line: 5,
      col: 2,
      endCol: 6,
    });
  });

  it('severityCounts:按阈值四级归档(≥8 错误/≥4 警告/≥2 提示/其余 Hint)', () => {
    expect(
      severityCounts([
        mk({ severity: SEVERITY.Error }),
        mk({ severity: SEVERITY.Error }),
        mk({ severity: SEVERITY.Warning }),
        mk({ severity: SEVERITY.Info }),
        mk({ severity: SEVERITY.Hint }),
        mk({ severity: 0 }),
      ]),
    ).toEqual({ errors: 2, warnings: 1, infos: 1, hints: 2 });
  });
});

// ---- 单源生命周期(fake monaco) ----

describe('initDiagnostics / setDiagnostics', () => {
  it('初始化幂等;重复调用不重复订阅', () => {
    disposeDiag = initDiagnostics(fake);
    const before = fake.editor.getModels().length;
    expect(initDiagnostics(fake)).toBeInstanceOf(Function);
    expect(fake.editor.getModels().length).toBe(before);
  });

  it('owner 隔离全量替换:各 owner 独立;同 owner 幂等覆盖', () => {
    disposeDiag = initDiagnostics(fake);
    const model = fake.editor.createTestModel({ path: '/a.md' });
    setDiagnostics('a.md', 'o1', [mk({ line: 1 }), mk({ line: 2 })]);
    setDiagnostics('a.md', 'o2', [mk({ line: 3 })]);
    expect(fake.editor.getModelMarkers({ resource: model.uri })).toHaveLength(3);
    // 同 owner 全量替换(非追加)
    setDiagnostics('a.md', 'o1', [mk({ line: 9 })]);
    const all = fake.editor.getModelMarkers({ resource: model.uri });
    expect(all).toHaveLength(2);
    expect(all.filter((m) => m.owner === 'o1').map((m) => m.startLineNumber)).toEqual([9]);
  });

  it('聚合进 problems store:分组/组内排序/计数', () => {
    disposeDiag = initDiagnostics(fake);
    fake.editor.createTestModel({ path: '/b.md' });
    fake.editor.createTestModel({ path: '/a.json' });
    setDiagnostics('b.md', 'markdown.lint', [
      mk({ severity: SEVERITY.Hint, line: 2 }),
      mk({ severity: SEVERITY.Error, line: 1 }),
    ]);
    setDiagnostics('a.json', 'json', [mk({ severity: SEVERITY.Warning, line: 4 })]);
    const snap = get(problems);
    expect(snap.groups.map((g) => g.path)).toEqual(['a.json', 'b.md']);
    expect(snap.groups[1].markers.map((m) => m.severity)).toEqual([8, 1]);
    expect(snap.counts).toEqual({ errors: 1, warnings: 1, infos: 0, hints: 1 });
  });

  it('model 未建时入缓存,创建时回放(B5 消费契约)', () => {
    disposeDiag = initDiagnostics(fake);
    setDiagnostics('later.md', 'lsp', [mk({ line: 7 })]);
    expect(get(problems).groups).toHaveLength(0); // 未打开不进面板
    const model = fake.editor.createTestModel({ path: '/later.md' });
    const all = fake.editor.getModelMarkers({ resource: model.uri });
    expect(all).toHaveLength(1);
    expect(all[0].owner).toBe('lsp');
    expect(get(problems).groups.map((g) => g.path)).toEqual(['later.md']);
  });

  it('model dispose:marker 消亡 + 缓存清理(重开不回放旧诊断)', () => {
    disposeDiag = initDiagnostics(fake);
    const model = fake.editor.createTestModel({ path: '/a.md' });
    setDiagnostics('a.md', 'o1', [mk({ line: 1 })]);
    expect(get(problems).groups).toHaveLength(1);
    fake.editor.disposeTestModel(model);
    expect(get(problems).groups).toHaveLength(0);
    const model2 = fake.editor.createTestModel({ path: '/a.md' });
    expect(fake.editor.getModelMarkers({ resource: model2.uri })).toHaveLength(0);
  });

  it('clearDiagnostics:单 owner 清不影响他者;全清含缓存外的实际挂载者', () => {
    disposeDiag = initDiagnostics(fake);
    const model = fake.editor.createTestModel({ path: '/a.md' });
    setDiagnostics('a.md', 'o1', [mk({ line: 1 })]);
    setDiagnostics('a.md', 'o2', [mk({ line: 2 })]);
    clearDiagnostics('a.md', 'o2');
    let all = fake.editor.getModelMarkers({ resource: model.uri });
    expect(all.map((m) => m.owner)).toEqual(['o1']);
    // json worker 类「缓存外挂载」由全清兜底摘除
    fake.editor.setModelMarkers(model, 'json', [mk({ line: 3 })]);
    clearDiagnostics('a.md');
    all = fake.editor.getModelMarkers({ resource: model.uri });
    expect(all).toHaveLength(0);
    expect(get(problems).groups).toHaveLength(0);
  });

  it('rebindPath:重命名后注册表/缓存重绑,聚合以新 path 呈现', () => {
    disposeDiag = initDiagnostics(fake);
    const model = fake.editor.createTestModel({ path: '/old.md' });
    setDiagnostics('old.md', 'o1', [mk({ line: 1 })]);
    rebindPath('old.md', 'new.md');
    expect(get(problems).groups.map((g) => g.path)).toEqual(['new.md']);
    expect(collectMarkers().map((p) => p.path)).toEqual(['new.md']);
    // 缓存同步重绑:dispose 后重开按新 path 回放
    fake.editor.disposeTestModel(model);
    const model2 = fake.editor.createTestModel({ path: '/new.md' });
    expect(fake.editor.getModelMarkers({ resource: model2.uri })).toHaveLength(1);
  });

  it('dispose 清理:订阅摘除 + store 复位', () => {
    const d = initDiagnostics(fake);
    fake.editor.createTestModel({ path: '/a.md' });
    setDiagnostics('a.md', 'o1', [mk()]);
    expect(get(problems).groups).toHaveLength(1);
    d();
    expect(get(problems)).toEqual({ groups: [], counts: { errors: 0, warnings: 0, infos: 0, hints: 0 } });
    setDiagnostics('a.md', 'o1', [mk()]); // 摘除后仅入缓存,不再下发
    expect(get(problems).groups).toHaveLength(0);
    clearDiagnostics('a.md'); // 清缓存收尾,不留残留到后续用例
  });
});

describe('F8 导航', () => {
  it('全工作区循环(排序:severity→path→行);反向循环;文件内过滤', () => {
    disposeDiag = initDiagnostics(fake);
    fake.editor.createTestModel({ path: '/b.md' });
    fake.editor.createTestModel({ path: '/a.md' });
    setDiagnostics('b.md', 'o', [mk({ severity: SEVERITY.Error, line: 1, code: 'eb1' })]);
    setDiagnostics('a.md', 'o', [
      mk({ severity: SEVERITY.Warning, line: 5, code: 'wa1' }),
      mk({ severity: SEVERITY.Error, line: 2, code: 'ea1' }),
    ]);
    const seq = [];
    resetProblemCursor();
    for (let i = 0; i < 4; i++) {
      const hit = nextProblem();
      seq.push(markerId(hit));
    }
    // 顺序:a.md error → b.md error → a.md warning → 回到 a.md error(循环)
    expect(seq[0]).toBe(markerId({ marker: mk({ line: 2, owner: 'o', code: 'ea1' }), path: 'a.md' }));
    expect(seq[1]).toBe(markerId({ marker: mk({ line: 1, owner: 'o', code: 'eb1' }), path: 'b.md' }));
    expect(seq[2]).toBe(markerId({ marker: mk({ line: 5, owner: 'o', code: 'wa1' }), path: 'a.md' }));
    expect(seq[3]).toBe(seq[0]);

    // 反向:从游标处往前
    const back = nextProblem({ backward: true });
    expect(markerId(back)).toBe(seq[2]);

    // 文件内循环:仅 a.md
    resetProblemCursor();
    const inFileSeq = [];
    for (let i = 0; i < 3; i++) {
      const hit = nextProblem({ activePath: 'a.md', inFile: true });
      inFileSeq.push(markerId(hit));
    }
    expect(inFileSeq[0]).toBe(seq[0]);
    expect(inFileSeq[1]).toBe(seq[2]);
    expect(inFileSeq[2]).toBe(seq[0]);
  });

  it('无诊断返回 null;游标随标记失效自动重定位', () => {
    disposeDiag = initDiagnostics(fake);
    expect(nextProblem()).toBeNull();
    fake.editor.createTestModel({ path: '/a.md' });
    setDiagnostics('a.md', 'o', [mk({ line: 1 })]);
    const first = markerId(nextProblem());
    setDiagnostics('a.md', 'o', [mk({ line: 9 })]); // 旧标记消失,游标失效
    const hit = nextProblem();
    expect(hit).not.toBeNull(); // 失效重置后仍可导航
    expect(hit.marker.startLineNumber).toBe(9);
  });
});
