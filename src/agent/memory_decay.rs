// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 共享记忆读取时衰减（读面视图降级，纯策略层）。
//!
//! 定位与边界：
//! - 只作用于**共享分区**读视图。会话分区不适用：会话 payload 是每会话
//!   短生命周期数据（随会话存亡、存在管理器外写者），不存在"长期闲置"
//!   语义；跨会话长期事实账在共享分区，衰减针对的是它读面上的陈旧记忆。
//! - **零物理处理**：判定为归档的记录原值永远留在共享账本，衰减只改读面
//!   视图——原值换为引用卡（与压缩视图同构：账面不瘦，读面变小）。回溯
//!   经 digest / fact_id 锚点走账面 join；投影/证据等原始读取通道不受影响。
//! - **判定可回放**：归档与否是确定性纯函数——有效时间戳（访问戳与写入
//!   时间取较新者）+ 配置闲置上限，对传入的 `now` 参数做比较，无墙钟之外
//!   的隐状态。同一账面数据 + 同一配置重跑判定必得同一结论，因此本批
//!   **判定结果不落账**（无 journal 事件）：结论可由账面数据 + 配置重放
//!   复算，无需事件日志。
//! - **访问戳存储**（最佳努力）：每键一条会话 payload 记录，落点
//!   `__memory__.{ns}.__access_stamps__.shared.{key}`。选型理由（对比
//!   备选）：记录内嵌字段需每次读改写共享账本（会 bump 共享版本号、扰动
//!   前缀索引版本守卫、账本膨胀）——否决；命名空间单记录哈希表需读-改-写
//!   （并发访问互相覆盖、无限增长）——次优。每键一条零读-改-写竞争，且
//!   与前缀索引同款存储哲学：第三结构段固定为 `__access_stamps__`，而
//!   会话键一律嵌在 `session_{sid}.` 之下、共享键在 `shared.` 根之下，
//!   任何用户记忆键在结构上不可达该路径（零碰撞）；经会话 payload 端点
//!   写入，不触碰共享账本版本号。已知最佳努力边界：戳随访问会话的 payload
//!   存亡，跨会话读不可见——此时判定时钟回落写入时间（纯函数对未知戳的
//!   既定输入，行为确定）。
//! - fail-soft 铁律：访问戳读/写任何故障不得影响读结果（回落写入时间时钟
//!   / 静默放弃刷新），衰减分支任何故障不得改变既有读路径的正确性。

use crate::agent::memory::MemoryRecord;

/// 闲置时长上限缺省值（30 天；仅 `enabled` 时生效）
pub const DEFAULT_MAX_IDLE_AGE_SECS: u64 = 30 * 24 * 3600;

/// 读取时衰减策略（缺省 = 关闭，现行为零变化；显式配置激活）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecayPolicy {
    /// 是否启用（缺省 false：读路径整体短路，既有行为逐字节不变）
    pub enabled: bool,
    /// 闲置时长上限（秒；按最后访问时间/写入时间较新者计）
    pub max_idle_age_secs: u64,
}

impl Default for DecayPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_idle_age_secs: DEFAULT_MAX_IDLE_AGE_SECS,
        }
    }
}

impl DecayPolicy {
    /// 设置键消费面：从工作台合并设置映射装配策略。
    ///
    /// 与 `CompactionPolicy::from_settings` 同款装配口径——缺键/类型不符走
    /// 缺省，数值越界 clamp 到设置声明范围（与 settings.rs range 一致）。
    /// `memory.decay.enabled` 缺省 false = 读路径整体短路（缺省不变式）。
    pub fn from_settings(settings: &serde_json::Map<String, serde_json::Value>) -> Self {
        let mut p = Self::default();
        if let Some(v) = settings
            .get("memory.decay.enabled")
            .and_then(serde_json::Value::as_bool)
        {
            p.enabled = v;
        }
        if let Some(v) = settings
            .get("memory.decay.maxIdleAgeSecs")
            .and_then(serde_json::Value::as_f64)
        {
            // clamp 与 settings.rs 声明 range [60, 31536000] 一致
            p.max_idle_age_secs = (v.round() as i64).clamp(60, 31_536_000) as u64;
        }
        p
    }
}

