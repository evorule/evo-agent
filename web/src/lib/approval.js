// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 批次一 PR-4 审批策略分区 store:条件文件状态 + 试判定 + 纯函数(渲染映射)。
//
// serve 是条件文件与 override 的单源(approval_api.rs),前端零判定逻辑——
// 试判定状态机(red_line/条件/限额)全部在 serve 端机械预检,本文件只做
// 请求编排与展示层映射(与 mcp.js 同款模式)。

import { writable, get } from 'svelte/store';
import { getApprovalPolicy, trialApproval, readFile, writeFile } from './api.js';

/** 条件文件工作区路径(相对 workdir;编辑器/读写/试判定同源) */
export const POLICY_FILE_PATH = '.evo/approval-policy.toml';

/** store 形态:{ loading, error, exists, parseError, groups, limits, override }
 *  - exists=false 是合法态(未预置条件,policy 档运行时降级 manual)
 *  - parseError 非空 = 文件存在但无效(fail-visible 显式留痕)
 *  - groups: {工具组: [条件…]}  limits: {max_corrections, max_actions_per_run} */
export const approvalState = writable({
  loading: false,
  error: '',
  exists: false,
  parseError: null,
  groups: {},
  limits: { max_corrections: 3, max_actions_per_run: 200 },
  override: false,
});

/** 拉取条件文件状态(失败 → error 横幅,保留旧数据) */
export async function loadApproval() {
  approvalState.update((s) => ({ ...s, loading: true, error: '' }));
  try {
    const v = await getApprovalPolicy();
    approvalState.set({
      loading: false,
      error: '',
      exists: !!v?.exists,
      parseError: v?.parse_error ?? null,
      groups: v?.groups ?? {},
      limits: v?.limits ?? { max_corrections: 3, max_actions_per_run: 200 },
      override: !!v?.override,
    });
    return true;
  } catch (e) {
    approvalState.update((s) => ({ ...s, loading: false, error: String(e?.message || e) }));
    return false;
  }
}

/** 读条件文件原文(编辑器草稿初始化;文件不存在回空串) */
export async function readPolicyText() {
  try {
    const v = await readFile(POLICY_FILE_PATH);
    return typeof v?.content === 'string' ? v.content : '';
  } catch {
    return '';
  }
}

/** 写条件文件并重载状态(PUT files/write 整体覆盖 + GET approval-policy 校验回填)。
 *  返回 {ok, parseError}——parseError 非空 = 已落盘但无效(显式展示,不静默)。 */
export async function savePolicyText(content) {
  try {
    await writeFile(POLICY_FILE_PATH, content);
  } catch (e) {
    approvalState.update((s) => ({ ...s, error: String(e?.message || e) }));
    return { ok: false, parseError: null };
  }
  await loadApproval();
  const s = get(approvalState);
  return { ok: true, parseError: s.parseError };
}

/** 试判定(透传 serve 机械预检结果;失败返 error 消息) */
export async function runTrial(tool, args) {
  try {
    return { ok: true, result: await trialApproval(tool, args), error: '' };
  } catch (e) {
    return { ok: false, result: null, error: String(e?.message || e) };
  }
}

/** 试判定状态 → 展示三色(kind: red|amber|grey|blue)+ 中文标签(纯函数,vitest 锁) */
export function trialStatusView(status) {
  switch (status) {
    case 'forced_manual':
      return { kind: 'red', label: '强制人工审批' };
    case 'fallback_manual':
      return { kind: 'grey', label: '降级人工(不可评审)' };
    case 'file_missing':
      return { kind: 'grey', label: '条件文件未预置' };
    case 'file_invalid':
      return { kind: 'grey', label: '条件文件无效' };
    case 'reviewed':
      return { kind: 'amber', label: '进入评审判定' };
    default:
      return { kind: 'grey', label: status || '(未知)' };
  }
}

// ---- 审批档位选择器(批B):四档快捷预设,底层=既有两键(mode+豁免覆盖) ----
//
// 静态路由层无放行权:本组函数只产出「设置键写入序列」,决策端/红线闸/审计链
// 零触碰。「完全访问」=auto_policy+豁免开启,调用方必须先过确认条(FULL_ACCESS_CONFIRM)
// 才可下发 full 预设——绝不静默开红线豁免(自我扩权防护精神)。

/** 档位设置键与豁免键(与 serve settings.rs 注册条目同源) */
export const APPROVAL_MODE_KEY = 'approval.mode.shellExec';
export const APPROVAL_EXEMPTION_KEY = 'approval.exemptionOverride';

/** 「完全访问」确认文案(与审批策略分区豁免开启确认同口径) */
export const FULL_ACCESS_CONFIRM =
  '确认开启完全访问?红线类操作(删除文件/推送远端/数据外发/修改治理配置文件)将不再强制人工确认,由评审自动判定,留痕携带豁免标记。';

/** 四档预设 → 两键写入序列(纯函数,vitest 锁)。
 *  manual/auto 复位豁免(安全向);full 开豁免(调用方先过确认);
 *  custom 只写 mode——豁免不动,条件文件由用户在审批策略分区自管。 */
export function approvalPresetWrites(preset) {
  switch (preset) {
    case 'manual':
      return [
        [APPROVAL_MODE_KEY, 'manual'],
        [APPROVAL_EXEMPTION_KEY, false],
      ];
    case 'auto':
      return [
        [APPROVAL_MODE_KEY, 'auto_policy'],
        [APPROVAL_EXEMPTION_KEY, false],
      ];
    case 'full':
      return [
        [APPROVAL_MODE_KEY, 'auto_policy'],
        [APPROVAL_EXEMPTION_KEY, true],
      ];
    case 'custom':
      return [[APPROVAL_MODE_KEY, 'policy']];
    default:
      return [];
  }
}

/** 两键实况 → 档位展示(纯函数,vitest 锁):主键映射+豁免角标。
 *  未设/未知 mode → serve 缺省 manual(preset=null 表示未显式写入过)。 */
export function approvalPresetView(mode, exemption) {
  const exempt = !!exemption;
  switch (mode) {
    case 'manual':
      return { preset: 'manual', label: exempt ? '手动审批(红线豁免)' : '手动审批', exempt };
    case 'auto_policy':
      return { preset: exempt ? 'full' : 'auto', label: exempt ? '完全访问' : '自动审批', exempt };
    case 'policy':
      return { preset: 'custom', label: exempt ? '自定义配置(红线豁免)' : '自定义配置', exempt };
    default:
      return { preset: null, label: '手动审批', exempt };
  }
}
