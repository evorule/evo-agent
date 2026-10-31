// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! MCP 拉起批准清单的治理源（治理原生线：许可在治理面）
//!
//! ## 信任清单来源优先级（装载门消费）
//!
//! 1. **治理清单**：从治理服务拉取约定数据集（`org.evorule.mcp.servers`）的生效
//!    快照包（`GET /v1/datasets/{id}/bundle`，`X-Api-Key` scope=pull），进程内
//!    TTL 缓存；批准清单 = 数据集生效条目，许可走治理审批链产生。
//! 2. **降级**：本地白名单文件（`.evorule/mcp-allowlist.json`）。治理未配置/不
//!    可达/缓存过期时显式降级——降级原因与持续时长随装载门判定事件留痕，不静默。
//!
//! 信任根基：会话能写 mcp.toml（意愿面），写不进治理审批链（许可面）。
//!
//! ## 配置（env；URL+KEY 齐全才启用，未配置 = 本地文件即权威，零旁路开销）
//!
//! - `EVORULE_MCP_GOV_URL`：治理服务基址（如 `http://127.0.0.1:18081`）
//! - `EVORULE_MCP_GOV_API_KEY`：pull scope 的 X-Api-Key（治理面由 org admin 签发）
//! - `EVORULE_MCP_GOV_DATASET`：数据集 id，缺省 `org.evorule.mcp.servers`
//! - `EVORULE_MCP_GOV_TTL_SECS`：缓存新鲜度窗口，缺省 300（钳制 5..=3600）
//!
//! ## 线程模型
//!
//! 后台刷新线程（普通 OS 线程，reqwest::blocking 的合法宿主）；装载门判定只读
//! 快照，**永不阻塞等网络**——冷启动窗口内先走本地文件 + 显式降级事件，首轮拉取
//! 成功后自动回到治理源。新鲜度语义：`last_success` 落在 TTL 内 = 治理源可用；
//! 超窗（刷新未成功）= 降级，直到下次刷新成功。

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use evorule_bundle::{DatasetBundle, EntryKind};

use super::spawn_gate::{
    canonical_server_key, validate_approval_entry, AllowlistEntry, SpawnAllowlist,
    MCP_SERVER_SCHEMA_REF,
};

/// 治理源快照（批准清单 + 包内容哈希=版本标记，变更可观测）
#[derive(Clone, Debug)]
pub(crate) struct GovSnapshot {
    pub allow: Arc<SpawnAllowlist>,
    pub content_hash: String,
}

/// 装载门视角的治理源可用性
#[derive(Debug)]
pub(crate) enum Availability {
    /// 治理清单新鲜（last_success 在 TTL 内）
    Governance(GovSnapshot),
    /// 降级本地文件（reason: cold_start=从未成功 / stale_or_unreachable=曾成功但超窗）
    Fallback {
        reason: &'static str,
        degraded_for: Option<Duration>,
    },
    /// 未配置治理源：本地文件即权威（部署形态，非降级）
    NotConfigured,
}

/// 治理源连接配置
#[derive(Debug, Clone)]
pub(crate) struct GovConfig {
    pub base_url: String,
    pub api_key: String,
    pub dataset_id: String,
    pub ttl: Duration,
}

impl GovConfig {
    /// env 解析（URL+KEY 齐全才启用；未知/畸形值按缺省处理——可用性旋钮不承担
    /// 安全语义，安全语义在清单内容闸与 enforce 档位）
    pub(crate) fn from_env() -> Option<Self> {
        let base_url = std::env::var("EVORULE_MCP_GOV_URL").unwrap_or_default();
        let api_key = std::env::var("EVORULE_MCP_GOV_API_KEY").unwrap_or_default();
        if base_url.trim().is_empty() || api_key.trim().is_empty() {
            return None;
        }
        let dataset_id = std::env::var("EVORULE_MCP_GOV_DATASET")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "org.evorule.mcp.servers".to_string());
        let ttl_secs = std::env::var("EVORULE_MCP_GOV_TTL_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(300)
            .clamp(5, 3600);
        Some(Self {
            base_url,
            api_key,
            dataset_id,
            ttl: Duration::from_secs(ttl_secs),
        })
    }
}

/// 治理源进程内缓存（快照 + 新鲜度状态；状态迁移显式留痕）
pub(crate) struct GovCache {
    pub cfg: GovConfig,
    state: Mutex<GovState>,
}

