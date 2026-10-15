// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 审批策略分区 store 测试(审批三档制 PR-4):试判定状态三色映射、
// 状态拉取回填、保存并校验(parseError 显式透出)、试判定透传与失败路径。
// api.js 全 mock(同 mcp.test.js 模式)。
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';

vi.mock('../src/lib/api.js', () => ({
  getApprovalPolicy: vi.fn(),
  trialApproval: vi.fn(),
  readFile: vi.fn(),
  writeFile: vi.fn(),
}));

import {
  approvalState,
  POLICY_FILE_PATH,
  loadApproval,
  readPolicyText,
  savePolicyText,
  runTrial,
  trialStatusView,
} from '../src/lib/approval.js';
import { getApprovalPolicy, trialApproval, readFile, writeFile } from '../src/lib/api.js';

const DEFAULT_LIMITS = { max_corrections: 3, max_actions_per_run: 200 };

function policyResponse(overrides = {}) {
  return {
    exists: true,
    parse_error: null,
    groups: {},
    limits: DEFAULT_LIMITS,
    override: false,
    ...overrides,
  };
}

function resetStore() {
  approvalState.set({
    loading: false,
    error: '',
    exists: false,
    parseError: null,
    groups: {},
    limits: DEFAULT_LIMITS,
    override: false,
  });
}

beforeEach(() => {
  resetStore();
  vi.clearAllMocks();
});

describe('POLICY_FILE_PATH', () => {
  it('工作区路径与 serve 侧同源', () => {
    expect(POLICY_FILE_PATH).toBe('.evo/approval-policy.toml');
  });
});

describe('trialStatusView(状态 → 三色展示)', () => {
  it('五态映射:红=强制人工 / 黄=评审判定 / 灰=降级·未预置·无效', () => {
    expect(trialStatusView('forced_manual')).toEqual({ kind: 'red', label: '强制人工审批' });
    expect(trialStatusView('reviewed')).toEqual({ kind: 'amber', label: '进入评审判定' });
    expect(trialStatusView('fallback_manual')).toEqual({
      kind: 'grey',
      label: '降级人工(不可评审)',
    });
    expect(trialStatusView('file_missing')).toEqual({ kind: 'grey', label: '条件文件未预置' });
    expect(trialStatusView('file_invalid')).toEqual({ kind: 'grey', label: '条件文件无效' });
  });

  it('未知状态灰色透出,防静默丢失', () => {
    expect(trialStatusView('something_new')).toEqual({ kind: 'grey', label: 'something_new' });
    expect(trialStatusView('')).toEqual({ kind: 'grey', label: '(未知)' });
    expect(trialStatusView(undefined)).toEqual({ kind: 'grey', label: '(未知)' });
  });
});

describe('loadApproval(状态拉取)', () => {
  it('响应字段回填 store', async () => {
    getApprovalPolicy.mockResolvedValue(
      policyResponse({
        groups: { shell_exec: ['条件一', '条件二'] },
        limits: { max_corrections: 5, max_actions_per_run: 100 },
        override: true,
      })
    );
    expect(await loadApproval()).toBe(true);
    const s = get(approvalState);
    expect(s.exists).toBe(true);
    expect(s.parseError).toBeNull();
    expect(s.groups).toEqual({ shell_exec: ['条件一', '条件二'] });
    expect(s.limits).toEqual({ max_corrections: 5, max_actions_per_run: 100 });
    expect(s.override).toBe(true);
    expect(s.error).toBe('');
    expect(s.loading).toBe(false);
  });

  it('exists=false 是合法态(未预置条件)', async () => {
    getApprovalPolicy.mockResolvedValue(policyResponse({ exists: false, groups: {} }));
    await loadApproval();
    const s = get(approvalState);
    expect(s.exists).toBe(false);
    expect(s.error).toBe('');
  });

  it('parse_error 非空 = 文件存在但无效', async () => {
    getApprovalPolicy.mockResolvedValue(policyResponse({ parse_error: 'TOML 解析失败' }));
    await loadApproval();
    expect(get(approvalState).parseError).toBe('TOML 解析失败');
  });

  it('请求失败挂 error 横幅,返回 false', async () => {
    getApprovalPolicy.mockRejectedValue(new Error('HTTP 500'));
    expect(await loadApproval()).toBe(false);
    const s = get(approvalState);
    expect(s.error).toBe('HTTP 500');
    expect(s.loading).toBe(false);
  });
});

describe('savePolicyText(保存并校验)', () => {
  it('写盘后重载状态,parseError 显式透出(不静默)', async () => {
    writeFile.mockResolvedValue({});
    getApprovalPolicy.mockResolvedValue(policyResponse({ parse_error: 'schema 校验失败' }));
    const r = await savePolicyText('[shell_exec]\nconditions = ["x"]');
    expect(writeFile).toHaveBeenCalledWith(POLICY_FILE_PATH, '[shell_exec]\nconditions = ["x"]');
    expect(r).toEqual({ ok: true, parseError: 'schema 校验失败' });
  });

  it('校验通过返回 parseError:null', async () => {
    writeFile.mockResolvedValue({});
    getApprovalPolicy.mockResolvedValue(policyResponse({ groups: { shell_exec: ['c1'] } }));
    const r = await savePolicyText('ok toml');
    expect(r).toEqual({ ok: true, parseError: null });
    expect(get(approvalState).groups).toEqual({ shell_exec: ['c1'] });
  });

  it('写盘失败返回 ok:false 并挂 error', async () => {
    writeFile.mockRejectedValue(new Error('disk full'));
    const r = await savePolicyText('x');
    expect(r).toEqual({ ok: false, parseError: null });
    expect(get(approvalState).error).toBe('disk full');
  });
});

describe('runTrial(试判定透传)', () => {
  it('透传 serve 机械预检结果', async () => {
    const result = {
      status: 'forced_manual',
      red_line: 'red_line.delete',
      conditions: [],
      note: '红线命中且豁免覆盖未开启',
      limits: DEFAULT_LIMITS,
      override: false,
    };
    trialApproval.mockResolvedValue(result);
    const r = await runTrial('file_delete', {});
    expect(trialApproval).toHaveBeenCalledWith('file_delete', {});
    expect(r).toEqual({ ok: true, result, error: '' });
  });

  it('失败返 error 消息', async () => {
    trialApproval.mockRejectedValue(new Error('HTTP 400: tool must not be empty'));
    const r = await runTrial(' ', {});
    expect(r.ok).toBe(false);
    expect(r.result).toBeNull();
    expect(r.error).toBe('HTTP 400: tool must not be empty');
  });
});

describe('readPolicyText(编辑器草稿初始化)', () => {
  it('返回文件内容;读取失败回空串', async () => {
    readFile.mockResolvedValue({ content: '[policy_limits]\nmax_corrections = 3' });
    expect(await readPolicyText()).toBe('[policy_limits]\nmax_corrections = 3');
    readFile.mockRejectedValue(new Error('HTTP 404'));
    expect(await readPolicyText()).toBe('');
  });
});
