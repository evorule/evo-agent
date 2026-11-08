// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 播放目标裁定单测:不可播容器黑名单 / .preview.mp4 代理路径派生 /
// resolvePlaybackTarget 三态(直连/切代理/显式报错)
import { describe, it, expect } from 'vitest';
import {
  UNPLAYABLE_VIDEO_EXTS,
  isUnplayableVideo,
  proxyPathFor,
  resolvePlaybackTarget,
} from '../src/lib/media-stage.js';

describe('UNPLAYABLE_VIDEO_EXTS', () => {
  it('covers exactly the three legacy/working-codec containers', () => {
    expect(UNPLAYABLE_VIDEO_EXTS).toEqual(['mov', 'mkv', 'avi']);
  });
});

describe('isUnplayableVideo', () => {
  it('flags mov/mkv/avi regardless of directory or case', () => {
    expect(isUnplayableVideo('build/final.mov')).toBe(true);
    expect(isUnplayableVideo('build\\shot.mkv')).toBe(true);
    expect(isUnplayableVideo('render.AVI')).toBe(true);
  });

  it('does not flag natively playable containers or non-video files', () => {
    expect(isUnplayableVideo('build/final.mp4')).toBe(false);
    expect(isUnplayableVideo('build/clip.webm')).toBe(false);
    expect(isUnplayableVideo('cover.png')).toBe(false);
    expect(isUnplayableVideo('notes.txt')).toBe(false);
    expect(isUnplayableVideo('')).toBe(false);
  });
});

describe('proxyPathFor', () => {
  it('derives sibling .preview.mp4 next to the original', () => {
    expect(proxyPathFor('build/final.mov')).toBe('build/final.preview.mp4');
    expect(proxyPathFor('build\\nested\\shot.mkv')).toBe('build/nested/shot.preview.mp4');
  });

  it('keeps multi-dot stems intact', () => {
    expect(proxyPathFor('build/shot.prores.mov')).toBe('build/shot.prores.preview.mp4');
  });

  it('handles extension-less paths gracefully', () => {
    expect(proxyPathFor('build/clip')).toBe('build/clip.preview.mp4');
  });
});

describe('resolvePlaybackTarget', () => {
  it('plays mp4 directly without probing (no proxy round-trip)', async () => {
    let probed = 0;
    const t = await resolvePlaybackTarget('build/final.mp4', async () => {
      probed += 1;
      return { ok: true, playable: true };
    });
    expect(t).toEqual({ target: 'original', path: 'build/final.mp4' });
    expect(probed).toBe(0);
  });

  it('switches to proxy when HEAD says the transcoded mp4 exists and is playable', async () => {
    const t = await resolvePlaybackTarget('build/final.mov', async (p) => ({
      ok: true,
      playable: p.endsWith('.preview.mp4'),
    }));
    expect(t.target).toBe('proxy');
    expect(t.path).toBe('build/final.preview.mp4');
    expect(t.originalPath).toBe('build/final.mov');
  });

  it('errors explicitly when no proxy exists (never silently plays unplayable)', async () => {
    const t = await resolvePlaybackTarget('build/final.mov', async () => ({ ok: false }));
    expect(t).toEqual({
      target: 'error',
      path: 'build/final.mov',
      proxy: 'build/final.preview.mp4',
      reason: 'no-proxy',
    });
  });

  it('errors explicitly when proxy exists but is not a playable video', async () => {
    const t = await resolvePlaybackTarget('build/final.mkv', async () => ({
      ok: true,
      playable: false,
    }));
    expect(t.target).toBe('error');
    expect(t.reason).toBe('proxy-not-playable');
  });

  it('treats probe exceptions as missing proxy (fail-visible, not crash)', async () => {
    const t = await resolvePlaybackTarget('build/final.avi', async () => {
      throw new Error('network down');
    });
    expect(t.target).toBe('error');
    expect(t.reason).toBe('no-proxy');
  });
});
