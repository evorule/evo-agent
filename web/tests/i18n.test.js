// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// i18n 基建纯函数测试:key 即源文案/查表命中/{占位} 插值/未命中原样回退/多占位。
import { describe, it, expect } from 'vitest';
import { t, currentLang } from '../src/lib/i18n.js';

describe('t 文案取值', () => {
  it('注册表命中返回表值(zh 源语言下表值=key 本身)', () => {
    expect(t('中断')).toBe('中断');
    expect(t('批准')).toBe('批准');
    expect(currentLang).toBe('zh');
  });

  it('{占位} 插值:多占位全替换', () => {
    expect(t('第 {n} 步', { n: 3 })).toBe('第 3 步');
    expect(t('{tool} · {cmd} · 已批准', { tool: 'shell_exec', cmd: 'ls' })).toBe(
      'shell_exec · ls · 已批准',
    );
  });

  it('未命中的 key 原样返回(未迁移文案零破坏)', () => {
    expect(t('还没有进表的文案')).toBe('还没有进表的文案');
    expect(t('还没有进表的文案', { n: 1 })).toBe('还没有进表的文案');
  });

  it('params 缺省:占位符原样保留', () => {
    expect(t('第 {n} 步')).toBe('第 {n} 步');
  });
});
