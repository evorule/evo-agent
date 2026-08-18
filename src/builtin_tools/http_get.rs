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
//! - **Max 3 个 redirect**(防 redirect-based SSRF)

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use evorule_tcb::JsonValue;

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

        // IP 检查(SSRF)
        if let Ok(ip) = host.parse::<IpAddr>() {
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

    /// 实际 HTTP GET(G13:原生 async,不再创建独立 runtime)
    async fn fetch(&self, url: &str) -> IoResult {
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS as usize))
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

        let mut map = std::collections::BTreeMap::new();
        map.insert("status".to_string(), JsonValue::string("ok"));
        map.insert("url".to_string(), JsonValue::string(url_owned));
        map.insert("final_url".to_string(), JsonValue::string(final_url));
        map.insert(
            "http_status".to_string(),
            JsonValue::Integer(status.as_u16() as i64),
        );
        map.insert("content_type".to_string(), JsonValue::string(content_type));
        map.insert(
            "body_size".to_string(),
            JsonValue::Integer(body_bytes.len() as i64),
        );
        map.insert("body".to_string(), JsonValue::string(body_text));
        Ok(JsonValue::object(map))
    }

    /// 构造 proposal
    fn make_proposal(url: &str, host: &str) -> IoResult {
        let mut map = std::collections::BTreeMap::new();
        map.insert("status".to_string(), JsonValue::string("needs_approval"));
        map.insert("url".to_string(), JsonValue::string(url.to_string()));
        map.insert("host".to_string(), JsonValue::string(host.to_string()));
        map.insert("category".to_string(), JsonValue::string("candidate"));
        map.insert(
            "description".to_string(),
            JsonValue::string("HTTP GET to a host not in the active allowlist"),
        );
        map.insert(
            "risk".to_string(),
            JsonValue::string(
                "Leak data to an external server; download malicious content; potential SSRF if host resolves to private IP",
            ),
        );
        map.insert(
            "alternative".to_string(),
            JsonValue::string(
                "Use a host in the active list (docs.rs, crates.io, github.com); or download manually and use file_read",
            ),
        );
        map.insert(
            "instructions".to_string(),
            JsonValue::string("Ask the user. If approved, call with approved=true."),
        );
        Ok(JsonValue::object(map))
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
    async fn call(&self, args: &JsonValue) -> IoResult {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: url (string)".to_string())?;

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
}

// =============================================================================
// 辅助函数
// =============================================================================

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
            .call(&JsonValue::object({
                let mut m = std::collections::BTreeMap::new();
                m.insert(
                    "url".to_string(),
                    JsonValue::string("https://example.com/foo"),
                );
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
            .call(&JsonValue::object({
                let mut m = std::collections::BTreeMap::new();
                m.insert(
                    "url".to_string(),
                    JsonValue::string("https://192.168.1.1/admin"),
                );
                m.insert("approved".to_string(), JsonValue::Bool(true));
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
        let result = tool.call(&JsonValue::object(Default::default())).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing required arg"));
    }
}
