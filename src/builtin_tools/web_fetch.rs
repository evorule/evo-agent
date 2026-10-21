// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `web_fetch` —— 网页抓取取读（网络面第二件；http_get 防线全量复用 + HTML→文本提取）
//!
//! ## 定位（批K 段2）
//!
//! `http_get` 返回原始 body，LLM 直接吞整页 HTML 既费 token 又引入脚本噪声。
//! `web_fetch` 补「抓下来→读得懂」这一段：
//! - **防线同源**：SSRF 分类 / ACTIVE_HOSTS 白名单 / redirect 落点复检 /
//!   net_guard 负面域红线 / 超时与 1MB 帽——全部复用 [`HttpGetTool`] 实例
//!   （组合而非复制，http_get 防线演进时 web_fetch 零漂移）；
//! - **审批同族**：active host 直取；candidate host 出 needs_approval 提案
//!   （管道⑤ evaluate_proposal 单源）；blocked/invalid 永拒；
//! - **HTML→文本**：手写零依赖提取器——script/style/注释剥离、块级标签
//!   换行边界、命名+数字实体解码、空白折叠；`<title>` 随行；
//! - **content-type 判别**：text/* 与常见文本型 application/* 走提取；
//!   二进制类型 fail-visible（`raw=true` 可强制取原始 body）；
//! - **大小帽**：提取文本默认 20000 字符、上限 50000（截断回报
//!   `truncated=true`）。
//!
//! ## 治理面
//!
//! manifest: Standard（免 P2——无 fs 写面，出站面由 exfil_scan 词形打标
//! 管辖）+ ManualDefault（candidate 族同 http_get）+ Network 域 + 开关
//! `agentTools.webFetch`（默认开）。

use serde_json::Value;

use crate::builtin_tools::http_get::{HostCategory, HttpGetTool};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 提取文本默认字符帽
pub const DEFAULT_MAX_CHARS: usize = 20_000;

/// 提取文本字符帽上限（参数钳制顶）
pub const MAX_CHARS_CAP: usize = 50_000;

/// `web_fetch` 工具（组合 [`HttpGetTool`]，防线零复制）
#[derive(Clone)]
pub struct WebFetchTool {
    fetcher: HttpGetTool,
}

impl WebFetchTool {
    /// TODO: doc
    pub fn new() -> Self {
        Self {
            fetcher: HttpGetTool::new(),
        }
    }

