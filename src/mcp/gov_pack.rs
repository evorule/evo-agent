// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! MCP 任务能力包执行面（无人值守防护层 design-57 T₀ 的门组合半边）
//!
//! pack 绑定的 run 进程：按 task_ref 从治理面拉取能力包（与生效清单同一
//! pull 通路+同一 pull key），装载门判定收窄为 **pack∩list 双闸**——
//! 治理清单命中还不够，必须在任务能力包内且包未过期。
//!
//! ## 授权界（不变式 4：授权必有界）
//!
//! - 时间界：包激活时刻（本进程首次成功拉取）起算 `ttl_secs`，超时=同未授权
//!   处置（有界到期自动收缩）；
//! - 能力界：servers 为批准清单键子集（治理面创建时声明）；
//! - 任务界：pack_ref 即任务绑定（本模块的启用开关）。
//!
//! ## 降级时限（不变式 4 的连接维度）
//!
//! 治理不可达持续超过 `EVORULE_MCP_DEGRADE_TTL_SECS`（缺省 1800）后，
//! 装载门**冻结新 spawn**（已建立连接不中断）：无限期降级运行=违反授权
//! 必有界不变式。TTL 内照常走降级文件形态（design-55 件 A）。
//!
//! ## fail-soft 红线
//!
//! pack 拉取/解析失败绝不 panic 阻断主流程——pack 缺失时门组合如实按
//! 「能力未授权」处置（pack 模式下 fail-closed），事件留痕。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// MCP 任务能力包数据集约定 id（与治理面/spawn_gate 侧常量镜像对齐）
pub(crate) const MCP_PACKS_DATASET_ID: &str = "org.evorule.mcp.packs";

/// 降级时限（秒）：治理不可达持续超过该窗口即冻结新 spawn。env
/// `EVORULE_MCP_DEGRADE_TTL_SECS` 可配（钳制 60..=86400），缺省 1800。
pub(crate) fn degrade_ttl() -> Duration {
    let secs = std::env::var("EVORULE_MCP_DEGRADE_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(1800)
        .clamp(60, 86400);
    Duration::from_secs(secs)
}

/// 降级冻结判定（纯函数）：治理源已配置且降级持续超过 TTL = 冻结
pub(crate) fn frozen(degraded_for: Option<Duration>, ttl: Duration) -> bool {
    degraded_for.map(|d| d >= ttl).unwrap_or(false)
}

/// 能力包状态（本进程激活后缓存）
#[derive(Debug, Clone)]
pub(crate) struct PackState {
    pub task_ref: String,
    pub servers: HashSet<String>,
    pub ttl_secs: u64,
    /// 包激活时刻（本进程首次成功拉取；TTL 从此起算——有界授权跟随
    /// 任务进程生命周期，不跨进程继承）
    pub activated_at: Instant,
}

impl PackState {
    /// TTL 是否已到（激活时刻起算）
    pub(crate) fn expired(&self) -> bool {
        self.activated_at.elapsed().as_secs() >= self.ttl_secs
    }

    pub(crate) fn contains(&self, server_key: &str) -> bool {
        self.servers.contains(server_key)
    }
}

/// 门组合判定（纯函数）：pack 模式下，治理清单命中（server_key 在场）后
/// 的第二闸。
///
/// - `pack=None`（pack_ref 未配/未成功拉取/已过期清除）→ `NotAuthorized`：
///   pack 模式 fail-closed——无法确证任务授权即不拉起；
/// - 包内且未过期 → `Allowed`；包外 → `NotInPack`；已过期 → `Expired`
///   （三者事件面 verdict 均为 reject，reason 供报告区分）。
pub(crate) fn compose(
    pack_mode: bool,
    pack: Option<&PackState>,
    matched_server_key: Option<&str>,
) -> PackComposed {
    if !pack_mode {
        return PackComposed::PackOff;
    }
    let Some(key) = matched_server_key else {
        // 清单未命中（无规范键）在 pack 模式下同样拒载：能力包语义要求
        // 可核验的规范键归属
        return PackComposed::NotInPack;
    };
    let Some(pack) = pack else {
        return PackComposed::NotAuthorized;
    };
    if pack.expired() {
        return PackComposed::Expired;
    }
    if pack.contains(key) {
        PackComposed::Allowed
    } else {
        PackComposed::NotInPack
    }
}

/// 组合结论（装载门最终 verdict 的输入）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackComposed {
    /// 未启用能力包模式（现行为：清单单闸）
    PackOff,
    /// 双闸通过
    Allowed,
    /// 清单未命中（沿用清单判定：enforce 拒载/提案）
    NotInPack,
    /// 双闸未过：包外/包未就绪（fail-closed）
    NotAuthorized,
    /// 双闸未过：包已过期（TTL 到期自动收缩）
    Expired,
}

