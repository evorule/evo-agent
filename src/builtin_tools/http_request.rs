// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `http_request` —— HTTP 写动词（POST/PUT/PATCH/DELETE；批K 段5，网络面第四件）
//!
//! ## 定位（全批最严治理面）
//!
//! `http_get` 覆盖读语义；写动词改远端状态、请求头/体天然是凭据出域载体，
//! 治理面全批最严——四层叠加，无豁免面：
//!
//! 1. **net_guard 负面域红线**（执行体第一步）：命中负面域即拒，先于
//!    SSRF 分类与审批链，无审批通道；
//! 2. **SSRF 黑名单**：组合继承 [`HttpGetTool`] 分类（localhost/私有段/
//!    云元数据永拒），G7 放宽旗标不暴露给本工具（写动词无放宽）；
//! 3. **exfil 横切**：入 [`crate::builtin_tools::exfil_scan::EXFIL_SCAN_TOOLS`]
//!    名单，凭据词形打标 + P2 事前裁决通道自动获得（URL/headers/body 全
//!    遍历；Authorization 等语义键经 SENSITIVE_KEYS 正本自动打标）；
//! 4. **双闸**：manifest `Sensitive`（意图必报）+ `ManualDefault`（每次
//!    调用出 needs_approval 提案，人工默认否决点）——**Active host 也不
//!    直跑**（与 http_get 的语义差：AutoPolicy 直跑是读面特权，写动词
//!    一律审批，审批卡=事前意图呈现载体）。
//!
//! ## 防线零复制（组合模式）
//!
//! [`HttpRequestTool`] 组合 [`HttpGetTool`] 实例：SSRF 分类 / host 三层
//! 模型 / net_guard / 超时——全部经组合继承（http_get 防线演进时本工具
//! 零漂移）。**唯一分叉 = redirect 策略**：`Policy::none()` 禁跟随，3xx
//! 响应原样返回（status + Location 头）——不引入 301/302 转 GET 的隐式
//! 语义；不跟随即无落点复检需求，http_get 的 redirect_guard 不触碰
//! （分叉面最小）。执行路径自建 client，不复用 fetcher 的 fetch。
//!
//! ## 参数与限制
//!
//! - `method`（必填）：白名单 `POST|PUT|PATCH|DELETE`，大小写不敏感归一；
//!   GET/HEAD 拒并引导走 http_get（职责不混）；
//! - `url`（必填）：https-only（明文 http 一律拒，allow_http 放宽旗标不
//!   暴露给本工具——写动词更严）；
//! - `headers`（可选 object）：值限 string；键不限，语义键自动打标；
//! - `body`（可选 string，帽 1MB）；`max_response_bytes`（可选，帽 1MB
//!   缺省，超帽钳制）；
//! - 校验 fail-visible 一次报全（method 非法时 headers/body 仍参与校验
//!   收集——一次把所有参数问题摊开，不让 LLM 挤牙膏式重试）。
//!
//! ## 返回形态
//!
//! `{status, headers, body, truncated}`：headers 只回筛选白名单
//! （content-type / content-length / location / retry-after / date /
//! server / etag / last-modified；**set-cookie 剔除**——会话固定防线，
//! 其余一律不回）；body 帽内原样（UTF-8 lossy，正文理解非本工具职责）；
//! 帽截断标 `truncated=true`（流式读，不信任 content-length——防谎报）。
//! 3xx 是合法 HTTP 事实，不在工具层报错，由 LLM 据此自行决策。

use std::time::Duration;

use serde_json::Value;

use crate::builtin_tools::http_get::{HostCategory, HttpGetTool};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 请求体字节帽
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// 响应字节帽（`max_response_bytes` 参数钳制顶与缺省值）
pub const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// 写动词白名单（GET/HEAD 走 http_get，职责不混）
const WRITE_METHODS: &[&str] = &["POST", "PUT", "PATCH", "DELETE"];

/// 响应头筛选白名单（set-cookie 不在集内——会话固定防线；其余一律不回）
const RESPONSE_HEADER_ALLOWLIST: &[&str] = &[
    "content-type",
    "content-length",
    "location",
    "retry-after",
    "date",
    "server",
    "etag",
    "last-modified",
];