/// 衰减判定的有效时间戳：访问戳与写入时间取**较新者**。
///
/// 取较新者的理由：重写共享键会产生新版本（写入时间更新），而旧访问戳
/// 不会随之刷新——取 max 保证"刚重写过"的记忆绝不被误判归档（判定对
/// 输入确定；两输入皆已知时结论唯一）。访问戳未知（None）→ 回落写入时间。
pub fn decay_effective_ts(last_access: Option<u64>, write_ts: u64) -> u64 {
    match last_access {
        Some(ts) => ts.max(write_ts),
        None => write_ts,
    }
}

/// 归档判定（确定性纯函数，可回放）。
///
/// 归档当且仅当 `now - effective_ts > max_idle_age_secs`（严格大于——与
/// 既有 TTL 判定的 `>` 语义同构：恰在上限当刻不算归档）。禁用时恒 false。
pub fn is_archived(policy: &DecayPolicy, effective_ts: u64, now: u64) -> bool {
    if !policy.enabled {
        return false;
    }
    now.saturating_sub(effective_ts) > policy.max_idle_age_secs
}

/// 归档引用卡锚点（账面 join 面；缺省字段不序列化）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ArchivedCardAnchors {
    /// 共享账本身份锚点（fact_id，回溯经账面 join 取原文）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fact_id: Option<u64>,
    /// 因果锚点（事件投影记录指向的源 fact_id；KV/根事实为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause_fact_id: Option<u64>,
}

/// 归档引用卡（读面降级产物；零原文留存）。
///
/// 卡内只有 digest + 账面锚点（path/ts/fact_id），回溯经账面 join 取原文；
/// `digest` 沿用生态既有 blake3 口径（`evorule_digest`，`"blake3:"+64hex`，
/// 与审计查询超限降级标注同源）——被摘要对象是**原 value 字符串**。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ArchivedCard {
    /// 归档标记（恒 true）
    pub archived: bool,
    /// 原 value 的 blake3 digest（生态既有口径，含前缀）
    pub digest: String,
    /// 共享账本 path（账面 join 锚点）
    pub path: String,
    /// 原记录写入时间（Unix 秒）
    pub ts: u64,
    /// 判定时读到的访问戳（未知时不序列化）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_access: Option<u64>,
    /// 账面锚点（无任何锚点时不序列化）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchors: Option<ArchivedCardAnchors>,
}

/// 从原记录构建归档引用卡（零原文：卡内不含 value 字段）。
pub fn build_archived_card(
    record: &MemoryRecord,
    path: &str,
    last_access: Option<u64>,
) -> ArchivedCard {
    let fact_id = record.fact_id.filter(|f| *f != 0);
    let anchors = if fact_id.is_none() && record.cause_fact_id.is_none() {
        None
    } else {
        Some(ArchivedCardAnchors {
            fact_id,
            cause_fact_id: record.cause_fact_id,
        })
    };
    ArchivedCard {
        archived: true,
        digest: crate::agent::journal::evorule_digest(&record.value),
        path: path.to_string(),
        ts: record.timestamp,
        last_access,
        anchors,
    }
}

/// 访问戳记录的 kind 标识（sanity 校验用；外来数据占据戳路径时判定损坏）
pub const ACCESS_STAMP_KIND: &str = "memory_access_stamp";

/// 共享键访问戳的会话 payload 路径（命名空间层结构段，零碰撞；见模块头）。
pub fn access_stamp_path(namespace: &str, key: &str) -> String {
    format!("__memory__.{namespace}.__access_stamps__.shared.{key}")
}

/// 访问戳记录载荷（serde 形态即存储形态；单键单条，无读-改-写竞争）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AccessStampPayload {
    /// 种类标识（恒 [`ACCESS_STAMP_KIND`]；他值=损坏）
    pub kind: String,
    /// 载荷形态版本
    pub v: u32,
    /// 所属共享键（防外来数据误配；读回时须与请求键一致）
    pub key: String,
    /// 最后访问时间（Unix 秒）
    pub last_access_secs: u64,
}

