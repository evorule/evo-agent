// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 会话重建纯函数 — 死会话检测与历史回注块合成(无 IO,可单测)
// (B17 持久会话:引擎侧会话 30min 闲置 TTL 回收/重启清零后,「新建会话 +
// 消息前缀回注历史块」= 重建续聊;块格式与 serve 侧 compose_rebuild_context
// 同格式同口径,保证 HTTP 面与工作台面续聊上下文一致)

const REBUILD_HEADER =
  '[会话重建回注] 此前会话在引擎侧已失效(闲置回收或服务重启),以下为历史消息投影,供续接上下文;请勿复述,直接回应文末的新输入。';
const MAX_MESSAGES = 20;
const MAX_PER_MESSAGE = 1500;
const MAX_TOTAL = 6000;

/**
 * 死会话检测:引擎侧会话已失效(evorule-server 重启或 30min 闲置 TTL 回收),
 * evo-agent serve 转发 REST 失败回灌的错误体为 404 形态
 * ("Evorule error: API returned error: 404 {...\"message\":\"Not Found\"...}")。
 */
export function looksLikeDeadSession(text) {
  return (
    text.includes('Session not found') || // serve 死会话错误文案(WS 残留失效 id)
    text.includes('NOT_FOUND') ||
    /"status":\s*404/.test(text) ||
    /returned error: 404/.test(text) ||
    text.includes('"message":"Not Found"')
  );
}

/**
 * 历史消息 → 重建回注块(不含新输入;新输入由调用方以 `\n\n` 追加在块后)。
 *
 * 压缩口径:仅 user/assistant 角色;最近 20 条;单条 1500 字符截断
 * (按字符数,防切破多字节);总预算 6000 字符(自最新往回收集,首条
 * 无条件保留=至少保 1 条)。输出序为时间正序(旧→新)。
 *
 * @param {Array<{role: string, content: string}>} messages transcript REST 响应的消息投影
 * @returns {string} 回注块字符串;空历史时退化为仅含空 <recovered_history> 的块
 */
export function buildRebuildBlock(messages) {
  const list = Array.isArray(messages) ? messages : [];
  const candidates = list
    .filter(
      (m) =>
        m &&
        (m.role === 'user' || m.role === 'assistant') &&
        typeof m.content === 'string',
    )
    .map((m) => {
      const chars = [...m.content];
      const truncated = chars.length > MAX_PER_MESSAGE;
      return {
        role: m.role,
        content: truncated ? chars.slice(0, MAX_PER_MESSAGE).join('') : m.content,
        len: truncated ? MAX_PER_MESSAGE : chars.length,
      };
    });

  // 自最新往回收集,总预算内(首条无条件保留;条数上限同步生效)
  const picked = [];
  let total = 0;
  for (let i = candidates.length - 1; i >= 0; i -= 1) {
    if (picked.length >= MAX_MESSAGES) break;
    if (picked.length > 0 && total + candidates[i].len > MAX_TOTAL) break;
    total += candidates[i].len;
    picked.unshift(candidates[i]);
  }

  const lines = [REBUILD_HEADER, '<recovered_history>'];
  for (const m of picked) lines.push(`[${m.role}] ${m.content}`);
  lines.push('</recovered_history>');
  return lines.join('\n');
}

/**
 * 取最后一条 user 气泡文本(死会话自愈自动续发用)。
 * @param {Array<{kind: string, text: string}>} messages messages store 数组
 * @returns {string | null} 无可续发文本时返回 null
 */
export function lastUserTextOf(messages) {
  const list = Array.isArray(messages) ? messages : [];
  for (let i = list.length - 1; i >= 0; i -= 1) {
    const m = list[i];
    if (m && m.kind === 'user' && typeof m.text === 'string' && m.text.trim()) {
      return m.text;
    }
  }
  return null;
}