#[derive(Default)]
struct GovState {
    snapshot: Option<GovSnapshot>,
    last_success: Option<Instant>,
    last_error: Option<String>,
}

impl GovCache {
    pub(crate) fn new(cfg: GovConfig) -> Self {
        Self {
            cfg,
            state: Mutex::new(GovState::default()),
        }
    }

    /// 当前可用性（只读快照，不触网络）
    pub(crate) fn availability(&self) -> Availability {
        let st = self.state.lock().unwrap();
        if let (Some(snap), Some(t)) = (&st.snapshot, st.last_success) {
            if t.elapsed() <= self.cfg.ttl {
                return Availability::Governance(snap.clone());
            }
        }
        Availability::Fallback {
            reason: if st.last_success.is_some() {
                "stale_or_unreachable"
            } else if st.last_error.is_some() {
                "unreachable"
            } else {
                "cold_start"
            },
            degraded_for: st.last_success.map(|t| t.elapsed()),
        }
    }

    /// 拉取一轮并更新状态（注入 fetcher 便于测试；返回是否成功）。
    /// 状态迁移显式留痕：加载/变更=info，首次失败=warn（持续失败降频 debug），
    /// 恢复=info（携带降级持续时长）。
    pub(crate) fn refresh_once<F>(&self, fetch: F) -> bool
    where
        F: FnOnce(&GovConfig) -> Result<GovSnapshot, String>,
    {
        let mut st = self.state.lock().unwrap();
        let prev_success = st.last_success;
        let was_snapshot = st.snapshot.is_some();
        match fetch(&self.cfg) {
            Ok(new) => {
                let changed = st
                    .snapshot
                    .as_ref()
                    .is_none_or(|old| old.content_hash != new.content_hash);
                if changed {
                    tracing::info!(
                        target: "mcp_spawn_gate",
                        entries = new.allow.allow.len(),
                        content_hash = %new.content_hash,
                        "MCP 治理批准清单已加载/更新"
                    );
                }
                if let Some(t) = prev_success {
                    tracing::info!(
                        target: "mcp_spawn_gate",
                        degraded_secs = t.elapsed().as_secs(),
                        "MCP 治理批准清单恢复，退出本地降级"
                    );
                }
                st.last_error = None;
                st.last_success = Some(Instant::now());
                st.snapshot = Some(new);
                true
            }
            Err(e) => {
                if was_snapshot && st.last_error.is_none() {
                    tracing::warn!(
                        target: "mcp_spawn_gate",
                        error = %e,
                        "MCP 治理批准清单拉取失败：装载门转入本地白名单文件降级（恢复后自动回到治理源）"
                    );
                } else {
                    tracing::debug!(
                        target: "mcp_spawn_gate",
                        error = %e,
                        "MCP 治理批准清单尚未成功拉取（持续降级本地文件）"
                    );
                }
                st.last_error = Some(e);
                false
            }
        }
    }
}

/// 进程级治理源单例（首次装载门判定时惰性初始化）。
///
/// 首拉**有界同步等待**（3s 封顶）：治理源在场时，首个装载门判定应以治理清单
/// 为准——冷启动窗口内先判后到会把治理已批准的 server 误拒在门外（真机实证），
/// 只有治理真不可达才降级本地文件。等待超时/失败即交还后台线程接管，降级
/// 语义不变（未成功前 availability() 仍按 Fallback 显式留痕）。
pub(crate) fn global() -> Option<&'static GovCache> {
    static GOV: OnceLock<Option<&'static GovCache>> = OnceLock::new();
    GOV.get_or_init(|| {
        GovConfig::from_env().map(|cfg| {
            let cache: &'static GovCache = Box::leak(Box::new(GovCache::new(cfg)));
            let bg: &'static GovCache = cache;
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("mcp-gov-source".into())
                .spawn(move || {
                    let mut ok = bg.refresh_once(fetch_bundle);
                    let _ = tx.send(ok);
                    loop {
                        std::thread::sleep(if ok {
                            bg.cfg.ttl
                        } else {
                            Duration::from_secs(30).min(bg.cfg.ttl)
                        });
                        ok = bg.refresh_once(fetch_bundle);
                    }
                })
                .expect("spawn MCP governance source refresh thread");
            let _ = rx.recv_timeout(Duration::from_secs(3));
            cache
        })
    })
    .as_deref()
}

