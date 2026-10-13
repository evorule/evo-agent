// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G7:HTTP API 鉴权中间件
//!
//! 复用 evorule tier2 的 AuthConfig 模式:
//! - Bearer token in `Authorization` header(首选)
//! - `?token=xxx` query param fallback(仅 SSE 端点,浏览器 EventSource 不支持自定义 header)
//! - `subtle::ConstantTimeEq` 恒定时间比较(防时序攻击)
//! - `current_tokens` + `previous_tokens` 双 token 轮换(无缝过渡)
//! - `validate()` 遍历所有 token,不提前返回(防枚举攻击)
//! - 公开路由(`/health`)豁免鉴权
//!
//! # 配置
//!
//! ```toml
//! [auth]
//! enabled = true
//! tokens = ["secret-token-1", "secret-token-2"]
//! ```
//!
//! 环境变量:`EVO_AGENT_AUTH__ENABLED=true`、`EVO_AGENT_AUTH__TOKENS=t1,t2`
//!
//! CLI:`--auth-token secret`(可多次指定,覆盖配置)、`--no-auth`(禁用)

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// 公开路由前缀(不需要鉴权)
///
/// `/health` 供 load balancer / K8s liveness probe 使用,必须免鉴权。
/// `/metrics` 供 Prometheus 抓取,必须免鉴权(否则抓取器无法携带 token)。
const PUBLIC_PATHS: &[&str] = &["/health", "/metrics"];

/// 鉴权配置
///
/// 设计同 evorule tier2 `AuthConfig`,evo-agent 独立一份以保持 crate 独立性。
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// 当前合法 token 列表
    current_tokens: Arc<Vec<String>>,
    /// 上轮轮换前的 token 列表(过渡期仍可使用,用于无缝轮换)
    previous_tokens: Arc<Vec<String>>,
    /// 是否启用鉴权(false 时跳过检查)
    enabled: bool,
}

impl AuthConfig {
    /// 创建新鉴权配置
    ///
    /// - `tokens`:合法 token 列表(设为 current_tokens,previous_tokens 为空)
    /// - `enabled`:是否启用鉴权
    pub fn new(tokens: Vec<String>, enabled: bool) -> Self {
        Self {
            current_tokens: Arc::new(tokens),
            previous_tokens: Arc::new(Vec::new()),
            enabled,
        }
    }

    /// 禁用鉴权(开发模式)
    pub fn disabled() -> Self {
        Self {
            current_tokens: Arc::new(Vec::new()),
            previous_tokens: Arc::new(Vec::new()),
            enabled: false,
        }
    }

    /// 是否已启用鉴权
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 轮换 token:将 current_tokens 移入 previous_tokens,设置新的 current_tokens
    ///
    /// 轮换后,旧 token 在 `previous_tokens` 中仍可使用(过渡期),
    /// 客户端可在任意时间切换到新 token,实现无缝轮换。
    ///
    /// 再次轮换时,旧的 `previous_tokens` 会被丢弃(仅保留一轮过渡)。
    pub fn rotate_tokens(&self, new_tokens: Vec<String>) -> Self {
        Self {
            current_tokens: Arc::new(new_tokens),
            previous_tokens: self.current_tokens.clone(),
            enabled: self.enabled,
        }
    }

    /// 恒定时间比较两个字符串是否相等
    ///
    /// 长度不同时仍执行比较以避免长度信息泄露,内容比较使用 `subtle::ConstantTimeEq`。
    fn ct_eq(a: &str, b: &str) -> bool {
        let a_bytes = a.as_bytes();
        let b_bytes = b.as_bytes();
        if a_bytes.len() != b_bytes.len() {
            // 长度不同:比较 a 与自身(消耗相同时间),然后返回 false
            let _ = a_bytes.ct_eq(a_bytes);
            return false;
        }
        bool::from(a_bytes.ct_eq(b_bytes))
    }

    /// 验证 token 是否合法
    ///
    /// 遍历 `current_tokens` 和 `previous_tokens` 中的所有 token,
    /// 使用恒定时间比较,且不因匹配到就提前返回(防止通过时序枚举有效 token)。
    pub fn validate(&self, token: &str) -> bool {
        if !self.enabled {
            return true;
        }
        let mut found = false;
        for t in self.current_tokens.iter() {
            if Self::ct_eq(token, t) {
                found = true;
            }
        }
        for t in self.previous_tokens.iter() {
            if Self::ct_eq(token, t) {
                found = true;
            }
        }
        found
    }
}

