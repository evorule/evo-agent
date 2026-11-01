// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `http_get` —— HTTP GET(3 层 host 模型 + SSRF 防护)
//!
//! ## 设计原则(同 shell_exec:3 层 + propose)
//!
//! | 类别 | 例子 | 行为 |
//! |---|---|---|
//! | **active host** | `docs.rs`/`crates.io`/`github.com` 等代码托管+官方文档站+标准+百科 26 域（见 [`ACTIVE_HOSTS`]） | **直接请求** |
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
    // 代码托管与包管理
    "docs.rs",
    "crates.io",
    "static.crates.io",
    "github.com",
    "raw.githubusercontent.com",
    "api.github.com",
    "objects.githubusercontent.com",
    "gitlab.com",
    "npmjs.com",
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    // 官方文档站（通用任务高频）
    "developer.mozilla.org",
    "doc.rust-lang.org",
    "static.rust-lang.org",
    "nodejs.org",
    "learn.microsoft.com",
    "python.org",
    "go.dev",
    // 标准与规范
    "w3.org",
    "www.w3.org",
    "datatracker.ietf.org",
    "www.rfc-editor.org",
    "openid.net",
    "modelcontextprotocol.io",
    // 百科与知识面（只读参考）
    "en.wikipedia.org",
    "zh.wikipedia.org",
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
/// 默认 redirect 上限
pub const DEFAULT_MAX_REDIRECTS: u32 = 3;
/// TODO: doc
pub const MAX_REDIRECTS: u32 = DEFAULT_MAX_REDIRECTS;

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
    max_redirects: u32,
    #[allow(dead_code)]
    allow_insecure: bool,
    /// 放宽明文 http://(G7 配置化;false=既有行为——http:// 拒,https-only)
    allow_http: bool,
    /// 放宽内网/私有 IP 段(G7 配置化;false=既有 SSRF 黑名单——Blocked 永不
    /// 批准;true=私有段降级 Candidate,仍走审批人在环,永不自动直放)
    allow_private_hosts: bool,
    /// agent 定义级 active host 声明:声明 host 在分类时由
    /// Candidate 升级为 Active(直接执行);空=既有行为逐字节不变。
    /// Blocked/Invalid 永不升级(SSRF 黑名单与负面域守卫不受影响)。
    extra_allowed_hosts: Vec<String>,
}

