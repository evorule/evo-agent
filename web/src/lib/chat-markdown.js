// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 聊天流 LLM 正文富文本渲染(单源出口):marked(GFM) + DOMPurify 消毒 + highlight.js 代码块。
// 安全红线:{@html} 仅允许经本模块消毒后的文本;script/iframe/事件属性/javascript: URI
// 由 DOMPurify 缺省白名单拦截,链接经钩子强制 target=_blank + rel=noopener。
// 代码块复制走注册表(id→原文)+气泡层事件委托(Svelte {@html} 内无法绑定事件)。
import { marked } from 'marked';
import DOMPurify from 'dompurify';
import hljs from 'highlight.js/lib/core';
import javascript from 'highlight.js/lib/languages/javascript';
import typescript from 'highlight.js/lib/languages/typescript';
import json from 'highlight.js/lib/languages/json';
import python from 'highlight.js/lib/languages/python';
import rust from 'highlight.js/lib/languages/rust';
import bash from 'highlight.js/lib/languages/bash';
import ini from 'highlight.js/lib/languages/ini';
import markdown from 'highlight.js/lib/languages/markdown';
import xml from 'highlight.js/lib/languages/xml';
import css from 'highlight.js/lib/languages/css';
import yaml from 'highlight.js/lib/languages/yaml';
import sql from 'highlight.js/lib/languages/sql';

hljs.registerLanguage('javascript', javascript);
hljs.registerLanguage('typescript', typescript);
hljs.registerLanguage('json', json);
hljs.registerLanguage('python', python);
hljs.registerLanguage('rust', rust);
hljs.registerLanguage('bash', bash);
hljs.registerLanguage('ini', ini);
// TOML 无独立文法,由 ini 承接(官方全量包同款路由)
hljs.registerAliases(['toml'], { languageName: 'ini' });
hljs.registerLanguage('markdown', markdown);
hljs.registerLanguage('xml', xml);
hljs.registerLanguage('css', css);
hljs.registerLanguage('yaml', yaml);
hljs.registerLanguage('sql', sql);

// ---- 代码块注册表:id→原文,容量淘汰防长会话无界增长 ----
const codeTexts = new Map();
let codeSeq = 0;
const CODE_CAP = 200;

// ---- 渲染结果记忆化:cacheKey→{text,html},流式 delta 变更即失效 ----
const htmlCache = new Map();
const HTML_CACHE_CAP = 300;

function escapeHtml(s) {
  return String(s)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

// 链接安全钩子:外链新窗打开并断开 opener
DOMPurify.addHook('afterSanitizeAttributes', (node) => {
  if (node.tagName === 'A' && node.getAttribute('href')) {
    node.setAttribute('target', '_blank');
    node.setAttribute('rel', 'noopener noreferrer');
  }
});

marked.use({
  gfm: true,
  breaks: true,
  renderer: {
    // marked 18:renderer 收 token 对象({type,raw,lang,text})
    code(token) {
      const raw = String(token.text ?? '');
      const lang = String(token.lang ?? '').trim().split(/\s+/)[0] || '';
      const body =
        lang && hljs.getLanguage(lang)
          ? hljs.highlight(raw, { language: lang }).value
          : escapeHtml(raw);
      const id = 'mdc' + ++codeSeq;
      codeTexts.set(id, raw);
      if (codeTexts.size > CODE_CAP) {
        codeTexts.delete(codeTexts.keys().next().value);
      }
      const label = escapeHtml(lang || 'text');
      return (
        `<div class="md-code"><div class="md-code-head">` +
        `<span class="md-code-lang">${label}</span>` +
        `<button type="button" class="md-copy" data-md-copy="${id}">复制</button>` +
        `</div><pre><code class="hljs">${body}</code></pre></div>`
      );
    },
  },
});

/**
 * LLM 正文渲染出口:marked 解析 → DOMPurify 消毒 → html 串。
 * @param {string} text 助手消息原文(流式中间态可含未闭合围栏,按代码块容错)
 * @param {string|number} [cacheKey] 消息 id;同 key 同 text 直接命中缓存
 * @returns {string} 可安全进入 {@html} 的 html
 */
export function renderChatHtml(text, cacheKey) {
  const src = String(text ?? '');
  if (cacheKey !== undefined) {
    const hit = htmlCache.get(cacheKey);
    if (hit && hit.text === src) {
      // 命中刷新次序(近似 LRU)
      htmlCache.delete(cacheKey);
      htmlCache.set(cacheKey, hit);
      return hit.html;
    }
  }
  const dirty = marked.parse(src, { async: false });
  const clean = DOMPurify.sanitize(dirty, { USE_PROFILES: { html: true } });
  if (cacheKey !== undefined) {
    if (htmlCache.size >= HTML_CACHE_CAP) {
      htmlCache.delete(htmlCache.keys().next().value);
    }
    htmlCache.set(cacheKey, { text: src, html: clean });
  }
  return clean;
}

/** 取注册表中代码块原文(测试与复制共用)。 */
export function chatCodeText(id) {
  return codeTexts.get(id);
}

/**
 * 复制代码块原文;成功时按钮文案瞬时反馈。
 * @returns {Promise<boolean>} 是否复制成功
 */
export function copyChatCode(id, btn) {
  const code = codeTexts.get(id);
  if (code === undefined) return Promise.resolve(false);
  const write =
    typeof navigator !== 'undefined' && navigator.clipboard?.writeText
      ? navigator.clipboard.writeText(code)
      : Promise.reject(new Error('clipboard unavailable'));
  return write
    .then(() => {
      if (btn) {
        const old = btn.textContent;
        btn.textContent = '已复制';
        btn.disabled = true;
        setTimeout(() => {
          btn.textContent = old;
          btn.disabled = false;
        }, 1200);
      }
      return true;
    })
    .catch(() => false);
}
