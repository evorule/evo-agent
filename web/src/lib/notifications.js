// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 通知中心(F2):全工作台统一通知入口——右下 toast 队列 + 状态栏铃铛下拉面板。
// 队列条目 {id, level:'info'|'warn'|'err', title, body, ts, read};
// sessionStorage 持久化(会话内),toast 队列仅内存(err 常驻点叉,其余 4s 自动收)。
import { writable } from 'svelte/store';

const STORE_KEY = 'evo_notifications';
export const NOTIF_CAP = 50; // 面板数据源容量(展示最近 20 条,存 50 防抖丢)
export const TOAST_MS = 4000;

/** 追加一条通知(纯函数):新条目置顶,超出容量截断 */
export function appendNotification(list, entry, cap = NOTIF_CAP) {
  return [entry, ...list].slice(0, cap);
}

/** 未读数(纯函数) */
export function unreadCount(list) {
  return list.reduce((n, x) => n + (x.read ? 0 : 1), 0);
}

/** toast 是否自动关闭(纯函数):err 常驻须人工点叉 */
export function toastShouldAutoClose(level) {
  return level !== 'err';
}

function loadStored() {
  try {
    const raw = JSON.parse(sessionStorage.getItem(STORE_KEY) || '[]');
    return Array.isArray(raw)
      ? raw.filter((x) => x && typeof x.id === 'string').slice(0, NOTIF_CAP)
      : [];
  } catch {
    return [];
  }
}

function persist(list) {
  try {
    sessionStorage.setItem(STORE_KEY, JSON.stringify(list));
  } catch {
    // 存储满/不可用:通知仍入内存队列(fail-soft)
  }
}

/** 面板数据源(持久化) */
export const notifications = writable(loadStored());
/** 右下 toast 队列(内存;条目同通知形状) */
export const toasts = writable([]);

let seq = 0;
const timers = new Map(); // toast id → 自动收计时器

function nextId() {
  seq += 1;
  return `n_${Date.now().toString(36)}_${seq}`;
}

/**
 * 统一通知入口:入面板队列(持久化)+右下 toast(err 常驻)。
 * @param {'info'|'warn'|'err'} level
 * @param {string} title
 * @param {string} [body]
 */
export function push(level, title, body = '') {
  const entry = {
    id: nextId(),
    level: level === 'warn' || level === 'err' ? level : 'info',
    title: String(title || ''),
    body: String(body || ''),
    ts: Date.now(),
    read: false,
  };
  notifications.update((l) => {
    const next = appendNotification(l, entry);
    persist(next);
    return next;
  });
  toasts.update((l) => appendNotification(l, entry, 4)); // 同屏 toast 至多 4 条
  if (toastShouldAutoClose(entry.level)) {
    timers.set(
      entry.id,
      setTimeout(() => dismissToast(entry.id), TOAST_MS),
    );
  }
  return entry.id;
}

/** 收起单条 toast(人工点叉/自动到时共用;面板记录不受影响) */
export function dismissToast(id) {
  const t = timers.get(id);
  if (t) {
    clearTimeout(t);
    timers.delete(id);
  }
  toasts.update((l) => l.filter((x) => x.id !== id));
}

/** 面板:全部标已读 */
export function markAllRead() {
  notifications.update((l) => {
    const next = l.map((x) => (x.read ? x : { ...x, read: true }));
    persist(next);
    return next;
  });
}

/** 面板:清空(含未收 toast 与计时器) */
export function clearAll() {
  for (const t of timers.values()) clearTimeout(t);
  timers.clear();
  toasts.set([]);
  notifications.set([]);
  try {
    sessionStorage.removeItem(STORE_KEY);
  } catch {
    /* fail-soft */
  }
}