impl HttpGetTool {
    /// TODO: doc
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_bytes: DEFAULT_MAX_BYTES,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            allow_insecure: false,
            allow_http: false,
            allow_private_hosts: false,
            extra_allowed_hosts: Vec::new(),
        }
    }

    /// 声明额外 active host 白名单(大小写归一存储;语义见结构体字段注释)
    pub fn with_allowed_hosts(mut self, hosts: Vec<String>) -> Self {
        self.extra_allowed_hosts = hosts
            .into_iter()
            .map(|h| h.trim().to_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        self
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

    /// redirect 上限(G7 配置化;默认 [`DEFAULT_MAX_REDIRECTS`])
    pub fn with_max_redirects(mut self, n: u32) -> Self {
        self.max_redirects = n;
        self
    }

    /// 明文 http:// 放宽(G7 配置化;默认 false=https-only 不变)
    pub fn with_allow_http(mut self, allow: bool) -> Self {
        self.allow_http = allow;
        self
    }

    /// 内网/私有段放宽(G7 配置化;默认 false=SSRF 黑名单不变;true=私有段
    /// 降级 Candidate 仍审批,Blocked 硬语义仅对永拒类保留)
    pub fn with_allow_private_hosts(mut self, allow: bool) -> Self {
        self.allow_private_hosts = allow;
        self
    }

    /// 解析 URL 并分类 host(缺省策略=既有行为)
    pub fn classify(url: &str) -> HostCategory {
        Self::classify_with(url, false, false)
    }

    /// 策略化分类(G7 配置化单源):`allow_http` 放宽明文 scheme;
    /// `allow_private_hosts` 把 SSRF 私有段从 Blocked(永不批准)降级为
    /// Candidate(仍审批)——放宽只改可达性,不放掉人在环。
    /// `(false, false)` 与既有 `classify` 逐字节等价。
    pub fn classify_with(url: &str, allow_http: bool, allow_private_hosts: bool) -> HostCategory {
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

        // 拒绝 http://(只允许 https://;G7:allow_http 显式开才放明文)
        if scheme == "http" && !allow_http {
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
                // G7:allow_private_hosts 仅把可放宽私有段(回环/内网三段/
                // IPv6 本地)降级 Candidate(仍审批);云元数据(169.254)与
                // 未指明地址(0.0.0.0/8)永不放宽
                if allow_private_hosts && Self::is_relaxable_private_ip(&ip) {
                    return HostCategory::Candidate {
                        host: host.to_string(),
                    };
                }
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

    /// agent 实例级分类:静态分类结果为 Candidate 且 host 命中本实例声明的
    /// 额外白名单时升级为 Active;Blocked/Invalid 永不升级(安全优先)。
    /// 无声明(extra_allowed_hosts 空)时与 `Self::classify` 逐字节等价。
    ///
    /// 批K 段2:pub(crate) 供 web_fetch 组合复用(防线同源,见 web_fetch.rs)
    pub(crate) fn classify_for_agent(&self, url: &str) -> HostCategory {
        match Self::classify_with(url, self.allow_http, self.allow_private_hosts) {
            HostCategory::Candidate { host } => {
                let normalized = host.to_lowercase();
                if self.extra_allowed_hosts.contains(&normalized) {
                    HostCategory::Active
                } else {
                    HostCategory::Candidate { host }
                }
            }
            other => other,
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

    /// G7 可放宽私有段判定:`allow_private_hosts` 显式开时,回环/内网三段/
    /// IPv6 本地段可降级 Candidate(仍审批);云元数据(169.254/16)与未指明
    /// 地址(0.0.0.0/8)是 SSRF 王冠珍珠,永不放宽。
    fn is_relaxable_private_ip(ip: &IpAddr) -> bool {
        if !Self::is_blocked_ip(ip) {
            return false;
        }
        match ip {
            IpAddr::V4(v4) => {
                !(v4_octets_match(*v4, Ipv4Addr::new(169, 254, 0, 0), 16)
                    || v4_octets_match(*v4, Ipv4Addr::new(0, 0, 0, 0), 8))
            }
            // IPv6 loopback/ULA/link-local 可放宽(本地开发语义)
            IpAddr::V6(_) => true,
        }
    }

    /// redirect 落点复检(redirect-based SSRF 防线):每跳落点在**跟随前**
    /// 重过网络负面域守卫 + host 分类(IP 字面量含归一化形态)
    ///
    /// 返回 Err = 拒绝跟随的显式原因(落点命中负面域/SSRF blocklist/降级
    /// http/非法 URL);blocked 语义与首跳一致——永不批准,无审批通道。
    /// 仅测试目标编译(缺省策略命名锚;生产通路走 with_policy 单源)。
    #[cfg(test)]
    fn check_redirect_target(target: &reqwest::Url) -> Result<(), String> {
        Self::check_redirect_target_with_policy(target, false, false)
    }

    /// 策略化落点复检(G7):与首跳同策略(allow_http/allow_private_hosts
    /// 同源透传)——放宽只经实例配置单源,重定向不构成第二放宽通道。
    fn check_redirect_target_with_policy(
        target: &reqwest::Url,
        allow_http: bool,
        allow_private_hosts: bool,
    ) -> Result<(), String> {
        // 负面域红线(与首跳 call() 同源)
        super::net_guard::check_denied_network_target(target.as_str())
            .map_err(|e| format!("redirect blocked: {e}"))?;

        // SSRF 复检:落点重跑 host 分类(http 降级/IP 字面量内网均拒;
        // G7 策略化——实例显式放宽才可达)
        match Self::classify_with(target.as_str(), allow_http, allow_private_hosts) {
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
    /// (决策单源在 [`Self::check_redirect_target_with_policy`])
    fn redirect_guard_with(
        attempt: reqwest::redirect::Attempt,
        allow_http: bool,
        allow_private_hosts: bool,
        max_redirects: u32,
    ) -> reqwest::redirect::Action {
        // 与 reqwest Policy::limited 同判据:previous 首项是初始 URL 非重定向
        if attempt.previous().len() > max_redirects as usize {
            return attempt.error("redirect limit exceeded");
        }
        match Self::check_redirect_target_with_policy(
            attempt.url(),
            allow_http,
            allow_private_hosts,
        ) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    }

    /// 实际 HTTP GET(G13:原生 async,不再创建独立 runtime)
    ///
    /// 批K 段2:pub(crate) 供 web_fetch 组合复用(防线零复制,fetch_and_render
    /// 在返回体上做 content-type 判别与 HTML→文本提取)
    pub(crate) async fn fetch(&self, url: &str) -> IoResult {
        // G7:redirect 策略按实例配置(上限/放宽旗标)闭包捕获——重定向
        // 与首跳同源单策略,不构成第二放宽通道
        let (allow_http, allow_private, max_redirects) = (
            self.allow_http,
            self.allow_private_hosts,
            self.max_redirects,
        );
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                Self::redirect_guard_with(attempt, allow_http, allow_private, max_redirects)
            }))
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

        match self.classify_for_agent(url) {
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

    // =========================================================================
    // definition 级 active host 声明(声明内 Active 直放/声明外
    // 仍 Candidate/Blocked 永不升级/大小写归一)
    // =========================================================================

    #[test]
    fn test_classify_for_agent_declared_host_upgrades_to_active() {
        let tool = HttpGetTool::new().with_allowed_hosts(vec![
            "api.example.com".to_string(),
            "API.OpenWeather.org".to_string(), // 大写声明,归一命中
        ]);
        // 声明内 → Active
        assert_eq!(
            tool.classify_for_agent("https://api.example.com/v2/data"),
            HostCategory::Active
        );
        assert_eq!(
            tool.classify_for_agent("https://api.openweather.org/x"),
            HostCategory::Active
        );
        // 声明外(含内置 6 host 之外的普通公网域) → 仍 Candidate
        assert_eq!(
            tool.classify_for_agent("https://other.example.org/x"),
            HostCategory::Candidate {
                host: "other.example.org".to_string()
            }
        );
        // 无声明实例与静态分类逐字节等价(既有行为零变化)
        let plain = HttpGetTool::new();
        assert_eq!(
            plain.classify_for_agent("https://api.example.com/v2/data"),
            HttpGetTool::classify("https://api.example.com/v2/data")
        );
    }

    #[test]
    fn test_classify_for_agent_never_upgrades_blocked_or_invalid() {
        // SSRF 黑名单 IP 即使被声明也不升级(Blocked 永不放行)
        let tool = HttpGetTool::new()
            .with_allowed_hosts(vec!["192.168.1.1".to_string(), "127.0.0.1".to_string()]);
        match tool.classify_for_agent("https://192.168.1.1/admin") {
            HostCategory::Blocked { .. } => {}
            other => panic!("blocked host must stay blocked, got {:?}", other),
        }
        // 声明无法把非法 URL 变合法
        assert_eq!(
            tool.classify_for_agent("not a url"),
            HttpGetTool::classify("not a url")
        );
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

    // === G7 配置化(缺省零变化/放宽生效/红线永不放宽)===

    #[test]
    fn test_g7_default_config_zero_behavior_change() {
        // definition 缺省(None)与显式全缺省配置同判:内置 new() 行为逐字节等价
        let plain = HttpGetTool::new();
        let from_cfg = HttpGetTool::new()
            .with_timeout(crate::agent::definition::HttpGetToolConfig::default().timeout_secs)
            .with_max_bytes(crate::agent::definition::HttpGetToolConfig::default().max_bytes)
            .with_max_redirects(
                crate::agent::definition::HttpGetToolConfig::default().max_redirects,
            );
        // 缺省策略下既有判定全部保持:http 拒/内网拒/元数据拒/公网 candidate
        for url in [
            "http://example.com/foo",
            "https://127.0.0.1/admin",
            "https://192.168.1.1/router",
            "http://169.254.169.254/latest/meta-data/",
        ] {
            assert_eq!(
                plain.classify_for_agent(url),
                from_cfg.classify_for_agent(url),
                "default config must not change classification of {url}"
            );
            assert!(
                matches!(
                    from_cfg.classify_for_agent(url),
                    HostCategory::Blocked { .. }
                ),
                "{url} must stay blocked under default config"
            );
        }
        assert_eq!(
            from_cfg.classify_for_agent("https://example.com/x"),
            HttpGetTool::classify("https://example.com/x")
        );
        // 内置常量未被改动(零变化锚)
        assert_eq!(DEFAULT_TIMEOUT_SECS, 10);
        assert_eq!(DEFAULT_MAX_BYTES, 1024 * 1024);
        assert_eq!(DEFAULT_MAX_REDIRECTS, 3);
    }

    #[test]
    fn test_g7_relaxation_takes_effect_still_gated() {
        // allow_private_hosts:回环/内网段 Blocked → Candidate(仍审批,非直放)
        let tool = HttpGetTool::new().with_allow_private_hosts(true);
        match tool.classify_for_agent("https://192.168.1.1/router") {
            HostCategory::Candidate { host } => assert_eq!(host, "192.168.1.1"),
            other => panic!("private host must downgrade to Candidate, got {:?}", other),
        }
        match tool.classify_for_agent("https://127.0.0.1:8080/health") {
            HostCategory::Candidate { .. } => {}
            other => panic!("loopback must downgrade to Candidate, got {:?}", other),
        }
        // 放宽不等于 Active:candidate 仍走审批提案(人在环不放掉)
        // allow_http:明文 http:// Blocked → Candidate(公开 host)
        let http_ok = HttpGetTool::new().with_allow_http(true);
        match http_ok.classify_for_agent("http://example.com/foo") {
            HostCategory::Candidate { host } => assert_eq!(host, "example.com"),
            other => panic!("http must downgrade to Candidate, got {:?}", other),
        }
        // GET-only 不放宽:配置面无 method 通道(编译面保证,此处锁定分类不受
        // 放宽旗标影响的语义——https 公网路径行为与缺省一致)
        assert_eq!(
            http_ok.classify_for_agent("https://example.com/foo"),
            HttpGetTool::classify("https://example.com/foo")
        );
    }

    #[test]
    fn test_g7_ssrf_redline_stays_by_default_and_metadata_never_relaxed() {
        // 缺省(不放宽):SSRF 黑名单语义完整保留
        let plain = HttpGetTool::new();
        for url in [
            "https://127.0.0.1/admin",
            "https://10.0.0.1/secret",
            "https://172.16.0.1/x",
            "https://192.168.1.1/router",
            "https://[::1]/admin",
            "http://169.254.169.254/latest/meta-data/",
            "https://2130706433/secret",
        ] {
            assert!(
                matches!(plain.classify_for_agent(url), HostCategory::Blocked { .. }),
                "{url} must stay BLOCKED by default"
            );
        }
        // 即便显式放宽内网段:云元数据与未指明地址永不放宽(王冠珍珠红线)
        let relaxed = HttpGetTool::new().with_allow_private_hosts(true);
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "https://0.0.0.0/x",
        ] {
            assert!(
                matches!(
                    relaxed.classify_for_agent(url),
                    HostCategory::Blocked { .. }
                ),
                "{url} must NEVER be relaxed (metadata/unspecified)"
            );
        }
        // 重定向落点复检同策略:放宽后内网落点可达(与首跳一致),
        // 缺省下仍拒;元数据落点两种策略下均拒
        let t_priv = reqwest::Url::parse("https://192.168.1.1/x").unwrap();
        assert!(HttpGetTool::check_redirect_target(&t_priv).is_err());
        assert!(
            HttpGetTool::check_redirect_target_with_policy(&t_priv, false, true).is_ok(),
            "relaxed policy must allow private redirect target (approval-gated)"
        );
        let t_meta = reqwest::Url::parse("https://169.254.169.254/meta").unwrap();
        assert!(HttpGetTool::check_redirect_target_with_policy(&t_meta, false, true).is_err());
    }

    #[test]
    fn test_g7_definition_config_validate_and_wire() {
        use crate::agent::definition::HttpGetToolConfig;
        // 缺省配置过门卫
        assert!(HttpGetToolConfig::default().validate().is_ok());
        // 荒谬值 fail-fast
        assert!(HttpGetToolConfig {
            timeout_secs: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HttpGetToolConfig {
            timeout_secs: 301,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HttpGetToolConfig {
            max_bytes: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HttpGetToolConfig {
            max_redirects: 11,
            ..Default::default()
        }
        .validate()
        .is_err());
        // serde 缺省:空对象 = 全缺省值(零变化)
        let cfg: HttpGetToolConfig = serde_json::from_value(Value::Object(Default::default()))
            .expect("empty config must deserialize with defaults");
        assert_eq!(cfg, HttpGetToolConfig::default());
        // 放宽旗标反序列化
        let cfg: HttpGetToolConfig = serde_json::from_value(serde_json::json!({
            "allow_http": true, "allow_private_hosts": true
        }))
        .unwrap();
        assert!(cfg.allow_http && cfg.allow_private_hosts);
    }
}
