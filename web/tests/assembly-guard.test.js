// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 装配守卫:静态扫描源码,拦截两类装配期运行时错误——
// 「settingsState is not defined」「onDidPaste null」均中断 onMount
// 装配链,且不经过编译期报错,现有功能单测无法捕捉:
//   1. 源文件引用 settingsState 却缺对应 import(运行时 ReferenceError)
//   2. EditorPane 中编辑器实例先用后建(registerAiCompletion 订阅
//      editor.onDidPaste 等编辑器级事件,必须晚于 monaco.editor.create)
import { describe, it, expect } from 'vitest';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = fileURLToPath(new URL('.', import.meta.url)); // tests/
const SRC = join(HERE, '..', 'src');

/** 递归收集 src 下 .svelte/.js 源文件 */
function collectFiles(dir) {
  const out = [];
  for (const name of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, name.name);
    if (name.isDirectory()) out.push(...collectFiles(p));
    else if (/\.(svelte|js)$/.test(name.name)) out.push(p);
  }
  return out;
}

/** 剔除 // 注释行后,文件是否真实引用标识符(排除 import 语句本身与 $ 自动订阅形式由调用方定) */
function codeLines(text) {
  return text
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith('//') && !l.startsWith('*') && !l.startsWith('/*'));
}

describe('settingsState 导入守卫', () => {
  it('所有引用 settingsState 的源文件都有对应 import 语句', () => {
    const offenders = [];
    for (const f of collectFiles(SRC)) {
      const norm = f.replace(/\\/g, '/');
      if (norm.endsWith('/lib/settings.js')) continue; // 定义处豁免
      const text = readFileSync(f, 'utf8');
      const refs = codeLines(text).filter((l) => l.includes('settingsState'));
      if (refs.length === 0) continue;
      const hasImport = text.match(/import\s*\{[^}]*\bsettingsState\b[^}]*\}\s*from/);
      if (!hasImport) offenders.push(norm.replace(/.*\/src\//, 'src/'));
    }
    expect(offenders).toEqual([]);
  });
});

describe('编辑器实例装配顺序守卫', () => {
  it('EditorPane: registerAiCompletion 必须晚于 monaco.editor.create', () => {
    const text = readFileSync(join(SRC, 'components', 'EditorPane.svelte'), 'utf8');
    const createAt = text.indexOf('monaco.editor.create(');
    const registerAt = text.indexOf('registerAiCompletion(editor');
    expect(createAt).toBeGreaterThan(-1); // 创建点存在
    expect(registerAt).toBeGreaterThan(createAt); // 订阅编辑器级事件前必须已建实例
  });
});
