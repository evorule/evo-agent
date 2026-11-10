// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 对话流回合布局纯函数:输入消息数组,输出渲染组序列。三条规则:
// ①连续 agent 活动(tool/assistant/审批回执/「审批结果:」兜底信息条)归入
//   同一过程段,段内最后一条带正文的 assistant 是回答(留框外作答案气泡);
//   背景:推理模型每个思考步都带中间陈述文本,若按「无正文才算活动」判定
//   会把它们散开成 N 个碎框,故按连续活动段归组。
// ②段内相邻「同工具+同结局」的审批回执 ≥RECEIPT_AGG_THRESHOLD 条折叠为
//   一个 receipt_agg 汇总步(点击展开逐条),消审批回执刷屏。
// ③其余消息(用户/待批卡/提问/计划/产物/系统信息条)各自独立成组。
// 每个过程段附 steps(段内原始步数)与 tookMs(段内工具耗时合计)供头部
// 摘要与答案气泡 meta 行。折叠开合状态属组件交互态,不在此处。

/** 回执折叠阈值:同工具同结局连续回执达到该条数才收敛为汇总行 */
export const RECEIPT_AGG_THRESHOLD = 3;

const BOXED_KINDS = new Set(['tool', 'assistant', 'approval_receipt']);

/** 回执结局归一:ok(批准)/deny(拒绝)/bad(超时·失效·失败)/other(中间态) */
function receiptOutcome(m) {
  if (m?.decision === 'sent') return m.sentApproved === false ? 'deny' : 'ok';
  if (m?.decision === 'error' || m?.decision === 'timeout' || m?.decision === 'expired') {
    return 'bad';
  }
  return 'other';
}

/** 审批结果兜底信息条(双写消除后的幸存形态)入框;其余系统信息条不入框 */
function isApprovalInfoLine(m) {
  return m?.kind === 'info' && String(m?.text || '').startsWith('审批结果:');
}

/** 相邻同工具同结局回执折叠(纯函数):run 长度达阈值替换为 receipt_agg */
export function collapseReceiptRuns(items) {
  const out = [];
  let run = null;
  const flushRun = () => {
    if (!run) return;
    if (run.items.length >= RECEIPT_AGG_THRESHOLD) {
      out.push({
        id: `agg${run.items[0].id}`,
        kind: 'receipt_agg',
        toolName: run.items[0].toolName,
        outcome: run.outcome,
        count: run.items.length,
        items: run.items,
      });
    } else {
      out.push(...run.items);
    }
    run = null;
  };
  for (const m of items) {
    if (m?.kind === 'approval_receipt') {
      const oc = receiptOutcome(m);
      if (run && (run.outcome !== oc || run.items[0].toolName !== m.toolName)) flushRun();
      if (!run) run = { outcome: oc, items: [] };
      run.items.push(m);
    } else {
      flushRun();
      out.push(m);
    }
  }
  flushRun();
  return out;
}

function sumTookMs(items) {
  let total = 0;
  for (const m of items) {
    if (typeof m?.tookMs === 'number') total += m.tookMs;
  }
  return total;
}

/**
 * 回合布局(纯函数):返回组序列。
 * - {type:'process', key, items, closed, steps, tookMs}:过程段(items 内
 *   回执已按阈值折叠;steps=折叠前原始步数;closed=段是否已闭合,闭合段由
 *   组件按交互态决定展开,未闭合段=进行中恒展开)。
 * - {type:'single', key, m, meta}:独立消息;从过程段提取的回答带 meta
 *   {steps, tookMs} 供答案气泡 meta 行。
 */
export function buildTurnLayout(layoutMessages) {
  const out = [];
  let buf = null;
  const flushBuf = (closed) => {
    if (!buf) return;
    let answerIdx = -1;
    for (let i = buf.items.length - 1; i >= 0; i--) {
      const m = buf.items[i];
      if (m.kind === 'assistant' && (m.text || '').trim()) {
        answerIdx = i;
        break;
      }
    }
    const boxedRaw = buf.items.filter((_, i) => i !== answerIdx);
    const boxed = collapseReceiptRuns(boxedRaw);
    const tookMs = sumTookMs(boxedRaw);
    if (boxed.length) {
      out.push({ type: 'process', key: `p${boxed[0].id}`, items: boxed, closed, steps: boxedRaw.length, tookMs });
    }
    if (answerIdx >= 0) {
      out.push({
        type: 'single',
        key: `m${buf.items[answerIdx].id}`,
        m: buf.items[answerIdx],
        meta: { steps: boxedRaw.length, tookMs },
      });
    }
    buf = null;
  };
  for (const m of layoutMessages) {
    if (BOXED_KINDS.has(m.kind) || isApprovalInfoLine(m)) {
      if (!buf) buf = { items: [] };
      buf.items.push(m);
    } else {
      flushBuf(true);
      out.push({ type: 'single', key: `m${m.id}`, m });
    }
  }
  // 末尾未闭合段=进行中的实时活动,保持框内展开
  flushBuf(false);
  return out;
}
