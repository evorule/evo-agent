// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `web_search` —— 互联网搜索（网络面第三件；ai-plugin 中继，key 归边）
//!
//! ## 定位（批K 段2）
//!
//! agent 面互联网检索缺口收口。**不在 agent 侧持任何搜索凭据**：本工具
//! 只把查询 POST 到本机 ai-plugin 中继端点 `/tools/web_search`，由
//! ai-plugin 侧持 Brave Search API key 出站（与 SG-1 件1 LLM key 归边
//! 同纪律：evo-agent 进程零密钥）。
//!
//! ## 通路（与 LLM 执行体同款）
//!
//! - 中继 URL 派生（纯函数 [`derive_relay_url`]）：env
//!   `EVORULE_AI_PLUGIN_WEB_SEARCH_URL` 显式指定 > 由 `EVORULE_LLM_EXECUTOR_URL`
//!   剥 `/llm/execute` 后缀派生 > 缺省 `http://127.0.0.1:9130/tools/web_search`；
//! - 调用方令牌：env `EVORULE_LLM_EXECUTOR_TOKEN`（与 LLM 出站同面），
//!   Some = Authorization: Bearer 携带，ai-plugin 校验匹配即放行；
//! - **fail-visible 自诊断**：中继未配 key / 未启用 provider / 令牌不匹配 /
//!   上游失败——全部显式报错回喂，无静默兜底、无降级假结果。
//!
//! ## 治理面
//!
//! manifest: Standard（免 P2——查询词出站面由 exfil_scan 词形打标管辖，
//! 命中凭据形态走 A1/A2 锚）+ AutoPolicy（直执行，只读检索）+ Network 域
//! + 开关 `agentTools.webSearch`（默认开）。

use serde_json::{json, Value};

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 中继端点缺省（ai-plugin 默认监听 + 本批新路由）
pub const DEFAULT_RELAY_URL: &str = "http://127.0.0.1:9130/tools/web_search";

/// 结果条数缺省
pub const DEFAULT_COUNT: u64 = 5;

/// 结果条数帽
pub const MAX_COUNT: u64 = 10;

/// 请求超时（秒）
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;

/// 出站连接超时（秒）
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 5;

/// 中继 URL 派生（纯函数）：执行体 URL 剥 `/llm/execute` 后缀拼中继路由；
/// 无后缀/空值回落缺省（派生失败不猜测——自定义执行体形态不适用时用
/// 显式 env `EVORULE_AI_PLUGIN_WEB_SEARCH_URL` 指定）。
pub fn derive_relay_url(executor_url: Option<&str>) -> String {
    if let Some(u) = executor_url.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(base) = u.strip_suffix("/llm/execute") {
            let base = base.trim_end_matches('/');
            if !base.is_empty() {
                return format!("{base}/tools/web_search");
            }
        }
    }
    DEFAULT_RELAY_URL.to_string()
}

/// env 解析：显式中继 URL > 执行体派生 > 缺省
fn resolve_relay_url() -> String {
    if let Ok(v) = std::env::var("EVORULE_AI_PLUGIN_WEB_SEARCH_URL") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    derive_relay_url(std::env::var("EVORULE_LLM_EXECUTOR_URL").ok().as_deref())
}