impl AccessStampPayload {
    /// 构建访问戳载荷
    pub fn new(key: &str, last_access_secs: u64) -> Self {
        Self {
            kind: ACCESS_STAMP_KIND.to_string(),
            v: 1,
            key: key.to_string(),
            last_access_secs,
        }
    }

    /// 内部一致性校验（种类/形态版本/键与请求一致）
    pub fn is_sane(&self, key: &str) -> bool {
        self.kind == ACCESS_STAMP_KIND && self.v == 1 && self.key == key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 纯函数判定：边界三态（恰在上限 / 刚超 / 未到）+ 禁用短路 ----

    #[test]
    fn decay_boundary_exactly_at_age_is_not_archived() {
        // 恰在上限当刻：不归档（与既有 TTL `>` 语义同构）
        let policy = DecayPolicy {
            enabled: true,
            max_idle_age_secs: 1000,
        };
        assert!(!is_archived(&policy, 0, 1000));
    }

    #[test]
    fn decay_boundary_just_over_age_is_archived() {
        let policy = DecayPolicy {
            enabled: true,
            max_idle_age_secs: 1000,
        };
        assert!(is_archived(&policy, 0, 1001));
    }

    #[test]
    fn decay_boundary_just_under_age_is_not_archived() {
        let policy = DecayPolicy {
            enabled: true,
            max_idle_age_secs: 1000,
        };
        assert!(!is_archived(&policy, 0, 999));
    }

    #[test]
    fn decay_disabled_never_archives_even_ancient() {
        // 禁用（含缺省策略）→ 恒不归档：缺省不变式
        let policy = DecayPolicy::default();
        assert!(!policy.enabled);
        assert!(!is_archived(&policy, 0, u64::MAX / 2));
    }

    #[test]
    fn decay_effective_ts_takes_newer_of_stamp_and_write() {
        // 访问戳未知 → 回落写入时间
        assert_eq!(decay_effective_ts(None, 500), 500);
        // 访问戳较旧（重写后未再访问）→ 写入时间胜（刚重写绝不误判归档）
        assert_eq!(decay_effective_ts(Some(100), 500), 500);
        // 访问戳较新 → 访问戳胜
        assert_eq!(decay_effective_ts(Some(900), 500), 900);
    }

    // ---- 归档引用卡：digest 口径 + 零原文留存 ----

    #[test]
    fn decay_card_digest_matches_evorule_digest_of_original() {
        let original = "deploy pipeline rollback quota 42";
        let mut record = MemoryRecord::new("topic", original, 1_700_000_000);
        record.fact_id = Some(52);
        let card = build_archived_card(&record, "shared.ns.topic", Some(1_700_001_000));
        assert!(card.archived);
        assert_eq!(
            card.digest,
            crate::agent::journal::evorule_digest(original),
            "digest 口径 = 生态既有 blake3（对原 value 字符串）"
        );
        assert_eq!(card.path, "shared.ns.topic");
        assert_eq!(card.ts, 1_700_000_000);
        assert_eq!(card.last_access, Some(1_700_001_000));
        let anchors = card.anchors.expect("fact_id 在场时锚点必须序列化");
        assert_eq!(anchors.fact_id, Some(52));
        assert_eq!(anchors.cause_fact_id, None);
    }

    #[test]
    fn decay_card_contains_zero_original_value() {
        let original = "SECRET-ORIGINAL-VALUE-CONTENT";
        let record = MemoryRecord::new("topic", original, 1_700_000_000);
        let card = build_archived_card(&record, "shared.ns.topic", None);
        let serialized = serde_json::to_string(&card).unwrap();
        assert!(
            !serialized.contains(original),
            "归档卡零原文留存: {serialized}"
        );
        assert!(serialized.contains("\"archived\":true"));
        assert!(serialized.contains("\"digest\":\"blake3:"));
        // 无锚点时 anchors 整段缺席
        assert!(!serialized.contains("fact_id"));
    }

    /// epoch 契约：归档卡身份锚=fact_id（跨压实稳定键），卡内零 version
    /// 游标在驻（压实重编号后 version 数值跨 epoch 不连续，锚错位即静默漏数据）
    #[test]
    fn decay_card_anchor_is_fact_id_with_no_version_cursors() {
        let mut record = MemoryRecord::new("topic", "some-value", 1_700_000_000);
        record.fact_id = Some(52);
        let card = build_archived_card(&record, "shared.ns.topic", Some(1_700_001_000));
        let j = serde_json::to_value(&card).unwrap();
        assert_eq!(j["anchors"]["fact_id"], 52, "锚=fact_id");
        let s = j.to_string();
        assert!(
            !s.contains("version"),
            "归档卡不得携带任何 version 游标字段: {s}"
        );
        assert!(s.contains("\"digest\":\"blake3:"));
    }

    // ---- 访问戳载荷与路径 ----

    #[test]
    fn access_stamp_path_is_structurally_collision_free() {
        let p = access_stamp_path("ns", "topic");
        assert_eq!(p, "__memory__.ns.__access_stamps__.shared.topic");
        // 命名空间层第三结构段固定：会话键（session_{sid}. 之下）与共享键
        // （shared. 根之下）在结构上均不可达该路径
        assert!(p.starts_with("__memory__.ns.__access_stamps__.shared."));
    }

    #[test]
    fn access_stamp_payload_sanity() {
        let payload = AccessStampPayload::new("topic", 1_700_000_000);
        assert!(payload.is_sane("topic"));
        assert!(!payload.is_sane("other"), "键不匹配=损坏");
        // 反序列化形态往返
        let value = serde_json::to_value(&payload).unwrap();
        let back: AccessStampPayload = serde_json::from_value(value).unwrap();
        assert!(back.is_sane("topic"));
        assert_eq!(back.last_access_secs, 1_700_000_000);
        // 外来数据（kind 不符）判损坏
        let foreign: AccessStampPayload = serde_json::from_value(
            serde_json::json!({"kind":"x","v":1,"key":"topic","last_access_secs":1}),
        )
        .unwrap();
        assert!(!foreign.is_sane("topic"));
    }

    // ---- 设置键消费面（settings→policy 装配映射） ----

    #[test]
    fn from_settings_enabled_reflects_configured_keys() {
        let map = serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(
            serde_json::json!({
                "memory.decay.enabled": true,
                "memory.decay.maxIdleAgeSecs": 3600
            }),
        )
        .unwrap();
        let policy = DecayPolicy::from_settings(&map);
        assert!(
            policy.enabled,
            "显式开启必须生效（开关存在但从不被消费=死开关）"
        );
        assert_eq!(policy.max_idle_age_secs, 3600, "上限键装配进策略");
    }

    #[test]
    fn from_settings_absent_yields_default_off() {
        // 键全缺省（多数用户未配置面）→ 关闭 + 缺省上限
        let empty = serde_json::Map::new();
        let policy = DecayPolicy::from_settings(&empty);
        assert!(!policy.enabled);
        assert_eq!(policy.max_idle_age_secs, DEFAULT_MAX_IDLE_AGE_SECS);
        // enabled=false 显式关闭 → 关闭
        let map = serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(
            serde_json::json!({"memory.decay.enabled": false, "memory.decay.maxIdleAgeSecs": 60}),
        )
        .unwrap();
        let policy = DecayPolicy::from_settings(&map);
        assert!(!policy.enabled);
    }

    #[test]
    fn from_settings_value_clamps_and_type_mismatch_falls_back() {
        // 越界 clamp 到设置声明范围 [60, 31536000]（照既有装配口径）
        let map = serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(
            serde_json::json!({
                "memory.decay.enabled": true,
                "memory.decay.maxIdleAgeSecs": 30
            }),
        )
        .unwrap();
        let policy = DecayPolicy::from_settings(&map);
        assert_eq!(policy.max_idle_age_secs, 60);
        let map = serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(
            serde_json::json!({
                "memory.decay.enabled": true,
                "memory.decay.maxIdleAgeSecs": 99_999_999
            }),
        )
        .unwrap();
        assert_eq!(
            DecayPolicy::from_settings(&map).max_idle_age_secs,
            31_536_000
        );
        // 类型不符（bool 键给字符串）→ 走缺省（不猜）
        let map = serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(
            serde_json::json!({"memory.decay.enabled": "true"}),
        )
        .unwrap();
        assert!(!DecayPolicy::from_settings(&map).enabled);
    }
}