    /// 实际抓取 + 渲染（classify 门后调用；测试直驱入口，同 http_get 先例）
    async fn fetch_and_render(&self, url: &str, raw: bool, max_chars: usize) -> IoResult {
        let resp = self.fetcher.fetch(url).await?;
        let obj = resp
            .as_object()
            .ok_or_else(|| "web_fetch: fetch 返回非对象（内部契约破坏）".to_string())?;
        let body = obj
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let content_type = obj
            .get("content_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let mut map = serde_json::Map::new();
        map.insert("status".to_string(), Value::from("ok"));
        map.insert("url".to_string(), Value::from(url.to_string()));
        if let Some(v) = obj.get("final_url") {
            map.insert("final_url".to_string(), v.clone());
        }
        if let Some(v) = obj.get("http_status") {
            map.insert("http_status".to_string(), v.clone());
        }
        map.insert(
            "content_type".to_string(),
            Value::from(content_type.clone()),
        );
        if let Some(v) = obj.get("body_size") {
            map.insert("body_size".to_string(), v.clone());
        }

        if raw {
            map.insert("mode".to_string(), Value::from("raw"));
            map.insert("body".to_string(), Value::from(body));
            return Ok(Value::Object(map));
        }

        let base_ct = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let is_html = base_ct.contains("html");
        if !base_ct.starts_with("text/") && !is_textual_application_type(&base_ct) {
            return Err(format!(
                "unsupported content-type: '{}' (non-textual) — set raw=true to fetch the raw body",
                content_type
            ));
        }

        map.insert("mode".to_string(), Value::from("text"));
        let (title, text) = if is_html {
            (extract_title(&body), html_to_text(&body))
        } else {
            (None, body)
        };
        let (text, truncated) = truncate_chars(&text, max_chars.min(MAX_CHARS_CAP));
        if let Some(t) = title {
            map.insert("title".to_string(), Value::from(t));
        }
        map.insert("text".to_string(), Value::from(text.clone()));
        map.insert(
            "text_chars".to_string(),
            Value::from(text.chars().count() as i64),
        );
        map.insert("truncated".to_string(), Value::from(truncated));
        Ok(Value::Object(map))
    }

    /// 构造 proposal（web 语义版，六字段形态同 http_get）
    fn make_proposal(url: &str, host: &str) -> IoResult {
        let mut map = serde_json::Map::new();
        map.insert("status".to_string(), Value::from("needs_approval"));
        map.insert("url".to_string(), Value::from(url.to_string()));
        map.insert("host".to_string(), Value::from(host.to_string()));
        map.insert("category".to_string(), Value::from("candidate"));
        map.insert(
            "description".to_string(),
            Value::from("Web page fetch from a host not in the active allowlist"),
        );
        map.insert(
            "risk".to_string(),
            Value::from(
                "Sends the URL to an external server; page content enters the agent context (prompt-injection surface); potential SSRF if host resolves to a private IP",
            ),
        );
        map.insert(
            "alternative".to_string(),
            Value::from(
                "Use a host in the active list (docs.rs, crates.io, github.com); or download manually and use file_read",
            ),
        );
        map.insert(
            "instructions".to_string(),
            Value::from(
                "Ask the user. The request pauses for approval and executes once the user approves.",
            ),
        );
        Ok(Value::Object(map))
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

/// 文本型 application/* 判别（纯函数）：JSON/XML/YAML 等机器可读文本
fn is_textual_application_type(base_ct: &str) -> bool {
    matches!(
        base_ct,
        "application/json"
            | "application/ld+json"
            | "application/xml"
            | "application/javascript"
            | "application/x-javascript"
            | "application/ecmascript"
            | "application/x-yaml"
            | "application/yaml"
            | "application/toml"
            | "application/rss+xml"
            | "application/atom+xml"
    ) || base_ct.ends_with("+xml")
        || base_ct.ends_with("+json")
}

/// HTML → 纯文本（纯函数；零依赖扫描器）
///
/// 步骤：注释/`<script>`/`<style>`/`<title>` 剥离（title 正文由
/// [`extract_title`] 单独提取）→ 块级标签换行边界 → 非块标签
/// 剥壳 → 实体解码 → 空白折叠（行内多空白归一、空行丢弃）。
fn html_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0usize;
    while i < n {
        if bytes[i] == b'<' {
            // 注释剥离（HTML 注释定界符大小写固定）
            if lower[i..].starts_with("<!--") {
                match lower[i + 4..].find("-->") {
                    Some(off) => i += 4 + off + 3,
                    None => break,
                }
                continue;
            }
            // 标签名解析（ASCII 扫描，切点均在字符边界）
            let mut name_start = i + 1;
            let closing = name_start < n && bytes[name_start] == b'/';
            if closing {
                name_start += 1;
            }
            let mut name_end = name_start;
            while name_end < n && bytes[name_end].is_ascii_alphanumeric() {
                name_end += 1;
            }
            if name_end == name_start {
                // 非标签形态（如 "a < b"）：按文本放行
                out.push('<');
                i += 1;
                continue;
            }
            let name = &lower[name_start..name_end];
            let gt = match lower[i..].find('>') {
                Some(off) => i + off,
                None => break,
            };
            if (name == "script" || name == "style" || name == "title") && !closing {
                // 跳过整个元素内容：找对应闭合标签，吃掉到 '>' 为止
                // （title 正文单独经 extract_title 提取，不重复入 text）
                let close = format!("</{}", name);
                match lower[gt..].find(&close) {
                    Some(off) => {
                        i = gt + off + close.len();
                        while i < n && bytes[i] != b'>' {
                            i += 1;
                        }
                        if i < n {
                            i += 1;
                        }
                    }
                    None => break,
                }
                out.push('\n');
                continue;
            }
            i = gt + 1;
            if is_block_tag(name) {
                out.push('\n');
            }
            continue;
        }
        // 文本段：到下一个 '<' 为止
        let end = lower[i..].find('<').map(|off| i + off).unwrap_or(n);
        out.push_str(&decode_entities(&html[i..end]));
        i = end;
    }
    collapse_whitespace(&out)
}

/// 块级标签判别（出现即换行边界；纯函数）
fn is_block_tag(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "br"
            | "hr"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "li"
            | "tr"
            | "table"
            | "thead"
            | "tbody"
            | "ul"
            | "ol"
            | "dl"
            | "dt"
            | "dd"
            | "blockquote"
            | "pre"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "nav"
            | "aside"
            | "main"
            | "form"
            | "figure"
            | "figcaption"
            | "address"
    )
}

/// `<title>` 提取（大小写不敏感；空标题返回 None）
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let gt = lower[start..].find('>')? + start + 1;
    let end = lower[gt..].find("</title")? + gt;
    let t = html[gt..end].trim();
    if t.is_empty() {
        None
    } else {
        Some(decode_entities(t))
    }
}