/// pack 模式开关：env `EVORULE_MCP_PACK_REF` 非空即启用（schedule fire_job
/// 为 pack 绑定任务注入）
pub(crate) fn pack_ref() -> Option<String> {
    std::env::var("EVORULE_MCP_PACK_REF")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// 进程级 pack 缓存（激活后不再重拉——TTL 界定的是授权时效而非清单新鲜度；
/// 清单新鲜度由 gov_source 独立维护）
fn cache() -> &'static Mutex<Option<Arc<PackState>>> {
    static CACHE: OnceLock<Mutex<Option<Arc<PackState>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 取当前能力包（已激活且未过期返回 Some；过期即视为 None 并留痕——
/// 有界到期自动收缩）
pub(crate) fn current_pack() -> Option<Arc<PackState>> {
    let expired = {
        let guard = cache().lock().unwrap();
        match guard.as_ref() {
            Some(pack) if !pack.expired() => return Some(pack.clone()),
            Some(pack) => {
                tracing::warn!(
                    target: "mcp_spawn_gate",
                    task_ref = %pack.task_ref,
                    "MCP pack: TTL 到期，任务授权自动收缩（后续 spawn 按未授权处置）"
                );
                Some(pack.task_ref.clone())
            }
            None => None,
        }
    };
    if expired.is_some() {
        cache().lock().unwrap().take();
    }
    None
}

/// 首次装载：拉取 pack（独立线程，reqwest::blocking 与治理源同款宿主纪律）。
/// 失败=pack 缺失（pack 模式 fail-closed 拒载+事件留痕），不重试不阻塞——
/// 与提案回流同款 fail-soft；恢复重试=下次进程启动。
pub(crate) fn ensure_loaded() {
    let Some(pack_ref) = pack_ref() else {
        return;
    };
    let base_url = std::env::var("EVORULE_MCP_GOV_URL").unwrap_or_default();
    let api_key = std::env::var("EVORULE_MCP_GOV_API_KEY").unwrap_or_default();
    if base_url.trim().is_empty() || api_key.trim().is_empty() {
        tracing::warn!(
            target: "mcp_spawn_gate",
            task_ref = %pack_ref,
            "MCP pack: 治理源未配置（URL/KEY 缺失），pack 模式将 fail-closed 拒载"
        );
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("mcp-pack-load".into())
        .spawn(move || match pull_pack(&base_url, &api_key, &pack_ref) {
            Ok(state) => {
                *cache().lock().unwrap() = Some(Arc::new(state));
                tracing::info!(
                    target: "mcp_spawn_gate",
                    task_ref = %pack_ref,
                    servers = ?cache().lock().unwrap().as_ref().map(|p| p.servers.len()),
                    "MCP pack: 能力包已装载（task 授权生效）"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "mcp_spawn_gate",
                    task_ref = %pack_ref,
                    error = %e,
                    "MCP pack: 拉取失败（pack 模式 fail-closed 拒载直至进程重启；事件留痕）"
                );
            }
        })
        .is_ok();
    if !spawned {
        tracing::warn!(target: "mcp_spawn_gate", "MCP pack 加载线程启动失败（fail-soft）");
    }
}

/// 拉取 packs 数据集 bundle 并定位 task_ref 条目（与生效清单同端点族）
fn pull_pack(base_url: &str, api_key: &str, pack_ref: &str) -> Result<PackState, String> {
    let url = format!(
        "{}/v1/datasets/{}/bundle",
        base_url.trim_end_matches('/'),
        MCP_PACKS_DATASET_ID
    );
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client 构建失败: {e}"))?;
    let resp = client
        .get(&url)
        .header("X-Api-Key", api_key)
        .send()
        .map_err(|e| format!("拉取失败 {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("拉取失败 {url}: HTTP {status}"));
    }
    let body = resp.text().map_err(|e| format!("读取响应失败: {e}"))?;
    parse_pack_bundle(&body, pack_ref)
}

/// bundle 文本 → 能力包状态（严格：JSON 解析失败/条目缺失 = 整体失败）
fn parse_pack_bundle(raw: &str, pack_ref: &str) -> Result<PackState, String> {
    let bundle: evorule_bundle::DatasetBundle =
        serde_json::from_str(raw).map_err(|e| format!("bundle JSON 解析失败: {e}"))?;
    let entry = bundle
        .entries
        .iter()
        .find(|e| {
            e.entry_id == pack_ref
                || e.rule_body.get("task_ref").and_then(|v| v.as_str()) == Some(pack_ref)
        })
        .ok_or_else(|| format!("bundle 内无 task_ref=`{pack_ref}` 的能力包条目"))?;
    let servers: Vec<String> = entry
        .rule_body
        .get("servers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "pack 缺 servers 数组".to_string())?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let ttl_secs = entry
        .rule_body
        .get("ttl_secs")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "pack 缺 ttl_secs".to_string())?;
    if servers.is_empty() || ttl_secs == 0 {
        return Err("pack servers 空 / ttl 非法（治理面写核漏网，执行侧拒绝采信）".to_string());
    }
    Ok(PackState {
        task_ref: pack_ref.to_string(),
        servers: servers.into_iter().collect(),
        ttl_secs,
        activated_at: Instant::now(),
    })
}

/// 供 spawn_gate 装载钩子调用（进程首次装载门判定前触发一次）
pub(crate) fn startup_load_once() {
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| {
        if pack_ref().is_some() {
            ensure_loaded();
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn pack(ttl: u64, servers: &[&str]) -> PackState {
        PackState {
            task_ref: "t1".to_string(),
            servers: servers.iter().map(|s| s.to_string()).collect(),
            ttl_secs: ttl,
            activated_at: Instant::now(),
        }
    }

    const KEY: &str = "blake3:aa";

    #[test]
    fn compose_pack_off_is_passthrough() {
        assert_eq!(compose(false, None, Some(KEY)), PackComposed::PackOff);
        assert_eq!(
            compose(false, Some(&pack(60, &[KEY])), None),
            PackComposed::PackOff
        );
    }

    #[test]
    fn compose_pack_membership_gates() {
        let p = pack(3600, &[KEY]);
        // 包内+未过期 → 放行
        assert_eq!(compose(true, Some(&p), Some(KEY)), PackComposed::Allowed);
        // 包外 → 拒（NotInPack）
        assert_eq!(
            compose(true, Some(&p), Some("blake3:bb")),
            PackComposed::NotInPack
        );
        // 包未就绪（未拉到）→ fail-closed 拒
        assert_eq!(compose(true, None, Some(KEY)), PackComposed::NotAuthorized);
    }

    #[test]
    fn compose_pack_expiry_shrinks_authorization() {
        let mut p = pack(3600, &[KEY]);
        // 人为把激活时刻拨回 TTL 之前
        p.activated_at = Instant::now() - Duration::from_secs(7200);
        assert!(p.expired());
        assert_eq!(compose(true, Some(&p), Some(KEY)), PackComposed::Expired);
    }

    #[test]
    fn pack_bundle_parse_extracts_task_entry() {
        let raw = serde_json::json!({
            "bundle_schema_version": "1.0",
            "bundle_id": "b1",
            "dataset": {
                "dataset_id": "org.evorule.mcp.packs",
                "name": "packs",
                "tenant_id": "t",
                "instance_id": "i",
                "versioning": { "current": "v1", "history": [] }
            },
            "entries": [
                {
                    "entry_id": "nightly",
                    "entry_kind": "knowledge",
                    "rule_body": {
                        "task_ref": "nightly",
                        "servers": ["blake3:aa", "blake3:bb"],
                        "ttl_secs": 3600,
                        "on_gap": "block"
                    },
                    "schema_ref": "builtin:mcp/pack",
                    "provenance": { "source": "gov" },
                    "domain": "mcp",
                    "tags": []
                },
                {
                    "entry_id": "other",
                    "entry_kind": "knowledge",
                    "rule_body": { "task_ref": "other", "servers": ["blake3:cc"], "ttl_secs": 60, "on_gap": "block" },
                    "schema_ref": "builtin:mcp/pack",
                    "provenance": { "source": "gov" },
                    "domain": "mcp",
                    "tags": []
                }
            ],
            "tests": { "verdict": "pass", "cases": [] },
            "audit": { "exported_at": "t", "exported_by": "u", "source_version": "v1", "content_hash": "blake3:abc" }
        })
        .to_string();
        let state = parse_pack_bundle(&raw, "nightly").unwrap();
        assert!(state.contains("blake3:aa"));
        assert!(state.contains("blake3:bb"));
        assert!(!state.contains("blake3:cc"));
        assert_eq!(state.ttl_secs, 3600);
        // 无该 task_ref 的 pack → 显式失败
        assert!(parse_pack_bundle(&raw, "no-such-task").is_err());
    }

    #[test]
    fn freeze_requires_configured_governance_and_ttl() {
        // 纯函数：降级时长过窗=冻结
        assert!(frozen(
            Some(Duration::from_secs(1801)),
            Duration::from_secs(1800)
        ));
        assert!(!frozen(
            Some(Duration::from_secs(1799)),
            Duration::from_secs(1800)
        ));
        // None=未降级（治理源新鲜或未配置）
        assert!(!frozen(None, Duration::from_secs(1800)));
        assert_eq!(
            degrade_ttl(),
            Duration::from_secs(1800).clamp(Duration::from_secs(60), Duration::from_secs(86400))
        );
    }
}
