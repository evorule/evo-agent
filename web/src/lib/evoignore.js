// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// .evoignore/.gitignore 工作区排除文件解析(gitignore 子集:注释/空行/目录前缀
// `dir/`/扩展名 `*.ext`/简式 glob `a/*/c`/negation 例外 `!pattern`/
// 根锚定 前导 `/`)+ 路径匹配。匹配语义同 gitignore 标准:按序扫描,
// 最后一条命中的规则决定去留(negation 重新包含)。
// 命中文件内容零外发(拦在 prompt 组装层——治理接点,补全设计档 §四)。

const RULE = { NONE: 0, DIR: 1, GLOB: 2 };

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

/** 单行 → 规则(null=注释/空行)。
 *  规则形态:{kind, value?(DIR), re?(GLOB), negated, anchored}:
 *  - `!` 前缀 → negated(例外/重新包含)
 *  - 前导 `/` 或主体含 `/`(gitignore 标准:中间分隔符即锚定,尾 `/` 不算)
 *    → anchored(只匹配根下路径,剥掉前导 `/`)
 *  - 尾 `/` → DIR(目录规则);其余 → GLOB */
function parseLine(raw) {
  let line = raw.trim();
  if (!line || line.startsWith('#')) return null;
  let negated = false;
  if (line.startsWith('!')) {
    negated = true;
    line = line.slice(1).trim();
    if (!line) return null;
  }
  let hadLeadingSlash = false;
  if (line.startsWith('/')) {
    hadLeadingSlash = true;
    line = line.slice(1);
    if (!line) return null;
  }
  const isDir = line.endsWith('/');
  const body = isDir ? line.slice(0, -1) : line;
  if (!body) return null;
  const anchored = hadLeadingSlash || body.includes('/');
  if (isDir) {
    return { kind: RULE.DIR, value: body, negated, anchored };
  }
  return { kind: RULE.GLOB, re: globToRegex(body), negated, anchored };
}

/** 解析 ignore 文本 → 规则集(纯函数,不触模块状态;.gitignore 独立消费用) */
export function parseIgnoreLines(text) {
  const out = [];
  for (const raw of text.split(/\r?\n/)) {
    const r = parseLine(raw);
    if (r) out.push(r);
  }
  return out;
}

/** 单条规则是否命中路径(不含 negation 判定;ignored 决策在 matchIgnore) */
function ruleHits(r, path) {
  const segments = path.split('/');
  const basename = segments[segments.length - 1];
  if (r.kind === RULE.DIR) {
    if (r.anchored) {
      // 根锚定目录:路径即该目录或在其子树内
      return path === r.value || path.startsWith(r.value + '/');
    }
    // 非锚定目录:路径恰为该目录,或路径中任一父目录段命中(gitignore
    // 语义——规则 config/ 命中任意层级下 config 目录及其内容)
    if (path === r.value) return true;
    return segments.slice(0, -1).includes(r.value);
  }
  if (r.anchored) return r.re.test(path); // 根锚定 glob:只测全路径
  return r.re.test(path) || r.re.test(basename);
}

/** 路径匹配(成员路径/其任一父目录命中即排除;negation 例外后命中者胜出)。
 *  纯函数,规则集由调用方持有(.gitignore 独立状态) */
export function matchIgnore(rules, path) {
  let ignored = false;
  for (const r of rules) {
    if (ruleHits(r, path)) ignored = !r.negated; // 最后命中者胜出(gitignore 标准)
  }
  return ignored;
}

// ---- .evoignore 模块级单例(ai-completion/inlineChat 消费;行为不变) ----

let rules = [];
let loaded = false;

/** 解析 .evoignore 文本 → 规则集(即生效;refresh 外的调用方同样命中) */
export function parseEvoignore(text) {
  rules = parseIgnoreLines(text);
  loaded = true;
  return rules;
}

/** 路径匹配(委托 matchIgnore,单例规则集) */
export function isIgnored(path) {
  if (!rules.length) return false;
  return matchIgnore(rules, path);
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
