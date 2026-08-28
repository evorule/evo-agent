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
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),
    #[error("API returned error: {status} {message}")]
    ApiError { status: u16, message: String },
    #[error("Invalid response format")]
    InvalidResponse,
    #[error("Session not found")]
    SessionNotFound,
    #[error("Invalid version: {0}")]
    InvalidVersion(String),
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
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        let auth_token = resolve_auth_token(
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
}

/// B5-server：双 token 解析——service 优先，缺省回退 user；均缺失/为空则 None。
///
/// 独立成纯函数以便单测（避免测试中改动进程级 env）。
fn resolve_auth_token(service: Option<String>, user: Option<String>) -> Option<String> {
    service
        .filter(|s| !s.is_empty())
        .or_else(|| user.filter(|s| !s.is_empty()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_resolve_auth_token_service_takes_precedence() {
        let token = resolve_auth_token(
            Some("svc".to_string()),
            Some("user".to_string()),
        );
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
}
