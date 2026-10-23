// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// @vitest-environment jsdom
import { describe, it, expect } from 'vitest';
import { renderChatHtml, chatCodeText, copyChatCode } from '../src/lib/chat-markdown.js';

describe('renderChatHtml GFM 渲染', () => {
  it('标题/列表/表格/粗体/行内代码各得其所', () => {
    const html = renderChatHtml(
      '# 标题\n\n- 项一\n- 项二\n\n| 列 |\n|---|\n| 值 |\n\n**粗体** 与 `code`'
    );
    expect(html).toContain('<h1>');
    expect(html).toContain('<li>');
    expect(html).toContain('<table>');
    expect(html).toContain('<strong>');
    expect(html).toContain('<code>code</code>');
  });

  it('breaks:单换行转 <br>(聊天习惯)', () => {
    expect(renderChatHtml('行一\n行二')).toContain('<br');
  });
});

describe('renderChatHtml 消毒红线', () => {
  it('script/iframe/事件属性全部剥离', () => {
    const html = renderChatHtml(
      '前<script>alert(1)</script>后\n\n<iframe src="https://x"></iframe>\n\n<img src=x onerror="alert(1)">'
    );
    expect(html).not.toContain('<script');
    expect(html).not.toContain('<iframe');
    expect(html).not.toContain('onerror');
  });

  it('javascript: URI 链接被中和;普通链接强制新窗+noopener', () => {
    const evil = renderChatHtml('[x](javascript:alert(1))');
    expect(evil).not.toContain('javascript:');
    const ok = renderChatHtml('[y](https://example.com)');
    expect(ok).toContain('target="_blank"');
    expect(ok).toContain('rel="noopener noreferrer"');
  });
});

describe('renderChatHtml 代码块', () => {
  it('语言标签+hljs 高亮+注册表原文可取', () => {
    const html = renderChatHtml('```js\nconst a = 1\n```');
    expect(html).toContain('md-code');
    expect(html).toContain('md-code-lang">js<');
    expect(html).toContain('hljs-keyword');
    const id = html.match(/data-md-copy="(mdc\d+)"/)?.[1];
    expect(id).toBeTruthy();
    expect(chatCodeText(id)).toBe('const a = 1');
  });

  it('未知语言保留原标签,正文退纯文本转义(不注入标签)', () => {
    const html = renderChatHtml('```weirdlang\n<b>&x\n```');
    expect(html).toContain('md-code-lang">weirdlang<');
    expect(html).not.toContain('hljs-');
    expect(html).toContain('&lt;b&gt;&amp;x');
  });

  it('未闭合围栏(流式中间态)按代码块容错不抛错', () => {
    const html = renderChatHtml('```python\nx = 1');
    expect(html).toContain('<pre>');
    expect(html).toContain('md-code-lang">python<');
  });
});

describe('renderChatHtml 流式与缓存', () => {
  it('delta 追加逐帧产出合法 html,同 key 同 text 命中缓存', () => {
    const k = 'msg-1';
    const h1 = renderChatHtml('第一', k);
    const h2 = renderChatHtml('第一\n\n第二', k);
    expect(h1).toContain('第一');
    expect(h2).toContain('第二');
    expect(renderChatHtml('第一\n\n第二', k)).toBe(h2);
  });
});

describe('copyChatCode', () => {
  it('未注册 id 返回 false 不抛错', async () => {
    await expect(copyChatCode('mdc-nope')).resolves.toBe(false);
  });
});
