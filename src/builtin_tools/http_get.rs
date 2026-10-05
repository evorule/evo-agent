// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `http_get` —— HTTP GET(3 层 host 模型 + SSRF 防护)
//!
//! ## 设计原则(同 shell_exec:3 层 + propose)
//!
//! | 类别 | 例子 | 行为 |
//! |---|---|---|
//! | **active host** | `docs.rs` `crates.io` `github.com` `raw.githubusercontent.com` `api.github.com` `static.crates.io` | **直接请求** |
//! | **candidate host** | 任何其他公开 host | 返回 `proposal`,问用户 |
//! | **blocked** | localhost / 私有 IP / 内网段(SSRF) | **永不**批准 |
//!
//! ## SSRF 防护(关键)
//!
//! 即使用户批准,以下也**绝对拒**(硬编码,不能通过 candidate 绕过):
//! - `127.0.0.0/8` (localhost)
//! - `10.0.0.0/8` (内网)
//! - `172.16.0.0/12` (内网)
//! - `192.168.0.0/16` (内网)
//! - `169.254.0.0/16` (**cloud metadata endpoint** — AWS / Azure / GCP)
//! - `::1` (IPv6 localhost)
//! - `fc00::/7` (IPv6 ULA)
//! - `fe80::/10` (IPv6 link-local)
//!
//! ## 其他限制
//!
//! - **只 GET**(无 POST/PUT/DELETE)
//! - **强制 https://**(http:// 被拒)
//! - **Timeout 10s** + connect timeout 5s
//! - **Max response 1 MB**
//! - **Max 3 个 redirect**(防 redirect-based SSRF);每跳落点在**跟随前**
//!   复检——重跑网络负面域守卫 + host 分类(IP 字面量含归一化形态),
//!   重定向跳入内网/元数据地址在请求发出前即拒
//!
//! ## host 归一化(SSRF 分类前置)
//!
//! IP 字面量的「形态特殊」变体(`[::1]` bracket、十进制整数 `2130706433`、
//! 点分简写 `127.1`、十六进制段 `0x7f.0.0.1`)不受 `IpAddr::from_str`
//! 直解,但底层解析器仍会当 IP 连接——分类前先经 WHATWG host 解析
//! (与 reqwest 连接语义同源)归一化为标准 IP,防绕过 SSRF 分类。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use serde_json::Value;

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

// =============================================================================
// 3 层 host 分类
// =============================================================================

/// **Active host 白名单**:直接请求,无需请示
pub const ACTIVE_HOSTS: &[&str] = &[
    "docs.rs",
    "crates.io",
    "static.crates.io",
    "github.com",
    "raw.githubusercontent.com",
    "api.github.com",
];

/// **Blocked IP 段**:SSRF 防护
const BLOCKED_IPV4_RANGES: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(0, 0, 0, 0), 8),
];

/// 默认超时
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
/// TODO: doc
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;
/// TODO: doc
pub const MAX_REDIRECTS: u32 = 3;

/// Host 分类
#[derive(Debug, Clone, PartialEq)]
pub enum HostCategory {
    /// TODO: doc
    Active,
    /// TODO: doc
    Candidate {
        /// Candidate host name (public, requires user approval)
        host: String,
    },
    /// TODO: doc
    Blocked {
        /// Reason why the host is blocked
        reason: &'static str,
    },
    /// TODO: doc
    Invalid,
}

/// `http_get` 工具
#[derive(Clone)]
pub struct HttpGetTool {
    timeout: Duration,
    max_bytes: u64,
    #[allow(dead_code)]
    allow_insecure: bool,
}

