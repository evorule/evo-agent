// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// transcript 投影单测:历史回灌三缺陷的契约锁定——
// ① assistant 记录带 tool_calls 时叙述文字不得丢弃(文本气泡在前);
// ② 调用卡无结果时 result 必须为 undefined(不得渲染字面量 "null");
// ③ 调用卡与随后的 tool 结果记录按「工具名+顺序」配对合并为一卡。
// 另含 file_write 基线 diff 载荷与 wire 层截断元数据的纯函数契约。
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';

vi.mock('../src/lib/api.js', () => ({
  listSessions: vi.fn(async () => []),
  listDir: vi.fn(async () => ({ entries: [] })),
  readFile: vi.fn(async () => ({ content: '' })),
  writeFile: vi.fn(async () => ({})),
  getAgentDef: vi.fn(async () => ({})),
  getEvolutionSignals: vi.fn(async () => []),
  getWorkbenchSettings: vi.fn(async () => ({})),
  getWorkbenchSettingsSchema: vi.fn(async () => ({ entries: [] })),
  putWorkbenchSetting: vi.fn(async () => ({})),
}));
vi.mock('../src/lib/orchestration.js', () => ({
  refreshOrchestration: vi.fn(),
  orchestrationToolCall: vi.fn(),
  orchestrationToolResult: vi.fn(),
}));
vi.mock('../src/lib/audit.js', () => ({
  initAuditWatcher: vi.fn(),
}));

const { recordToMessage, loadTranscriptInto, messages } = await import(
  '../src/lib/stores.js'
);
const { toolDiffPayload } = await import('../src/lib/chat-diff.js');
const { cappedJson } = await import('../src/lib/ws.js');

beforeEach(() => {
  messages.set([]);
});

describe('recordToMessage 叙述文字保留', () => {
  it('assistant 带 tool_calls 且 content 非空 → 文本气泡在前、工具卡在后', () => {
    const out = recordToMessage({
      role: 'assistant',
      content: '我先读一下配置文件。',
      tool_calls: [{ tool_name: 'file_read', args: { path: 'a.md' } }],
    });
    expect(Array.isArray(out)).toBe(true);
    expect(out).toHaveLength(2);
    expect(out[0]).toMatchObject({ kind: 'assistant', text: '我先读一下配置文件。' });
    expect(out[1]).toMatchObject({ kind: 'tool', name: 'file_read' });
  });

  it('思考留痕(reasoning)挂到气泡 thinking 字段——历史思考卡数据源', () => {
    const out = recordToMessage({
      role: 'assistant',
      content: '结论。',
      reasoning: '先推理一下……',
    });
    expect(out).toMatchObject({ kind: 'assistant', text: '结论。', thinking: '先推理一下……' });
    const withCards = recordToMessage({
      role: 'assistant',
      content: '先读再答。',
      reasoning: '思考中……',
      tool_calls: [{ tool_name: 'file_read', args: { path: 'a' } }],
    });
    expect(withCards[0].thinking).toBe('思考中……');
    // 无思考 → thinking 不设键
    const plain = recordToMessage({ role: 'assistant', content: '无思考。' });
    expect(plain.thinking).toBeUndefined();
  });

  it('assistant 带 tool_calls 且 content 为空 → 仅工具卡(现状保持)', () => {
    const out = recordToMessage({
      role: 'assistant',
      content: '',
      tool_calls: [{ tool_name: 'file_read', args: { path: 'a.md' } }],
    });
    expect(Array.isArray(out)).toBe(true);
    expect(out).toHaveLength(1);
    expect(out[0].kind).toBe('tool');
  });

  it('调用卡无结果时 result 为 undefined(不是 null——防渲染字面量 "null")', () => {
    const out = recordToMessage({
      role: 'assistant',
      content: '',
      tool_calls: [{ tool_name: 'file_read', args: { path: 'a.md' } }],
    });
    expect(out[0].result).toBeUndefined();
    expect(out[0].result).not.toBeNull();
  });
});

