// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// launch 配置读取:.evo/debug.json 自有形态,兼容 .vscode/launch.json 同名字段。
//
// 读取序(设计档 §三.1 裁决):.evo/debug.json 优先;不存在时回落
// .vscode/launch.json(launch.json 允许 JSONC 注释——剥离后再解析)。

import { readFile } from '../api.js';
import { normalizeLaunchConfigs } from './session.js';

/** 剥离 JSONC 注释(// 与 /* *​/ 两种;字符串字面量内不误伤) */
export function stripJsonComments(text) {
  let out = '';
  let inStr = false;
  let inLine = false;
  let inBlock = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    const n = text[i + 1];
    if (inLine) {
      if (c === '\n') {
        inLine = false;
        out += c;
      }
      continue;
    }
    if (inBlock) {
      if (c === '*' && n === '/') {
        inBlock = false;
        i++;
      }
      continue;
    }
    if (inStr) {
      out += c;
      if (c === '\\' && n) {
        out += n;
        i++;
      } else if (c === '"') inStr = false;
      continue;
    }
    if (c === '"') {
      inStr = true;
      out += c;
      continue;
    }
    if (c === '/' && n === '/') {
      inLine = true;
      i++;
      continue;
    }
    if (c === '/' && n === '*') {
      inBlock = true;
      i++;
      continue;
    }
    out += c;
  }
  return out;
}

/** 解析配置文本(容错:JSONC 注释/尾逗号) */
export function parseLaunchText(text) {
  if (!text || !text.trim()) return [];
  const cleaned = stripJsonComments(text)
    .replace(/,(\s*[}\]])/g, '$1'); // 尾逗号
  try {
    return normalizeLaunchConfigs(JSON.parse(cleaned));
  } catch {
    return [];
  }
}

/**
 * 读取 launch 配置(.evo/debug.json 优先,.vscode/launch.json 回落)。
 * 返回 { configs, source } ;两处皆无 → { configs: [], source: null }。
 */
export async function loadLaunchConfigs() {
  try {
    const d = await readFile('.evo/debug.json');
    return { configs: parseLaunchText(d?.content || ''), source: '.evo/debug.json' };
  } catch {
    /* 无 debug.json:回落 */
  }
  try {
    const d = await readFile('.vscode/launch.json');
    return { configs: parseLaunchText(d?.content || ''), source: '.vscode/launch.json' };
  } catch {
    return { configs: [], source: null };
  }
}
