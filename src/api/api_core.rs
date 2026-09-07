// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 共享 HTTP 基建 —— 两 client（EvoruleApiClient / WorkspaceApiClient）组合持有的 ApiCore + 统一 ApiError。

use reqwest::{Client, RequestBuilder, Response};
use std::time::Duration;
use thiserror::Error;

/// 统一 API 错误（全仓唯一，由原 EvoruleApiError 演化而来）
#[derive(Error, Debug)]
pub enum ApiError {
    /// HTTP 传输层失败（连接/超时/解码等）。
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),
    /// server 返回非 2xx 状态。
    #[error("API returned error: {status} {message}")]
    ApiError {
        /// HTTP 状态码。
        status: u16,
        /// 错误详情（server error 字段或响应体）。
        message: String,
    },
    /// 响应体结构不符合预期契约。
    #[error("Invalid response format")]
    InvalidResponse,
    /// 目标会话不存在（404 语义化）。
    #[error("Session not found")]
    SessionNotFound,
    /// 指定版本号非法（如超出会话版本范围）。
    #[error("Invalid version: {0}")]
    InvalidVersion(String),
    /// 请求/响应序列化失败。
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}

/// 共享 HTTP 核心：base_url + reqwest Client + Bearer auth
#[derive(Debug, Clone)]
pub struct ApiCore {
    base_url: String,
    client: Client,
    /// Optional Bearer token for HTTP API auth.
    /// B5-server：`EVORULE_SERVICE_TOKEN` 优先，缺省回退 `EVORULE_AUTH_TOKEN`。
    auth_token: Option<String>,
}

impl ApiCore {
    /// Create new API core.
    /// B5-server 双 token 解析：`EVORULE_SERVICE_TOKEN`（service 身份，可写
    /// 受保护域 `stable.llm` / `stable.system`）优先；未设置时回退
    /// `EVORULE_AUTH_TOKEN`（user 身份，受保护域写入将被 server 以 403 拒绝，
    /// 调用侧走既有 best-effort warn 链路——行为退化可观测、不崩溃）。
    /// 均未设置时，发送无认证请求（server 须为 dev mode / no auth）。
    pub fn new(base_url: &str) -> Self {
        Self::with_auth_token(base_url, None)
    }

    /// 带显式鉴权 token 构造（配置文件 `evorum.api_key` 声明的接线入口）。
    ///
    /// token 优先级：**显式声明 > 环境变量 > 无**（与 server `--secret`
    /// 「显式传入 > 密钥文件 > 随机生成」的优先级语义对齐）：
    /// - `Some(非空)`：直接作为 Bearer token，覆盖环境变量；
    /// - `Some(空串)/None`：回落环境变量解析（`EVORULE_SERVICE_TOKEN` 优先，
    ///   缺省回退 `EVORULE_AUTH_TOKEN`），均未设置时发送无认证请求。
    pub fn with_auth_token(base_url: &str, explicit_token: Option<&str>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        let auth_token = resolve_effective_auth_token(
            explicit_token,
            std::env::var("EVORULE_SERVICE_TOKEN").ok(),
            std::env::var("EVORULE_AUTH_TOKEN").ok(),
        );

        Self {
            base_url: base_url.to_string(),
            client,
            auth_token,
        }
    }

    /// Full URL for a given path (e.g. "/api/sessions" → "{base_url}/api/sessions")
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Base URL getter (for tests / compatibility)
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// reqwest Client getter
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Attach auth header (if token set) to a request builder.
    pub(crate) fn auth_header(&self, req: RequestBuilder) -> RequestBuilder {
        match &self.auth_token {
            Some(token) => req.header("Authorization", format!("Bearer {}", token)),
            None => req,
        }
    }

    /// Check response status; return error on non-2xx.
    pub(crate) async fn check_response(&self, resp: &Response) -> Result<(), ApiError> {
        if !resp.status().is_success() {
            let status = resp.status().as_u16();

            if status == 404 {
                return Err(ApiError::SessionNotFound);
            }

            return Err(ApiError::ApiError {
                status,
                message: String::new(),
            });
        }

        Ok(())
    }

