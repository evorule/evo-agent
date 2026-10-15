// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// B17 持久会话:会话重建纯函数单测(死会话检测/历史回注块/最后 user 气泡)
import { describe, it, expect } from 'vitest';
import {
  looksLikeDeadSession,
  buildRebuildBlock,
  lastUserTextOf,
} from '../src/lib/sessionRebuild.js';

describe('looksLikeDeadSession', () => {
  it('命中四判据形态', () => {
    expect(looksLikeDeadSession('Evorule error: API returned error: 404')).toBe(true);
    expect(looksLikeDeadSession('{"status": 404,"detail":"gone"}')).toBe(true);
    expect(looksLikeDeadSession('NOT_FOUND: session missing')).toBe(true);
    expect(looksLikeDeadSession('{"message":"Not Found"}')).toBe(true);
  });

  it('普通错误与正常输出不误报', () => {
    expect(looksLikeDeadSession('LLM timeout after 30s')).toBe(false);
    expect(looksLikeDeadSession('HTTP status: 200 ok')).toBe(false);
    expect(looksLikeDeadSession('status: 5001 internal')).toBe(false);
    expect(looksLikeDeadSession('')).toBe(false);
  });
});

describe('buildRebuildBlock', () => {
  it('角色过滤 + 时间正序 + 头尾标记', () => {
    const block = buildRebuildBlock([
      { role: 'system', content: 'sys prompt' },
      { role: 'user', content: 'u1' },
      { role: 'assistant', content: 'a1' },
      { role: 'tool', content: 'tool out' },
      { role: 'user', content: 'u2' },
    ]);
    expect(block.startsWith('[会话重建回注]')).toBe(true);
    expect(block).toContain('<recovered_history>');
    expect(block).toContain('</recovered_history>');
    expect(block).not.toContain('sys prompt');
    expect(block).not.toContain('tool out');
    const u1 = block.indexOf('[user] u1');
    const a1 = block.indexOf('[assistant] a1');
    const u2 = block.indexOf('[user] u2');
    expect(u1).toBeGreaterThanOrEqual(0);
    expect(u1 < a1 && a1 < u2).toBe(true);
  });

  it('单条 1500 字符截断(按字符数)', () => {
    const block = buildRebuildBlock([{ role: 'user', content: 'x'.repeat(3000) }]);
    expect(block).toContain('x'.repeat(1500));
    expect(block).not.toContain('x'.repeat(1501));
  });

  it('总预算 6000 字符,自最新往回收集(25×500 → 恰收 12 条)', () => {
    const msgs = Array.from({ length: 25 }, (_, i) => ({
      role: 'user',
      content: `m${String(i).padStart(2, '0')}${'y'.repeat(497)}`,
    }));
    const block = buildRebuildBlock(msgs);
    const picked = block.split('\n').filter((l) => l.startsWith('[user] m')).length;
    expect(picked).toBe(12);
    expect(block).toContain('[user] m24');
    expect(block).toContain('[user] m13');
    expect(block).not.toContain('[user] m12');
  });

  it('空/非法输入退化为空块', () => {
    const empty = buildRebuildBlock([]);
    expect(empty).toContain('<recovered_history>\n</recovered_history>');
    expect(buildRebuildBlock(null)).toBe(empty);
    expect(buildRebuildBlock([{ role: 'tool', content: 'x' }])).toBe(empty);
  });
});

describe('lastUserTextOf', () => {
  it('取最后一条非空 user 气泡', () => {
    const msgs = [
      { kind: 'user', text: 'first' },
      { kind: 'assistant', text: 'reply' },
      { kind: 'user', text: 'second' },
    ];
    expect(lastUserTextOf(msgs)).toBe('second');
  });

  it('跳过空文本 user 气泡', () => {
    expect(lastUserTextOf([{ kind: 'user', text: '   ' }, { kind: 'user', text: 'real' }])).toBe(
      'real',
    );
  });

  it('无 user 气泡/非法输入返回 null', () => {
    expect(lastUserTextOf([{ kind: 'assistant', text: 'hi' }])).toBeNull();
    expect(lastUserTextOf([])).toBeNull();
    expect(lastUserTextOf(null)).toBeNull();
  });
});