/// 装载门入口：治理源可用性判定（未配置 = 本地文件即权威）
pub(crate) fn availability() -> Availability {
    match global() {
        None => Availability::NotConfigured,
        Some(cache) => cache.availability(),
    }
}

/// 拉取生效快照包（阻塞 HTTP；只在后台刷新线程调用）
pub(crate) fn fetch_bundle(cfg: &GovConfig) -> Result<GovSnapshot, String> {
    let url = format!(
        "{}/v1/datasets/{}/bundle",
        cfg.base_url.trim_end_matches('/'),
        cfg.dataset_id
    );
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("HTTP client 构建失败: {e}"))?;
    let resp = client
        .get(&url)
        .header("X-Api-Key", &cfg.api_key)
        .send()
        .map_err(|e| format!("拉取失败 {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("拉取失败 {url}: HTTP {status}"));
    }
    let body = resp
        .text()
        .map_err(|e| format!("读取响应失败 {url}: {e}"))?;
    parse_bundle_json(&body)
}

/// bundle 文本 → 治理快照（严格：解析失败/条目不合规 = 整包拒绝，fail-closed）
pub(crate) fn parse_bundle_json(raw: &str) -> Result<GovSnapshot, String> {
    let bundle: DatasetBundle =
        serde_json::from_str(raw).map_err(|e| format!("bundle JSON 解析失败: {e}"))?;
    bundle_to_allowlist(&bundle)
}

/// 快照包 → 批准清单（专集专用：非 mcp 批准条目整包拒绝——批准清单数据集
/// 不允许搭车携带其他内容，消费侧不猜语义）
pub(crate) fn bundle_to_allowlist(bundle: &DatasetBundle) -> Result<GovSnapshot, String> {
    let mut allow = Vec::new();
    for e in &bundle.entries {
        if e.entry_kind != EntryKind::Knowledge {
            return Err(format!(
                "条目 `{}` 非 knowledge 条目：批准清单数据集必须专集专用",
                e.entry_id
            ));
        }
        match e.schema_ref.as_deref() {
            Some(MCP_SERVER_SCHEMA_REF) => allow.push(entry_from_payload(&e.rule_body)?),
            other => {
                return Err(format!(
                    "条目 `{}` schema_ref 非 {MCP_SERVER_SCHEMA_REF}（实际 {other:?}）：批准清单数据集必须专集专用",
                    e.entry_id
                ));
            }
        }
    }
    Ok(GovSnapshot {
        allow: Arc::new(SpawnAllowlist { allow }),
        content_hash: bundle.audit.content_hash.clone(),
    })
}

