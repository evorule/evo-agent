// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! serve 面限流装配(对标 evorule-server 同款 tower-governor 形态)
//!
//! - 纯函数 [`resolve_governor_config`] 决定是否挂 `GovernorLayer`:
//!   `per_sec == 0 → None`(彻底跳过层,不落 builder fallback 陷阱——
//!   `GovernorConfigBuilder::finish` 可能返回 None,fallback 到
//!   `GovernorConfig::default` 会让「禁用限速」名义下仍被缺省低限速 429,
//!   server 侧 2026-09-01 实测定谳的同款坑);
//! - tower_governor 0.8 的 `per_second(n)` 语义是「每 n 秒回补 1 个令牌」
//!   而非「每秒 n 个请求」——本参数语义 = 持续速率 req/s,故换算
//!   `per_millisecond((1000 / per_sec).max(1))`(≥1ms 下限防零);
//! - 挂载要求:bin 启动必须 `into_make_service_with_connect_info::<SocketAddr>`
//!   (PeerIpKeyExtractor 依赖 ConnectInfo 提取客户端 IP)。

#![forbid(unsafe_code)]

/// 缺省持续速率(req/s;与 evorule-server 生产档同值)
pub const DEFAULT_RATE_LIMIT_PER_SEC: u64 = 200;

/// 令牌桶容量(突发上限;与 evorule-server 同值)
pub const DEFAULT_RATE_LIMIT_BURST: u32 = 200;

/// 限流配置决策(纯函数,可单元测试)
///
/// - `per_sec == 0` → `None`(调用方跳过 GovernorLayer = 真正禁用限速)
/// - 其余 → `Some(cfg)`(调用方 `.layer(tower_governor::GovernorLayer::new(cfg))`)
pub fn resolve_governor_config(
    per_sec: u64,
    burst: u32,
) -> Option<
    tower_governor::governor::GovernorConfig<
        tower_governor::key_extractor::PeerIpKeyExtractor,
        governor::middleware::NoOpMiddleware,
    >,
> {
    if per_sec == 0 {
        return None;
    }
    // 换算:持续速率 per_sec req/s → 每 (1000/per_sec) ms 回补 1 令牌
    let period_ms = (1000 / per_sec).max(1);
    tower_governor::governor::GovernorConfigBuilder::default()
        .per_millisecond(period_ms)
        .burst_size(burst)
        .finish()
        .or_else(|| {
            tracing::warn!("GovernorConfigBuilder::finish() returned None; falling back to GovernorConfig::default()");
            Some(tower_governor::governor::GovernorConfig::default())
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn test_zero_per_sec_disables_limiting() {
        // per_sec=0 → None(彻底跳过层,无 fallback 限速)
        assert!(resolve_governor_config(0, DEFAULT_RATE_LIMIT_BURST).is_none());
    }

    #[test]
    fn test_positive_per_sec_enables_limiting() {
        assert!(
            resolve_governor_config(DEFAULT_RATE_LIMIT_PER_SEC, DEFAULT_RATE_LIMIT_BURST).is_some()
        );
        assert!(resolve_governor_config(1, 10).is_some());
    }

    #[test]
    fn test_defaults_match_server_production_posture() {
        // 对标锁定:与 evorule-server 生产档同值(200 req/s, burst=200)
        assert_eq!(DEFAULT_RATE_LIMIT_PER_SEC, 200);
        assert_eq!(DEFAULT_RATE_LIMIT_BURST, 200);
    }
}