/// 实体解码（纯函数）：命名表 + 十进制/十六进制数字形态；未知实体原样保留
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    const NAMED: &[(&str, &str)] = &[
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&#39;", "'"),
        ("&nbsp;", "\u{00A0}"),
    ];
    let b = s.as_bytes();
    let n = b.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < n {
        if b[i] == b'&' {
            let rest = &s[i..];
            if let Some((pat, rep)) = NAMED.iter().find(|(p, _)| rest.starts_with(p)) {
                out.push_str(rep);
                i += pat.len();
                continue;
            }
            if let Some((rep, consumed)) = decode_numeric_entity(rest) {
                out.push(rep);
                i += consumed;
                continue;
            }
            out.push('&');
            i += 1;
        } else {
            let ch_len = utf8_char_len(b[i]);
            let end = (i + ch_len).min(n);
            out.push_str(&s[i..end]);
            i = end;
        }
    }
    out
}

/// 数字实体解码：`&#123;` / `&#x1F600;` → 字符；非法/越界返回 None
/// （返回 (字符, 消耗字节数)；数字位数上限 7 防病态长串）
fn decode_numeric_entity(rest: &str) -> Option<(char, usize)> {
    let bytes = rest.as_bytes();
    let hex = rest.starts_with("&#x") || rest.starts_with("&#X");
    let digits_start = if hex { 3 } else { 2 };
    if bytes.len() <= digits_start {
        return None;
    }
    let mut end = digits_start;
    while end < bytes.len() && bytes[end] != b';' && end - digits_start < 7 {
        let c = bytes[end];
        let valid = if hex {
            c.is_ascii_hexdigit()
        } else {
            c.is_ascii_digit()
        };
        if !valid {
            return None;
        }
        end += 1;
    }
    if end >= bytes.len() || bytes[end] != b';' || end == digits_start {
        return None;
    }
    let digits = &rest[digits_start..end];
    let value = if hex {
        u32::from_str_radix(digits, 16).ok()?
    } else {
        digits.parse::<u32>().ok()?
    };
    let ch = char::from_u32(value)?;
    Some((ch, end + 1))
}

/// UTF-8 首字节 → 字符字节长（合法 UTF-8 输入下切点即字符边界）
fn utf8_char_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// 空白折叠：行内多空白归一为单空格、去行首尾空白、丢空行
fn collapse_whitespace(text: &str) -> String {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 字符数截断（按 chars 迭代，字符边界天然安全；返回 (文本, 是否截断)）
fn truncate_chars(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        // 第 max+1 个字符的起点即截断位（0 基计数恰 max 个字符保留）
        Some((idx, _)) => (text[..idx].to_string(), true),
        None => (text.to_string(), false),
    }
}

