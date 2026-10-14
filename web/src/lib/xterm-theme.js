// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// xterm 主题:从 lib/theme.js token 表派生(background 透明,
// 前景/光标/选区随明暗主题切换——终端板块预留挂点由本文件兑现)。
import { xtermThemeFor, onThemeChange, resolveMode, readStoredMode } from './theme.js';

export const terminalTheme = xtermThemeFor(readStoredMode());

/** 主题变化订阅(切换时 xterm.options.theme 更新由 GroupPane/终端持有方消费) */
export function onTerminalThemeChange(fn) {
  return onThemeChange((resolved) => fn(xtermThemeFor(resolved === 'light' ? 'evorule-light' : 'evorule-dark')));
}

export { resolveMode };
