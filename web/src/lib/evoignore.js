// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// .evoignore 工作区排除文件解析(v1 gitignore 子集:注释/空行/目录前缀
// `dir/`/扩展名 `*.ext`/简式 glob `a/*/c`)+ 路径匹配。
// 命中文件内容零外发(拦在 prompt 组装层——治理接点,补全设计档 §四)。

const RULE = { NONE: 0, DIR: 1, GLOB: 2 };

let rules = [];
let loaded = false;

/** 简式 glob → 正则(`**` 跨段,`*` 单段,`?` 单字符;其余字面量转义) */
function globToRegex(pattern) {
  let re = '';
  for (let i = 0; i < pattern.length; i++) {
    const ch = pattern[i];
    if (ch === '*') {
      if (pattern[i + 1] === '*') {
        re += '.*';
        i++;
      } else {
        re += '[^/]*';
      }
    } else if (ch === '?') {
      re += '[^/]';
    } else {
      re += ch.replace(/[.+^${}()|[\]\\]/g, '\\$&');
    }
  }
  return new RegExp('^' + re + '$');
}

/** 解析 .evoignore 文本 → 规则集(即生效;refresh 外的调用方同样命中) */
export function parseEvoignore(text) {
  const out = [];
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith('#')) continue;
    if (line.endsWith('/')) {
      out.push({ kind: RULE.DIR, value: line.slice(0, -1) });
    } else if (/[*?]/.test(line)) {
      out.push({ kind: RULE.GLOB, re: globToRegex(line) });
    } else {
      out.push({ kind: RULE.GLOB, re: globToRegex(line) });
    }
  }
  rules = out;
  loaded = true;
  return out;
}

/** 路径匹配(成员路径/其任一父目录命中即排除) */
export function isIgnored(path) {
  if (!rules.length) return false;
  const segments = path.split('/');
  // 目录规则:路径中任一目录段等于规则名即排除(gitignore 语义——规则
  // config/ 命中任意层级下 config 目录及其内容);路径恰为目录本身同样命中
  for (const r of rules) {
    if (r.kind !== RULE.DIR) continue;
    if (path === r.value) return true;
    if (segments.slice(0, -1).includes(r.value)) return true;
  }
  const basename = segments[segments.length - 1];
  for (const r of rules) {
    if (r.kind === RULE.GLOB && (r.re.test(path) || r.re.test(basename))) return true;
  }
  return false;
}

/** 拉取并解析工作区根 .evoignore(404=空规则;失败静默降级) */
export async function refreshEvoignore() {
  try {
    const res = await fetch('/api/files/read?path=.evoignore');
    if (!res.ok) {
      rules = [];
      loaded = true;
      return;
    }
    const d = await res.json();
    rules = parseEvoignore(d.content || '');
    loaded = true;
  } catch {
    rules = [];
    loaded = true;
  }
}

export function evoignoreLoaded() {
  return loaded;
}

/** 测试辅助:直接注入规则 */
export function setRulesForTests(list) {
  rules = list;
}
