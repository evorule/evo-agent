// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 壳版本探测(shellVersion.js)单测:桌面形态取 tauri 壳版本(=安装版本);
// 非桌面/桥缺件/权限拒答/坏值一律 null 回落(fail-soft)。
import { describe, it, expect } from 'vitest';
import { detectShellVersion } from '../src/lib/shellVersion.js';

describe('detectShellVersion', () => {
  it('桥在位且 getVersion 返回字符串 → 壳版本', async () => {
    const tauri = { app: { getVersion: async () => '0.5.3' } };
    expect(await detectShellVersion(tauri)).toBe('0.5.3');
  });

  it('无桥(浏览器形态) → null 回落', async () => {
    expect(await detectShellVersion(undefined)).toBeNull();
  });

  it('getVersion 缺位 → null', async () => {
    expect(await detectShellVersion({ app: {} })).toBeNull();
  });

  it('getVersion 拒答(reject) → null 不抛', async () => {
    const tauri = {
      app: { getVersion: async () => Promise.reject(new Error('not allowed')) },
    };
    expect(await detectShellVersion(tauri)).toBeNull();
  });

  it('坏值(非字符串/空串) → null', async () => {
    expect(await detectShellVersion({ app: { getVersion: async () => 42 } })).toBeNull();
    expect(await detectShellVersion({ app: { getVersion: async () => '' } })).toBeNull();
  });
});