describe('loadTranscriptInto 调用/结果配对合并', () => {
  it('同轮调用+结果配对合并为一卡(args+result 两段齐备)', () => {
    loadTranscriptInto([
      { role: 'user', content: '帮我整理文件' },
      {
        role: 'assistant',
        content: '先读文件。',
        tool_calls: [
          { tool_name: 'file_read', args: { path: 'a.md' } },
          { tool_name: 'shell_exec', args: { command: 'ls' } },
        ],
      },
      { role: 'tool', tool_name: 'file_read', content: '文件内容……' },
      { role: 'tool', tool_name: 'shell_exec', content: 'a.md\nb.md' },
      { role: 'assistant', content: '整理完成。' },
    ]);
    const arr = get(messages);
    const cards = arr.filter((m) => m.kind === 'tool');
    expect(cards).toHaveLength(2);
    const read = cards.find((c) => c.name === 'file_read');
    const shell = cards.find((c) => c.name === 'shell_exec');
    expect(read).toBeDefined();
    expect(shell).toBeDefined();
    expect(read.args).toEqual({ path: 'a.md' });
    expect(read.result).toBe('文件内容……');
    expect(shell.args).toEqual({ command: 'ls' });
    expect(shell.result).toBe('a.md\nb.md');
    // 时序:用户 → 叙述气泡 → 两张合并卡 → 结尾气泡
    expect(arr.map((m) => m.kind)).toEqual(['user', 'assistant', 'tool', 'tool', 'assistant']);
  });

  it('叙述文字夹在调用与结果之间不破坏配对', () => {
    loadTranscriptInto([
      { role: 'user', content: 'go' },
      { role: 'assistant', content: '读一下', tool_calls: [{ tool_name: 'file_read', args: { path: 'x' } }] },
      { role: 'assistant', content: '读到了,继续分析。' },
      { role: 'tool', tool_name: 'file_read', content: '正文' },
    ]);
    const arr = get(messages);
    const cards = arr.filter((m) => m.kind === 'tool');
    expect(cards).toHaveLength(1);
    expect(cards[0].args).toEqual({ path: 'x' });
    expect(cards[0].result).toBe('正文');
  });

  it('无前置调用的孤立结果卡保持独立(降级不丢)', () => {
    loadTranscriptInto([{ role: 'tool', tool_name: 'file_read', content: '孤儿结果' }]);
    const arr = get(messages);
    expect(arr).toHaveLength(1);
    expect(arr[0]).toMatchObject({ kind: 'tool', name: 'file_read', result: '孤儿结果' });
  });

  it('无后续结果的调用卡保持独立且 result 为 undefined', () => {
    loadTranscriptInto([
      { role: 'assistant', content: '', tool_calls: [{ tool_name: 'file_read', args: { path: 'y' } }] },
    ]);
    const arr = get(messages);
    expect(arr).toHaveLength(1);
    expect(arr[0].args).toEqual({ path: 'y' });
    expect(arr[0].result).toBeUndefined();
  });
});

describe('toolDiffPayload file_write 基线 diff', () => {
  it('file_write 有草稿基线 → {oldText: 基线, newText: 写入内容}', () => {
    const p = toolDiffPayload(
      { name: 'file_write', args: JSON.stringify({ path: 'a.md', content: '新全文' }) },
      '旧全文'
    );
    expect(p).toEqual({ oldText: '旧全文', newText: '新全文' });
  });

  it('file_write 无基线 → null(降级,内容经动作段展示)', () => {
    const p = toolDiffPayload(
      { name: 'file_write', args: { path: 'a.md', content: '新全文' } },
      null
    );
    expect(p).toBeNull();
  });

  it('edit_file 维持现状(baseline 参数不影响)', () => {
    const p = toolDiffPayload(
      {
        name: 'edit_file',
        args: { path: 'a.md', old_string: '旧', new_string: '新' },
      },
      '被忽略'
    );
    expect(p).toEqual({ oldText: '旧', newText: '新' });
  });

  it('非编辑类工具 → null', () => {
    expect(toolDiffPayload({ name: 'file_read', args: { path: 'a' } }, 'x')).toBeNull();
  });
});

describe('cappedJson wire 截断元数据', () => {
  it('未超帽 → 原文,truncated=false', () => {
    const t = cappedJson({ a: 1 }, 400);
    expect(t).toEqual({ text: '{"a":1}', truncated: false, originalLen: 7 });
  });

  it('超帽 → 截断加省略号,truncated=true 携带原始长度', () => {
    const big = 'x'.repeat(5000);
    const t = cappedJson({ big }, 100);
    expect(t.truncated).toBe(true);
    expect(t.originalLen).toBe(5010);
    expect(t.text.length).toBe(101);
    expect(t.text.endsWith('…')).toBe(true);
  });
});
