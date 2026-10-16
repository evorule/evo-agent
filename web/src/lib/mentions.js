// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// @上下文 mention 系统 — provider 注册表 + 组装核心(纯函数,无 Svelte/WS 依赖)。
// 双轨模型:composer 文本内 @token + mentions[] 元数组 {type, ref, insertAt}。
// 发送时 assembleContext 逐个 collect→serialize 拼成 <evo-context> XML 追加在
// 消息正文之后(WS 帧形状不变,扩展全部在 content 字符串内)。
//
// provider 三接口(按 type 注册,注册时校验齐全):
//   candidates(query, ctx)          → 候选 [{type, ref, label, kind?, detail?, warn?}](弹层)
//   collect(mention, ctx)           → Promise<collected|null> 抓取实体(失败→null,调用方降级)
//   serialize(mention, collected)   → {xml, summary} XML 块 + 回显摘要(供 Used References)
// file 与 folder 共用文件 provider(候选各自带 type,collect 按 mention.type 分支)。

/** 弹层分组顺序与文案(候选按此排序;空查询快捷项由 composer 层另行拼装) */
export const MENTION_GROUPS = [
  { type: 'file', label: '文件' },
  { type: 'folder', label: '文件夹' },
  { type: 'symbol', label: '符号' },
  { type: 'selection', label: '选区' },
  { type: 'terminal', label: '终端' },
  { type: 'problems', label: '问题' },
];

/** type → 中文分组文案(chip 托盘/回显用) */
export const MENTION_GROUP_LABELS = Object.fromEntries(
  MENTION_GROUPS.map((g) => [g.type, g.label]),
);

const providers = new Map();

/**
 * 注册/替换某类型的 provider。三接口缺一即拒绝注册(防半成品 provider
 * 在运行期才爆)。file 与 folder 可注册同一个 provider 实例。
 * @returns {boolean} 是否注册成功
 */
export function registerProvider(type, provider) {
  if (typeof type !== 'string' || !type) return false;
  if (!provider || typeof provider !== 'object') return false;
  if (
    typeof provider.candidates !== 'function' ||
    typeof provider.collect !== 'function' ||
    typeof provider.serialize !== 'function'
  ) {
    return false;
  }
  providers.set(type, provider);
  return true;
}

export function getProvider(type) {
  return providers.get(type) || null;
}

export function registeredTypes() {
  return [...providers.keys()];
}

/** 清空注册表(测试辅助;生产运行期勿用) */
export function resetProviders() {
  providers.clear();
}

/**
 * 组装 <evo-context> XML。顺序保持 mentions 数组顺序(与 chip 托盘一致)。
 * 单个 mention 失败不阻断整体:记入 dropped(供 Used References 回显「未带上」
 * 及 MentionSent 留痕);全部失败时 xml 为空串(正文即完整消息,不追加空壳)。
 * @param {Array<{type: string, ref: string, insertAt?: number}>} mentions
 * @param {object} ctx 传给 provider 的环境(文件索引/活动编辑器/终端实例/设置…)
 * @returns {Promise<{xml: string, used: Array<{type, ref, summary}>,
 *                     dropped: Array<{type, ref, reason}>}>}
 */
export async function assembleContext(mentions, ctx = {}) {
  const blocks = [];
  const used = [];
  const dropped = [];
  for (const m of Array.isArray(mentions) ? mentions : []) {
    if (!m || typeof m.type !== 'string' || typeof m.ref !== 'string') {
      dropped.push({ type: String(m?.type ?? '?'), ref: String(m?.ref ?? '?'), reason: 'malformed' });
      continue;
    }
    const p = providers.get(m.type);
    if (!p) {
      dropped.push({ type: m.type, ref: m.ref, reason: 'no_provider' });
      continue;
    }
    let collected = null;
    try {
      collected = await p.collect(m, ctx);
    } catch {
      collected = null;
    }
    if (!collected) {
      dropped.push({ type: m.type, ref: m.ref, reason: 'collect_failed' });
      continue;
    }
    let ser = null;
    try {
      ser = p.serialize(m, collected);
    } catch {
      ser = null;
    }
    if (!ser || typeof ser.xml !== 'string' || !ser.xml) {
      dropped.push({ type: m.type, ref: m.ref, reason: 'empty' });
      continue;
    }
    blocks.push(ser.xml);
    used.push({ type: m.type, ref: m.ref, summary: ser.summary || '' });
  }
  const xml =
    blocks.length > 0 ? `<evo-context>\n${blocks.join('\n')}\n</evo-context>` : '';
  return { xml, used, dropped };
}

