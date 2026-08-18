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
    /// Read from `EVORULE_AUTH_TOKEN` env var at construction time.
    auth_token: Option<String>,
}

impl ApiCore {
    /// Create new API core.
    /// If `EVORULE_AUTH_TOKEN` env var is set, use it as Bearer token.
    /// Otherwise, send no auth header (server must be in dev mode / no auth).
    pub fn new(base_url: &str) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        let auth_token = std::env::var("EVORULE_AUTH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());

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
