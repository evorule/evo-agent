// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 审批会话记忆与工作区放行清单测试:归一化前缀/危险子命令守卫/三档判定/
// 清单读写幂等/回执与汇总卡文案。api.js 全 mock(同 approval.test.js 模式)。
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';

vi.mock('../src/lib/api.js', () => ({
  readFile: vi.fn(),
  writeFile: vi.fn(),
}));

import {
  ALLOWLIST_PATH,
  AGGREGATE_THRESHOLD,
  normalizeCommand,
  commandPrefix,
  isAutoDeny,
  recordSessionAllow,
  clearSessionMemory,
  workspaceAllowlist,
  loadAllowlist,
  addWorkspaceAllow,
  autoApprovalVerdict,
  evaluateAutoApproval,
  approvalReceiptText,
  aggregateSummary,
} from '../src/lib/approvalMemory.js';
import { readFile, writeFile } from '../src/lib/api.js';

beforeEach(() => {
  vi.clearAllMocks();
  clearSessionMemory();
  workspaceAllowlist.set([]);
});

describe('normalizeCommand / commandPrefix', () => {
  it('归一化:去首尾空白+内部折叠+小写', () => {
    expect(normalizeCommand('  FIND   D:\\knowledge  ')).toBe('find d:\\knowledge');
    expect(normalizeCommand(null)).toBe('');
  });

  it('前缀=首个词元', () => {
    expect(commandPrefix('python scripts/seed.py')).toBe('python');
    expect(commandPrefix('  Git  status')).toBe('git');
    expect(commandPrefix('')).toBe('');
  });
});

describe('isAutoDeny 危险子命令守卫', () => {
  it('删除/推送/网络外发命中守卫', () => {
    expect(isAutoDeny('rm -rf build')).toBe(true);
    expect(isAutoDeny('Remove-Item D:\\x')).toBe(true);
    expect(isAutoDeny('git push origin main')).toBe(true);
    expect(isAutoDeny('git reset --hard HEAD~1')).toBe(true);
    expect(isAutoDeny('curl https://example.com')).toBe(true);
    expect(isAutoDeny('taskkill /pid 123')).toBe(true);
  });

  it('只读/常规命令不命中', () => {
    expect(isAutoDeny('find D:\\knowledge -name *.md')).toBe(false);
    expect(isAutoDeny('python scripts/seed.py')).toBe(false);
    expect(isAutoDeny('git status')).toBe(false);
    expect(isAutoDeny('git add -A')).toBe(false);
  });

  it('空命令按守卫处理', () => {
    expect(isAutoDeny('')).toBe(true);
    expect(isAutoDeny(null)).toBe(true);
  });
});

describe('会话记忆三档判定', () => {
  it('无记忆 → ask;登记后同 (tool, prefix) → session', () => {
    expect(autoApprovalVerdict('shell_exec', 'find D:\\knowledge', [])).toBe('ask');
    recordSessionAllow('shell_exec', 'find D:\\knowledge -name *.md');
    expect(autoApprovalVerdict('shell_exec', 'find D:\\src', [])).toBe('session');
  });

  it('工具名不同不命中;同前缀同类命中(首词元前缀语义)', () => {
    recordSessionAllow('shell_exec', 'git status');
    expect(autoApprovalVerdict('shell_exec', 'git status', [])).toBe('session');
    expect(autoApprovalVerdict('shell_exec', 'git diff', [])).toBe('session');
    expect(autoApprovalVerdict('python_exec', 'git status', [])).toBe('ask');
  });

  it('工作区清单 → workspace;守卫恒压过一切记忆(deny 优先)', () => {
    const allow = [{ tool: 'python_exec', prefix: 'python' }];
    expect(autoApprovalVerdict('python_exec', 'python scripts/a.py', allow)).toBe('workspace');
    recordSessionAllow('python_exec', 'python x.py');
    expect(autoApprovalVerdict('python_exec', 'git push origin main', allow)).toBe('deny');
    expect(autoApprovalVerdict('python_exec', 'rm -rf /', allow)).toBe('deny');
  });

  it('clearSessionMemory 只清会话记忆,判定回落', () => {
    recordSessionAllow('shell_exec', 'find x');
    clearSessionMemory();
    expect(autoApprovalVerdict('shell_exec', 'find x', [])).toBe('ask');
  });
});