/// env 解析：调用方令牌（与 LLM 出站同面；空白视为未设）
fn resolve_caller_token() -> Option<String> {
    std::env::var("EVORULE_LLM_EXECUTOR_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// `web_search` 工具
#[derive(Clone)]
pub struct WebSearchTool {
    relay_url: String,
    caller_token: Option<String>,
    timeout_secs: u64,
}

impl WebSearchTool {
    /// TODO: doc
    pub fn new() -> Self {
        Self {
            relay_url: resolve_relay_url(),
            caller_token: resolve_caller_token(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    /// 覆盖中继 URL（测试注入/显式装配）
    pub fn with_relay_url(mut self, url: &str) -> Self {
        self.relay_url = url.to_string();
        self
    }

    /// 覆盖调用方令牌（测试注入/显式装配）
    pub fn with_caller_token(mut self, token: Option<String>) -> Self {
        self.caller_token = token;
        self
    }

    /// TODO: doc
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// 解析参数（纯函数）：query 非空必填；count 钳制 [1, 10]
    fn parse_args(args: &Value) -> Result<(String, u64), String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "missing required arg: query (non-empty string)".to_string())?
            .to_string();
        let count = match args.get("count") {
            None | Some(Value::Null) => DEFAULT_COUNT,
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| "arg 'count' must be a positive integer".to_string())?;
                n.clamp(1, MAX_COUNT)
            }
        };
        Ok((query, count))
    }

    /// 实际执行（mockito 测试与 call 共用；URL/令牌/超时已定格）
    async fn search(&self, query: &str, count: u64) -> IoResult {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .connect_timeout(std::time::Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("web_search client build: {e}"))?;

        let mut req = client
            .post(&self.relay_url)
            .json(&json!({"query": query, "count": count}));
        if let Some(tok) = &self.caller_token {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("web_search relay request: {e}"))?;

        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();

        if status.as_u16() == 401 {
            return Err("web_search relay denied (401): caller token mismatch — \
                 evo-agent 侧 EVORULE_LLM_EXECUTOR_TOKEN 与 ai-plugin 侧 \
                 EVORULE_AI_PLUGIN_CALLER_TOKEN 不一致（或一侧未配置）"
                .to_string());
        }
        if !status.is_success() {
            // 优先透传中继自诊断 JSON（如未配 Brave key 的 500）
            if let Ok(v) = serde_json::from_str::<Value>(&body_text) {
                if let Some(err) = v.get("error").and_then(Value::as_str) {
                    if !err.is_empty() {
                        return Err(err.to_string());
                    }
                }
            }
            let snippet: String = body_text.chars().take(200).collect();
            return Err(format!(
                "web_search relay failed (HTTP {}): {snippet}",
                status.as_u16()
            ));
        }

        let v: Value = serde_json::from_str(&body_text)
            .map_err(|e| format!("web_search relay response JSON parse: {e}"))?;
        let provider = v
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let empty = Vec::new();
        let results = v.get("results").and_then(Value::as_array).unwrap_or(&empty);
        if !results
            .iter()
            .all(|r| r.get("url").and_then(Value::as_str).is_some())
        {
            return Err(
                "web_search relay response malformed: results missing url field".to_string(),
            );
        }

        let mut map = serde_json::Map::new();
        map.insert("status".to_string(), Value::from("ok"));
        map.insert("provider".to_string(), Value::from(provider));
        map.insert("query".to_string(), Value::from(query));
        map.insert(
            "result_count".to_string(),
            Value::from(results.len() as i64),
        );
        map.insert("results".to_string(), Value::Array(results.clone()));
        Ok(Value::Object(map))
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ToolFunction for WebSearchTool {
    async fn call(&self, args: &Value) -> IoResult {
        let (query, count) = Self::parse_args(args)?;
        self.search(&query, count).await
    }

    /// AutoPolicy 直执行工具：无 candidate 语义，恒 None（管道⑤不介入）
    fn evaluate_proposal(&self, _args: &Value) -> Option<Value> {
        None
    }
}

// =============================================================================
// 单元测试
// =============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // === URL 派生（纯函数面） ===

    #[test]
    fn test_derive_relay_url_from_executor() {
        assert_eq!(
            derive_relay_url(Some("http://127.0.0.1:9130/llm/execute")),
            "http://127.0.0.1:9130/tools/web_search"
        );
        // 带尾斜杠形态
        assert_eq!(
            derive_relay_url(Some("http://127.0.0.1:9130/llm/execute/")),
            "http://127.0.0.1:9130/tools/web_search"
        );
        // 自定义端口
        assert_eq!(
            derive_relay_url(Some("http://10.0.0.5:9000/llm/execute")),
            "http://10.0.0.5:9000/tools/web_search"
        );
    }

    #[test]
    fn test_derive_relay_url_fallbacks() {
        // 无后缀不猜测（自定义执行体形态不适用）
        assert_eq!(
            derive_relay_url(Some("http://other-host:9999/custom")),
            DEFAULT_RELAY_URL
        );
        assert_eq!(derive_relay_url(None), DEFAULT_RELAY_URL);
        assert_eq!(derive_relay_url(Some("  ")), DEFAULT_RELAY_URL);
    }

    // === 参数校验 ===

    #[tokio::test]
    async fn test_missing_query_arg() {
        let tool = WebSearchTool::new();
        let result = tool.call(&Value::Object(Default::default())).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing required arg"));
    }

    #[tokio::test]
    async fn test_blank_query_rejected() {
        let tool = WebSearchTool::new();
        let result = tool.call(&serde_json::json!({"query": "   "})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing required arg"));
    }

    #[tokio::test]
    async fn test_non_numeric_count_rejected() {
        let tool = WebSearchTool::new();
        let result = tool
            .call(&serde_json::json!({"query": "q", "count": "many"}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("count"));
    }

    #[test]
    fn test_parse_args_count_clamp() {
        let (q, c) =
            WebSearchTool::parse_args(&serde_json::json!({"query": " rust "})).expect("ok");
        assert_eq!(q, "rust");
        assert_eq!(c, DEFAULT_COUNT);
        let (_, c) =
            WebSearchTool::parse_args(&serde_json::json!({"query": "q", "count": 11})).expect("ok");
        assert_eq!(c, MAX_COUNT);
        let (_, c) =
            WebSearchTool::parse_args(&serde_json::json!({"query": "q", "count": 0})).expect("ok");
        assert_eq!(c, 1);
    }

    // === 中继回路（mockito） ===

    #[tokio::test]
    async fn test_happy_path_normalized_results() {
        let mut server = mockito::Server::new_async().await;
        let body = serde_json::json!({
            "provider": "brave",
            "query": "rust async",
            "count": 5,
            "results": [
                {"title": "tokio", "url": "https://tokio.rs", "description": "runtime"},
                {"title": "docs", "url": "https://docs.rs", "description": "docs"}
            ]
        });
        server
            .mock("POST", "/tools/web_search")
            .match_body(mockito::Matcher::Json(serde_json::json!(
                {"query": "rust async", "count": 5}
            )))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body.to_string())
            .create_async()
            .await;
        let tool =
            WebSearchTool::new().with_relay_url(&format!("{}/tools/web_search", server.url()));
        let v = tool
            .call(&serde_json::json!({"query": "rust async"}))
            .await
            .expect("relay ok");
        assert_eq!(v["status"], serde_json::json!("ok"));
        assert_eq!(v["provider"], serde_json::json!("brave"));
        assert_eq!(v["result_count"], serde_json::json!(2));
        assert_eq!(v["results"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_count_clamped_in_request_body() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/tools/web_search")
            .match_body(mockito::Matcher::Json(serde_json::json!(
                {"query": "q", "count": MAX_COUNT}
            )))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::json!({"provider": "brave", "results": []}).to_string())
            .create_async()
            .await;
        let tool =
            WebSearchTool::new().with_relay_url(&format!("{}/tools/web_search", server.url()));
        let v = tool
            .call(&serde_json::json!({"query": "q", "count": 99}))
            .await
            .expect("clamped request must match mock");
        assert_eq!(v["result_count"], serde_json::json!(0));
    }

    #[tokio::test]
    async fn test_caller_token_sent_as_bearer() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/tools/web_search")
            .match_header("authorization", "Bearer tok-123")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::json!({"provider": "brave", "results": []}).to_string())
            .create_async()
            .await;
        let tool = WebSearchTool::new()
            .with_relay_url(&format!("{}/tools/web_search", server.url()))
            .with_caller_token(Some("tok-123".to_string()));
        tool.call(&serde_json::json!({"query": "q"}))
            .await
            .expect("token matched");
    }

    #[tokio::test]
    async fn test_401_fail_visible_with_self_diagnosis() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/tools/web_search")
            .with_status(401)
            .with_body(r#"{"error": "caller token missing or mismatched"}"#)
            .create_async()
            .await;
        let tool =
            WebSearchTool::new().with_relay_url(&format!("{}/tools/web_search", server.url()));
        let err = tool
            .call(&serde_json::json!({"query": "q"}))
            .await
            .expect_err("401 must fail visible");
        assert!(err.contains("401"), "got: {err}");
        assert!(err.contains("EVORULE_LLM_EXECUTOR_TOKEN"), "got: {err}");
    }

    #[tokio::test]
    async fn test_500_diagnostic_passthrough() {
        // ai-plugin 未配 Brave key 的 fail-visible 自诊断 JSON 原文透传
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/tools/web_search")
            .with_status(500)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error": "web_search 未就绪: Brave API key 未配置（env EVORULE_AI_PLUGIN_BRAVE_API_KEY 或配置键 brave_api_key）"}"#)
            .create_async()
            .await;
        let tool =
            WebSearchTool::new().with_relay_url(&format!("{}/tools/web_search", server.url()));
        let err = tool
            .call(&serde_json::json!({"query": "q"}))
            .await
            .expect_err("diag must pass through");
        assert!(err.contains("Brave"), "got: {err}");
        assert!(
            !err.contains("HTTP 500"),
            "diag JSON wins over raw status: {err}"
        );
    }

    #[tokio::test]
    async fn test_non_json_error_body_falls_back_to_snippet() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/tools/web_search")
            .with_status(502)
            .with_body("upstream exploded")
            .create_async()
            .await;
        let tool =
            WebSearchTool::new().with_relay_url(&format!("{}/tools/web_search", server.url()));
        let err = tool
            .call(&serde_json::json!({"query": "q"}))
            .await
            .expect_err("must fail visible");
        assert!(err.contains("502"), "got: {err}");
        assert!(err.contains("upstream exploded"), "got: {err}");
    }

    #[tokio::test]
    async fn test_malformed_results_rejected() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/tools/web_search")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({"provider": "brave", "results": [{"title": "no url"}]})
                    .to_string(),
            )
            .create_async()
            .await;
        let tool =
            WebSearchTool::new().with_relay_url(&format!("{}/tools/web_search", server.url()));
        let err = tool
            .call(&serde_json::json!({"query": "q"}))
            .await
            .expect_err("malformed results must fail visible");
        assert!(err.contains("malformed"), "got: {err}");
    }

    #[tokio::test]
    async fn test_evaluate_proposal_always_none() {
        let tool = WebSearchTool::new();
        assert!(tool
            .evaluate_proposal(&serde_json::json!({"query": "q"}))
            .is_none());
    }
}