// =============================================================================
// SG-0 fail-closed:serve 启动期鉴权姿态决策
// =============================================================================
//
// 改造前(G7 旧缺省):无任何鉴权配置 -> AuthConfig::disabled(),静默放行。
// 后果:启动横幅照样把每个端点标成 `(auth)`,但实际全开 —— 横幅与事实相反,
// 且 `serve --port 8081` 这类裸命令把整个 agent API 暴露给本机任何进程。
//
// 改造后(SG-0):缺省不再是「静默 disabled」,而是按**绑定面**分流:
//   - 已配 token            -> Enabled          (正常)
//   - 显式 `--no-auth`      -> ExplicitNoAuth   (本地开发豁免,告警不拦)
//   - 缺省无 token + 回环   -> LoopbackDegraded (降级放行 + 显著告警)
//   - 缺省无 token + 对外   -> RefuseExternalBind (**拒起**)
// 决策是纯函数(无 IO、无 DNS),启动期单测可直接覆盖。

/// 判定监听地址是否属于回环(loopback)。
///
/// SG-0 fail-closed 的判据:缺省无鉴权时,只有绑定回环才是「仅本机可达」的
/// 可降级形态;`0.0.0.0` / `::` / 任何具体对外 IP 一律视为对外暴露。
///
/// 空字符串按 CLI 缺省(= `127.0.0.1`)处理,视为回环。
/// 主机名只认 `localhost`;不做 DNS 解析(避免启动期网络依赖与 TOCTOU)。
pub fn is_loopback_bind(host: &str) -> bool {
    let h = host.trim();
    if h.is_empty() {
        return true;
    }
    // IPv6 字面量可能带方括号:`[::1]`
    let h = h.trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    h.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// serve 启动期的鉴权姿态(SG-0 fail-closed 决策产物)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPosture {
    /// 鉴权已开启,正常放行。
    Enabled,
    /// 显式 `--no-auth`:本地开发豁免。显著告警但不拦。
    ExplicitNoAuth,
    /// 缺省无鉴权 + 回环绑定:降级放行 + 显著告警(仅本机可达)。
    LoopbackDegraded,
    /// 缺省无鉴权 + 对外绑定:fail-closed 的真正收口 —— **拒起**。
    RefuseExternalBind {
        /// 触发拒起的监听地址(诊断用)
        host: String,
    },
}

/// SG-0 fail-closed 决策。
///
/// - `auth_enabled`     : 最终解析出的鉴权是否开启(--no-auth / --auth-token / 配置文件)
/// - `explicit_no_auth` : 是否由 CLI 显式传入 `--no-auth`(开发豁免,不看绑定面)
/// - `host`             : 监听地址(仅 host,不含端口)
pub fn decide_auth_posture(
    auth_enabled: bool,
    explicit_no_auth: bool,
    host: &str,
) -> AuthPosture {
    if auth_enabled {
        return AuthPosture::Enabled;
    }
    if explicit_no_auth {
        // 显式旗标 = 人的明确决定,豁免 fail-closed(但仍告警)
        return AuthPosture::ExplicitNoAuth;
    }
    if is_loopback_bind(host) {
        AuthPosture::LoopbackDegraded
    } else {
        AuthPosture::RefuseExternalBind {
            host: host.to_string(),
        }
    }
}

/// 从请求中提取 token
///
/// 优先级:
/// 1. `Authorization: Bearer <token>` header(首选,所有端点)
/// 2. `?token=xxx` query param(SSE 端点 fallback,浏览器 EventSource 不支持自定义 header)
fn extract_token(req: &Request) -> Option<String> {
    // 1. Bearer token from Authorization header
    if let Some(token) = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
    {
        return Some(token);
    }

    // 2. Query param fallback (for SSE / EventSource)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(token) = pair.strip_prefix("token=") {
                return Some(token.to_string());
            }
        }
    }

    None
}

