// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect } from 'vitest';
import {
  parseJson,
  isSceneContract,
  summarize,
  buildTree,
} from '../src/lib/json-structured.js';

const SCENE = {
  version: 'scene/1.0',
  fps: 30,
  duration: 8,
  camera: { pos: [0, 1, 6], target: [0, 0, 0] },
  objects: [
    { id: 'o1', mesh: 'box' },
    { id: 'o2', mesh: 'box' },
  ],
  timeline: [{ t: 0, shot: 'o1', op: 'scale' }],
};

describe('parseJson', () => {
  it('parses valid text and reports parse failure with message', () => {
    expect(parseJson('{"a":1}')).toEqual({ ok: true, value: { a: 1 } });
    const bad = parseJson('{oops');
    expect(bad.ok).toBe(false);
    expect(bad.error.length).toBeGreaterThan(0);
  });
});

describe('isSceneContract', () => {
  it('detects explicit scene/1.0 marker', () => {
    expect(isSceneContract(SCENE)).toBe(true);
    expect(isSceneContract({ v: 'scene/1.0' })).toBe(true);
  });

  it('detects objects+timeline composition without marker', () => {
    expect(isSceneContract({ objects: [], timeline: [] })).toBe(true);
  });

  it('rejects plain config objects', () => {
    expect(isSceneContract({ model: 'm3', temperature: 0.2 })).toBe(false);
    expect(isSceneContract(null)).toBe(false);
    expect(isSceneContract('str')).toBe(false);
  });
});

describe('summarize', () => {
  it('builds scene cards with fps/duration/camera/objects/timeline', () => {
    const s = summarize(SCENE);
    expect(s.kind).toBe('scene');
    const labels = s.cards.map((c) => c.label);
    expect(labels).toEqual(['契约', 'fps', 'duration', 'camera', 'objects', 'timeline']);
    expect(s.cards.find((c) => c.label === 'objects').value).toBe('2 项');
    expect(s.cards.find((c) => c.label === 'duration').value).toBe('8s');
  });

  it('builds generic cards for plain objects', () => {
    const s = summarize({ model: 'm3', temperature: 0.2, nested: { a: 1 }, arr: [1, 2] });
    expect(s.kind).toBe('generic');
    expect(s.cards[0]).toEqual({ label: '顶层键', value: '4 个' });
    expect(s.cards.find((c) => c.label === 'arr').value).toBe('数组(2)');
  });

  it('handles arrays and primitives as top-level values', () => {
    expect(summarize([1, 2, 3]).cards[0].value).toBe('数组(3)');
    expect(summarize(42).cards[0].value).toBe('number');
  });
});

describe('buildTree', () => {
  it('builds nested nodes with type previews', () => {
    const tree = buildTree(SCENE);
    expect(tree.type).toBe('object');
    expect(tree.children.map((c) => c.name)).toEqual([
      'version',
      'fps',
      'duration',
      'camera',
      'objects',
      'timeline',
    ]);
    const objects = tree.children.find((c) => c.name === 'objects');
    expect(objects.preview).toBe('array(2)');
    expect(objects.children).toHaveLength(2);
  });

  it('leaf nodes carry quoted string previews and scalar values', () => {
    const tree = buildTree({ s: 'hello world this is long string', n: 42, z: null });
    expect(tree.children.find((c) => c.name === 's').preview).toBe(
      '"hello world this is long string"'
    );
    expect(tree.children.find((c) => c.name === 'n').preview).toBe('42');
    expect(tree.children.find((c) => c.name === 'z').type).toBe('null');
  });

  it('caps depth and width with truncated markers', () => {
    const deep = { l1: { l2: { l3: { l4: { l5: { l6: { l7: { l8: 'bottom' } } } } } } } };
    const tree = buildTree(deep);
    let node = tree;
    let depth = 0;
    while (node.children) {
      node = node.children[0];
      depth += 1;
    }
    expect(depth).toBeLessThanOrEqual(6);
    const wide = Object.fromEntries(
      Array.from({ length: 30 }, (_, i) => [`k${i}`, i])
    );
    const wtree = buildTree(wide);
    expect(wtree.truncated).toBe(true);
    expect(wtree.children).toHaveLength(24);
  });
});
