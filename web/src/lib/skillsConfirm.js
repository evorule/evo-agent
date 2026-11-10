// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 技能两段式确认的 web 侧解析桥：202 挑战 → 壳原生对话框（Tauri IPC，
// webview 沙箱外的人裁）→ confirm/deny → 动作结果。
//
// 形态分叉：桌面壳（__TAURI__ 在位）= invoke 原生裁决通道；无壳环境
// （armed 模式理论不可达——serve 端模式由壳 env 置位）= 状态轮询兜底，
// 60s 超时显式报未完成，不静默。

const POLL_INTERVAL_MS = 1000;
const POLL_TIMEOUT_MS = 60000;

function storageToken() {
  try {
    return localStorage.getItem('evo_agent_token') || '';
  } catch {
    return '';
  }
}

/**
 * 解析 202 挑战：经壳原生对话框人裁后消费/拒绝。
 * @param {{challenge_id: string, action: string}} challenge 202 响应体
 * @param {unknown} [tauri] 注入点（缺省 window.__TAURI__；测试可传桩）
 * @returns {Promise<{executed: boolean, state: string, result?: unknown, detail?: unknown}>}
 */
export async function resolveChallenge(challenge, tauri) {
  const bridge = tauri ?? globalThis.window?.__TAURI__;
  if (bridge?.invoke) {
    try {
      const out = await bridge.invoke('skills_native_confirm', {
        challengeId: challenge.challenge_id,
        action: challenge.action,
      });
      if (out && typeof out === 'object' && 'executed' in out) return out;
    } catch {
      // IPC 失败（权限/壳版本旧）→ 落轮询兜底，不静默吞
    }
  }
  const deadline = Date.now() + POLL_TIMEOUT_MS;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS));
    try {
      const r = await fetch(`/api/skills/challenge/${encodeURIComponent(challenge.challenge_id)}`, {
        headers: { Authorization: `Bearer ${storageToken()}` },
      });
      if (r.ok) {
        const v = await r.json();
        if (v.state && v.state !== 'pending') {
          return {
            executed: v.state === 'executed',
            state: v.state,
            result: v.result,
          };
        }
      }
    } catch {
      // 网络抖动继续轮询，超时统一口径
    }
  }
  return { executed: false, state: 'timeout' };
}