    /// Check response status with full error body extraction (UV-084 W2).
    ///
    /// 与 `check_response` 的差异（新方法专用，既有调用不迁移）：
    /// - 错误时读取响应 body，提取 server 统一错误格式 `{"error": "..."}`
    ///   作为 message 透出（旧实现 message 为空串——400 校验失败详情丢失，
    ///   LLM agent 无法自诊断修复，属静默吞错形态）；
    /// - 404 不再特判为 `SessionNotFound`（对 knowledge/ bundles 端点误导：
    ///   数据集未承载 ≠ 会话不存在），同样读 body 带 status 返回。
    ///
    /// 成功时原样返回 `Response` 供调用方继续 `resp.json()`（错误路径提前
    /// 返回，body 已耗尽不影响——调用方 `?` 后不会再读）。
    pub(crate) async fn check_response_full(&self, resp: Response) -> Result<Response, ApiError> {
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp
                .json::<serde_json::Value>()
                .await
                .unwrap_or(serde_json::Value::Null);
            let message = body
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or_default()
                .to_string();
            return Err(ApiError::ApiError { status, message });
        }
        Ok(resp)
    }
}

/// B5-server：双 token 解析——service 优先，缺省回退 user；均缺失/为空则 None。
///
/// 独立成纯函数以便单测（避免测试中改动进程级 env）。
fn resolve_auth_token(service: Option<String>, user: Option<String>) -> Option<String> {
    service
        .filter(|s| !s.is_empty())
        .or_else(|| user.filter(|s| !s.is_empty()))
}

/// Explicit declaration > env vars synthesis (config evorum.api_key wiring).
/// Pure function so unit tests never mutate process env.
fn resolve_effective_auth_token(
    explicit: Option<&str>,
    service: Option<String>,
    user: Option<String>,
) -> Option<String> {
    match explicit.map(str::trim).filter(|t| !t.is_empty()) {
        Some(token) => Some(token.to_string()),
        None => resolve_auth_token(service, user),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_resolve_auth_token_service_takes_precedence() {
        let token = resolve_auth_token(Some("svc".to_string()), Some("user".to_string()));
        assert_eq!(token.as_deref(), Some("svc"));
    }

    #[test]
    fn test_resolve_auth_token_fallback_and_empty_filter() {
        // service 缺失 → 回退 user
        let token = resolve_auth_token(None, Some("user".to_string()));
        assert_eq!(token.as_deref(), Some("user"));
        // service 为空串视为缺失 → 回退 user
        let token = resolve_auth_token(Some(String::new()), Some("user".to_string()));
        assert_eq!(token.as_deref(), Some("user"));
        // 均缺失 / 均为空 → None（dev mode，无 auth header）
        assert_eq!(resolve_auth_token(None, None), None);
        assert_eq!(
            resolve_auth_token(Some(String::new()), Some(String::new())),
            None
        );
    }

    #[test]
    fn test_explicit_token_overrides_env_resolution() {
        // Explicit declaration (config evorum.api_key) wins over env resolution:
        // assert the Bearer header injected by auth_header, no process env touched.
        let core = ApiCore::with_auth_token("http://x", Some("cfg-key"));
        let req = core
            .auth_header(reqwest::Client::new().get("http://x"))
            .build()
            .unwrap();
        assert_eq!(
            req.headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer cfg-key")
        );
    }

    #[test]
    fn test_effective_token_priority_semantics() {
        // Full priority table (pure function, no env dependency):
        // explicit non-empty > service > user > none; empty explicit falls back.
        assert_eq!(
            resolve_effective_auth_token(
                Some("cfg"),
                Some("svc".to_string()),
                Some("user".to_string()),
            )
            .as_deref(),
            Some("cfg")
        );
        assert_eq!(
            resolve_effective_auth_token(Some("  "), Some("svc".to_string()), None).as_deref(),
            Some("svc")
        );
        assert_eq!(
            resolve_effective_auth_token(None, None, Some("user".to_string())).as_deref(),
            Some("user")
        );
    }
}
