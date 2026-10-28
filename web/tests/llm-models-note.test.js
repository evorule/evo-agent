// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 入场券:执行体不在位的失败话术映射(用户报「模型选择器打不开」——raw 错误
// 不指引配置入口;映射后桌面版指向托盘「LLM 凭据…」,CLI 指向 executor_url)。
import { describe, it, expect } from 'vitest';
import { llmModelsNoteFromError } from '../src/lib/api.js';

describe('llmModelsNoteFromError', () => {
  it('执行体不可达(serve 中继 503 原文) → 配置指引', () => {
    const out = llmModelsNoteFromError(
      '模型清单中继失败: executor unreachable (http://127.0.0.1:9130/llm/models): error trying to connect'
    );
    expect(out).toContain('未检测到模型服务');
    expect(out).toContain('LLM 凭据');
    expect(out).toContain('executor_url');
  });

  it('执行体未就绪(拒切话术) → 同指引', () => {
    expect(llmModelsNoteFromError('扩展执行体未就绪,拒绝切换: no provider')).toContain(
      '未检测到模型服务'
    );
  });

  it('其他错误原样透传(不吞执行体/serve 自有诊断)', () => {
    const raw = 'executor responded 500: {"error":"boom"}';
    expect(llmModelsNoteFromError(raw)).toBe(raw);
  });

  it('空值容错返回空串', () => {
    expect(llmModelsNoteFromError('')).toBe('');
    expect(llmModelsNoteFromError(undefined)).toBe('');
  });
});