impl HttpGetTool {
    /// TODO: doc
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_bytes: DEFAULT_MAX_BYTES,
            allow_insecure: false,
        }
    }

    /// TODO: doc
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    /// TODO: doc
    pub fn with_max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = n;
        self
    }

    /// 解析 URL 并分类 host
    pub fn classify(url: &str) -> HostCategory {
        let url = url.trim();
        if url.is_empty() {
            return HostCategory::Invalid;
        }

        // scheme
        let (scheme, rest) = if let Some(s) = url.strip_prefix("https://") {
            ("https", s)
        } else if let Some(s) = url.strip_prefix("http://") {
            ("http", s)
        } else {
            return HostCategory::Invalid;
        };

        // 拒绝 http://(只允许 https://)
        if scheme == "http" {
            return HostCategory::Blocked {
                reason: "http:// not allowed (use https://)",
            };
        }

        // 提取 host(去 path / query / fragment / port)
        let host_port = match rest.find('/') {
            Some(i) => &rest[..i],
            None => rest,
        };
        let host_port = host_port.split('?').next().unwrap_or(host_port);
        let host_port = host_port.split('#').next().unwrap_or(host_port);

        let host = if let Some(colon_pos) = host_port.rfind(':') {
            if host_port.contains('.') {
                &host_port[..colon_pos]
            } else {
                host_port
            }
        } else {
            host_port
        };

        if host.is_empty() {
            return HostCategory::Invalid;
        }

        // IP 检查(SSRF):先直解,再经 WHATWG host 解析归一化
        // (bracket/十进制整数/点分简写/十六进制段形态,见模块文档)
        let ip = host
            .parse::<IpAddr>()
            .ok()
            .or_else(|| normalize_host_ip(host));
        if let Some(ip) = ip {
            if Self::is_blocked_ip(&ip) {
                return HostCategory::Blocked {
                    reason: "IP is in SSRF blocklist (private/localhost/link-local)",
                };
            }
            // 公开 IP → candidate
            return HostCategory::Candidate {
                host: host.to_string(),
            };
        }

        // Active?
        if ACTIVE_HOSTS.contains(&host) {
            return HostCategory::Active;
        }

        // 其他 → candidate
        HostCategory::Candidate {
            host: host.to_string(),
        }
    }

    fn is_blocked_ip(ip: &IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => BLOCKED_IPV4_RANGES
                .iter()
                .any(|(net, prefix)| v4_octets_match(*v4, *net, *prefix)),
            IpAddr::V6(v6) => {
                if v6.is_loopback() {
                    return true;
                }
                if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                    return true;
                }
                if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                    return true;
                }
                if let Some(v4) = ipv6_to_ipv4(v6) {
                    return BLOCKED_IPV4_RANGES
                        .iter()
                        .any(|(net, prefix)| v4_octets_match(v4, *net, *prefix));
                }
                false
            }
        }
    }

    /// redirect 落点复检(redirect-based SSRF 防线):每跳落点在**跟随前**
    /// 重过网络负面域守卫 + host 分类(IP 字面量含归一化形态)
    ///
    /// 返回 Err = 拒绝跟随的显式原因(落点命中负面域/SSRF blocklist/降级
    /// http/非法 URL);blocked 语义与首跳一致——永不批准,无审批通道。
    fn check_redirect_target(target: &reqwest::Url) -> Result<(), String> {
        // 负面域红线(与首跳 call() 同源)
        super::net_guard::check_denied_network_target(target.as_str())
            .map_err(|e| format!("redirect blocked: {e}"))?;

        // SSRF 复检:落点重跑 host 分类(http 降级/IP 字面量内网均拒)
        match Self::classify(target.as_str()) {
            HostCategory::Blocked { reason } => Err(format!(
                "redirect blocked: target '{}' is BLOCKED (reason: {})",
                target.as_str(),
                reason
            )),
            HostCategory::Invalid => Err(format!(
                "redirect blocked: target '{}' is not a valid URL",
                target.as_str()
            )),
            _ => Ok(()),
        }
    }

    /// reqwest redirect 策略:限次语义与 `Policy::limited` 一致 + 落点复检
    /// (决策单源在 [`Self::check_redirect_target`])
    fn redirect_guard(attempt: reqwest::redirect::Attempt) -> reqwest::redirect::Action {
        // 与 reqwest Policy::limited 同判据:previous 首项是初始 URL 非重定向
        if attempt.previous().len() > MAX_REDIRECTS as usize {
            return attempt.error("redirect limit exceeded");
        }
        match Self::check_redirect_target(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    }

    /// 实际 HTTP GET(G13:原生 async,不再创建独立 runtime)
    async fn fetch(&self, url: &str) -> IoResult {
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::custom(Self::redirect_guard))
            .user_agent("evo-agent/0.1.0")
            .build()
            .map_err(|e| format!("client build: {}", e))?;

        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("request: {}", e))?;

        let status = resp.status();
        let final_url = resp.url().to_string();

        // redirect 落点复检(网络负面域守卫):防重定向跳入 benchmark 基础设施域
        // 后把内容带回观察面;红线拦截无审批通道
        super::net_guard::check_denied_network_target(&final_url)?;

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if let Some(len) = resp.content_length() {
            if len > self.max_bytes {
                return Err(format!(
                    "response too large: {} bytes (max {})",
                    len, self.max_bytes
                ));
            }
        }

        let body_bytes = resp.bytes().await.map_err(|e| format!("body: {}", e))?;

        if body_bytes.len() as u64 > self.max_bytes {
            return Err(format!(
                "response too large: {} bytes (max {})",
                body_bytes.len(),
                self.max_bytes
            ));
        }

        let body_text = String::from_utf8_lossy(&body_bytes).to_string();
        let url_owned = url.to_string();

        let mut map = serde_json::Map::new();
        map.insert("status".to_string(), Value::from("ok"));
        map.insert("url".to_string(), Value::from(url_owned));
        map.insert("final_url".to_string(), Value::from(final_url));
        map.insert(
            "http_status".to_string(),
            Value::from(status.as_u16() as i64),
        );
        map.insert("content_type".to_string(), Value::from(content_type));
        map.insert(
            "body_size".to_string(),
            Value::from(body_bytes.len() as i64),
        );
        map.insert("body".to_string(), Value::from(body_text));
        Ok(Value::Object(map))
    }

    /// 构造 proposal
    fn make_proposal(url: &str, host: &str) -> IoResult {
        let mut map = serde_json::Map::new();
        map.insert("status".to_string(), Value::from("needs_approval"));
        map.insert("url".to_string(), Value::from(url.to_string()));
        map.insert("host".to_string(), Value::from(host.to_string()));
        map.insert("category".to_string(), Value::from("candidate"));
        map.insert(
            "description".to_string(),
            Value::from("HTTP GET to a host not in the active allowlist"),
        );
        map.insert(
            "risk".to_string(),
            Value::from(
                "Leak data to an external server; download malicious content; potential SSRF if host resolves to private IP",
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

impl Default for HttpGetTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ToolFunction for HttpGetTool {
    /// G13:async 入口 — 直接 await reqwest(无需 spawn_blocking)
    async fn call(&self, args: &Value) -> IoResult {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: url (string)".to_string())?;

        // 网络负面域守卫(红线):URL 命中 benchmark 基础设施域即拒绝,
        // 先于 SSRF 分类与审批链,无审批通道
        super::net_guard::check_denied_network_target(url)?;

        let approved = args
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        match Self::classify(url) {
            HostCategory::Active => self.fetch(url).await,
            HostCategory::Candidate { host } => {
                if approved {
                    self.fetch(url).await
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

    /// 管道⑤评估单源(PR-4 收编):candidate 分类钩子——复刻 call 的判定
    /// 次序(net_guard 红线 → classify),仅 Candidate 形态返回 proposal。
    /// 纯函数:无网络请求。call 内保留的 approved 旗标检查降级为人工面
    /// 直调旗标 + 防御双保险(评估与执行同源,agent 面正常路径不会再触达
    /// 该分支)。
    fn evaluate_proposal(&self, args: &Value) -> Option<Value> {
        let url = args.get("url").and_then(|v| v.as_str())?;
        if super::net_guard::check_denied_network_target(url).is_err() {
            return None;
        }
        match Self::classify(url) {
            HostCategory::Candidate { host } => {
                Some(Self::make_proposal(url, &host).expect("proposal construction is infallible"))
            }
            _ => None,
        }
    }
}

// =============================================================================
// 辅助函数
// =============================================================================

/// host 归一化为 IP(WHATWG 语义,与 reqwest 实际连接解析同源)
///
/// 覆盖 `IpAddr::from_str` 拒收、但底层解析器仍按 IP 连接的「形态特殊」
/// 字面量:IPv6 bracket(`[::1]`)、十进制整数(`2130706433` = 127.0.0.1)、
/// 点分简写(`127.1`)、十六进制段(`0x7f.0.0.1`)。非 IP 字面量(域名的
/// WHATWG 解析结果为 Domain)返回 None,交由既有分类路径处理。
fn normalize_host_ip(host: &str) -> Option<IpAddr> {
    // IPv6 bracket 去括号后可直解,避免再走一遍 URL 解析
    let candidate = if host.starts_with('[') && host.ends_with(']') {
        &host[1..host.len() - 1]
    } else {
        host
    };
    if let Ok(ip) = candidate.parse::<IpAddr>() {
        return Some(ip);
    }
    // WHATWG host 解析:十进制整数/点分简写/十六进制段 → 标准 IPv4/IPv6
    let parsed = url::Url::parse(&format!("https://{candidate}/")).ok()?;
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
        _ => None,
    }
}

fn v4_octets_match(ip: Ipv4Addr, net: Ipv4Addr, prefix: u8) -> bool {
    if prefix == 0 {
        return true;
    }
    let ip_bits = u32::from(ip);
    let net_bits = u32::from(net);
    let mask = if prefix >= 32 {
        u32::MAX
    } else {
        u32::MAX << (32 - prefix)
    };
    (ip_bits & mask) == (net_bits & mask)
}

fn ipv6_to_ipv4(v6: &Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = v6.segments();
    if segments[0] == 0
        && segments[1] == 0
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
        && segments[5] == 0xffff
    {
        let a = (segments[6] >> 8) as u8;
        let b = (segments[6] & 0xff) as u8;
        let c = (segments[7] >> 8) as u8;
        let d = (segments[7] & 0xff) as u8;
        Some(Ipv4Addr::new(a, b, c, d))
    } else {
        None
    }
}

// =============================================================================
// 单元测试
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_active_hosts() {
        assert_eq!(
            HttpGetTool::classify("https://docs.rs/tokio"),
            HostCategory::Active
        );
        assert_eq!(
            HttpGetTool::classify("https://crates.io/api/v1/crates/serde"),
            HostCategory::Active
        );
        assert_eq!(
            HttpGetTool::classify("https://github.com/rust-lang/rust"),
            HostCategory::Active
        );
        assert_eq!(
            HttpGetTool::classify("https://raw.githubusercontent.com/foo/bar/main.rs"),
            HostCategory::Active
        );
    }

    #[test]
    fn test_classify_candidate_host() {
        match HttpGetTool::classify("https://stackoverflow.com/questions/123") {
            HostCategory::Candidate { host } => assert_eq!(host, "stackoverflow.com"),
            other => panic!("expected Candidate, got {:?}", other),
        }
        match HttpGetTool::classify("https://example.com") {
            HostCategory::Candidate { host } => assert_eq!(host, "example.com"),
            other => panic!("expected Candidate, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_blocked_localhost() {
        assert!(matches!(
            HttpGetTool::classify("https://127.0.0.1/admin"),
            HostCategory::Blocked { .. }
        ));
        assert!(matches!(
            HttpGetTool::classify("https://10.0.0.1/secret"),
            HostCategory::Blocked { .. }
        ));
        assert!(matches!(
            HttpGetTool::classify("https://192.168.1.1/router"),
            HostCategory::Blocked { .. }
        ));
    }

    #[test]
    fn test_classify_blocked_aws_metadata() {
        // 169.254.169.254 — AWS / Azure / GCP metadata,最危险的 SSRF
        assert!(matches!(
            HttpGetTool::classify("http://169.254.169.254/latest/meta-data/"),
            HostCategory::Blocked { .. }
        ));
    }

    #[test]
    fn test_classify_blocked_insecure_scheme() {
        assert!(matches!(
            HttpGetTool::classify("http://example.com/foo"),
            HostCategory::Blocked { .. }
        ));
    }

    #[test]
    fn test_classify_invalid() {
        assert_eq!(HttpGetTool::classify(""), HostCategory::Invalid);
        assert_eq!(HttpGetTool::classify("not-a-url"), HostCategory::Invalid);
        assert_eq!(
            HttpGetTool::classify("ftp://example.com"),
            HostCategory::Invalid
        );
    }

    #[test]
    fn test_v4_octets_match() {
        assert!(v4_octets_match(
            Ipv4Addr::new(127, 0, 0, 1),
            Ipv4Addr::new(127, 0, 0, 0),
            8
        ));
        assert!(!v4_octets_match(
            Ipv4Addr::new(128, 0, 0, 1),
            Ipv4Addr::new(127, 0, 0, 0),
            8
        ));
        assert!(v4_octets_match(
            Ipv4Addr::new(192, 168, 1, 100),
            Ipv4Addr::new(192, 168, 0, 0),
            16
        ));
    }

    #[tokio::test]
    async fn test_candidate_returns_proposal_without_approval() {
        let tool = HttpGetTool::new();
        let result = tool
            .call(&Value::Object({
                let mut m = serde_json::Map::new();
                m.insert("url".to_string(), Value::from("https://example.com/foo"));
                m
            }))
            .await;
        let v = result.expect("should return proposal, not error");
        assert_eq!(v.get("status").unwrap().as_str().unwrap(), "needs_approval");
        assert!(v.get("description").is_some());
        assert!(v.get("risk").is_some());
        assert!(v.get("alternative").is_some());
    }

    #[tokio::test]
    async fn test_blocked_host_always_rejected() {
        let tool = HttpGetTool::new();
        let result = tool
            .call(&Value::Object({
                let mut m = serde_json::Map::new();
                m.insert("url".to_string(), Value::from("https://192.168.1.1/admin"));
                m.insert("approved".to_string(), Value::Bool(true));
                m
            }))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("BLOCKED"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_missing_url_arg() {
        let tool = HttpGetTool::new();
        let result = tool.call(&Value::Object(Default::default())).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing required arg"));
    }

    // === 网络负面域守卫(net_guard,红线前置无审批通道)===

    #[tokio::test]
    async fn test_net_guard_denies_benchmark_url_before_request() {
        let tool = HttpGetTool::new();
        let result = tool
            .call(&Value::Object({
                let mut m = serde_json::Map::new();
                m.insert("url".to_string(), Value::from("https://tbench.ai/docs"));
                m
            }))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("denied by policy"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_net_guard_denies_benchmark_url_even_with_approval() {
        let tool = HttpGetTool::new();
        let result = tool
            .call(&Value::Object({
                let mut m = serde_json::Map::new();
                m.insert(
                    "url".to_string(),
                    Value::from("https://huggingface.co/datasets/terminal-bench/terminal-bench-2"),
                );
                m.insert("approved".to_string(), Value::Bool(true));
                m
            }))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        // 红线拦截不受 approved 影响(区别于 SSRF blocked 与 candidate 语义)
        assert!(err.contains("denied by policy"), "got: {}", err);
        assert!(!err.contains("needs_approval"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_net_guard_does_not_block_legitimate_hf_model_url() {
        // 不实际发起网络请求的断言形态:命中 candidate 白名单外的正常域
        // 走 proposal 而非红线拒绝(有网络环境下 active 域则真实放行)
        let tool = HttpGetTool::new();
        let result = tool
            .call(&Value::Object({
                let mut m = serde_json::Map::new();
                m.insert(
                    "url".to_string(),
                    Value::from("https://huggingface.co/meta-llama/Meta-Llama-3-8B"),
                );
                m
            }))
            .await;
        if let Err(e) = result {
            assert!(
                !e.contains("denied by policy"),
                "legitimate HF model URL must not be net-guard denied: {e}"
            );
        }
    }

    // === host 归一化(SSRF 分类前置:形态特殊 IP 字面量不降级)===

    #[test]
    fn test_classify_blocked_ipv6_bracket() {
        // bracket 形态:`IpAddr::from_str` 拒收 "[::1]",归一化后必须命中
        assert!(matches!(
            HttpGetTool::classify("https://[::1]/admin"),
            HostCategory::Blocked { .. }
        ));
        // bracket + 端口:提取面连 bracket 都剥不净,WHATWG 解析兜底
        assert!(matches!(
            HttpGetTool::classify("https://[::1]:8080/x"),
            HostCategory::Blocked { .. }
        ));
        // IPv6 link-local bracket 形态
        assert!(matches!(
            HttpGetTool::classify("https://[fe80::1]/"),
            HostCategory::Blocked { .. }
        ));
    }

    #[test]
    fn test_classify_blocked_normalized_ip_forms() {
        // 十进制整数 2130706433 = 127.0.0.1
        assert!(matches!(
            HttpGetTool::classify("https://2130706433/secret"),
            HostCategory::Blocked { .. }
        ));
        // 十进制整数 2852039166 = 169.254.169.254(云元数据端点)
        assert!(matches!(
            HttpGetTool::classify("https://2852039166/latest/meta-data/"),
            HostCategory::Blocked { .. }
        ));
        // 点分简写 127.1 = 127.0.0.1
        assert!(matches!(
            HttpGetTool::classify("https://127.1/admin"),
            HostCategory::Blocked { .. }
        ));
        // 十六进制段 0x7f.0.0.1 = 127.0.0.1
        assert!(matches!(
            HttpGetTool::classify("https://0x7f.0.0.1/admin"),
            HostCategory::Blocked { .. }
        ));
    }

    #[test]
    fn test_classify_public_ipv6_bracket_still_candidate() {
        // 归一化不得误伤:公开 IPv6 bracket 形态仍走 candidate(需审批)
        match HttpGetTool::classify("https://[2606:4700::1111]/dns-query") {
            HostCategory::Candidate { host } => assert_eq!(host, "[2606:4700::1111]"),
            other => panic!("expected Candidate, got {:?}", other),
        }
    }

    #[test]
    fn test_normalize_host_ip_domain_is_none() {
        // 域名不是 IP 字面量:归一化返回 None,交既有分类路径
        assert_eq!(normalize_host_ip("example.com"), None);
        assert_eq!(normalize_host_ip("docs.rs"), None);
    }

    // === redirect 落点复检(redirect-based SSRF 防线)===

    #[test]
    fn test_redirect_target_allows_public_host() {
        let target = reqwest::Url::parse("https://example.com/path").unwrap();
        assert!(HttpGetTool::check_redirect_target(&target).is_ok());
    }

    #[test]
    fn test_redirect_target_blocks_private_ip() {
        let target = reqwest::Url::parse("https://127.0.0.1/secret").unwrap();
        let err = HttpGetTool::check_redirect_target(&target)
            .expect_err("private IP redirect must be rejected");
        assert!(err.contains("redirect blocked"), "got: {err}");
        assert!(err.contains("BLOCKED"), "got: {err}");
    }

    #[test]
    fn test_redirect_target_blocks_normalized_private_ip() {
        // 落点为十进制整数 IP(= 127.0.0.1):归一化复检,不得绕过
        let target = reqwest::Url::parse("https://2130706433/secret").unwrap();
        assert!(HttpGetTool::check_redirect_target(&target).is_err());
    }

    #[test]
    fn test_redirect_target_blocks_http_downgrade() {
        let target = reqwest::Url::parse("http://example.com/").unwrap();
        assert!(HttpGetTool::check_redirect_target(&target).is_err());
    }

    #[tokio::test]
    async fn test_redirect_target_denies_negative_domain() {
        let target = reqwest::Url::parse("https://tbench.ai/solution").unwrap();
        let err = HttpGetTool::check_redirect_target(&target)
            .expect_err("negative domain redirect must be rejected");
        assert!(err.contains("denied by policy"), "got: {err}");
    }

    #[tokio::test]
    async fn test_fetch_redirect_to_private_ip_rejected_before_follow() {
        // 策略接线层(直驱 fetch 绕过首跳分类——mockito 地址本身是内网
        // 字面量):302 落点 https://127.0.0.1/secret 必须在发出请求前被拒
        let mut server = mockito::Server::new_async().await;
        let url = format!("{}/jump", server.url());
        server
            .mock("GET", "/jump")
            .with_status(302)
            .with_header("Location", "https://127.0.0.1/secret")
            .create_async()
            .await;

        let tool = HttpGetTool::new();
        let result = tool.fetch(&url).await;
        assert!(result.is_err(), "redirect to private IP must fail");
        let err = result.unwrap_err();
        // reqwest 重定向错误形态(区别于连接失败,证明是策略拒绝)
        assert!(err.contains("redirect"), "got: {err}");
    }
}
