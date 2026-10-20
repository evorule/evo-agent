// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 通知中心(F2)纯函数与 store 行为(会话存储在 node 环境缺省,loadStored 容错回落空)
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { get } from 'svelte/store';
import {
  notifications,
  toasts,
  push,
  dismissToast,
  markAllRead,
  clearAll,
  appendNotification,
  unreadCount,
  toastShouldAutoClose,
  NOTIF_CAP,
  TOAST_MS,
} from '../src/lib/notifications.js';

describe('纯函数', () => {
  it('appendNotification:新条目置顶', () => {
    const base = [{ id: 'a', title: 'A' }];
    const next = appendNotification(base, { id: 'b', title: 'B' });
    expect(next.map((x) => x.id)).toEqual(['b', 'a']);
  });

  it('appendNotification:超出容量截断(最旧丢失)', () => {
    const base = Array.from({ length: NOTIF_CAP }, (_, i) => ({ id: `n${i}` })); // n0 最新…n49 最旧
    const next = appendNotification(base, { id: 'new' });
    expect(next.length).toBe(NOTIF_CAP);
    expect(next[0].id).toBe('new');
    expect(next.some((x) => x.id === 'n49')).toBe(false); // 尾部最旧被截
    expect(next.some((x) => x.id === 'n0')).toBe(true);
  });

  it('unreadCount:只计未读', () => {
    const list = [
      { id: '1', read: false },
      { id: '2', read: true },
      { id: '3', read: false },
    ];
    expect(unreadCount(list)).toBe(2);
  });

  it('toastShouldAutoClose:err 常驻,info/warn 自动收', () => {
    expect(toastShouldAutoClose('err')).toBe(false);
    expect(toastShouldAutoClose('warn')).toBe(true);
    expect(toastShouldAutoClose('info')).toBe(true);
  });
});

describe('store 行为', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    clearAll();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('push:入面板队列+右下 toast,非法级别归一 info', () => {
    push('info', '标题A', '正文');
    push('bogus', '标题B');
    const panel = get(notifications);
    expect(panel.map((x) => x.title)).toEqual(['标题B', '标题A']);
    expect(panel[0].level).toBe('info');
    expect(panel[1].read).toBe(false);
    expect(get(toasts).length).toBe(2);
  });

  it('push:非 err 级 4s 自动收 toast,面板记录不受影响', () => {
    push('info', '临时提示');
    expect(get(toasts).length).toBe(1);
    vi.advanceTimersByTime(TOAST_MS + 1);
    expect(get(toasts).length).toBe(0);
    expect(get(notifications).length).toBe(1);
  });

  it('push:err 级常驻,点叉才收', () => {
    const id = push('err', '失败', '详情');
    vi.advanceTimersByTime(TOAST_MS * 5);
    expect(get(toasts).length).toBe(1);
    dismissToast(id);
    expect(get(toasts).length).toBe(0);
  });

  it('dismissToast:只收对应 toast', () => {
    const a = push('err', 'A');
    push('err', 'B');
    dismissToast(a);
    expect(get(toasts).map((x) => x.title)).toEqual(['B']);
  });

  it('markAllRead:未读清零', () => {
    push('warn', 'w1');
    push('warn', 'w2');
    expect(unreadCount(get(notifications))).toBe(2);
    markAllRead();
    expect(unreadCount(get(notifications))).toBe(0);
  });

  it('clearAll:面板/toast/未读全清', () => {
    push('err', '常驻');
    push('info', '临时');
    clearAll();
    expect(get(notifications).length).toBe(0);
    expect(get(toasts).length).toBe(0);
  });

  it('push:同屏 toast 超量挤掉最旧', () => {
    for (let i = 0; i < 6; i++) push('err', `t${i}`);
    const visible = get(toasts);
    expect(visible.length).toBe(4);
    expect(visible[0].title).toBe('t5');
    expect(visible.some((x) => x.title === 't0')).toBe(false);
  });
});