#[async_trait::async_trait]
impl ToolFunction for WebFetchTool {
    /// async 入口（fetcher 内部原生 async，无需 spawn_blocking）
    async fn call(&self, args: &Value) -> IoResult {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing required arg: url (string)".to_string())?;

        // 网络负面域守卫（红线）：先于 SSRF 分类与审批链，无审批通道
        super::net_guard::check_denied_network_target(url)?;

        let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
        let max_chars = match args.get("max_chars") {
            None | Some(Value::Null) => DEFAULT_MAX_CHARS,
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| "arg 'max_chars' must be a positive integer".to_string())?;
                n.clamp(1, MAX_CHARS_CAP as u64) as usize
            }
        };
        // 人工面直调旗标（同 http_get 防御双保险；agent 面正常路径走
        // needs_approval 提案，不触达该分支）
        let approved = args
            .get("approved")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        match self.fetcher.classify_for_agent(url) {
            HostCategory::Active => self.fetch_and_render(url, raw, max_chars).await,
            HostCategory::Candidate { host } => {
                if approved {
                    self.fetch_and_render(url, raw, max_chars).await
                } else {
                    Self::make_proposal(url, &host)
                }
            }
            HostCategory::Blocked { reason } => {
                Err(format!("host BLOCKED: {} (reason: {})", url, reason))
            }
            HostCategory::Invalid => Err(format!("invalid URL: '{}'", url)),
        }
    }

    /// 管道⑤评估单源（PR-4 收编）：candidate 分类钩子——复刻 call 的判定
    /// 次序（net_guard 红线 → classify），仅 Candidate 形态返回 proposal。
    /// 纯函数：无网络请求。分类用静态表（与 http_get evaluate_proposal 同
    /// 口径——agent 定义级 host 声明的 Active 升级属实例面，评估臂保守取
    /// 静态分类，多出的提案经 approved 旗标/人工面放行，不漏放）。
    fn evaluate_proposal(&self, args: &Value) -> Option<Value> {
        let url = args.get("url").and_then(Value::as_str)?;
        if super::net_guard::check_denied_network_target(url).is_err() {
            return None;
        }
        match HttpGetTool::classify(url) {
            HostCategory::Candidate { host } => {
                Some(Self::make_proposal(url, &host).expect("proposal construction is infallible"))
            }
            _ => None,
        }
    }
}