describe('工作区放行清单读写', () => {
  it('loadAllowlist:正常解析+坏文档 fail-soft 置空', async () => {
    readFile.mockResolvedValueOnce({
      content: JSON.stringify({ version: 1, allow: [{ tool: 'shell_exec', prefix: 'find' }] }),
    });
    expect(await loadAllowlist()).toEqual([{ tool: 'shell_exec', prefix: 'find' }]);
    expect(get(workspaceAllowlist)).toEqual([{ tool: 'shell_exec', prefix: 'find' }]);

    readFile.mockResolvedValueOnce({ content: 'not-json{{{' });
    expect(await loadAllowlist()).toEqual([]);
    readFile.mockRejectedValueOnce(new Error('404'));
    expect(await loadAllowlist()).toEqual([]);
  });

  it('addWorkspaceAllow:追加落盘+同项幂等', async () => {
    // 模拟 serve 文件通道:写后读得到最新内容(读-改-写以盘上为准)
    const disk = {};
    readFile.mockImplementation(async (p) => ({ content: disk[p] ?? '' }));
    writeFile.mockImplementation(async (p, c) => {
      disk[p] = c;
    });

    await addWorkspaceAllow('shell_exec', 'Git  STATUS');
    expect(writeFile).toHaveBeenCalledTimes(1);
    const [path, content] = writeFile.mock.calls[0];
    expect(path).toBe(ALLOWLIST_PATH);
    expect(JSON.parse(content)).toEqual({ version: 1, allow: [{ tool: 'shell_exec', prefix: 'git' }] });

    // 同项重复:不再追加,不落盘
    await addWorkspaceAllow('shell_exec', 'git status');
    expect(writeFile).toHaveBeenCalledTimes(1);
    expect(get(workspaceAllowlist).length).toBe(1);

    // 异项:追加成功
    await addWorkspaceAllow('python_exec', 'python scripts/x.py');
    expect(writeFile).toHaveBeenCalledTimes(2);
    expect(JSON.parse(disk[ALLOWLIST_PATH]).allow.length).toBe(2);
  });

  it('evaluateAutoApproval 用内存镜像判定', async () => {
    readFile.mockResolvedValue({
      content: JSON.stringify({ version: 1, allow: [{ tool: 'shell_exec', prefix: 'find' }] }),
    });
    await loadAllowlist();
    expect(evaluateAutoApproval('shell_exec', 'find D:\\x')).toBe('workspace');
    expect(evaluateAutoApproval('shell_exec', 'curl http://x')).toBe('deny');
  });
});

describe('回执与汇总文案', () => {
  it('回执:提交中/批准各档/拒绝/失败/超时', () => {
    expect(approvalReceiptText({ toolName: 'shell_exec', command: 'find x', decision: 'sending' })).toBe(
      'shell_exec · find x · 提交中…',
    );
    expect(
      approvalReceiptText({ toolName: 'shell_exec', command: 'find x', decision: 'sent', sentApproved: true, scope: 'session' }),
    ).toBe('shell_exec · find x · 已批准(本会话内允许)');
    expect(
      approvalReceiptText({ toolName: 'shell_exec', command: 'find x', decision: 'sent', sentApproved: true, scope: 'auto-workspace' }),
    ).toBe('shell_exec · find x · 已自动批准(本工作区始终允许)');
    expect(
      approvalReceiptText({ toolName: 'python_exec', command: 'p x', decision: 'sent', sentApproved: false }),
    ).toBe('python_exec · p x · 已拒绝');
    expect(
      approvalReceiptText({ toolName: 'shell_exec', command: 'find x', decision: 'error', errText: '提交失败:超时' }),
    ).toBe('shell_exec · find x · 提交失败:超时');
    expect(approvalReceiptText({ toolName: 'shell_exec', command: 'find x', decision: 'timeout' })).toBe(
      'shell_exec · find x · 已超时自动拒绝',
    );
  });

  it('回执:超长命令截断', () => {
    const long = 'a'.repeat(80);
    const text = approvalReceiptText({ toolName: 'shell_exec', command: long, decision: 'sent', sentApproved: true });
    expect(text).toContain('…');
    expect(text.length).toBeLessThan(100);
  });

  it('汇总卡摘要:按工具计数', () => {
    const items = [
      { toolName: 'python_exec' },
      { toolName: 'python_exec' },
      { toolName: 'python_exec' },
      { toolName: 'python_exec' },
      { toolName: 'shell_exec' },
    ];
    expect(aggregateSummary(items)).toBe('4×python_exec shell_exec');
    expect(aggregateSummary([])).toBe('');
  });

  it('聚合阈值常量>0', () => {
    expect(AGGREGATE_THRESHOLD).toBeGreaterThan(0);
  });
});