/// 已校验请求（fail-visible 一次报全的产物；字段名/值合法性已验）
struct ValidatedRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    max_response_bytes: u64,
}

/// 参数校验（fail-visible 一次报全）：全部错误收集后单条 Err 返回；
/// method 非法时 headers/body 仍参与校验收集（一次摊开，不让 LLM 逐项试错）。
fn validate_args(args: &Value) -> Result<ValidatedRequest, String> {
    let mut errors: Vec<String> = Vec::new();

    // method：白名单归一；GET/HEAD 显式引导走 http_get
    let method = match args.get("method").and_then(Value::as_str) {
        None | Some("") => {
            errors.push(
                "missing or empty required arg: method (one of POST/PUT/PATCH/DELETE)".to_string(),
            );
            None
        }
        Some(raw) => {
            let upper = raw.trim().to_ascii_uppercase();
            if WRITE_METHODS.contains(&upper.as_str()) {
                Some(upper)
            } else if upper == "GET" || upper == "HEAD" {
                errors.push(format!(
                    "method '{}' not allowed: read verbs are served by http_get (this tool is write-only)",
                    raw
                ));
                None
            } else {
                errors.push(format!(
                    "invalid method '{}': must be one of POST/PUT/PATCH/DELETE",
                    raw
                ));
                None
            }
        }
    };

    // url
    let url = match args.get("url").and_then(Value::as_str) {
        None | Some("") => {
            errors.push("missing or empty required arg: url (string)".to_string());
            None
        }
        Some(u) => Some(u.trim().to_string()),
    };

    // headers（method 非法仍校验——fail-visible 一次报全）
    let mut headers: Vec<(String, String)> = Vec::new();
    match args.get("headers") {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) => {
            for (k, v) in map {
                match v.as_str() {
                    Some(s) if reqwest::header::HeaderName::from_bytes(k.as_bytes()).is_err() => {
                        errors.push(format!("headers key '{k}' is not a valid HTTP header name"));
                        let _ = s;
                    }
                    Some(s) if reqwest::header::HeaderValue::from_str(s).is_err() => {
                        errors.push(format!(
                            "headers['{k}'] value is not a valid HTTP header value"
                        ));
                    }
                    Some(s) => headers.push((k.clone(), s.to_string())),
                    None => errors.push(format!("headers['{k}'] must be a string value")),
                }
            }
        }
        Some(_) => errors.push("arg 'headers' must be an object with string values".to_string()),
    }

    // body
    let body = match args.get("body") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            if s.len() > MAX_BODY_BYTES {
                errors.push(format!(
                    "arg 'body' too large: {} bytes (max {})",
                    s.len(),
                    MAX_BODY_BYTES
                ));
                None
            } else {
                Some(s.clone())
            }
        }
        Some(_) => {
            errors.push("arg 'body' must be a string".to_string());
            None
        }
    };

    // max_response_bytes（帽 1MB 缺省；超帽钳制——帽是参数钳制顶非拒单）
    let max_response_bytes = match args.get("max_response_bytes") {
        None | Some(Value::Null) => MAX_RESPONSE_BYTES,
        Some(v) => match v.as_u64() {
            Some(0) => {
                errors.push("arg 'max_response_bytes' must be >= 1".to_string());
                MAX_RESPONSE_BYTES
            }
            Some(n) => n.min(MAX_RESPONSE_BYTES),
            None => {
                errors.push("arg 'max_response_bytes' must be a positive integer".to_string());
                MAX_RESPONSE_BYTES
            }
        },
    };

    if let (Some(method), Some(url)) = (method, url) {
        if errors.is_empty() {
            return Ok(ValidatedRequest {
                method,
                url,
                headers,
                body,
                max_response_bytes,
            });
        }
    }
    Err(format!(
        "http_request: {} invalid arg(s): {}",
        errors.len(),
        errors.join("; ")
    ))
}

/// 从 url 提取 host[:port]（proposal 展示面；Active host 的 host 名）
fn host_of(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or(rest)
        .to_string()
}

