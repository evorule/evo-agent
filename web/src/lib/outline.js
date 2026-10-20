// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// H3 大纲符号收集(monaco 公开 API 无 DocumentSymbolProviderRegistry 收集口,
// 故按语言直采):JSON = 本地词法扫描(键定位,纯函数 vitest 锁);JS/TS =
// ts worker getNavigationTree(monaco.languages.typescript 公开 worker 口,
// 与内建 OutlineAdapter 同源);其余语言返回 [](面板显「该语言无符号信息」)。

/** 统一大纲符号:{name, kind, depth, line, col}(line/col = 1-based 符号起点) */

/**
 * JSON 文本 → 大纲符号(对象键为节点;词法扫描,转义字符串感知;
 * 定位 = 键名引号起点。残缺 JSON 尽力返回已扫出的键,不抛错)。
 */
export function jsonSymbols(text) {
  const out = [];
  if (typeof text !== 'string' || !text) return out;
  const n = text.length;
  let i = 0;
  let line = 1;
  let col = 1;
  let objDepth = 0; // 当前未闭合 '{' 数(键缩进 = objDepth - 1)
  const adv = (ch) => {
    if (ch === '\n') {
      line += 1;
      col = 1;
    } else {
      col += 1;
    }
  };
  while (i < n) {
    const ch = text[i];
    if (ch === '"') {
      // 字符串(转义感知):先记键位起点,再吞到收尾引号
      const startLine = line;
      const startCol = col;
      let j = i + 1;
      while (j < n) {
        if (text[j] === '\\') {
          j += 2; // 转义对整体跳过(尾部悬挂反斜杠由 j>=n 兜底)
          continue;
        }
        if (text[j] === '"') break;
        j += 1;
      }
      if (j >= n) break; // 未闭合:截断
      const raw = text.slice(i, j + 1);
      for (let k = i; k <= j; k++) adv(text[k]);
      i = j + 1;
      // 其后第一个非空白字符为 ':' → 该字符串是对象键
      let k2 = i;
      while (k2 < n && (text[k2] === ' ' || text[k2] === '\t' || text[k2] === '\n' || text[k2] === '\r')) {
        adv(text[k2]);
        k2 += 1;
      }
      if (text[k2] === ':' && objDepth > 0) {
        let name = raw;
        try {
          name = JSON.parse(raw);
        } catch {
          /* 解码失败保留原文 */
        }
        out.push({ name: String(name), kind: 'key', depth: objDepth - 1, line: startLine, col: startCol });
        i = k2 + 1;
        adv(':');
      } else {
        i = k2;
      }
      continue;
    }
    if (ch === '{') objDepth += 1;
    else if (ch === '}') objDepth = Math.max(0, objDepth - 1);
    adv(ch);
    i += 1;
  }
  return out;
}

/**
 * ts worker getNavigationTree 结果 → 大纲符号(扁平化;positionAt 取符号起点;
 * 纯函数,vitest 用桩 positionAt 锁)。
 */
export function navTreeToSymbols(tree, positionAt) {
  const out = [];
  const walk = (items, depth) => {
    for (const it of items || []) {
      if (!it || it.kind === 'script' || it.kind === 'keyword') continue;
      const span = (it.spans || [])[0];
      if (!span) continue;
      const pos = positionAt(span.start);
      if (!pos) continue;
      const name = Array.isArray(it.text) ? it.text.join('') : String(it.text ?? '');
      if (!name) continue;
      out.push({ name, kind: String(it.kind || ''), depth, line: pos.lineNumber, col: pos.column });
      walk(it.childItems, depth + 1);
    }
  };
  if (tree?.childItems) walk(tree.childItems, 0);
  return out;
}

/** 大纲条目展示形制(kind 文本 → 字形 + 样式类) */
export function symbolGlyph(kind) {
  const t = String(kind || '');
  if (t === 'class') return { glyph: 'C', cls: 'class' };
  if (t === 'interface') return { glyph: 'I', cls: 'iface' };
  if (t === 'enum') return { glyph: 'E', cls: 'enum' };
  if (t === 'module') return { glyph: 'M', cls: 'module' };
  if (t === 'type' || t === 'type parameter') return { glyph: 'T', cls: 'type' };
  if (t.includes('function') || t === 'method') return { glyph: 'ƒ', cls: 'fn' };
  if (t === 'const' || t === 'let' || t === 'var' || t === 'local var') return { glyph: 'v', cls: 'var' };
  if (t === 'property' || t === 'getter' || t === 'setter') return { glyph: 'p', cls: 'prop' };
  if (t === 'key') return { glyph: '{}', cls: 'key' };
  return { glyph: '·', cls: 'other' };
}

/** 按语言收集大纲(monaco + model → Promise<符号[]>);不支持的语言返回 [] */
export async function collectOutline(monaco, model) {
  const lang = model.getLanguageId();
  if (lang === 'json') return jsonSymbols(model.getValue());
  if (lang === 'javascript' || lang === 'typescript') {
    const ts = monaco?.languages?.typescript;
    if (!ts) return [];
    try {
      const getWorker = lang === 'typescript' ? ts.getTypeScriptWorker() : ts.getJavaScriptWorker();
      const client = await (await getWorker)(model.uri);
      if (model.isDisposed()) return [];
      const tree = await client.getNavigationTree(model.uri.toString());
      if (model.isDisposed()) return [];
      return navTreeToSymbols(tree, (offset) => model.getPositionAt(offset));
    } catch {
      return []; // worker 未就绪/失败:按无符号呈现(面板空态兜底)
    }
  }
  return [];
}
