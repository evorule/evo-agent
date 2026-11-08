// @vitest-environment jsdom
// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect } from 'vitest';
import {
  extractAssetEvent,
  previewKindFor,
  buildAssetPath,
} from '../src/lib/progress-assets.js';

describe('extractAssetEvent', () => {
  const doneAsset = {
    seq: 30,
    phase: 'done',
    asset: { path: 'final.mp4', kind: 'mp4', bytes: 100 },
  };

  it('picks the newest event with a previewable asset (newest-first list)', () => {
    const newer = { seq: 31, phase: 'done', asset: { path: 'cover.png', kind: 'png' } };
    expect(extractAssetEvent([newer, doneAsset])).toBe(newer);
  });

  it('skips events without asset node or with empty path', () => {
    expect(extractAssetEvent([{ seq: 1, phase: 'done' }, { seq: 2, asset: {} }, doneAsset])).toBe(
      doneAsset
    );
    expect(
      extractAssetEvent([{ seq: 2, phase: 'done', asset: { path: '   ', kind: 'mp4' } }, doneAsset])
    ).toBe(doneAsset);
  });

  it('skips non-previewable asset kinds (subtitles/docs are not preview cards)', () => {
    const ass = { seq: 5, phase: 'done', asset: { path: 'subs.ass', kind: 'ass' } };
    const docx = { seq: 6, phase: 'done', asset: { path: 'r.docx', kind: 'docx' } };
    expect(extractAssetEvent([docx, ass, doneAsset])).toBe(doneAsset);
  });

  it('returns null on empty/garbage input', () => {
    expect(extractAssetEvent([])).toBeNull();
    expect(extractAssetEvent(null)).toBeNull();
    expect(extractAssetEvent([null, 'x', 42])).toBeNull();
  });
});

describe('previewKindFor', () => {
  it('maps video and image kinds case-insensitively', () => {
    expect(previewKindFor('mp4')).toBe('video');
    expect(previewKindFor('WEBM')).toBe('video');
    expect(previewKindFor('PNG')).toBe('image');
    expect(previewKindFor('jpeg')).toBe('image');
  });

  it('returns null for non-previewable kinds', () => {
    expect(previewKindFor('ass')).toBeNull();
    expect(previewKindFor('srt')).toBeNull();
    expect(previewKindFor('docx')).toBeNull();
    expect(previewKindFor('')).toBeNull();
    expect(previewKindFor(undefined)).toBeNull();
  });
});

describe('buildAssetPath', () => {
  it('joins ndjson dir + build/ + asset basename (root layout)', () => {
    expect(buildAssetPath('video_engine/projects/acceptance/build.progress.ndjson', 'final.mp4'))
      .toBe('video_engine/projects/acceptance/build/final.mp4');
  });

  it('normalizes when ndjson lives inside build/ (same target)', () => {
    expect(buildAssetPath('video_engine/projects/acceptance/build/build.progress.ndjson', 'final.mp4'))
      .toBe('video_engine/projects/acceptance/build/final.mp4');
  });

  it('strips directory components from asset path (contract: filename only)', () => {
    expect(buildAssetPath('p/a/build.progress.ndjson', 'shots/s3.mp4')).toBe('p/a/build/s3.mp4');
    expect(buildAssetPath('p/a/build.progress.ndjson', 'shots\\s3.mp4')).toBe('p/a/build/s3.mp4');
  });

  it('returns null when any input is missing', () => {
    expect(buildAssetPath('', 'final.mp4')).toBeNull();
    expect(buildAssetPath('p/a/build.progress.ndjson', '')).toBeNull();
    expect(buildAssetPath(null, 'final.mp4')).toBeNull();
  });
});
