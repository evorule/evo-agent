// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 壳版本探测:桌面形态(main 窗口=远程工作台)经 withGlobalTauri 全局桥读
// tauri 应用版本——与安装包/更新器同源(tauri.conf.json version,发版链
// 版本闸锚点),恒等于安装版本。非桌面形态(浏览器直开)/权限缺位/桥不可用
// 一律返回 null,调用方回落构建期版本(web/package.json,发版链同步 bump)。

/**
 * @param {unknown} tauri 注入点(缺省 window.__TAURI__;测试可传桩)
 * @returns {Promise<string|null>} 壳版本字符串;不可得 = null
 */
export async function detectShellVersion(tauri) {
  const bridge = tauri ?? globalThis.window?.__TAURI__;
  try {
    const v = await bridge?.app?.getVersion?.();
    return typeof v === 'string' && v ? v : null;
  } catch {
    return null; // 权限缺位/桥异常:fail-soft 回落
  }
}
