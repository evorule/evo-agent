// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// N-011:「复制 API Token」状态栏按钮的逻辑面(与组件分离供直测)。
//
// 语义:人类显式点击=token 交付动作——token 本就由壳 init_script 注入
// webview localStorage(零新增暴露面),按钮只把它送进剪贴板;
// 不落盘、不新增持久化、不放松任何红线。

/**
 * 复制 API Token 到剪贴板。
 * 双回退:navigator.clipboard.writeText → document.execCommand('copy')。
 * @returns {Promise<{ok: boolean, reason: string}>}
 *   reason: 'copied'(含回退成功) | 'empty'(未注入,auth 关闭态) | 'denied'(双链皆败)
 */
export async function copyApiToken() {
  const t = localStorage.getItem('evo_agent_token') || '';
  if (!t) return { ok: false, reason: 'empty' };
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(t);
      return { ok: true, reason: 'copied' };
    }
  } catch {
    // 掉入 execCommand 回退
  }
  // 回退链:隐藏 textarea + execCommand(jsdom/旧 webview 兼容)
  try {
    const ta = document.createElement('textarea');
    ta.value = t;
    ta.setAttribute('readonly', '');
    ta.style.position = 'fixed';
    ta.style.left = '-9999px';
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand('copy');
    ta.remove();
    return ok ? { ok: true, reason: 'copied' } : { ok: false, reason: 'denied' };
  } catch {
    return { ok: false, reason: 'denied' };
  }
}