/**
 * 汇总各 provider 候选并按 MENTION_GROUPS 顺序分组;全局截取 limit 条,
 * 超出置 truncated(弹层「继续输入缩小范围」提示)。同一 provider 实例多类型
 * 注册(file/folder 共用)只询问一次,防重复候选。
 * @returns {Promise<{groups: Array<{type, label, items}>, total: number, truncated: boolean}>}
 */
export async function collectCandidates(query, ctx = {}, limit = 50) {
  const byType = new Map();
  let total = 0;
  const seen = new Set();
  for (const type of providers.keys()) {
    const p = providers.get(type);
    if (!p || seen.has(p) || typeof p.candidates !== 'function') continue;
    seen.add(p);
    let items = [];
    try {
      items = await p.candidates(query, ctx);
    } catch {
      items = [];
    }
    for (const it of items) {
      if (!it || typeof it.type !== 'string' || typeof it.ref !== 'string') continue;
      if (!byType.has(it.type)) byType.set(it.type, []);
      byType.get(it.type).push(it);
      total++;
    }
  }
  const known = new Set(MENTION_GROUPS.map((g) => g.type));
  const order = [
    ...MENTION_GROUPS,
    ...[...byType.keys()].filter((t) => !known.has(t)).map((t) => ({ type: t, label: t })),
  ];
  let budget = limit;
  const groups = [];
  for (const g of order) {
    const items = byType.get(g.type);
    if (!items || items.length === 0 || budget <= 0) continue;
    const take = items.slice(0, budget);
    budget -= take.length;
    groups.push({ type: g.type, label: g.label, items: take });
  }
  return { groups, total, truncated: total > limit };
}

// ---- XML 组块工具(provider.serialize 共用) ----

/** XML 属性/文本转义(文件路径常含 & < > " 等字符) */
export function xmlEscape(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

/**
 * 组一个 XML 块。attrs 值经转义,null/undefined 属性整体省略;
 * body 为空产出自闭合标签,否则换行包体。返回完整块文本。
 */
export function xmlBlock(name, attrs, body) {
  const a = Object.entries(attrs || {})
    .filter(([, v]) => v !== undefined && v !== null)
    .map(([k, v]) => ` ${k}="${xmlEscape(v)}"`)
    .join('');
  const inner = body == null ? '' : String(body);
  return inner ? `<${name}${a}>\n${inner}\n</${name}>` : `<${name}${a}/>`;
}

// ---- token 估算(纯启发式:≈4 字符/token,供 composer 估算条分段着色) ----

/** 估算条分段阈值:safe < 8k ≤ warn ≤ 32k < over */
export const TOKEN_LIMITS = { safe: 8000, warn: 32000 };

export function estimateTokens(text) {
  if (typeof text !== 'string' || text.length === 0) return 0;
  return Math.ceil(text.length / 4);
}

/** 估算条分段着色:'safe' | 'warn' | 'over' */
export function tokenLevel(tokens) {
  if (tokens > TOKEN_LIMITS.warn) return 'over';
  if (tokens >= TOKEN_LIMITS.safe) return 'warn';
  return 'safe';
}

/**
 * 组装口径 token 估算:正文 + @文件引用已知权重(设计:组装后 chars/4)。
 * 文件权重=文件索引缓存的字节数(发送时全量抓取注入,占比绝对主导);
 * 选区/终端/问题等动态源发送时才抓取,v1 不计入(低估不虚报);
 * 文件夹引用按其展开注入同口径估算,v1 同样不计。
 */
export function estimateComposerTokens(draft, mentions = []) {
  let chars = typeof draft === 'string' ? draft.length : 0;
  for (const m of Array.isArray(mentions) ? mentions : []) {
    if (m && m.type === 'file' && Number.isFinite(m.size)) chars += m.size;
  }
  return Math.ceil(chars / 4);
}
