// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 底部面板激活 tab(共享基建):开合状态(panelVisible)在 stores.js,
// 本模块补「激活哪个 tab」的全局维度——问题计数 chip / 命令面板 / 终端与
// 任务面板转正后打开指定 tab 的统一入口。

import { writable } from 'svelte/store';
import { panelVisible } from './stores.js';

/** 底部面板当前激活 tab(BottomPanel tabs.id:'output' | 'problems' | ...) */
export const panelTab = writable('output');

/** 打开底部面板并切换到指定 tab */
export function openBottomPanel(tab) {
  panelTab.set(tab);
  panelVisible.set(true);
}