/// `http_request` 工具（组合 [`HttpGetTool`]，防线零复制）
#[derive(Clone)]
pub struct HttpRequestTool {
    /// 防线实例：仅复用分类纯函数（classify_for_agent——缺省策略下与
    /// 静态 classify 等价）；执行路径自建 client（Policy::none 分叉），
    /// 不复用 fetch 路径
    fetcher: HttpGetTool,
}

impl HttpRequestTool {
    /// TODO: doc
    pub fn new() -> Self {
        Self {
            fetcher: HttpGetTool::new(),
        }
    }

    /// 构造 proposal（写动词语义版，字段形态同 http_get族 + method）
    fn make_proposal(method: &str, url: &str, host: &str) -> IoResult {
        let mut map = serde_json::Map::new();
        map.insert("status".to_string(), Value::from("needs_approval"));
        map.insert("method".to_string(), Value::from(method.to_string()));
        map.insert("url".to_string(), Value::from(url.to_string()));
        map.insert("host".to_string(), Value::from(host.to_string()));
        map.insert("category".to_string(), Value::from("candidate"));
        map.insert(
            "description".to_string(),
            Value::from(format!(
                "HTTP write request ({method}) — every write-verb call requires user approval, including allowlisted hosts"
            )),
        );
        map.insert(
            "risk".to_string(),
            Value::from(
                "Modifies state on an external server; headers/body can carry credentials out of the host; potential SSRF if host resolves to a private IP",
            ),
        );
        map.insert(
            "alternative".to_string(),
            Value::from(
                "Use http_get for read-only fetches; prepare the request details and let the user run it manually",
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

    /// 实际执行（classify 门与审批后调用；redirect Policy::none——3xx 原样返回）
    async fn execute(&self, req: &ValidatedRequest) -> IoResult {
        // 自建 client：redirect 禁跟随（写动词分叉——不引入 301/302 转 GET
        // 的隐式语义；不跟随即无落点复检需求）
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(
                crate::builtin_tools::http_get::DEFAULT_TIMEOUT_SECS,
            ))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("evo-agent/0.1.0")
            .build()
            .map_err(|e| format!("client build: {}", e))?;

        let method = match req.method.as_str() {
            "POST" => reqwest::Method::POST,
            "PUT" => reqwest::Method::PUT,
            "PATCH" => reqwest::Method::PATCH,
            _ => reqwest::Method::DELETE,
        };
        let mut request = client.request(method, req.url.as_str());
        for (k, v) in &req.headers {
            request = request.header(k.as_str(), v.as_str());
        }
        if let Some(b) = &req.body {
            request = request.body(b.clone());
        }
        let mut resp = request
            .send()
            .await
            .map_err(|e| format!("request: {}", e))?;

        let status = resp.status().as_u16();

        // 响应头筛选（白名单；set-cookie 天然不在集——会话固定防线）。
        // 先收头再读体（借用序）
        let mut headers_out = serde_json::Map::new();
        for (name, value) in resp.headers() {
            let key = name.as_str();
            if RESPONSE_HEADER_ALLOWLIST.contains(&key) {
                if let Ok(v) = value.to_str() {
                    headers_out.insert(key.to_string(), Value::from(v.to_string()));
                }
            }
        }

        // 流式读体（帽+1 即停）：不信任 content-length（可缺/谎报），
        // 统一流式路径；截断标 truncated
        let cap = req.max_response_bytes as usize;
        let mut body_bytes: Vec<u8> = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("body: {}", e))? {
            if body_bytes.len() + chunk.len() > cap {
                let take = cap.saturating_sub(body_bytes.len());
                body_bytes.extend_from_slice(&chunk[..take]);
                truncated = true;
                break;
            }
            body_bytes.extend_from_slice(&chunk);
        }

        let mut map = serde_json::Map::new();
        map.insert("status".to_string(), Value::from(status as i64));
        map.insert("headers".to_string(), Value::Object(headers_out));
        map.insert(
            "body".to_string(),
            Value::from(String::from_utf8_lossy(&body_bytes).to_string()),
        );
        map.insert("truncated".to_string(), Value::from(truncated));
        Ok(Value::Object(map))
    }
}

impl Default for HttpRequestTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ToolFunction for HttpRequestTool {
    async fn call(&self, args: &Value) -> IoResult {
        // 1. 参数校验（fail-visible 一次报全）
        let req = validate_args(args)?;

        // 2. 网络负面域守卫（红线）：URL 命中负面域即拒绝，先于 SSRF 分类
        //    与审批链，无审批通道
        super::net_guard::check_denied_network_target(&req.url)?;

        // 3. host 分类（组合继承 http_get 三层模型）
        //    approved=true 仅来自审批批准重入（管道⑤→⑦ args 注入）或
        //    人工面直调；首调一律出 proposal（ManualDefault 双闸：
        //    Active host 也不直跑——意图必报是 Sensitive 语义的一部分）
        let approved = args
            .get("approved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match self.fetcher.classify_for_agent(&req.url) {
            HostCategory::Blocked { reason } => {
                Err(format!("host BLOCKED: {} (reason: {})", req.url, reason))
            }
            HostCategory::Invalid => Err(format!("invalid URL: '{}'", req.url)),
            category => {
                if approved {
                    return self.execute(&req).await;
                }
                let host = match &category {
                    HostCategory::Candidate { host } => host.clone(),
                    _ => host_of(&req.url),
                };
                Self::make_proposal(&req.method, &req.url, &host)
            }
        }
    }

