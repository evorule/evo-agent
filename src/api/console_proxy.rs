// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! console 嵌入态反向代理 —— `/console-api/*` → evorule-server。
//!
//! 背景:console 静态产物由 serve 同源托管(`/console`,见 bin/evo-agent.rs
//! 双形态接入)后,页面数据面缺省走 proxy 模式(同源 `/console-api` 前缀):
//! 请求由 serve 服务端转发至引擎并注入 Authorization —— **token 不进页面**
//! (与 netConfig 直连模式的 authToken 互补;嵌入态免 CORS、免前端持有凭据)。
//!
//! 挂载面:serve 外层 Router(与 workbench fallback 同级,不经 G7 鉴权中间件
//! —— 静态页须无 token 可打开)。代理能力仅限「serve 自身可达的引擎端点」,
//! 权限等同 serve 进程的引擎凭据,不放大攻击面;仅在 console 静态托管形态
//! 挂载(产物缺位/dev 形态不挂,页面此时不会从同源发起数据请求)。

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;

use super::evorule_client::EvoruleApiClient;

/// 代理面 URL 前缀(console 页面数据面 proxy 模式的同源基址)
pub const CONSOLE_API_PREFIX: &str = "/console-api";

/// 构造代理子路由(挂 serve 外层 Router;state = serve 的引擎客户端)。
pub fn router(client: EvoruleApiClient) -> Router {
    Router::new()
        .route("/console-api/{*rest}", any(proxy))
        .with_state(client)
}

/// 剥离代理前缀 + 拼 query,得到转发用 path?query(纯逻辑,独立可测)。
pub fn target_path(path: &str, query: Option<&str>) -> Option<String> {
    let rest = path.strip_prefix(CONSOLE_API_PREFIX)?;
    if rest.is_empty() || rest == "/" {
        return None;
    }
    Some(match query {
        Some(q) if !q.is_empty() => format!("{rest}?{q}"),
        _ => rest.to_string(),
    })
}

async fn proxy(
    State(client): State<EvoruleApiClient>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(path) = target_path(uri.path(), uri.query()) else {
        return (StatusCode::NOT_FOUND, "missing target path").into_response();
    };
    let req_method = match reqwest::Method::from_bytes(method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid method").into_response(),
    };
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let body_opt = (!body.is_empty()).then_some(body.as_ref());
    match client
        .proxy_passthrough(req_method, &path, content_type, body_opt)
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let mut builder = Response::builder().status(status.as_u16());
            for h in [axum::http::header::CONTENT_TYPE, axum::http::header::CONTENT_LENGTH] {
                if let Some(v) = resp.headers().get(&h) {
                    builder = builder.header(&h, v.clone());
                }
            }
            let bytes = resp.bytes().await.unwrap_or_default();
            match builder.body(axum::body::Body::from(bytes)) {
                Ok(r) => r,
                Err(e) => (
                    StatusCode::BAD_GATEWAY,
                    format!("proxy response build failed: {e}"),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("engine unreachable: {e}"),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_path_strips_prefix_and_keeps_query() {
        assert_eq!(
            target_path("/console-api/api/sessions", Some("include_content=true")).as_deref(),
            Some("/api/sessions?include_content=true")
        );
        assert_eq!(
            target_path("/console-api/api/health", None).as_deref(),
            Some("/api/health")
        );
    }

    #[test]
    fn test_target_path_rejects_non_proxy_or_bare() {
        assert!(target_path("/api/sessions", None).is_none());
        assert!(target_path("/console-api", None).is_none());
        assert!(target_path("/console-api/", None).is_none());
    }
}