// =============================================================================
// 单元测试
// =============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use serde_json::json;

    // === HTML→文本提取（纯函数面） ===

    #[test]
    fn test_html_to_text_strips_script_style_comments() {
        let html = "<html><head><script>var x = 1 < 2; alert('hi');</script>\
                    <style>.a { color: red; }</style></head>\
                    <body><!-- hidden comment --><p>visible</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("visible"), "got: {text}");
        assert!(!text.contains("alert"), "got: {text}");
        assert!(!text.contains("color"), "got: {text}");
        assert!(!text.contains("hidden comment"), "got: {text}");
    }

    #[test]
    fn test_html_to_text_block_boundaries() {
        let html =
            "<div><h1>Title</h1><p>para one</p><p>para two</p><ul><li>a</li><li>b</li></ul></div>";
        let text = html_to_text(html);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec!["Title", "para one", "para two", "a", "b"]);
    }

    #[test]
    fn test_html_to_text_inline_tags_no_boundary() {
        let html = "<p>hello <b>bold</b> and <a href='/x'>link</a></p>";
        let text = html_to_text(html);
        assert_eq!(text, "hello bold and link");
    }

    #[test]
    fn test_html_to_text_entities() {
        let html = "<p>a &amp; b &lt;c&gt; &quot;q&quot; &#39;x&#39;&nbsp;end &#65;&#x42;</p>";
        let text = html_to_text(html);
        assert!(text.contains("a & b <c> \"q\" 'x'"), "got: {text}");
        assert!(text.contains("AB"), "numeric entities: {text}");
    }

    #[test]
    fn test_html_to_text_case_insensitive_tags() {
        let html = "<P>UPPER</P><SCRIPT>bad()</SCRIPT><Br/>next";
        let text = html_to_text(html);
        assert!(text.contains("UPPER"), "got: {text}");
        assert!(!text.contains("bad()"), "got: {text}");
        assert!(text.contains("next"), "got: {text}");
    }

    #[test]
    fn test_html_to_text_cjk_and_whitespace_collapse() {
        let html = "<p>中文   内容\t测试</p>\n<p>第二段</p>";
        let text = html_to_text(html);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec!["中文 内容 测试", "第二段"]);
    }

    #[test]
    fn test_html_to_text_unclosed_script_drops_rest() {
        // 未闭合 script：保守丢弃其后全部内容（防脚本文本泄入正文）
        let html = "<p>before</p><script>evil = '<p>after</p>'";
        let text = html_to_text(html);
        assert!(text.contains("before"), "got: {text}");
        assert!(!text.contains("after"), "got: {text}");
    }

    #[test]
    fn test_extract_title() {
        assert_eq!(
            extract_title("<html><TITLE>My Page</TITLE></html>").as_deref(),
            Some("My Page")
        );
        assert_eq!(
            extract_title("<title>a &amp; b</title>").as_deref(),
            Some("a & b")
        );
        assert_eq!(extract_title("<html><body>x</body></html>"), None);
        assert_eq!(extract_title("<title></title>"), None);
    }

    #[test]
    fn test_truncate_chars_respects_char_boundary() {
        // 多字节字符中间截断必须回退到字符边界
        let s = "中文内容测试".repeat(10); // 60 字符 × 3 字节
        let (t, truncated) = truncate_chars(&s, 7);
        assert!(truncated);
        assert_eq!(t.chars().count(), 7);
        assert!(s.starts_with(t.as_str()));
        // 未超帽不截断
        let (t2, truncated2) = truncate_chars("短文本", 100);
        assert!(!truncated2);
        assert_eq!(t2, "短文本");
    }

    #[test]
    fn test_decode_numeric_entity_rejects_malformed() {
        assert_eq!(decode_entities("5 & 6"), "5 & 6");
        assert_eq!(decode_entities("&#999999999;"), "&#999999999;"); // 位数超限
        assert_eq!(decode_entities("&#ZZ;"), "&#ZZ;"); // 非数字
        assert_eq!(decode_entities("&#65"), "&#65"); // 缺分号
        assert_eq!(decode_entities("&#x110000;"), "&#x110000;"); // 越界码位
    }

    // === 参数与分类门（classify 集成） ===

    #[tokio::test]
    async fn test_missing_url_arg() {
        let tool = WebFetchTool::new();
        let result = tool.call(&Value::Object(Default::default())).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing required arg"));
    }

    #[tokio::test]
    async fn test_candidate_returns_proposal() {
        let tool = WebFetchTool::new();
        let result = tool
            .call(&json!({"url": "https://stackoverflow.com/questions/123"}))
            .await;
        let v = result.expect("candidate host returns proposal");
        assert_eq!(v.get("status").unwrap().as_str().unwrap(), "needs_approval");
        assert_eq!(
            v.get("host").unwrap().as_str().unwrap(),
            "stackoverflow.com"
        );
    }

    #[tokio::test]
    async fn test_blocked_host_rejected_even_with_approval() {
        let tool = WebFetchTool::new();
        let result = tool
            .call(&json!({"url": "https://192.168.1.1/admin", "approved": true}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("BLOCKED"));
    }

    #[tokio::test]
    async fn test_invalid_url_rejected() {
        let tool = WebFetchTool::new();
        let result = tool.call(&json!({"url": "ftp://example.com/x"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid URL"));
    }

    #[tokio::test]
    async fn test_net_guard_redline_before_classification() {
        let tool = WebFetchTool::new();
        let result = tool.call(&json!({"url": "https://tbench.ai/docs"})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("denied by policy"), "got: {err}");
    }

    #[tokio::test]
    async fn test_evaluate_proposal_candidate_only() {
        let tool = WebFetchTool::new();
        assert!(tool
            .evaluate_proposal(&json!({"url": "https://example.com/x"}))
            .is_some());
        assert!(tool
            .evaluate_proposal(&json!({"url": "https://docs.rs/tokio"}))
            .is_none());
        assert!(tool
            .evaluate_proposal(&json!({"url": "https://127.0.0.1/x"}))
            .is_none());
        assert!(tool.evaluate_proposal(&json!({})).is_none());
    }

    #[tokio::test]
    async fn test_max_chars_clamped_to_cap() {
        // 60000 > 帽 50000：经渲染路径验证钳制（mockito 本地直驱渲染面）
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/big")
            .with_status(200)
            .with_header("content-type", "text/plain; charset=utf-8")
            .with_body("x".repeat(60_000))
            .create_async()
            .await;
        let tool = WebFetchTool::new();
        let v = tool
            .fetch_and_render(&format!("{}/big", server.url()), false, 60_000)
            .await
            .expect("render ok");
        assert_eq!(v["truncated"], json!(true));
        assert_eq!(v["text_chars"], json!(MAX_CHARS_CAP as i64));
    }

    // === 渲染面（mockito 直驱 fetch_and_render） ===

    #[tokio::test]
    async fn test_html_extraction_end_to_end() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/page")
            .with_status(200)
            .with_header("content-type", "text/html; charset=utf-8")
            .with_body(
                "<html><head><title>T</title></head><body><p>hello <b>web</b></p></body></html>",
            )
            .create_async()
            .await;
        let tool = WebFetchTool::new();
        let v = tool
            .fetch_and_render(&format!("{}/page", server.url()), false, DEFAULT_MAX_CHARS)
            .await
            .expect("render ok");
        assert_eq!(v["mode"], json!("text"));
        assert_eq!(v["title"], json!("T"));
        assert_eq!(v["text"], json!("hello web"));
        assert_eq!(v["truncated"], json!(false));
    }

    #[tokio::test]
    async fn test_plain_text_passthrough() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/txt")
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body("plain body")
            .create_async()
            .await;
        let tool = WebFetchTool::new();
        let v = tool
            .fetch_and_render(&format!("{}/txt", server.url()), false, DEFAULT_MAX_CHARS)
            .await
            .expect("render ok");
        assert_eq!(v["text"], json!("plain body"));
        assert!(v.get("title").is_none());
    }

    #[tokio::test]
    async fn test_binary_content_type_rejected_fail_visible() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/bin")
            .with_status(200)
            .with_header("content-type", "application/octet-stream")
            .with_body(&[0u8, 1, 2, 3][..])
            .create_async()
            .await;
        let tool = WebFetchTool::new();
        let err = tool
            .fetch_and_render(&format!("{}/bin", server.url()), false, DEFAULT_MAX_CHARS)
            .await
            .expect_err("binary content-type must be rejected");
        assert!(err.contains("unsupported content-type"), "got: {err}");
        assert!(err.contains("raw=true"), "got: {err}");
    }

    #[tokio::test]
    async fn test_json_content_type_textual() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"k": "v"}"#)
            .create_async()
            .await;
        let tool = WebFetchTool::new();
        let v = tool
            .fetch_and_render(&format!("{}/api", server.url()), false, DEFAULT_MAX_CHARS)
            .await
            .expect("json is textual");
        assert_eq!(v["text"], json!(r#"{"k": "v"}"#));
    }

    #[tokio::test]
    async fn test_raw_mode_returns_body_unrendered() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/raw")
            .with_status(200)
            .with_header("content-type", "application/octet-stream")
            .with_body("RAW-BYTES")
            .create_async()
            .await;
        let tool = WebFetchTool::new();
        let v = tool
            .fetch_and_render(&format!("{}/raw", server.url()), true, DEFAULT_MAX_CHARS)
            .await
            .expect("raw mode ok");
        assert_eq!(v["mode"], json!("raw"));
        assert_eq!(v["body"], json!("RAW-BYTES"));
    }
}