/// Axum 鉴权中间件
///
/// 从 `Authorization: Bearer <token>` 头(或 `?token=` query param)提取 token,
/// 验证是否在合法 token 列表中。
///
/// 公开路由(`/health`)豁免鉴权。
pub async fn auth_middleware(
    State(auth_config): State<AuthConfig>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // 公开路由豁免
    if PUBLIC_PATHS.contains(&req.uri().path()) {
        return Ok(next.run(req).await);
    }

    // 未启用鉴权 → 放行
    if !auth_config.enabled {
        return Ok(next.run(req).await);
    }

    let token = extract_token(&req).ok_or(StatusCode::UNAUTHORIZED)?;

    if auth_config.validate(&token) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // ===== AuthConfig 单元测试(移植自 evorule tier2)=====

    #[test]
    fn test_disabled_auth_allows_all() {
        let config = AuthConfig::disabled();
        assert!(config.validate("anything"));
        assert!(config.validate(""));
    }

    #[test]
    fn test_enabled_auth_validates_token() {
        let config = AuthConfig::new(vec!["secret123".to_string()], true);
        assert!(config.validate("secret123"));
        assert!(!config.validate("wrong"));
        assert!(!config.validate(""));
    }

    #[test]
    fn test_enabled_with_empty_tokens_rejects_all() {
        let config = AuthConfig::new(vec![], true);
        assert!(!config.validate("anything"));
    }

    #[test]
    fn test_ct_eq_equal_strings() {
        assert!(AuthConfig::ct_eq("hello", "hello"));
        assert!(AuthConfig::ct_eq("", ""));
    }

    #[test]
    fn test_ct_eq_different_strings() {
        assert!(!AuthConfig::ct_eq("hello", "world"));
        assert!(!AuthConfig::ct_eq("hello", "hello!"));
        assert!(!AuthConfig::ct_eq("hello", ""));
    }

    #[test]
    fn test_multiple_tokens() {
        let config = AuthConfig::new(
            vec![
                "token_a".to_string(),
                "token_b".to_string(),
                "token_c".to_string(),
            ],
            true,
        );
        assert!(config.validate("token_a"));
        assert!(config.validate("token_b"));
        assert!(config.validate("token_c"));
        assert!(!config.validate("token_d"));
    }

    #[test]
    fn test_token_rotation_current_still_valid() {
        let config = AuthConfig::new(vec!["old_token".to_string()], true);
        let rotated = config.rotate_tokens(vec!["new_token".to_string()]);
        assert!(rotated.validate("new_token"));
        assert!(rotated.validate("old_token"));
        assert!(!rotated.validate("wrong_token"));
    }

    #[test]
    fn test_token_rotation_double_rotate_drops_oldest() {
        let config = AuthConfig::new(vec!["v1_token".to_string()], true);
        let rotated1 = config.rotate_tokens(vec!["v2_token".to_string()]);
        let rotated2 = rotated1.rotate_tokens(vec!["v3_token".to_string()]);

        assert!(rotated2.validate("v3_token"));
        assert!(rotated2.validate("v2_token"));
        assert!(!rotated2.validate("v1_token"));
    }

    #[test]
    fn test_rotation_preserves_disabled_state() {
        let config = AuthConfig::disabled();
        let rotated = config.rotate_tokens(vec!["new_token".to_string()]);
        assert!(rotated.validate("anything"));
        assert!(rotated.validate("new_token"));
    }

    #[test]
    fn test_enabled_flag() {
        let enabled = AuthConfig::new(vec!["t".to_string()], true);
        let disabled = AuthConfig::disabled();
        assert!(enabled.enabled());
        assert!(!disabled.enabled());
    }

    // ===== extract_token 测试 =====

    use axum::http::Request as HttpRequest;

    fn make_request_with_auth_header(token: &str) -> Request {
        HttpRequest::builder()
            .header("Authorization", format!("Bearer {}", token))
            .body(axum::body::Body::empty())
            .unwrap()
    }

    fn make_request_with_query_token(token: &str) -> Request {
        HttpRequest::builder()
            .uri(format!("/agents/x/run/stream?token={}", token))
            .body(axum::body::Body::empty())
            .unwrap()
    }

    fn make_request_no_auth() -> Request {
        HttpRequest::builder()
            .uri("/agents")
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[test]
    fn test_extract_token_from_header() {
        let req = make_request_with_auth_header("my-secret");
        assert_eq!(extract_token(&req), Some("my-secret".to_string()));
    }

    #[test]
    fn test_extract_token_from_query_param() {
        let req = make_request_with_query_token("query-secret");
        assert_eq!(extract_token(&req), Some("query-secret".to_string()));
    }

    #[test]
    fn test_extract_token_none() {
        let req = make_request_no_auth();
        assert_eq!(extract_token(&req), None);
    }

    #[test]
    fn test_extract_token_header_takes_precedence() {
        // 同时有 header 和 query param → header 优先
        let req = HttpRequest::builder()
            .uri("/agents/x/run/stream?token=query-secret")
            .header("Authorization", "Bearer header-secret")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), Some("header-secret".to_string()));
    }

    // ===== auth_middleware 集成测试 =====

    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    fn make_test_router(auth_config: AuthConfig) -> axum::Router {
        async fn handler() -> &'static str {
            "ok"
        }
        axum::Router::new()
            .route("/health", get(handler))
            .route("/agents", get(handler))
            .with_state(())
            .layer(axum::middleware::from_fn_with_state(
                auth_config,
                auth_middleware,
            ))
    }

    #[tokio::test]
    async fn test_middleware_disabled_allows_all() {
        let app = make_test_router(AuthConfig::disabled());

        // /agents 无 header → 200(auth disabled)
        let resp = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_middleware_enabled_rejects_no_token() {
        let app = make_test_router(AuthConfig::new(vec!["secret".to_string()], true));

        // /agents 无 Authorization header → 401
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_middleware_enabled_accepts_valid_token() {
        let app = make_test_router(AuthConfig::new(vec!["secret".to_string()], true));

        // /agents 带正确 Bearer token → 200
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/agents")
                    .header("Authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_middleware_enabled_rejects_wrong_token() {
        let app = make_test_router(AuthConfig::new(vec!["secret".to_string()], true));

        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/agents")
                    .header("Authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_middleware_health_exempt() {
        // /health 不需要 token,即使 auth enabled
        let app = make_test_router(AuthConfig::new(vec!["secret".to_string()], true));

        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_middleware_query_param_fallback() {
        // SSE 端点用 ?token=xxx fallback
        let app = make_test_router(AuthConfig::new(vec!["secret".to_string()], true));

        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/agents?token=secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_middleware_rotated_token_still_valid() {
        let config = AuthConfig::new(vec!["old".to_string()], true);
        let rotated = config.rotate_tokens(vec!["new".to_string()]);
        let app = make_test_router(rotated);

        // 新 token
        let resp = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/agents")
                    .header("Authorization", "Bearer new")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 旧 token(过渡期仍有效)
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/agents")
                    .header("Authorization", "Bearer old")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // =====================================================================
    // SG-0 fail-closed:姿态决策
    // =====================================================================

    #[test]
    fn test_loopback_recognized() {
        for h in [
            "127.0.0.1",
            "127.0.0.53",
            "127.1.2.3", // 整个 127.0.0.0/8 都是回环
            "::1",
            "[::1]",
            "localhost",
            "LocalHost",
            "  127.0.0.1  ",
            "", // CLI 缺省 = 127.0.0.1
        ] {
            assert!(is_loopback_bind(h), "should be loopback: {h:?}");
        }
    }

    #[test]
    fn test_non_loopback_rejected() {
        for h in [
            "0.0.0.0",     // 监听全部网卡 = 对外暴露
            "::",          // IPv6 全部网卡
            "[::]",
            "192.168.1.5",
            "10.0.0.3",
            "example.com", // 非 localhost 的主机名,不做 DNS 解析,一律判对外
        ] {
            assert!(!is_loopback_bind(h), "should NOT be loopback: {h:?}");
        }
    }

    #[test]
    fn test_posture_enabled_when_auth_on() {
        // 有 token 时,绑定面无关,一律 Enabled(哪怕对外绑定)
        assert_eq!(decide_auth_posture(true, false, "0.0.0.0"), AuthPosture::Enabled);
        assert_eq!(decide_auth_posture(true, false, "127.0.0.1"), AuthPosture::Enabled);
    }

    #[test]
    fn test_posture_default_loopback_degraded_not_refused() {
        // 关键回归:改造前这条路径静默 disabled 全开;
        // 改造后仍放行(比赛线/本机开发不被打断),但必须进入可告警姿态。
        assert_eq!(
            decide_auth_posture(false, false, "127.0.0.1"),
            AuthPosture::LoopbackDegraded
        );
    }

    #[test]
    fn test_posture_default_external_bind_refuses_startup() {
        // 真正的收口:缺省无 token + 对外绑定 = 拒起
        assert_eq!(
            decide_auth_posture(false, false, "0.0.0.0"),
            AuthPosture::RefuseExternalBind {
                host: "0.0.0.0".to_string()
            }
        );
        assert_eq!(
            decide_auth_posture(false, false, "::"),
            AuthPosture::RefuseExternalBind {
                host: "::".to_string()
            }
        );
    }

    #[test]
    fn test_posture_explicit_no_auth_overrides_external_bind() {
        // 显式 --no-auth 是人的明确决定,豁免拒起(本地开发要能 0.0.0.0 起)
        assert_eq!(
            decide_auth_posture(false, true, "0.0.0.0"),
            AuthPosture::ExplicitNoAuth
        );
    }
}