/// 批准条目 payload → 白名单条目（治理侧录入校验的镜像实现：形状规则 →
/// 归因完整性 → server_key 重算比对；任一不过 = 条目拒绝）
pub(crate) fn entry_from_payload(payload: &serde_json::Value) -> Result<AllowlistEntry, String> {
    let obj = payload
        .as_object()
        .ok_or_else(|| "payload 须为对象".to_string())?;
    let command = obj
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "缺 command（或非字符串）".to_string())?;
    let args_pattern: Option<Vec<String>> = match obj.get("args_pattern") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(a)) => Some(
            a.iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "args_pattern 含非字符串段".to_string())
                })
                .collect::<Result<Vec<String>, String>>()?,
        ),
        Some(_) => return Err("args_pattern 须为字符串数组".to_string()),
    };
    validate_approval_entry(command, args_pattern.as_deref())?;
    for key in ["comment", "approved_by", "approved_at", "approval_ref"] {
        match obj.get(key).and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => {}
            _ => return Err(format!("缺 {key}（或非非空字符串；批准必须带完整归因）")),
        }
    }
    let expected = canonical_server_key(command, args_pattern.as_deref());
    let server_key = match obj.get("server_key").and_then(|v| v.as_str()) {
        Some(k) if k == expected => k.to_string(),
        Some(other) => {
            return Err(format!(
                "server_key 不匹配：期望 {expected}，实际 {other}（以 canonical_server_key 重算为准）"
            ));
        }
        None => return Err(format!("缺 server_key（本条目计算值：{expected}）")),
    };
    let get = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::to_string);
    Ok(AllowlistEntry {
        command: command.to_string(),
        args_pattern,
        comment: get("comment").unwrap_or_default(),
        approved_by: get("approved_by"),
        approved_at: get("approved_at"),
        approval_ref: get("approval_ref"),
        server_key: Some(server_key),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn cfg(ttl: Duration) -> GovConfig {
        GovConfig {
            base_url: "http://127.0.0.1:18081".into(),
            api_key: "k".into(),
            dataset_id: "org.evorule.mcp.servers".into(),
            ttl,
        }
    }

    fn snapshot(entries: Vec<AllowlistEntry>, hash: &str) -> GovSnapshot {
        GovSnapshot {
            allow: Arc::new(SpawnAllowlist { allow: entries }),
            content_hash: hash.into(),
        }
    }

    fn payload_json(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    fn sample_payload() -> serde_json::Value {
        let key = canonical_server_key("npx", Some(&["-y".into(), "@scope/srv-*".into()]));
        payload_json(&format!(
            r#"{{"server_key":"{key}","command":"npx","args_pattern":["-y","@scope/srv-*"],
                "comment":"平台组登记","approved_by":"alice","approved_at":"2026-11-01T00:00:00Z",
                "approval_ref":"org.evorule.mcp.servers@v1"}}"#
        ))
    }

    // ===== 三测之一：命中（条目命中携带 server_key/approval_ref） =====

    #[test]
    fn entry_from_payload_carries_identity() {
        let e = entry_from_payload(&sample_payload()).unwrap();
        assert_eq!(e.command, "npx");
        assert_eq!(
            e.server_key.as_deref(),
            Some(canonical_server_key("npx", Some(&["-y".into(), "@scope/srv-*".into()])).as_str())
        );
        assert_eq!(
            e.approval_ref.as_deref(),
            Some("org.evorule.mcp.servers@v1")
        );
        assert_eq!(e.approved_by.as_deref(), Some("alice"));
        // 命中判定返回条目本体（事件面可携带 server_key/approval_ref 出证）
        let list = SpawnAllowlist { allow: vec![e] };
        let hit = list
            .matches("npx", &["-y".into(), "@scope/srv-abc".into()])
            .expect("pattern hit");
        assert!(hit.server_key.as_deref().unwrap().starts_with("blake3:"));
        assert!(list
            .matches("npx", &["-y".into(), "@evil/pkg".into()])
            .is_none());
    }

    // ===== 条目闸（镜像治理侧校验） =====

    #[test]
    fn entry_from_payload_rejects_bad_entries() {
        // server_key 打错
        let mut bad = sample_payload();
        bad["server_key"] = serde_json::Value::String(format!("blake3:{}", "0".repeat(64)));
        assert!(entry_from_payload(&bad)
            .unwrap_err()
            .contains("server_key 不匹配"));
        // 缺归因
        let mut no_attr = sample_payload();
        no_attr["approved_by"] = serde_json::Value::String("  ".into());
        assert!(entry_from_payload(&no_attr)
            .unwrap_err()
            .contains("approved_by"));
        // 裸解释器无 args_pattern
        let bare = payload_json(
            r#"{"command":"uvx","comment":"c","approved_by":"a","approved_at":"t","approval_ref":"r"}"#,
        );
        assert!(entry_from_payload(&bare).unwrap_err().contains("裸解释器"));
    }

    // ===== bundle 解析（专集专用 + 严格整包拒绝） =====

    fn bundle_json(entries: &[serde_json::Value]) -> String {
        serde_json::json!({
            "bundle_schema_version": "1.0",
            "bundle_id": "b1",
            "dataset": {
                "dataset_id": "org.evorule.mcp.servers",
                "name": "n",
                "tenant_id": "t",
                "instance_id": "i",
                "versioning": { "current": "v1", "history": [] }
            },
            "entries": entries,
            "tests": { "verdict": "pass", "cases": [] },
            "audit": { "exported_at": "t", "exported_by": "u", "source_version": "v1", "content_hash": "blake3:abc" }
        })
        .to_string()
    }

    #[test]
    fn bundle_parse_converts_and_rejects_strictly() {
        let key = canonical_server_key("npx", Some(&["-y".into(), "@scope/srv-*".into()]));
        let entry = serde_json::json!({
            "entry_id": key,
            "entry_kind": "knowledge",
            "rule_body": sample_payload(),
            "schema_ref": MCP_SERVER_SCHEMA_REF,
            "provenance": { "source": "gov" },
            "domain": "mcp",
            "tags": []
        });
        let snap = parse_bundle_json(&bundle_json(std::slice::from_ref(&entry))).unwrap();
        assert_eq!(snap.allow.allow.len(), 1);
        assert_eq!(snap.content_hash, "blake3:abc");

        // 非 knowledge 条目搭车 → 整包拒绝
        let rule_entry = serde_json::json!({
            "entry_id": "r1", "entry_kind": "rule", "rule_body": [],
            "provenance": { "source": "gov" }, "domain": "mcp", "tags": []
        });
        assert!(
            parse_bundle_json(&bundle_json(&[entry.clone(), rule_entry]))
                .unwrap_err()
                .contains("专集专用")
        );

        // 坏 payload → 整包拒绝
        let mut bad = sample_payload();
        bad["server_key"] = serde_json::Value::String("blake3:zz".into());
        let bad_entry = serde_json::json!({
            "entry_id": "e2", "entry_kind": "knowledge", "rule_body": bad,
            "schema_ref": MCP_SERVER_SCHEMA_REF,
            "provenance": { "source": "gov" }, "domain": "mcp", "tags": []
        });
        assert!(parse_bundle_json(&bundle_json(&[bad_entry]))
            .unwrap_err()
            .contains("server_key"));
    }

    // ===== 三测之二：降级（拉取失败/缓存超窗 → 本地文件 + 原因留痕） =====

    #[test]
    fn cache_falls_back_on_failure_and_staleness() {
        let cache = GovCache::new(cfg(Duration::from_millis(50)));
        // 冷启动：从未成功 → Fallback(cold_start)
        match cache.availability() {
            Availability::Fallback {
                reason,
                degraded_for,
            } => {
                assert_eq!(reason, "cold_start");
                assert!(degraded_for.is_none());
            }
            _ => panic!("cold start must fall back"),
        }
        // 拉取失败：仍降级（unreachable）
        assert!(!cache.refresh_once(|_| Err("conn refused".into())));
        match cache.availability() {
            Availability::Fallback { reason, .. } => assert_eq!(reason, "unreachable"),
            _ => panic!("failed fetch must fall back"),
        }
        // 成功：回到治理源
        assert!(cache.refresh_once(|_| Ok(snapshot(
            vec![entry_from_payload(&sample_payload()).unwrap()],
            "h1"
        ))));
        assert!(matches!(cache.availability(), Availability::Governance(_)));
        // 超窗（TTL=50ms 已过，刷新未成功）→ 降级（stale_or_unreachable，携带降级时长）
        std::thread::sleep(Duration::from_millis(60));
        match cache.availability() {
            Availability::Fallback {
                reason,
                degraded_for,
            } => {
                assert_eq!(reason, "stale_or_unreachable");
                assert!(degraded_for.is_some());
            }
            _ => panic!("stale snapshot must fall back"),
        }
        // 恢复：状态迁移后再次回到治理源
        assert!(cache.refresh_once(|_| Ok(snapshot(vec![], "h1"))));
        assert!(matches!(cache.availability(), Availability::Governance(_)));
    }

    // ===== 三测之三：双源（治理源在场时本地文件不参与判定） =====

    #[test]
    fn governance_source_wins_over_local_file() {
        let cache = GovCache::new(cfg(Duration::from_secs(300)));
        // 治理清单只登记 entry-a；本地文件语境里的 entry-b 不在治理清单
        assert!(cache.refresh_once(|_| Ok(snapshot(
            vec![entry_from_payload(&sample_payload()).unwrap()],
            "h1"
        ))));
        let gov_list = match cache.availability() {
            Availability::Governance(s) => s,
            _ => panic!("expected governance source"),
        };
        // 治理清单内 → 放行；治理清单外（哪怕降级文件里可能有）→ enforce 拒载
        assert!(super::super::spawn_gate::gate(
            "s",
            "npx",
            &["-y".into(), "@scope/srv-abc".into()],
            "enforce",
            Some(&gov_list.allow),
            "governance",
        )
        .is_ok());
        let r = super::super::spawn_gate::gate(
            "s",
            "local-only-cmd",
            &[],
            "enforce",
            Some(&gov_list.allow),
            "governance",
        );
        assert!(r.is_err());
        // 未配置治理源 → 本地文件即权威（NotConfigured 路径由 ensure_spawn_allowed 走 load()）
    }
}
