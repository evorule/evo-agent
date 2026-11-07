// @vitest-environment jsdom
// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { describe, it, expect } from 'vitest';
import {
  mediaCategoryFor,
  shouldRouteToMediaStage,
  isOversize,
  MEDIA_BLOB_LIMIT,
} from '../src/lib/media-stage.js';

describe('mediaCategoryFor', () => {
  it('classifies video extensions (slash and backslash paths)', () => {
    expect(mediaCategoryFor('media/sample.mp4')).toBe('video');
    expect(mediaCategoryFor('media\\clip.webm')).toBe('video');
    expect(mediaCategoryFor('render.mov')).toBe('video');
    expect(mediaCategoryFor('render.mkv')).toBe('video');
  });

  it('classifies audio extensions', () => {
    expect(mediaCategoryFor('voice.mp3')).toBe('audio');
    expect(mediaCategoryFor('voice.WAV')).toBe('audio');
    expect(mediaCategoryFor('dub.m4a')).toBe('audio');
    expect(mediaCategoryFor('tone.aac')).toBe('audio');
  });

  it('classifies binary image extensions', () => {
    expect(mediaCategoryFor('shot.png')).toBe('image');
    expect(mediaCategoryFor('shot.jpeg')).toBe('image');
    expect(mediaCategoryFor('frame.webp')).toBe('image');
    expect(mediaCategoryFor('icon.ico')).toBe('image');
  });

  it('returns null for editable text files (svg/json/md stay in editor)', () => {
    expect(mediaCategoryFor('logo.svg')).toBeNull();
    expect(mediaCategoryFor('manifest.json')).toBeNull();
    expect(mediaCategoryFor('README.md')).toBeNull();
    expect(mediaCategoryFor('main.rs')).toBeNull();
  });

  it('returns null for extensionless, dotfile and unknown names', () => {
    expect(mediaCategoryFor('noext')).toBeNull();
    expect(mediaCategoryFor('')).toBeNull();
    expect(mediaCategoryFor(null)).toBeNull();
    expect(mediaCategoryFor('.hidden.png')).toBe('image');
    expect(mediaCategoryFor('clip.unknownext')).toBeNull();
  });

  it('routes 3D model binaries to the explicit model category', () => {
    expect(mediaCategoryFor('scene.glb')).toBe('model');
    expect(mediaCategoryFor('asset.gltf')).toBe('model');
    expect(shouldRouteToMediaStage('scene.glb')).toBe(true);
  });
});

describe('shouldRouteToMediaStage', () => {
  it('routes binary media and not text', () => {
    expect(shouldRouteToMediaStage('a/build/final.mp4')).toBe(true);
    expect(shouldRouteToMediaStage('src/app.js')).toBe(false);
  });
});

describe('isOversize', () => {
  it('is false at and below the limit, true above it', () => {
    expect(isOversize(MEDIA_BLOB_LIMIT)).toBe(false);
    expect(isOversize(MEDIA_BLOB_LIMIT - 1)).toBe(false);
    expect(isOversize(MEDIA_BLOB_LIMIT + 1)).toBe(true);
  });

  it('treats non-numeric input as not oversize', () => {
    expect(isOversize(null)).toBe(false);
    expect(isOversize(undefined)).toBe(false);
    expect(isOversize(Number.NaN)).toBe(false);
  });

  it('limit is the 64 MiB preview contract', () => {
    expect(MEDIA_BLOB_LIMIT).toBe(64 * 1024 * 1024);
  });
});