    /// 管道⑤评估单源：复刻 call 判定次序（参数最小合法 → net_guard 红线
    /// → classify）。**Active/Candidate 均出 proposal**（ManualDefault
    /// 双闸：写动词意图必报，含 active 白名单域）；Blocked/Invalid →
    /// None（红线/分类拒面由 call 侧 fail-visible 报告）；参数面错误
    /// → None（call 侧一次报全）。
    fn evaluate_proposal(&self, args: &Value) -> Option<Value> {
        let url = args.get("url").and_then(Value::as_str)?;
        if super::net_guard::check_denied_network_target(url).is_err() {
            return None;
        }
        let method = args.get("method").and_then(Value::as_str)?;
        let upper = method.trim().to_ascii_uppercase();
        if !WRITE_METHODS.contains(&upper.as_str()) {
            return None;
        }
        match self.fetcher.classify_for_agent(url) {
            HostCategory::Candidate { host } => Some(
                Self::make_proposal(&upper, url, &host)
                    .expect("proposal construction is infallible"),
            ),
            HostCategory::Active => Some(
                Self::make_proposal(&upper, url, &host_of(url))
                    .expect("proposal construction is infallible"),
            ),
            _ => None,
        }
    }
}

// =============================================================================
// 单元测试
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn args_obj(pairs: &[(&str, Value)]) -> Value {
        let mut m = serde_json::Map::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v.clone());
        }
        Value::Object(m)
    }

    /// mockito 执行层直驱 helper：mockito 地址是 http://127.0.0.1 内网明文
    /// 形态，被 call 的 https-only 分类门拒（设计使然——执行层测试不触
    /// 分类门，分类/红线语义由下方非触网测试覆盖）；validate + execute
    /// 直驱（同 http_get 的 redirect 测试直驱 fetch 先例）
    async fn execute_via_mock(tool: &HttpRequestTool, pairs: &[(&str, Value)]) -> IoResult {
        let req = validate_args(&args_obj(pairs)).expect("mockito test args must validate");
        tool.execute(&req).await
    }

    // === 锁①method 矩阵：四动词实测 + 非法拒 + GET 引导 + 大小写归一 ===

    #[tokio::test]
    async fn test_write_methods_matrix() {
        let mut server = mockito::Server::new_async().await;
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            server
                .mock(method, "/resource")
                .with_status(200)
                .with_body("done")
                .create_async()
                .await;
        }
        let tool = HttpRequestTool::new();
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let v = execute_via_mock(
                &tool,
                &[
                    ("method", Value::from(method)),
                    ("url", Value::from(format!("{}/resource", server.url()))),
                ],
            )
            .await
            .unwrap_or_else(|e| panic!("{method} should execute: {e}"));
            assert_eq!(v.get("status").unwrap().as_i64().unwrap(), 200, "{method}");
            assert_eq!(v.get("body").unwrap().as_str().unwrap(), "done", "{method}");
            assert!(!v.get("truncated").unwrap().as_bool().unwrap());
        }
    }

    #[tokio::test]
    async fn test_method_case_normalized() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/case")
            .with_status(201)
            .create_async()
            .await;
        let tool = HttpRequestTool::new();
        let v = execute_via_mock(
            &tool,
            &[
                ("method", Value::from("post")),
                ("url", Value::from(format!("{}/case", server.url()))),
            ],
        )
        .await
        .expect("lowercase method must normalize to the mock's POST verb");
        assert_eq!(v.get("status").unwrap().as_i64().unwrap(), 201);
    }

    #[tokio::test]
    async fn test_get_and_invalid_methods_rejected() {
        let tool = HttpRequestTool::new();
        for method in ["GET", "HEAD", "TRACE", "CONNECT", "bogus"] {
            let result = tool
                .call(&args_obj(&[
                    ("method", Value::from(method)),
                    ("url", Value::from("https://example.com/x")),
                ]))
                .await;
            let err = result.expect_err("non-write method must be rejected");
            assert!(err.contains("invalid arg"), "{method}: {err}");
        }
        // GET 显式引导走 http_get
        let err = tool
            .call(&args_obj(&[
                ("method", Value::from("GET")),
                ("url", Value::from("https://example.com/x")),
            ]))
            .await
            .unwrap_err();
        assert!(
            err.contains("http_get"),
            "GET must guide to http_get: {err}"
        );
    }

    // === 锁②防线继承：红线/SSRF/https 降级/body 帽/响应帽 ===

    #[tokio::test]
    async fn test_net_guard_denies_write_url_even_with_approval() {
        let tool = HttpRequestTool::new();
        for approved in [false, true] {
            let result = tool
                .call(&args_obj(&[
                    ("method", Value::from("POST")),
                    ("url", Value::from("https://tbench.ai/docs")),
                    ("approved", Value::Bool(approved)),
                ]))
                .await;
            let err = result.expect_err("negative domain must be denied");
            assert!(err.contains("denied by policy"), "got: {err}");
        }
    }

    #[tokio::test]
    async fn test_ssrf_blocked_write_never_executes() {
        let tool = HttpRequestTool::new();
        for approved in [false, true] {
            let result = tool
                .call(&args_obj(&[
                    ("method", Value::from("POST")),
                    ("url", Value::from("https://127.0.0.1/admin")),
                    ("approved", Value::Bool(approved)),
                ]))
                .await;
            let err = result.expect_err("SSRF host must be blocked");
            assert!(err.contains("BLOCKED"), "got: {err}");
        }
    }

    #[tokio::test]
    async fn test_http_downgrade_rejected_for_write() {
        // allow_http 放宽旗标不暴露给本工具：明文 http 一律拒
        let tool = HttpRequestTool::new();
        let err = tool
            .call(&args_obj(&[
                ("method", Value::from("POST")),
                ("url", Value::from("http://example.com/x")),
            ]))
            .await
            .unwrap_err();
        assert!(err.contains("BLOCKED"), "got: {err}");
        assert!(err.contains("https"), "got: {err}");
    }

    #[tokio::test]
    async fn test_body_cap_rejected() {
        let tool = HttpRequestTool::new();
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let err = tool
            .call(&args_obj(&[
                ("method", Value::from("POST")),
                ("url", Value::from("https://example.com/x")),
                ("body", Value::from(big)),
            ]))
            .await
            .unwrap_err();
        assert!(err.contains("too large"), "got: {err}");
    }

    #[tokio::test]
    async fn test_response_cap_truncates_with_flag() {
        let mut server = mockito::Server::new_async().await;
        let big = "a".repeat(2 * 1024 * 1024); // 2MB > 1MB 帽
        server
            .mock("POST", "/big")
            .with_status(200)
            .with_body(big.as_str())
            .create_async()
            .await;
        let tool = HttpRequestTool::new();
        let v = execute_via_mock(
            &tool,
            &[
                ("method", Value::from("POST")),
                ("url", Value::from(format!("{}/big", server.url()))),
            ],
        )
        .await
        .expect("oversized response must truncate, not error");
        assert_eq!(v.get("body").unwrap().as_str().unwrap().len(), 1024 * 1024);
        assert!(v.get("truncated").unwrap().as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_validation_reports_all_errors_at_once() {
        // fail-visible 一次报全：method 非法时 headers/body 仍参与校验收集
        let tool = HttpRequestTool::new();
        let err = tool
            .call(&args_obj(&[
                ("method", Value::from("bogus")),
                ("url", Value::from("")),
                ("headers", serde_json::json!({"K": 1})),
                ("body", Value::from(42)),
                ("max_response_bytes", Value::from(0)),
            ]))
            .await
            .unwrap_err();
        assert!(err.contains("invalid method"), "got: {err}");
        assert!(err.contains("url"), "got: {err}");
        assert!(err.contains("headers"), "got: {err}");
        assert!(err.contains("body"), "got: {err}");
        assert!(err.contains("max_response_bytes"), "got: {err}");
        assert!(err.contains("5 invalid arg"), "one-shot report: {err}");
    }

    // === 锁③candidate 形态：一律出 proposal（Active 亦然）+ 打标面 ===

    #[tokio::test]
    async fn test_candidate_host_write_returns_proposal() {
        let tool = HttpRequestTool::new();
        let v = tool
            .call(&args_obj(&[
                ("method", Value::from("DELETE")),
                ("url", Value::from("https://example.com/resource/7")),
            ]))
            .await
            .expect("candidate write returns proposal, not error");
        assert_eq!(v.get("status").unwrap().as_str().unwrap(), "needs_approval");
        assert_eq!(v.get("method").unwrap().as_str().unwrap(), "DELETE");
        assert_eq!(
            v.get("url").unwrap().as_str().unwrap(),
            "https://example.com/resource/7"
        );
        assert_eq!(v.get("host").unwrap().as_str().unwrap(), "example.com");
        assert!(v.get("risk").is_some());
        assert!(v.get("instructions").is_some());
    }

    #[tokio::test]
    async fn test_active_host_write_also_requires_approval() {
        // 双闸语义锚：active 白名单域（docs.rs）写动词也不直跑——
        // 意图必报是 Sensitive 语义的一部分（不触网：proposal 先于执行）
        let tool = HttpRequestTool::new();
        let v = tool
            .call(&args_obj(&[
                ("method", Value::from("POST")),
                ("url", Value::from("https://docs.rs/crate/x")),
            ]))
            .await
            .expect("active host write still returns proposal");
        assert_eq!(v.get("status").unwrap().as_str().unwrap(), "needs_approval");
        assert_eq!(v.get("host").unwrap().as_str().unwrap(), "docs.rs");
    }

    #[test]
    fn test_exfil_scan_covers_http_request_and_flags_sensitive_header() {
        // exfil 横切：名单扩容后打标面自动工作（headers 语义键命中）
        assert!(crate::builtin_tools::exfil_scan::exfil_scan_applies(
            "http_request"
        ));
        let hits = crate::builtin_tools::exfil_scan::detect_exfil_hits(&serde_json::json!({
            "method": "POST",
            "url": "https://api.example.com/v1/x",
            "headers": {"Authorization": "Bearer sk-abc123"}
        }));
        assert!(
            hits.iter().any(|h| h == "sensitive_key:Authorization"),
            "Authorization header must be flagged: {hits:?}"
        );
        assert!(
            hits.iter().any(|h| h == "sk_prefix"),
            "sk- word shape: {hits:?}"
        );
        assert!(
            hits.iter().any(|h| h == "bearer_scheme"),
            "Bearer scheme: {hits:?}"
        );
    }

    // === 锁④redirect：3xx 原样返回不跟随 ===

    #[tokio::test]
    async fn test_3xx_returned_as_is_not_followed() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/jump")
            .with_status(302)
            .with_header("Location", "https://example.com/final")
            .with_body("")
            .create_async()
            .await;
        let tool = HttpRequestTool::new();
        let v = execute_via_mock(
            &tool,
            &[
                ("method", Value::from("POST")),
                ("url", Value::from(format!("{}/jump", server.url()))),
            ],
        )
        .await
        .expect("3xx is a legal HTTP fact, returned as-is");
        // status==302 即证明未跟随（跟随了终态会是 4xx/5xx 或连接错误）
        assert_eq!(v.get("status").unwrap().as_i64().unwrap(), 302);
        let headers = v.get("headers").unwrap().as_object().unwrap();
        assert_eq!(
            headers.get("location").unwrap().as_str().unwrap(),
            "https://example.com/final",
            "Location header must survive the allowlist filter"
        );
        assert!(!v.get("truncated").unwrap().as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_3xx_to_private_ip_also_returned_as_is() {
        // 不跟随=无落点复检需求（设计裁定）：落点内容不触发拒绝语义，
        // 3xx 一律原样返回，由 LLM 据此自行决策
        let mut server = mockito::Server::new_async().await;
        server
            .mock("PUT", "/redirect")
            .with_status(301)
            .with_header("Location", "https://127.0.0.1/secret")
            .create_async()
            .await;
        let tool = HttpRequestTool::new();
        let v = execute_via_mock(
            &tool,
            &[
                ("method", Value::from("PUT")),
                ("url", Value::from(format!("{}/redirect", server.url()))),
            ],
        )
        .await
        .expect("no follow = no target recheck");
        assert_eq!(v.get("status").unwrap().as_i64().unwrap(), 301);
    }

    // === 锁⑤evaluate_proposal 单源（管道⑤评估臂同形态） ===

    #[test]
    fn test_evaluate_proposal_covers_active_and_candidate() {
        let tool = HttpRequestTool::new();
        // candidate host → proposal
        let p = tool
            .evaluate_proposal(&args_obj(&[
                ("method", Value::from("post")),
                ("url", Value::from("https://example.com/x")),
            ]))
            .expect("candidate write must propose");
        assert_eq!(p.get("status").unwrap().as_str().unwrap(), "needs_approval");
        // method 大小写归一
        assert_eq!(p.get("method").unwrap().as_str().unwrap(), "POST");
        // active host 亦出 proposal（双闸：意图必报）
        let p = tool
            .evaluate_proposal(&args_obj(&[
                ("method", Value::from("DELETE")),
                ("url", Value::from("https://github.com/x")),
            ]))
            .expect("active host write must also propose");
        assert_eq!(p.get("status").unwrap().as_str().unwrap(), "needs_approval");
        // 红线域 → None（call 侧拒绝报告）
        assert!(tool
            .evaluate_proposal(&args_obj(&[
                ("method", Value::from("POST")),
                ("url", Value::from("https://tbench.ai/x")),
            ]))
            .is_none());
        // SSRF blocked → None
        assert!(tool
            .evaluate_proposal(&args_obj(&[
                ("method", Value::from("POST")),
                ("url", Value::from("https://192.168.1.1/x")),
            ]))
            .is_none());
        // 非法 method → None（参数面错误由 call 侧一次报全）
        assert!(tool
            .evaluate_proposal(&args_obj(&[
                ("method", Value::from("GET")),
                ("url", Value::from("https://example.com/x")),
            ]))
            .is_none());
    }

    #[tokio::test]
    async fn test_missing_method_or_url_fails_visible() {
        let tool = HttpRequestTool::new();
        let err = tool
            .call(&args_obj(&[("url", Value::from("https://example.com/x"))]))
            .await
            .unwrap_err();
        assert!(err.contains("method"), "got: {err}");
        let err = tool
            .call(&args_obj(&[("method", Value::from("POST"))]))
            .await
            .unwrap_err();
        assert!(err.contains("url"), "got: {err}");
    }

    #[test]
    fn test_host_of_extracts_host_port() {
        assert_eq!(host_of("https://docs.rs/crate/x"), "docs.rs");
        assert_eq!(
            host_of("https://api.example.com:8443/v1"),
            "api.example.com:8443"
        );
        assert_eq!(host_of("https://example.com"), "example.com");
    }
}
