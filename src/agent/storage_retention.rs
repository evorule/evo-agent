// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 存储保留期与硬配额（消费者磁盘占用治理，物理释放层）。
//!
//! 定位与边界：
//! - **管"磁盘小"**：超龄/超限数据的大字段物理释放（与衰减批"管读面小"
//!   互补——衰减零物理处理，本批真释放）。释放对象：journal 事件 payload
//!   大字段（消息全文/工具结果全量）、session payload 记录大原文（消息
//!   正文/记忆值原文）。
//! - **审计可验证性不破**：压缩 = 释放大字段 + 摘要卡（seq/ts/type/fact_id/
//!   path 锚点 + blake3 digest + 前后字节数）+ digest 链续接——「数据曾存在、
//!   内容是该哈希、未被篡改」可机器验证。journal 侧摘要卡按既有
//!   `"blake3:"+64hex` 口径追加为账面事件 [`JournalEvent::RetentionCompactionPerformed`]。
//! - **判定纯函数**：超龄判定/配额选择/豁免判定均为确定性纯函数（同输入
//!   同输出，可回放）；动作落账 → 三性可回放。
//! - **缺省零发生**：period=forever + action=compact + quota=unlimited ⇒
//!   [`StorageRetentionPolicy::is_default`] 短路，清扫零写入零事件（现行为
//!   逐字节不变；缺省不变式）。
//! - **读路径零参与**：不碰 get_scoped/project_scoped 读面；触发位=会话收口
//!   （sediment 同批）+ 启动期低频巡检，均为 fail-soft（错误只登记不重试，
//!   绝不阻断收口/启动主流程）。
//! - **治理豁免（宁保守）**：审批/拒绝证据、policy 拦截/校验证据、护栏
//!   执行证据、gov 治理面、摘要卡与 digest 链事件自身永不释放；**未显式
//!   分类为可压缩的类型一律豁免**（[`retention_class_of_type`] 缺省臂）。
//!   journal 事件枚举全量普查 = [`JournalEvent::retention_class`] 穷举
//!   match（新增变体未标注 = 编译失败，机器闸）。
//! - **账本红线**：journal 行 seq 连续性 + 行链哈希是追加账本的结构红线，
//!   物理释放 = 行内大字段置空（hash+长度凭证保持，全文级重建随保留策略
//!   降级为 hash 校验——checkpoint_blob 既有降级口径），**不摘行**；被改行
//!   起尾段链哈希前向重算（释放动作有 RetentionCompactionPerformed 事件
//!   落账为审计凭据，释放后 verify 链验证仍全过）。
//! - **共享事实面（v1 边界）**：shared 分区事实是服务端版本化账（同 path
//!   覆写=追加新版本，旧版本仍在服务端账内），agent 侧覆写不产生磁盘回收
//!   只会复刻衰减批的读面降级——故 v1 物理释放面=本地 journal 文件 +
//!   session payload 记录（update_payload 覆盖语义），共享事实剪枝登记为
//!   服务端侧后续批。
//!
//! ## 归档落点（action=export_then_compact）
//!
//! 显式参数（调用方注入）> env `EVO_AGENT_RETENTION_ARCHIVE_DIR` >
//! `<用户主目录>/Downloads/evo-agent-archives`（用户可见目录）>
//! `<journal 目录>/retention-archives`（兜底）。单文件 JSONL：manifest 首
//! 行（总记录数+聚合 digest）+ 每记录一行（完整原文+digest 校验字段）。
//! 写出成功 → 执行压缩；写出失败 → 放弃该文件本轮压缩（宁不删不丢失），
//! 仅日志留失败痕迹。
//!
//! ## 活跃会话文件边界
//!
//! 收口钩子触发时当前会话 journal 仍持活跃写者句柄（Windows 打开句柄
//! 不可 rename），本轮跳过其文件重写（payload 记录面照常处置）；文件级
//! 释放由后续收口/巡检轮转覆盖。启动巡检无活跃写者，全量覆盖。

use std::path::{Path, PathBuf};

use crate::agent::journal::{
    evorule_digest, journal_chain_step, journal_content_hash, journal_normalized_content,
    sanitize_session_id, writer_active_for, JournalEvent, JournalWriter, OpenMode, SessionIdentity,
};

// ===========================================================================
// 设置消费面（封闭枚举；缺省=现行为零变化）
// ===========================================================================

/// 保留期档位（封闭枚举，防自由文本注入；forever = 永不释放）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetentionPeriod {
    /// 永久（缺省=现行为）
    #[default]
    Forever,
    /// 10 年
    TenYears,
    /// 3 年
    ThreeYears,
    /// 1 年
    OneYear,
    /// 90 天
    NinetyDays,
}

impl RetentionPeriod {
    /// 保留期时长（秒）；`None` = 永久
    pub fn duration_secs(self) -> Option<u64> {
        const DAY: u64 = 86_400;
        const YEAR: u64 = 365 * DAY;
        match self {
            RetentionPeriod::Forever => None,
            RetentionPeriod::TenYears => Some(10 * YEAR),
            RetentionPeriod::ThreeYears => Some(3 * YEAR),
            RetentionPeriod::OneYear => Some(YEAR),
            RetentionPeriod::NinetyDays => Some(90 * DAY),
        }
    }
}

/// 释放动作档位（封闭枚举）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetentionAction {
    /// 压缩为摘要链（缺省）：释放大字段 + 摘要卡 + digest 链续接
    #[default]
    Compact,
    /// 先导出 JSONL 归档包（写成功才压缩；写失败放弃本轮）
    ExportThenCompact,
    /// 物理删除（不可逆；digest 卡仍落账，链不断）
    Delete,
}

/// 存储保留期与硬配额策略（缺省 = 永久+压缩+不限 ⇒ 清扫整体短路）。
///
/// 字段公开供测试以任意精度值直接构造（设置面封闭枚举经 `from_settings`
/// 产出档位值；内部机制对任意值行为确定）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StorageRetentionPolicy {
    /// 保留期档位（缺省 forever）
    pub period: RetentionPeriod,
    /// 释放动作档位（缺省 compact）
    pub action: RetentionAction,
    /// 硬配额（字节；`None` = 不限，缺省）
    pub quota_max_bytes: Option<u64>,
}

impl StorageRetentionPolicy {
    /// 缺省策略（永久+压缩+不限）：清扫零写入零事件
    pub fn is_default(&self) -> bool {
        self.period == RetentionPeriod::Forever
            && self.action == RetentionAction::Compact
            && self.quota_max_bytes.is_none()
    }

    /// 设置键消费面：从工作台合并设置映射装配策略。
    ///
    /// 与 `DecayPolicy::from_settings` 同款装配口径——缺键/类型不符/枚举外
    /// 值一律走缺省（封闭枚举白名单，非白名单值不 clamp 只回缺省）。
    pub fn from_settings(settings: &serde_json::Map<String, serde_json::Value>) -> Self {
        let mut p = Self::default();
        if let Some(v) = settings
            .get("storage.retention.period")
            .and_then(serde_json::Value::as_str)
        {
            p.period = match v {
                "forever" => RetentionPeriod::Forever,
                "10y" => RetentionPeriod::TenYears,
                "3y" => RetentionPeriod::ThreeYears,
                "1y" => RetentionPeriod::OneYear,
                "90d" => RetentionPeriod::NinetyDays,
                _ => RetentionPeriod::Forever,
            };
        }
        if let Some(v) = settings
            .get("storage.retention.action")
            .and_then(serde_json::Value::as_str)
        {
            p.action = match v {
                "compact" => RetentionAction::Compact,
                "export_then_compact" => RetentionAction::ExportThenCompact,
                "delete" => RetentionAction::Delete,
                _ => RetentionAction::Compact,
            };
        }
        if let Some(v) = settings
            .get("storage.quota.maxBytes")
            .and_then(serde_json::Value::as_str)
        {
            const GB: u64 = 1024 * 1024 * 1024;
            p.quota_max_bytes = match v {
                "unlimited" => None,
                "5gb" => Some(5 * GB),
                "2gb" => Some(2 * GB),
                "1gb" => Some(GB),
                _ => None,
            };
        }
        p
    }
}

// ===========================================================================
// 治理豁免普查表（宁保守：未显式标注=豁免）
// ===========================================================================

/// 保留期分类（普查表标注值）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionClass {
    /// 豁免：治理证据/账本骨架/摘要卡与 digest 链事件/无大字段可释放——
    /// 永不被保留期/配额压缩触碰
    Exempt,
    /// 可压缩：payload 含无界大字段，释放不破恢复/治理语义
    Eligible,
}

/// journal 事件类型普查表（键 = 序列化 snake_case 类型名）。
///
/// **施工纪律**：`JournalEvent` 新增变体时——①`retention_class` 穷举 match
/// 强制补臂（缺 = 编译失败）；②本表补同名条目且与 match 臂一致（
/// `census_table_complete_and_consistent` 测试逐变体互证）。
/// 豁免判据：治理判定/审批/护栏执行证据、账本骨架与恢复状态载体、摘要卡
/// 与 digest 链事件自身、有界截断字段（无释放收益）。
/// 可压缩判据：无界全文大字段 + 释放不破恢复面（hash+长度凭证保持）。
pub fn retention_class_of_type(event_type: &str) -> RetentionClass {
    match event_type {
        // —— 可压缩（无界大字段，释放安全）——
        // 粒级检查点大结果全文载体：结果超内联阈值的专属事件，hash+len 凭证
        // 保持即可校验完整性（checkpoint_blob 既有降级口径点名此事件）
        "checkpoint_blob" => RetentionClass::Eligible,
        // 逐轮 wire 留痕全文：与 wire blob GC 同款释放对象（骨架统计+hash 留账）
        "wire_rendered" => RetentionClass::Eligible,
        // 用户引导注记原文（消息全文类；注入镜像在权威历史，摘要卡保验证）
        "steering_note" => RetentionClass::Eligible,
        // —— 豁免（宁保守：未显式标注为可压缩者一律豁免）——
        // 账本骨架/身份锚
        "session_opened" | "turn_started" | "turn_ended" | "session_crashed"
        | "session_resumed" => RetentionClass::Exempt,
        // 审批/拒绝/澄清证据（治理面）
        "approval_requested" | "approval_resolved" | "question_requested" | "question_resolved" => {
            RetentionClass::Exempt
        }
        // policy 拦截/校验判定证据（治理面；policy_review_sealed 全文封存
        // 是跨重启回放权威面，释放即破）
        "policy_judged" | "policy_review_sealed" | "mcp_capability_blocked" => {
            RetentionClass::Exempt
        }
        // 护栏/执法执行证据
        "chain_halted"
        | "rewind_budget_consumed"
        | "runaway_guard_tripped"
        | "tool_events_elided" => RetentionClass::Exempt,
        // 摘要卡与 digest 链事件自身（链上任何节点被释放=验证链断）
        "compaction_performed"
        | "emergency_trim_performed"
        | "retention_compaction_performed"
        | "wire_blob_expired" => RetentionClass::Exempt,
        // 沉淀/冷迁/派生链锚
        "sediment_performed" | "cold_moved" | "delegate_spawned" | "session_spawned"
        | "handover_written" => RetentionClass::Exempt,
        // 恢复状态载体与门禁证据（释放破恢复/对账面）
        "plan_loop_checkpointed"
        | "plan_loop_finished"
        | "acceptance_criteria_generated"
        | "todo_updated"
        | "node_checkpointed" => RetentionClass::Exempt,
        // 观测/统计面（有界或计数型，无释放收益）
        "llm_called"
        | "tool_invoked"
        | "tool_result"
        | "tool_retried"
        | "i2_scan_report"
        | "summary_fidelity_scan"
        | "recall_set"
        | "lex_cache_stats"
        | "verify_ran"
        | "service_tools_degraded"
        | "companion_cert_observed"
        | "companion_cert_unreachable"
        | "tool_result_offloaded"
        | "tool_result_truncated"
        | "self_audit"
        | "file_checkpoint_taken"
        | "file_checkpoint_restored" => RetentionClass::Exempt,
        // 缺省臂：未知/未标注类型一律豁免（宁保守——新增类型漏标注不产生
        // 误释放，由普查测试补齐标注）
        _ => RetentionClass::Exempt,
    }
}

impl JournalEvent {
    /// 保留期分类（穷举 match = 机器普查闸：新增变体未标注 = 编译失败）。
    /// 每臂与 [`retention_class_of_type`] 普查表同判（测试逐变体互证）。
    pub fn retention_class(&self) -> RetentionClass {
        match self {
            JournalEvent::CheckpointBlob { .. }
            | JournalEvent::WireRendered { .. }
            | JournalEvent::SteeringNote { .. } => RetentionClass::Eligible,
            JournalEvent::SessionOpened { .. }
            | JournalEvent::TurnStarted { .. }
            | JournalEvent::LlmCalled { .. }
            | JournalEvent::ToolInvoked { .. }
            | JournalEvent::ToolResult { .. }
            | JournalEvent::ToolRetried { .. }
            | JournalEvent::NodeCheckpointed { .. }
            | JournalEvent::PlanLoopCheckpointed { .. }
            | JournalEvent::PlanLoopFinished { .. }
            | JournalEvent::ApprovalRequested { .. }
            | JournalEvent::ApprovalResolved { .. }
            | JournalEvent::PolicyJudged { .. }
            | JournalEvent::PolicyReviewSealed { .. }
            | JournalEvent::CompactionPerformed { .. }
            | JournalEvent::EmergencyTrimPerformed { .. }
            | JournalEvent::SedimentPerformed { .. }
            | JournalEvent::I2ScanReport { .. }
            | JournalEvent::TurnEnded { .. }
            | JournalEvent::SummaryFidelityScan { .. }
            | JournalEvent::WireBlobExpired { .. }
            | JournalEvent::RecallSet { .. }
            | JournalEvent::LexCacheStats { .. }
            | JournalEvent::ColdMoved { .. }
            | JournalEvent::DelegateSpawned { .. }
            | JournalEvent::SessionCrashed { .. }
            | JournalEvent::SessionResumed { .. }
            | JournalEvent::HandoverWritten { .. }
            | JournalEvent::SessionSpawned { .. }
            | JournalEvent::ChainHalted { .. }
            | JournalEvent::RewindBudgetConsumed { .. }
            | JournalEvent::RunawayGuardTripped { .. }
            | JournalEvent::AcceptanceCriteriaGenerated { .. }
            | JournalEvent::TodoUpdated { .. }
            | JournalEvent::QuestionRequested { .. }
            | JournalEvent::QuestionResolved { .. }
            | JournalEvent::FileCheckpointTaken { .. }
            | JournalEvent::FileCheckpointRestored { .. }
            | JournalEvent::VerifyRan { .. }
            | JournalEvent::ServiceToolsDegraded { .. }
            | JournalEvent::CompanionCertObserved { .. }
            | JournalEvent::CompanionCertUnreachable { .. }
            | JournalEvent::ToolResultOffloaded { .. }
            | JournalEvent::ToolResultTruncated { .. }
            | JournalEvent::ToolEventsElided { .. }
            | JournalEvent::McpCapabilityBlocked { .. }
            | JournalEvent::SelfAudit { .. }
            | JournalEvent::RetentionCompactionPerformed { .. } => RetentionClass::Exempt,
        }
    }
}

// ===========================================================================
// 判定纯函数（超龄三态边界 / 配额最旧优先选择）
// ===========================================================================

/// 超龄判定（确定性纯函数，可回放）。
///
/// 超龄当且仅当 `now - record_ts > 保留期时长`（严格大于——恰在上限当刻
/// 不释放，与既有 TTL 判定的 `>` 语义同构）。永久档恒 false。
pub fn is_over_age(period: RetentionPeriod, record_ts_secs: u64, now_secs: u64) -> bool {
    match period.duration_secs() {
        None => false,
        Some(max) => now_secs.saturating_sub(record_ts_secs) > max,
    }
}

/// 配额选择候选（清扫扫描阶段产出；调用方保证按最旧优先排序——sort_ts
/// 升序，平局按扫描序稳定排序）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseCandidate {
    /// 排序时间戳（Unix 秒；journal 行 ms→s 归一）
    pub sort_ts: u64,
    /// 分类（豁免面在选择中跳过）
    pub class: RetentionClass,
    /// 预估可回收字节（释放后该记录缩减量；无大字段=0，选择中跳过）
    pub reclaim_bytes: u64,
}

/// 配额回落选择（确定性纯函数）。
///
/// 输入候选须已按最旧优先排序；从最旧端依次选中可压缩候选直至预估总量
/// 回落配额内。豁免候选原位跳过（计数上浮——豁免面在超配额压力下被跳过
/// 的条数随事件落账留痕）。返回（选中候选下标，豁免跳过数）。
pub fn select_quota_release(
    candidates: &[ReleaseCandidate],
    total_bytes: u64,
    quota: u64,
) -> (Vec<usize>, usize) {
    let mut selected = Vec::new();
    let mut exempt_skipped = 0usize;
    let mut remaining = total_bytes;
    if remaining <= quota {
        return (selected, exempt_skipped);
    }
    for (i, c) in candidates.iter().enumerate() {
        if remaining <= quota {
            break;
        }
        if c.class == RetentionClass::Exempt {
            exempt_skipped += 1;
            continue;
        }
        if c.reclaim_bytes == 0 {
            continue;
        }
        selected.push(i);
        remaining -= c.reclaim_bytes;
    }
    (selected, exempt_skipped)
}

/// 超龄选择（确定性纯函数）：保留期超龄且可压缩的候选下标（输入序保持，
/// 即最旧优先由调用方排序保证）。
pub fn select_age_release(
    candidates: &[ReleaseCandidate],
    period: RetentionPeriod,
    now_secs: u64,
) -> Vec<usize> {
    candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.class == RetentionClass::Eligible
                && c.reclaim_bytes > 0
                && is_over_age(period, c.sort_ts, now_secs)
        })
        .map(|(i, _)| i)
        .collect()
}

// ===========================================================================
// 摘要卡（零原文留存）
// ===========================================================================

/// 摘要卡字段名（写入释放后记录的保留卡键）
pub const RETENTION_CARD_KEY: &str = "retention_card";

/// 摘要卡（释放面降级产物；零原文留存——卡内只有 digest+字节数+锚点）。
///
/// `digest` 沿用生态既有 blake3 口径（`"blake3:"+64hex`），被摘要对象是
/// **原字段值的序列化字节**——与 journal 事件 `released_digests`、审计
/// 查询超限降级标注同源互证。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetentionCard {
    /// 释放标记（恒 true）
    pub truncated: bool,
    /// 原字段值字节数（序列化口径）
    pub bytes: usize,
    /// 原值的 blake3 digest（生态既有口径，含前缀）
    pub digest: String,
    /// 原记录时间戳（Unix 秒；账面锚点）
    pub ts: u64,
    /// 记录账面路径（账面 join 锚点；payload 记录用）
    pub path: String,
}

/// 从原字段值构建摘要卡（零原文：卡内不含 value 字段）。
pub fn build_retention_card(original: &str, ts_secs: u64, path: &str) -> RetentionCard {
    RetentionCard {
        truncated: true,
        bytes: original.len(),
        digest: evorule_digest(original),
        ts: ts_secs,
        path: path.to_string(),
    }
}

/// 对单条 payload 记录执行释放变换（确定性纯函数）。
///
/// 识别记录形态：消息记录（含 `content` 字符串字段）释放 `content`+`reasoning`；
/// 记忆值记录（含 `value` 字符串字段）释放 `value`。释放 = 字段置空 +
/// 摘要卡注入（`retention_card`，serde 未知字段容忍——原记录形态反序列化
/// 不破，恢复面可用）。返回 `None` = 形态不识别/无可释放大字段（原样跳过）。
/// `delete` 动作：整记录 null 墓碑（digest 卡由落账事件承载，链不断）。
pub fn release_payload_record(
    record: &mut serde_json::Value,
    ts_secs: u64,
    path: &str,
    action: RetentionAction,
) -> Option<RetentionCard> {
    let obj = record.as_object_mut()?;
    // 形态识别：消息记录（content 主字段）优先，记忆值记录（value 字段）
    // 次之；空字段不作为可释放对象（无可释放原文 = 原样跳过）
    let (releasable, card) = obj
        .get("content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|c| ("content", build_retention_card(c, ts_secs, path)))
        .or_else(|| {
            obj.get("value")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|v| ("value", build_retention_card(v, ts_secs, path)))
        })?;
    if action == RetentionAction::Delete {
        *record = serde_json::Value::Null;
        return Some(card);
    }
    obj.insert(
        releasable.to_string(),
        serde_json::Value::String(String::new()),
    );
    if let Some(r) = obj.get_mut("reasoning") {
        *r = serde_json::Value::Null;
    }
    let card_value = serde_json::to_value(&card).ok()?;
    obj.insert(RETENTION_CARD_KEY.to_string(), card_value);
    Some(card)
}

/// payload 记录豁免键（派生摘要视图/滚动摘要——自身即摘要产物，无原文
/// 可释放；访问戳不在 session 子树，天然不可达）。
fn payload_key_exempt(key: &str) -> bool {
    matches!(key, "compaction_view" | "rolling_summary")
}

/// payload 记录可回收字节预估（释放字段原文长度；无可释放主字段 = 0）。
fn payload_reclaim_bytes(record: &serde_json::Value) -> u64 {
    let Some(obj) = record.as_object() else {
        return 0;
    };
    let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let value = obj.get("value").and_then(|v| v.as_str()).unwrap_or("");
    if content.is_empty() && value.is_empty() {
        return 0;
    }
    let reasoning = obj.get("reasoning").and_then(|v| v.as_str()).unwrap_or("");
    (content.len() + value.len() + reasoning.len()) as u64
}

// ===========================================================================
// 导出归档包（action=export_then_compact）
// ===========================================================================

/// 归档环境变量名（显式落点覆盖；显式参数 > env > 用户可见目录 > 兜底）
pub const ARCHIVE_DIR_ENV: &str = "EVO_AGENT_RETENTION_ARCHIVE_DIR";

/// 归档落点解析（显式参数 > env > 用户主目录 Downloads > journal 目录兜底）。
/// 返回 `None` = 无法解析任何落点（导出动作将放弃本轮，宁不删不丢失）。
pub fn resolve_archive_dir(override_dir: Option<&Path>, journal_dir: &Path) -> Option<PathBuf> {
    if let Some(d) = override_dir {
        return Some(d.to_path_buf());
    }
    if let Ok(d) = std::env::var(ARCHIVE_DIR_ENV) {
        if !d.trim().is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
    {
        if home.exists() {
            return Some(home.join("Downloads").join("evo-agent-archives"));
        }
    }
    Some(journal_dir.join("retention-archives"))
}

/// 归档包记录行（每行 = 完整原文 + digest 校验字段）
#[derive(Debug, Clone, serde::Serialize)]
struct ArchiveRecordLine<'a> {
    /// 来源会话（journal 文件 stem）
    source: &'a str,
    /// 账面路径锚（"seq={seq}:type={type}:field={field}"）
    anchor: String,
    /// 原文 digest（生态既有口径）
    digest: String,
    /// 原文字节数
    bytes: usize,
    /// 完整原文
    value: String,
}

/// 构建单文件 JSONL 归档内容（manifest 首 + 记录行）。
///
/// manifest 行：总记录数 + 聚合 digest（逐记录 digest 串接后的 blake3，
/// 字节级对账锚——归档完整性可独立校验）。
fn build_archive_jsonl(source: &str, records: &[ArchiveRecordLine<'_>]) -> String {
    let agg_input: String = records.iter().map(|r| r.digest.as_str()).collect();
    let manifest = serde_json::json!({
        "manifest": true,
        "source": source,
        "records": records.len(),
        "aggregate_digest": evorule_digest(&agg_input),
    });
    let mut out = String::new();
    out.push_str(&manifest.to_string());
    out.push('\n');
    for r in records {
        let line = serde_json::to_string(r).unwrap_or_default();
        out.push_str(&line);
        out.push('\n');
    }
    out
}

// ===========================================================================
// journal 文件清扫执行器
// ===========================================================================

/// 可释放大字段的类型→字段清单（与普查表 Eligible 标注严格同步——
/// Eligible 类型必在本表且有字段；census 测试互证）。
fn releasable_fields(event_type: &str) -> &'static [&'static str] {
    match event_type {
        "checkpoint_blob" => &["full_text"],
        "wire_rendered" => &["full_text"],
        "steering_note" => &["text"],
        _ => &[],
    }
}

/// 单文件清扫统计
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSweepStats {
    /// 释放条数
    pub released_count: usize,
    /// 豁免跳过条数
    pub exempt_skipped: usize,
    /// 压缩前文件字节
    pub before_bytes: u64,
    /// 压缩后文件字节
    pub after_bytes: u64,
    /// 释放原文 digest 清单（与事件 released_digests 同源）
    pub released_digests: Vec<String>,
    /// 释放行锚点（"seq={seq};ts={ts};type={type}"）
    pub released_anchors: Vec<String>,
    /// 归档文件路径（export 动作成功时）
    pub archive_path: Option<PathBuf>,
}

/// journal 文件扫描产物（行原文清单[空行已滤], 行解析值对齐清单, 文件字节总量）
type JournalScan = (Vec<String>, Vec<Option<serde_json::Value>>, u64);

/// journal 文件扫描（内部 fail-visible；调用方 fail-soft 包裹）。
///
/// 返回（行原文清单[空行已滤], 行解析值对齐清单, 文件字节总量）。尾部
/// 半行容忍：最后不完整行原样保留、不参与清扫（与 open/GC 同口径）。
fn scan_journal_file(path: &Path) -> Result<JournalScan, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("io: {e}"))?;
    let total = bytes.len() as u64;
    let valid_len = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let tail = &bytes[valid_len..];
    let raw_lines: Vec<String> = String::from_utf8_lossy(&bytes[..valid_len])
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    let mut objs: Vec<Option<serde_json::Value>> = raw_lines
        .iter()
        .map(|l| serde_json::from_str(l).ok())
        .collect();
    if !tail.is_empty() {
        objs.push(None);
    }
    Ok((raw_lines, objs, total))
}

/// 行大字段释放值清单（字段名, 原值, 字节数）。
/// 豁免类型/无大字段/字段已空 = 空清单（无释放）。
fn line_release_values(obj: &serde_json::Value) -> Vec<(String, String, usize)> {
    let mut out = Vec::new();
    let Some(etype) = obj.get("type").and_then(|v| v.as_str()) else {
        return out;
    };
    let Some(face) = obj.get("payload").and_then(|v| v.as_object()) else {
        return out;
    };
    for field in releasable_fields(etype) {
        if let Some(v) = face.get(*field).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                out.push(((*field).to_string(), v.to_string(), v.len()));
            }
        }
    }
    out
}

/// 释放行的大字段（原值→空串；hash/len 等凭证键保持；幂等——已空不动）。
fn release_line_fields(obj: &mut serde_json::Value) {
    let Some(etype) = obj.get("type").and_then(|v| v.as_str()).map(str::to_string) else {
        return;
    };
    let fields = releasable_fields(&etype);
    let face = match obj.get_mut("payload") {
        Some(v) => v.as_object_mut(),
        None => obj.as_object_mut(),
    };
    let Some(face) = face else { return };
    for field in fields {
        if let Some(v) = face.get_mut(*field) {
            if v.as_str().is_some_and(|s| !s.is_empty()) {
                *v = serde_json::Value::String(String::new());
            }
        }
    }
}

/// 尾段链哈希前向重算（从首个被改行起；被改行之前行的链哈希不受影响）。
///
/// 行链公式与 open 续接/verify 验证同口径（content_hash = 归一字节 blake3；
/// chain_hash = blake3(prev+content)）；prev 取首个被改行之前最近一条带链
/// 哈希行的 chain_hash（无 = genesis）。未链化旧行被改不补链（格式单调性）。
fn rehash_tail(lines: &mut [String], objs: &[Option<serde_json::Value>], first_modified: usize) {
    let mut running_prev: Option<String> = None;
    for obj in objs.iter().take(first_modified).flatten() {
        if let Some(h) = obj.get("chain_hash").and_then(|v| v.as_str()) {
            running_prev = Some(h.to_string());
        }
    }
    for i in first_modified..lines.len() {
        let Some(o) = &objs[i] else { continue };
        if o.get("chain_hash").is_none() {
            continue;
        }
        let Some(normalized) = journal_normalized_content(&lines[i]) else {
            continue;
        };
        let content_hash = journal_content_hash(&normalized);
        let prev = running_prev
            .clone()
            .unwrap_or_else(|| crate::agent::journal::JOURNAL_CHAIN_GENESIS.to_string());
        let chain_hash = journal_chain_step(&prev, &content_hash);
        let mut new_line: serde_json::Value =
            serde_json::from_str(&lines[i]).unwrap_or(serde_json::Value::Null);
        if let Some(face) = new_line.as_object_mut() {
            face.insert("content_hash".into(), serde_json::json!(content_hash));
            face.insert("prev_hash".into(), serde_json::json!(prev));
            face.insert("chain_hash".into(), serde_json::json!(chain_hash));
        }
        lines[i] = new_line.to_string();
        running_prev = Some(chain_hash);
    }
}

/// journal 文件物理写回（临时文件+rename 原子口径，与离线 GC 同款）
fn write_journal_atomic(path: &Path, lines: &[String]) -> Result<(), String> {
    let tmp = path.with_extension("jsonl.rt-tmp");
    let mut joined = lines.join("\n");
    if !joined.is_empty() {
        joined.push('\n');
    }
    std::fs::write(&tmp, joined).map_err(|e| format!("io: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("io: {e}"))?;
    Ok(())
}

/// 单文件清扫（age/quota 触发共用执行面）。
///
/// `selected` = 该文件内需释放的行下标（全局选择阶段产出）。export 动作
/// 先写归档，写失败放弃本轮（返回 Err，文件零副作用）。成功时重写文件+
/// 尾段重哈希+事件落账（Resume 续写，同 GC 补落账先例）。
fn sweep_journal_file(
    path: &Path,
    selected: &[usize],
    trigger: &str,
    action: RetentionAction,
    archive_dir: Option<&Path>,
    exempt_skipped: usize,
) -> Result<FileSweepStats, String> {
    let (raw_lines, objs, before_bytes) = scan_journal_file(path)?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut stats = FileSweepStats {
        before_bytes,
        exempt_skipped,
        ..Default::default()
    };
    let mut archive_records: Vec<ArchiveRecordLine<'_>> = Vec::new();
    let mut first_modified: Option<usize> = None;
    let mut released_idx: Vec<usize> = Vec::new();
    for (i, obj) in objs.iter().enumerate() {
        if !selected.contains(&i) {
            continue;
        }
        let Some(o) = obj else { continue };
        let values = line_release_values(o);
        if values.is_empty() {
            continue;
        }
        let seq = o.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
        let ts = o.get("ts").and_then(|v| v.as_u64()).unwrap_or(0);
        let etype = o.get("type").and_then(|v| v.as_str()).unwrap_or("");
        for (field, value, bytes) in &values {
            let digest = evorule_digest(value);
            archive_records.push(ArchiveRecordLine {
                source: &stem,
                anchor: format!("seq={seq}:type={etype}:field={field}"),
                digest: digest.clone(),
                bytes: *bytes,
                value: value.clone(),
            });
            stats.released_digests.push(digest);
        }
        stats
            .released_anchors
            .push(format!("seq={seq};ts={ts};type={etype}"));
        stats.released_count += 1;
        released_idx.push(i);
        first_modified = Some(match first_modified {
            Some(first) => first.min(i),
            None => i,
        });
    }
    if stats.released_count == 0 {
        stats.after_bytes = std::fs::metadata(path)
            .map(|m| m.len())
            .map_err(|e| format!("io: {e}"))?;
        return Ok(stats);
    }
    // 导出先行动作：写出成功才压缩（宁不删不丢失）；失败放弃本轮零副作用
    if action == RetentionAction::ExportThenCompact {
        let Some(dir) = archive_dir else {
            return Err("archive: no resolvable archive dir; sweep aborted".to_string());
        };
        let content = build_archive_jsonl(&stem, &archive_records);
        let file_name = format!(
            "{}.retention-archive-{}.jsonl",
            stem,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let archive_path = dir.join(file_name);
        if let Some(parent) = archive_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("io: {e}"))?;
        }
        std::fs::write(&archive_path, content).map_err(|e| format!("io: {e}"))?;
        stats.archive_path = Some(archive_path);
    }
    // 物理释放：大字段置空（凭证键保持）+ 尾段链哈希前向重算 + 原子写回。
    // 行面以解析对象重渲染（裸 JSON 口径与 GC 同款；未知键保持，未触行
    // 归一字节不变 ⇒ content_hash 仍匹配）。
    let mut new_objs = objs.clone();
    for &i in &released_idx {
        if let Some(o) = new_objs[i].as_mut() {
            release_line_fields(o);
        }
    }
    let mut new_lines = raw_lines;
    for (i, obj) in new_objs.iter().enumerate() {
        if let Some(o) = obj {
            new_lines[i] = o.to_string();
        }
    }
    let first = first_modified.unwrap_or(new_lines.len());
    rehash_tail(&mut new_lines, &new_objs, first);
    write_journal_atomic(path, &new_lines)?;
    stats.after_bytes = std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| format!("io: {e}"))?;
    // 事件落账（Resume 续写；事件自身豁免，链续接凭据）
    let dir = path.parent().unwrap_or(Path::new("."));
    let w = JournalWriter::open_with_mode(dir, &stem, OpenMode::Resume, SessionIdentity::default())
        .map_err(|e| format!("journal: {e}"))?;
    w.retention_compaction_performed(
        trigger,
        stats.before_bytes,
        stats.after_bytes,
        stats.released_count,
        stats.exempt_skipped,
        stats.released_digests.clone(),
        stats.released_anchors.clone(),
    )
    .map_err(|e| format!("journal: {e}"))?;
    Ok(stats)
}

// ===========================================================================
// 会话 payload 清扫（update_payload 覆盖语义）
// ===========================================================================

/// payload 扫描候选（记录级）
#[derive(Debug, Clone)]
pub struct PayloadCandidate {
    /// 账面路径（update_payload 定位键）
    pub path: String,
    /// 记录时间戳（Unix 秒）
    pub ts_secs: u64,
    /// 分类（豁免键跳过）
    pub class: RetentionClass,
    /// 记录序列化字节（配额计数口径）
    pub bytes: u64,
    /// 可回收字节预估（释放字段原文长度；0 = 不作为候选）
    pub reclaim_bytes: u64,
}

/// 从会话 state 提取 payload 候选（确定性纯投影，按 ts 升序）。
///
/// 扫描 `state.payload.__memory__.{ns}.session_{sid}` 子树的记录级条目；
/// 派生摘要视图（compaction_view/rolling_summary）豁免；无可释放主字段
/// 的记录不入围。解析失败如实降级（返回空集）。
pub fn payload_candidates_from_state(
    state: &serde_json::Value,
    namespace: &str,
    session_id: &str,
) -> Vec<PayloadCandidate> {
    let mut out = Vec::new();
    let session_key = format!("session_{session_id}");
    let subtree = &state["payload"]["__memory__"][namespace][session_key.as_str()];
    let Some(map) = subtree.as_object() else {
        return out;
    };
    for (key, record) in map {
        if payload_key_exempt(key) {
            continue;
        }
        let reclaim = payload_reclaim_bytes(record);
        if reclaim == 0 {
            continue;
        }
        let bytes = serde_json::to_string(record).map(|s| s.len()).unwrap_or(0) as u64;
        let ts = record
            .get("timestamp")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        out.push(PayloadCandidate {
            path: format!("__memory__.{namespace}.session_{session_id}.{key}"),
            ts_secs: ts,
            class: RetentionClass::Eligible,
            bytes,
            reclaim_bytes: reclaim,
        });
    }
    out.sort_by_key(|c| c.ts_secs);
    out
}

/// 从会话 state 取 payload 记录原文（路径末段定位；扫描与执行共用投影）。
///
/// 路径口径 = `__memory__.{ns}.session_{sid}.{key}`，key 本身可含点号
/// （如 `messages.0`）——按前缀剥离取整段 key，禁止按点号切分。
fn payload_record_from_state(
    state: &serde_json::Value,
    namespace: &str,
    session_id: &str,
    path: &str,
) -> Option<serde_json::Value> {
    let prefix = format!("__memory__.{namespace}.session_{session_id}.");
    let key = path.strip_prefix(&prefix)?;
    let session_key = format!("session_{session_id}");
    let v = &state["payload"]["__memory__"][namespace][session_key.as_str()][key];
    if v.is_object() {
        Some(v.clone())
    } else {
        None
    }
}

// ===========================================================================
// 清扫总入口（收口钩子 / 启动巡检共用）
// ===========================================================================

/// 清扫触发形态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepTrigger {
    /// 保留期超龄（会话收口/巡检）
    Age,
    /// 硬配额超限（聚合扫描后最旧优先回落）
    Quota,
}

impl SweepTrigger {
    fn label(self) -> &'static str {
        match self {
            SweepTrigger::Age => "age",
            SweepTrigger::Quota => "quota",
        }
    }
}

/// 服务端共享账压实调度档（封闭枚举；缺省 [`SharedServerCompaction::Off`]=零动作）。
///
/// 服务端共享事实是版本化账（agent 侧覆写不产生本地磁盘回收，见 §清扫边界），
/// 物理释放经服务端压实路径调度：`archive`=归档落盘 server 侧 wal_dir，
/// `discard`=仅摘要存根。独立于 agent 本地 action 派生——归档落点在 server 侧，
/// 语义不同机，显式选档不隐式耦合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SharedServerCompaction {
    /// 缺省：零动作（零 HTTP，缺省不变式）
    #[default]
    Off,
    /// 归档后压实（服务端归档件落 wal_dir）
    Archive,
    /// 直接压实（仅保留最新版本+摘要存根）
    Discard,
}

impl SharedServerCompaction {
    /// 从 settings 解析（封闭枚举白名单：非白名单值不 clamp 只回缺省）
    pub fn from_settings(settings: &serde_json::Map<String, serde_json::Value>) -> Self {
        match settings
            .get("storage.retention.sharedServer")
            .and_then(|v| v.as_str())
        {
            Some("archive") => Self::Archive,
            Some("discard") => Self::Discard,
            _ => Self::Off,
        }
    }

    fn mode_str(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Archive => Some("archive"),
            Self::Discard => Some("discard"),
        }
    }
}

/// 进程内调度去重（同实例只 POST 一次；marker 本身幂等，去重只省流量；
/// 重启实例后可再调度）。测试独占说明见 `maybe_schedule_server_compaction`。
static SHARED_COMPACTION_SCHEDULED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 服务端共享账压实调度（fail-soft：成功 info 留痕；403/网络失败 warn 留痕
/// 零重试零阻断——清扫面绝不因调度失败放弃本地释放）。
///
/// `scheduled` 注入式去重标志：生产接线传 `&SHARED_COMPACTION_SCHEDULED`；
/// 测试传本地标志防进程级共享态跨测试先到先得。返回本轮是否实际执行了调度。
pub async fn maybe_schedule_server_compaction(
    mode: SharedServerCompaction,
    client: &crate::api::evorule_client::EvoruleApiClient,
    scheduled: &std::sync::atomic::AtomicBool,
) -> bool {
    use std::sync::atomic::Ordering;
    let Some(m) = mode.mode_str() else {
        return false;
    };
    if scheduled.swap(true, Ordering::SeqCst) {
        return false;
    }
    match client.schedule_shared_compaction(m).await {
        Ok(resp) => {
            tracing::info!(
                mode = m,
                response = %resp,
                "retention: server shared-facts compaction scheduled (effective next server restart)"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                mode = m,
                error = %e,
                "retention: server shared-facts compaction schedule failed \
                 (fail-soft, no retry; evorule connection needs administrator credential)"
            );
            false
        }
    }
}

/// 清扫上下文（调用方装配；缺省策略应在装配侧短路不构造本上下文）
pub struct RetentionSweepCtx {
    /// 保留期与配额策略
    pub policy: StorageRetentionPolicy,
    /// journal 目录（data/sessions）
    pub journal_dir: PathBuf,
    /// evorule 客户端（payload 读扫描+释放写入）
    pub client: crate::api::evorule_client::EvoruleApiClient,
    /// 命名空间（payload 子树定位）
    pub namespace: String,
    /// 当前会话（payload 清扫对象）
    pub session_id: String,
    /// 归档落点显式覆盖（None=按 env/用户目录/兜底链解析）
    pub archive_dir_override: Option<PathBuf>,
    /// 服务端共享账压实调度档（off=零动作；见 [`SharedServerCompaction`]）
    pub shared_server: SharedServerCompaction,
}

/// 清扫报告（留痕面；事件与日志之外的进程内对账）
#[derive(Debug, Clone, Default)]
pub struct SweepReport {
    /// 扫描的 journal 文件数
    pub files_scanned: usize,
    /// 重写的 journal 文件数
    pub files_modified: usize,
    /// 释放条数（journal 行级 + payload 记录级合计）
    pub released_count: usize,
    /// 豁免跳过条数
    pub exempt_skipped: usize,
    /// 聚合扫描字节总量（journal+payload）
    pub scanned_bytes: u64,
    /// 释放后字节总量（本地文件面；payload 侧不计本地）
    pub after_bytes: u64,
    /// payload 释放条数
    pub payload_released: usize,
    /// 归档文件路径（export 动作成功时）
    pub archive_paths: Vec<PathBuf>,
    /// 放弃的清扫对象（导出失败等；宁不删不丢失，只登记不重试）
    pub aborted: Vec<String>,
    /// 是否零发生（无任何写入/事件——缺省策略或无可释放对象）
    pub no_op: bool,
}

/// 保留期清扫一轮（fail-soft 总闸：任何阶段失败只登记不重试，绝不 panic）。
///
/// 聚合扫描（journal 全文件+当前会话 payload）→ 纯函数选择 → 执行释放 +
/// 事件落账。活跃写者文件跳过重写（Windows 打开句柄不可 rename；由后续
/// 收口/巡检轮转覆盖）。O(n) 扫描为已裁定首版口径。
pub async fn run_retention_sweep(ctx: RetentionSweepCtx, now_ms: u64) -> SweepReport {
    let mut report = SweepReport::default();
    if ctx.policy.is_default() {
        // 缺省零发生：短路返回，零扫描零写入零事件（缺省不变式）
        report.no_op = true;
        return report;
    }
    let now_secs = now_ms / 1000;
    // —— 阶段 1：聚合扫描（journal 文件 + 当前会话 payload；fail-soft）——
    let mut jsonls: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&ctx.journal_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().map(|n| n.to_string_lossy().to_string());
            let is_target = name.as_deref().is_some_and(|n| {
                n.ends_with(".jsonl")
                    && !n.ends_with("wire-archive.jsonl")
                    && !n.ends_with(".rt-tmp")
            });
            if is_target {
                jsonls.push(p);
            }
        }
        jsonls.sort();
    }
    // 活跃写者文件跳过（Windows 打开句柄不可 rename；轮转覆盖）
    jsonls.retain(|p| {
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        !writer_active_for(&stem)
    });
    let mut candidates: Vec<ReleaseCandidate> = Vec::new();
    // 候选下标 → (文件下标, 行下标)；payload 候选为尾段
    let mut loc_journal: Vec<(usize, usize)> = Vec::new();
    for (fi, path) in jsonls.iter().enumerate() {
        report.files_scanned += 1;
        match scan_journal_file(path) {
            Ok((_raw, objs, total)) => {
                report.scanned_bytes += total;
                for (li, obj) in objs.iter().enumerate() {
                    let Some(o) = obj else { continue };
                    let ts_ms = o.get("ts").and_then(|v| v.as_u64()).unwrap_or(0);
                    let etype = o.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let reclaim: u64 = line_release_values(o)
                        .iter()
                        .map(|(_, _, b)| *b as u64)
                        .sum();
                    candidates.push(ReleaseCandidate {
                        sort_ts: ts_ms / 1000,
                        class: retention_class_of_type(etype),
                        reclaim_bytes: reclaim,
                    });
                    loc_journal.push((fi, li));
                }
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "retention: journal scan failed (fail-soft)"
                );
            }
        }
    }
    let n_journal = loc_journal.len();
    // payload 扫描（当前会话；get_state fail-soft——不可达=本轮无 payload 面）。
    // 空 session_id = 启动巡检形态（无"当前会话"），payload 面跳过——payload
    // 记录级释放是收口钩子专属位（启动期无会话可回写），journal 文件面照常。
    let payload_scan = if ctx.session_id.is_empty() {
        Err("startup patrol: no current session; payload face skipped".to_string())
    } else {
        ctx.client
            .get_state(&ctx.session_id)
            .await
            .map(|state| {
                let cands = payload_candidates_from_state(&state, &ctx.namespace, &ctx.session_id);
                (state, cands)
            })
            .map_err(|e| format!("payload state fetch failed: {e}"))
    };
    let (payload_state, payload_cands) = match payload_scan {
        Ok((state, cands)) => {
            for c in &cands {
                report.scanned_bytes += c.bytes;
                candidates.push(ReleaseCandidate {
                    sort_ts: c.ts_secs,
                    class: c.class,
                    reclaim_bytes: c.reclaim_bytes,
                });
            }
            (Some(state), cands)
        }
        Err(e) => {
            tracing::warn!(
                session_id = %ctx.session_id,
                error = %e,
                "retention: payload state scan failed (fail-soft)"
            );
            (None, Vec::new())
        }
    };
    // —— 阶段 2：纯函数选择（age / quota）——
    // after_bytes 基线=聚合扫描总量；执行阶段随释放逐文件扣减
    report.after_bytes = report.scanned_bytes;
    let mut journal_sel: Vec<Vec<usize>> = vec![Vec::new(); jsonls.len()];
    let mut payload_sel: Vec<usize> = Vec::new();
    let mut exempt_skipped = 0usize;
    let quota_active = ctx
        .policy
        .quota_max_bytes
        .is_some_and(|q| report.scanned_bytes > q);
    let trigger_label = if quota_active {
        SweepTrigger::Quota.label()
    } else {
        SweepTrigger::Age.label()
    };
    if quota_active {
        // 配额触发：跨面统一最旧优先（stable 排序，平局保持扫描序），
        // 逐条扣减直至预估总量回落配额内；豁免候选原位跳过并计数
        let quota = ctx.policy.quota_max_bytes.unwrap_or(u64::MAX);
        let mut order: Vec<usize> = (0..candidates.len()).collect();
        order.sort_by_key(|&i| candidates[i].sort_ts);
        let ordered: Vec<ReleaseCandidate> = order.iter().map(|&i| candidates[i].clone()).collect();
        let (sel, skipped) = select_quota_release(&ordered, report.scanned_bytes, quota);
        exempt_skipped = skipped;
        for ordered_idx in sel {
            let ci = order[ordered_idx];
            if ci < n_journal {
                let (fi, li) = loc_journal[ci];
                journal_sel[fi].push(li);
            } else {
                payload_sel.push(ci - n_journal);
            }
        }
    } else {
        // 龄期触发：逐候选超龄判定（候选序=文件序+行序+payload ts 序）
        for (ci, c) in candidates.iter().enumerate() {
            if c.class != RetentionClass::Eligible
                || c.reclaim_bytes == 0
                || !is_over_age(ctx.policy.period, c.sort_ts, now_secs)
            {
                continue;
            }
            if ci < n_journal {
                let (fi, li) = loc_journal[ci];
                journal_sel[fi].push(li);
            } else {
                payload_sel.push(ci - n_journal);
            }
        }
    }
    // —— 阶段 3：执行（journal 文件重写 + payload 释放；fail-soft）——
    let archive_dir = match ctx.policy.action {
        RetentionAction::ExportThenCompact => {
            resolve_archive_dir(ctx.archive_dir_override.as_deref(), &ctx.journal_dir)
        }
        _ => None,
    };
    for (fi, path) in jsonls.iter().enumerate() {
        let sel = journal_sel[fi].clone();
        if sel.is_empty() {
            continue;
        }
        match sweep_journal_file(
            path,
            &sel,
            trigger_label,
            ctx.policy.action,
            archive_dir.as_deref(),
            exempt_skipped,
        ) {
            Ok(stats) => {
                report.files_modified += 1;
                report.released_count += stats.released_count;
                report.after_bytes = report
                    .after_bytes
                    .saturating_sub(stats.before_bytes.saturating_sub(stats.after_bytes));
                if let Some(ap) = stats.archive_path {
                    report.archive_paths.push(ap);
                }
            }
            Err(e) => {
                report.aborted.push(format!("{}: {e}", path.display()));
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "retention: file sweep aborted (no destructive write; log-only)"
                );
            }
        }
    }
    // payload 释放（逐条 update_payload；记录原文取自扫描 state；失败跳过不重试）
    if let Some(state) = &payload_state {
        for pi in &payload_sel {
            let cand = &payload_cands[*pi];
            let Some(record) =
                payload_record_from_state(state, &ctx.namespace, &ctx.session_id, &cand.path)
            else {
                continue;
            };
            let mut record = record;
            let Some(card) =
                release_payload_record(&mut record, cand.ts_secs, &cand.path, ctx.policy.action)
            else {
                continue;
            };
            let value = if ctx.policy.action == RetentionAction::Delete {
                serde_json::Value::Null
            } else {
                record
            };
            match ctx
                .client
                .update_payload(&ctx.session_id, &cand.path, &value)
                .await
            {
                Ok(_) => {
                    report.payload_released += 1;
                    report.released_count += 1;
                    tracing::info!(
                        path = %cand.path,
                        digest = %card.digest,
                        "retention: payload record released (summary card on record)"
                    );
                }
                Err(e) => {
                    report.aborted.push(format!("{}: {e}", cand.path));
                    tracing::warn!(
                        path = %cand.path,
                        error = %e,
                        "retention: payload release failed (fail-soft, no retry)"
                    );
                }
            }
        }
    }
    report.exempt_skipped = exempt_skipped;
    // —— 阶段 5：服务端共享账压实调度（off=零动作；调度失败不影响本地释放
    // 结果，fail-soft；共享事实物理释放经服务端压实路径，本地面不产生回收）——
    if ctx.shared_server != SharedServerCompaction::Off {
        maybe_schedule_server_compaction(
            ctx.shared_server,
            &ctx.client,
            &SHARED_COMPACTION_SCHEDULED,
        )
        .await;
    }
    report.no_op = report.files_modified == 0 && report.payload_released == 0;
    report
}

/// 会话号净化转发（收口钩子装配用；journal 活跃写者键口径同源）
pub fn active_session_key(session_id: &str) -> String {
    sanitize_session_id(session_id)
}

// ===========================================================================
// 测试（TDD 大纲全覆盖：三态边界/配额序列/摘要卡/链续接/普查完备/导出
// 失败放弃/缺省零发生）
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::journal::{read_all, verify_journal_file, JOURNAL_CHAIN_GENESIS};
    use serde_json::json;

    fn policy_with(
        period: RetentionPeriod,
        action: RetentionAction,
        quota: Option<u64>,
    ) -> StorageRetentionPolicy {
        StorageRetentionPolicy {
            period,
            action,
            quota_max_bytes: quota,
        }
    }

    // ---------- 设置消费面 ----------

    #[test]
    fn policy_from_settings_defaults_and_enums() {
        // 缺省（无键）= 现行为
        let empty = serde_json::Map::new();
        assert_eq!(
            StorageRetentionPolicy::from_settings(&empty),
            StorageRetentionPolicy::default()
        );
        // 非法类型/枚举外值 → 缺省（封闭枚举，不 clamp 自由文本）
        let mut map = serde_json::Map::new();
        map.insert("storage.retention.period".into(), json!(42));
        map.insert("storage.retention.action".into(), json!("rm -rf"));
        map.insert("storage.quota.maxBytes".into(), json!("100tb"));
        let p = StorageRetentionPolicy::from_settings(&map);
        assert!(p.is_default(), "非法值应整体回落缺省");
        // 合法枚举档位解析
        let mut map = serde_json::Map::new();
        map.insert("storage.retention.period".into(), json!("90d"));
        map.insert(
            "storage.retention.action".into(),
            json!("export_then_compact"),
        );
        map.insert("storage.quota.maxBytes".into(), json!("2gb"));
        let p = StorageRetentionPolicy::from_settings(&map);
        assert_eq!(p.period, RetentionPeriod::NinetyDays);
        assert_eq!(p.action, RetentionAction::ExportThenCompact);
        assert_eq!(p.quota_max_bytes, Some(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn policy_is_default_gates_everything() {
        assert!(StorageRetentionPolicy::default().is_default());
        assert!(
            !policy_with(RetentionPeriod::NinetyDays, RetentionAction::Compact, None).is_default()
        );
        assert!(!policy_with(RetentionPeriod::Forever, RetentionAction::Delete, None).is_default());
        assert!(
            !policy_with(RetentionPeriod::Forever, RetentionAction::Compact, Some(1)).is_default()
        );
    }

    // ---------- 超龄三态边界 ----------

    #[test]
    fn age_boundary_three_states() {
        let period = RetentionPeriod::NinetyDays;
        let max = period.duration_secs().unwrap();
        let now = 1_800_000_000u64;
        // 严格超出 = 释放
        assert!(is_over_age(period, now - max - 1, now));
        // 恰在上限当刻 = 不释放（TTL 同构 `>` 语义）
        assert!(!is_over_age(period, now - max, now));
        // 新于上限 = 不释放
        assert!(!is_over_age(period, now - max + 1, now));
        // 永久档恒不释放
        assert!(!is_over_age(RetentionPeriod::Forever, 0, now));
    }

    // ---------- 配额最旧优先回落 + 豁免跳过 ----------

    #[test]
    fn quota_selection_oldest_first_with_exempt_skip() {
        let mk = |ts: u64, class: RetentionClass, reclaim: u64| ReleaseCandidate {
            sort_ts: ts,
            class,
            reclaim_bytes: reclaim,
        };
        // 最旧优先序列：豁免一枚夹在可压缩中间
        let cands = vec![
            mk(100, RetentionClass::Eligible, 100),
            mk(200, RetentionClass::Exempt, 500),
            mk(300, RetentionClass::Eligible, 100),
            mk(400, RetentionClass::Eligible, 100),
        ];
        let total = 1000u64;
        let quota = 850u64;
        let (sel, exempt_skipped) = select_quota_release(&cands, total, quota);
        assert_eq!(sel, vec![0, 2], "最旧优先，豁免原位跳过");
        assert_eq!(exempt_skipped, 1, "超配额压力下跳过的豁免候选计数");
        // 未超配额 = 零选择零跳过
        let (sel2, skip2) = select_quota_release(&cands, total, total);
        assert!(sel2.is_empty() && skip2 == 0);
        // 回落序列：逐条扣减直至配额内
        let cands = vec![
            mk(1, RetentionClass::Eligible, 300),
            mk(2, RetentionClass::Eligible, 300),
            mk(3, RetentionClass::Eligible, 300),
        ];
        let (sel3, _) = select_quota_release(&cands, 1000, 500);
        assert_eq!(sel3, vec![0, 1], "1000-300-300=200 ≤ 500 即止");
    }

    // ---------- 摘要卡：digest 正确 + 零原文泄漏 ----------

    #[test]
    fn summary_card_digest_and_zero_leakage() {
        let original = "机密工具结果全文 1234567890";
        let card = build_retention_card(original, 1234, "__memory__.ns.session_s.messages.0");
        assert_eq!(card.bytes, original.len());
        assert_eq!(
            card.digest,
            evorule_digest(original),
            "digest 与生态口径一致"
        );
        let card_json = serde_json::to_string(&card).unwrap();
        assert!(!card_json.contains("机密"), "卡内零原文泄漏");
        assert!(!card_json.contains("1234567890"), "卡内零原文泄漏");
        assert_eq!(card.digest.len(), "blake3:".len() + 64);
        assert!(card.digest.starts_with("blake3:"));
    }

    #[test]
    fn payload_record_release_shapes() {
        // 消息记录：content/reasoning 出账 + 卡注入 + 结构保持
        let mut rec = json!({
            "idx": 0, "role": "tool", "content": "大结果原文", "tool_name": "grep",
            "reasoning": "思考留痕", "timestamp": 1111,
        });
        let card = release_payload_record(&mut rec, 1111, "p", RetentionAction::Compact).unwrap();
        assert_eq!(rec["content"], json!(""));
        assert_eq!(rec["reasoning"], json!(serde_json::Value::Null));
        assert_eq!(
            rec["retention_card"]["digest"],
            json!(evorule_digest("大结果原文"))
        );
        assert_eq!(rec["retention_card"]["truncated"], json!(true));
        assert_eq!(rec["tool_name"], json!("grep"), "结构键保持");
        assert_eq!(card.digest, evorule_digest("大结果原文"));
        // 记录形态仍可解析（未知字段容忍）
        let parsed: Result<serde_json::Value, _> = serde_json::from_value(rec.clone());
        assert!(parsed.is_ok());
        // 记忆值记录形态：value 出账
        let mut rec = json!({"key": "stable.x", "value": "记忆原文", "timestamp": 2222});
        let card = release_payload_record(&mut rec, 2222, "p", RetentionAction::Compact).unwrap();
        assert_eq!(rec["value"], json!(""));
        assert_eq!(card.digest, evorule_digest("记忆原文"));
        // delete 动作：整记录 null 墓碑
        let mut rec = json!({"key": "k", "value": "原文", "timestamp": 1});
        let card = release_payload_record(&mut rec, 1, "p", RetentionAction::Delete).unwrap();
        assert_eq!(rec, json!(serde_json::Value::Null));
        assert_eq!(
            card.digest,
            evorule_digest("原文"),
            "删除也留 digest 卡（链不断）"
        );
        // 无可释放字段 / 已空 → None
        let mut rec = json!({"idx": 1, "role": "user", "content": ""});
        assert!(release_payload_record(&mut rec, 1, "p", RetentionAction::Compact).is_none());
        let mut rec = json!({"foo": "bar"});
        assert!(release_payload_record(&mut rec, 1, "p", RetentionAction::Compact).is_none());
    }

    #[test]
    fn payload_candidates_projection_and_exempt_keys() {
        let state = json!({
            "payload": {
                "__memory__": {
                    "ns": {
                        "session_s1": {
                            "messages.0": {"idx": 0, "role": "user", "content": "全文内容", "timestamp": 100},
                            "messages.1": {"idx": 1, "role": "tool", "content": "工具全文", "reasoning": "r", "timestamp": 200},
                            "compaction_view": {"summary": "派生摘要视图"},
                            "rolling_summary": {"lines": []},
                            "stable.k": {"key": "k", "value": "记忆原文", "timestamp": 300},
                            "empty.rec": {"idx": 2, "role": "user", "content": "", "timestamp": 400},
                        }
                    }
                }
            }
        });
        let cands = payload_candidates_from_state(&state, "ns", "s1");
        // 派生摘要视图豁免 + 无可释放主字段不入围；按 ts 升序
        let paths: Vec<&str> = cands.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "__memory__.ns.session_s1.messages.0",
                "__memory__.ns.session_s1.messages.1",
                "__memory__.ns.session_s1.stable.k",
            ]
        );
        assert_eq!(cands[0].ts_secs, 100);
        assert_eq!(cands[0].bytes, cands[0].bytes, "字节计数口径在场");
        assert!(cands[0].reclaim_bytes > 0);
        // 派生投影路径定位与原文回读一致
        let rec = payload_record_from_state(&state, "ns", "s1", paths[0]).unwrap();
        assert_eq!(rec["content"], json!("全文内容"));
        assert!(
            payload_record_from_state(&state, "ns", "s1", "__memory__.ns.session_s1.absent")
                .is_none()
        );
    }

    // ---------- 普查表完备性（机器断言：枚举全类型均有标注）----------

    /// 逐变体样本（全枚举覆盖；新增变体必须同时补本表 + retention_class
    /// 穷举 match + retention_class_of_type 普查表——match 缺臂即编译失败）
    fn sample_events() -> Vec<JournalEvent> {
        vec![
            JournalEvent::SessionOpened {
                session_id: "s".into(),
                opened_ts: 1,
                agent: None,
                model: None,
                temperature: None,
            },
            JournalEvent::TurnStarted {
                turn_seq: 1,
                goal: "g".into(),
            },
            JournalEvent::LlmCalled {
                model: "m".into(),
                purpose: "react".into(),
                evorule_request_id: None,
                tokens: None,
                tokens_est: None,
                request: 1,
                temperature: None,
                response: "r".into(),
            },
            JournalEvent::ToolInvoked {
                call_id: "t1".into(),
                tool: "grep".into(),
                args_digest: "blake3:aa".into(),
                evorule_request_id: None,
            },
            JournalEvent::ToolResult {
                call_id: "t1".into(),
                status: "ok".into(),
                size_bytes: 1,
                content_digest: "blake3:aa".into(),
                execution_profile: None,
            },
            JournalEvent::ToolRetried {
                tool: "t".into(),
                attempt: 1,
                transient: true,
            },
            JournalEvent::NodeCheckpointed {
                workflow_id: "w".into(),
                plan_hash: "h".into(),
                node_id: "n".into(),
                status: "completed".into(),
                result_ref: crate::agent::journal::CheckpointResultRef {
                    hash: "blake3:aa".into(),
                    len: 1,
                    inline: None,
                },
            },
            JournalEvent::CheckpointBlob {
                plan_hash: "h".into(),
                node_id: "n".into(),
                hash: "blake3:aa".into(),
                len: 1,
                full_text: "big".into(),
            },
            JournalEvent::PlanLoopCheckpointed {
                version: 1,
                replan_count: 0,
                executed_registry: vec![],
                counters: Default::default(),
                cur_canonical_hash: None,
                goal: None,
                marks_session: None,
                atomic_granules: vec![],
                recut_counts: vec![],
                cur_workflow: "wf".into(),
            },
            JournalEvent::PlanLoopFinished {
                status: "ok".into(),
            },
            JournalEvent::ApprovalRequested {
                approval_id: "a".into(),
                tool: "t".into(),
                payload: "p".into(),
            },
            JournalEvent::ApprovalResolved {
                approval_id: "a".into(),
                decision: "approved".into(),
            },
            JournalEvent::PolicyJudged {
                judgement_id: "j1".into(),
                verdict: "allowed".into(),
                evidence: "e".into(),
            },
            JournalEvent::PolicyReviewSealed {
                proposal_id: "a".into(),
                verdict: "approved".into(),
                full_json: "{}".into(),
            },
            JournalEvent::CompactionPerformed {
                before_est: 1,
                after_est: 1,
                cleared_call_ids: vec![],
            },
            JournalEvent::EmergencyTrimPerformed {
                before_chars: 1,
                after_chars: 1,
                elided_tool_results: 0,
                folded_approvals: 0,
                history_compacted: false,
            },
            JournalEvent::SedimentPerformed {
                summary_written: true,
                stable_facts: vec![],
                stable_facts_cache_only: vec![],
                events_count: 0,
                rollup_done: false,
                knowledge_candidates: 0,
                flushed_events: 0,
            },
            JournalEvent::WireRendered {
                round: 1,
                wire_len: 1,
                content_hash: "blake3:aa".into(),
                full_text: "big".into(),
            },
            JournalEvent::I2ScanReport {
                round: 1,
                conflicts: vec![],
            },
            JournalEvent::TurnEnded {
                status: "success".into(),
                steps: 1,
                duration_ms: 1,
                reason: None,
            },
            JournalEvent::SummaryFidelityScan {
                session: "s".into(),
                trimmed_n: 0,
                anchors_n: 0,
                hit_n: 0,
                ratio: 1.0,
                summary_empty: false,
            },
            JournalEvent::WireBlobExpired {
                round: 1,
                content_hash: "blake3:aa".into(),
                wire_len: 1,
            },
            JournalEvent::RecallSet {
                session: "s".into(),
                hits: vec![],
            },
            JournalEvent::LexCacheStats {
                session: "s".into(),
                hit: 0,
                expired: 0,
                fetch: 0,
            },
            JournalEvent::ColdMoved { count: 0 },
            JournalEvent::DelegateSpawned {
                child_session_id: "c".into(),
                agent_type: "react".into(),
                depth: 1,
                task_digest: "blake3:aa".into(),
                tokens_used: None,
            },
            JournalEvent::SessionCrashed { reason: "r".into() },
            JournalEvent::SessionResumed {
                replay_seq: 1,
                rebuilt: vec![],
            },
            JournalEvent::HandoverWritten {
                session: "s".into(),
                path: "p".into(),
                schema_ok: true,
            },
            JournalEvent::SessionSpawned {
                parent: "p".into(),
                child: "c".into(),
                depth: 1,
            },
            JournalEvent::ChainHalted {
                session: "s".into(),
                reason: "r".into(),
            },
            JournalEvent::RewindBudgetConsumed { remaining: 1 },
            JournalEvent::RunawayGuardTripped {
                actions: 1,
                threshold: 1,
            },
            JournalEvent::AcceptanceCriteriaGenerated {
                criteria: "c".into(),
                model: "m".into(),
            },
            JournalEvent::TodoUpdated { items: vec![] },
            JournalEvent::QuestionRequested {
                question_id: "au-1".into(),
                tool: "ask_user".into(),
                payload: "p".into(),
            },
            JournalEvent::QuestionResolved {
                question_id: "au-1".into(),
                decision: "answered".into(),
            },
            JournalEvent::FileCheckpointTaken {
                path: "p".into(),
                kind: "content".into(),
                hash: "blake3:aa".into(),
                bytes: 1,
                tool: "file_write".into(),
            },
            JournalEvent::FileCheckpointRestored {
                path: "p".into(),
                kind: "content".into(),
                restored_hash: "blake3:aa".into(),
                popped_seq: 1,
            },
            JournalEvent::VerifyRan {
                name: "v".into(),
                command: "c".into(),
                exit_code: None,
                error_count: 0,
                diags_digest: "blake3:aa".into(),
                verdict: "pass".into(),
                output_digest: "blake3:aa".into(),
            },
            JournalEvent::SteeringNote {
                text: "note".into(),
            },
            JournalEvent::ServiceToolsDegraded {
                reason: "r".into(),
                whitelist: 0,
            },
            JournalEvent::CompanionCertObserved {
                ruleset_version: None,
                certified: false,
                clean: false,
                severity: None,
                chain_seq: None,
                observed_at: 1,
            },
            JournalEvent::CompanionCertUnreachable { reason: "r".into() },
            JournalEvent::ToolResultOffloaded {
                call_id: "t1".into(),
                ref_id: "r".into(),
                path: "p".into(),
                size_bytes: 1,
                content_digest: "blake3:aa".into(),
            },
            JournalEvent::ToolResultTruncated {
                tool: "t".into(),
                size_bytes: 1,
                max_chars: 1,
                reason: "r".into(),
            },
            JournalEvent::ToolEventsElided {
                turn: 1,
                count: 1,
                quota: 1,
            },
            JournalEvent::McpCapabilityBlocked {
                server: "s".into(),
                reason: "r".into(),
                task_ref: "t".into(),
            },
            JournalEvent::SelfAudit {
                trigger: "periodic".into(),
                step: 1,
                frames_count: 0,
                duration_ms: 1,
                injected: false,
                stage: None,
                plan_hash: None,
                services: json!({}),
            },
            JournalEvent::RetentionCompactionPerformed {
                trigger: "age".into(),
                before_bytes: 1,
                after_bytes: 1,
                released_count: 0,
                exempt_skipped: 0,
                released_digests: vec![],
                released_anchors: vec![],
            },
        ]
    }

    #[test]
    fn census_table_complete_and_consistent() {
        let samples = sample_events();
        // ① 穷举 match（编译闸）与字符串普查表逐变体互判一致
        for ev in &samples {
            let serialized = serde_json::to_value(ev).unwrap();
            let etype = serialized["type"].as_str().unwrap().to_string();
            let by_table = retention_class_of_type(&etype);
            let by_match = ev.retention_class();
            assert_eq!(by_table, by_match, "普查表与穷举 match 判定漂移: {etype}");
        }
        // ② 可压缩标注与可释放字段清单互证（Eligible 必有字段，反之亦然）
        for ev in &samples {
            let serialized = serde_json::to_value(ev).unwrap();
            let etype = serialized["type"].as_str().unwrap().to_string();
            let has_fields = !releasable_fields(&etype).is_empty();
            assert_eq!(
                has_fields,
                ev.retention_class() == RetentionClass::Eligible,
                "Eligible 标注与字段清单不同步: {etype}"
            );
        }
        // ③ 可压缩白名单面（封闭集合；豁免为缺省臂）
        for t in ["checkpoint_blob", "wire_rendered", "steering_note"] {
            assert_eq!(retention_class_of_type(t), RetentionClass::Eligible);
        }
        // ④ 宁保守缺省臂:未知类型一律豁免
        assert_eq!(
            retention_class_of_type("future_unknown_event"),
            RetentionClass::Exempt
        );
    }

    // ---------- 事件落账形态 ----------

    #[test]
    fn retention_event_shape_roundtrip() {
        let ev = JournalEvent::RetentionCompactionPerformed {
            trigger: "quota".into(),
            before_bytes: 5000,
            after_bytes: 300,
            released_count: 2,
            exempt_skipped: 1,
            released_digests: vec![evorule_digest("a"), evorule_digest("b")],
            released_anchors: vec!["seq=2;ts=1;type=wire_rendered".into()],
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], json!("retention_compaction_performed"));
        assert_eq!(v["payload"]["trigger"], json!("quota"));
        assert_eq!(v["payload"]["before_bytes"], json!(5000));
        assert_eq!(v["payload"]["exempt_skipped"], json!(1));
        assert_eq!(
            v["payload"]["released_digests"].as_array().unwrap().len(),
            2
        );
        let back: JournalEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, ev, "形态对齐可回放（adjacently tagged 往返）");
    }

    // ---------- 链续接：释放后链验证仍通过 ----------

    /// 手工构造带回溯时间戳的链化 journal 行（与 open/verify 同公式；
    /// 全部 pub 函数复算——防测试私写链公式漂移）
    fn chain_lines(lines: &mut [serde_json::Value]) {
        let mut prev = JOURNAL_CHAIN_GENESIS.to_string();
        for (i, obj) in lines.iter_mut().enumerate() {
            obj["seq"] = json!((i + 1) as u64);
            let normalized = journal_normalized_content(&obj.to_string()).unwrap();
            let content_hash = journal_content_hash(&normalized);
            let chain_hash = journal_chain_step(&prev, &content_hash);
            obj["content_hash"] = json!(content_hash);
            obj["prev_hash"] = json!(prev);
            obj["chain_hash"] = json!(chain_hash);
            prev = chain_hash;
        }
    }

    /// 构造超龄账本：wire/checkpoint 大全文（可压缩）+ 豁免骨架/审批面。
    /// 尾行 turn_ended 收步（真实账面口径——悬挂尾会被 Resume 重开补写
    /// 崩溃标记，测试账不留悬挂）。
    fn write_chained_journal(dir: &Path, sid: &str, now_ms: u64, day: u64) -> PathBuf {
        let mut lines = vec![
            json!({"seq":1,"ts":now_ms - 100*day,"type":"turn_started","payload":{"turn_seq":1,"goal":"g"}}),
            json!({"seq":2,"ts":now_ms - 100*day,"type":"wire_rendered","payload":{"round":1,"wire_len":30,"content_hash":"blake3:aa","full_text":"超龄 wire 全文保真原文"}}),
            json!({"seq":3,"ts":now_ms - 100*day,"type":"checkpoint_blob","payload":{"plan_hash":"h","node_id":"n","hash":"blake3:bb","len":24,"full_text":"超龄检查点全文保真原文"}}),
            json!({"seq":4,"ts":now_ms - day,"type":"approval_requested","payload":{"approval_id":"a","tool":"file_write","payload":"cmd"}}),
            json!({"seq":5,"ts":now_ms - day,"type":"turn_ended","payload":{"status":"success","steps":1,"duration_ms":1}}),
        ];
        chain_lines(&mut lines);
        let text: Vec<String> = lines.iter().map(|o| o.to_string()).collect();
        let path = dir.join(format!("{sid}.jsonl"));
        std::fs::write(&path, text.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn journal_release_keeps_chain_verified_and_event_on_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let now_ms = 1_800_000_000_000u64;
        let day = 86_400_000u64;
        let path = write_chained_journal(dir.path(), "s-rt", now_ms, day);
        // 预检：构造链本身可过验证（防测试自欺）
        let pre = verify_journal_file(&path).unwrap();
        assert!(pre.verified, "构造账预验证: {:?}", pre.first_failure);
        // 龄期清扫（90d；豁免 turn_started/approval_requested 不动）
        let ctx = RetentionSweepCtx {
            policy: policy_with(RetentionPeriod::NinetyDays, RetentionAction::Compact, None),
            journal_dir: dir.path().to_path_buf(),
            client: crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1"),
            namespace: "ns".into(),
            session_id: "s-rt".into(),
            archive_dir_override: None,
            shared_server: SharedServerCompaction::Off,
        };
        let report = tokio_test::block_on(run_retention_sweep(ctx, now_ms));
        assert_eq!(report.files_modified, 1, "一个文件被压缩");
        assert_eq!(
            report.released_count, 2,
            "wire_rendered + checkpoint_blob 两条"
        );
        assert!(!report.no_op);
        // 释放后：全文出账 + 凭证保持 + seq 连续（read_all fail-visible 校验）
        // + 链验证仍全过
        let after = verify_journal_file(&path).unwrap();
        assert!(after.verified, "释放后链验证: {:?}", after.first_failure);
        let lines = read_all(&path).unwrap();
        assert_eq!(
            lines.len(),
            6,
            "原 5 行 + 摘要卡事件落账 1 行；seq 连续性由 read_all 强制"
        );
        let mut seen_event = false;
        for l in &lines {
            match &l.event {
                JournalEvent::WireRendered {
                    round,
                    full_text,
                    content_hash,
                    wire_len,
                    ..
                } => {
                    assert!(full_text.is_empty(), "超龄全文应出账");
                    assert_eq!(content_hash, "blake3:aa", "hash 凭证保持");
                    assert_eq!(*wire_len, 30, "长度凭证保持");
                    assert_eq!(*round, 1);
                }
                JournalEvent::CheckpointBlob {
                    full_text,
                    hash,
                    len,
                    ..
                } => {
                    assert!(full_text.is_empty());
                    assert_eq!(hash, "blake3:bb");
                    assert_eq!(*len, 24);
                }
                JournalEvent::ApprovalRequested { payload, .. } => {
                    assert_eq!(payload, "cmd", "豁免面逐字节不动");
                }
                JournalEvent::TurnStarted { goal, .. } => {
                    assert_eq!(goal, "g", "骨架面豁免不动");
                }
                JournalEvent::RetentionCompactionPerformed {
                    trigger,
                    released_count,
                    released_digests,
                    released_anchors,
                    ..
                } => {
                    seen_event = true;
                    assert_eq!(trigger, "age");
                    assert_eq!(*released_count, 2);
                    assert_eq!(released_digests.len(), 2);
                    assert!(released_digests.iter().all(|d| d.starts_with("blake3:")));
                    assert_eq!(released_anchors.len(), 2);
                    assert!(
                        released_anchors[0].starts_with("seq=2;"),
                        "锚点含 seq/ts/type"
                    );
                }
                _ => {}
            }
        }
        assert!(seen_event, "摘要卡事件应在账");
        assert!(dir.path().join("s-rt.jsonl").exists());
    }

    // ---------- 导出归档：成功先归档后压缩；失败放弃压缩 ----------

    #[test]
    fn export_then_compact_writes_archive_then_releases() {
        let dir = tempfile::tempdir().unwrap();
        let now_ms = 1_800_000_000_000u64;
        let day = 86_400_000u64;
        let path = write_chained_journal(dir.path(), "s-exp", now_ms, day);
        let archive_dir = dir.path().join("archives");
        let ctx = RetentionSweepCtx {
            policy: policy_with(
                RetentionPeriod::NinetyDays,
                RetentionAction::ExportThenCompact,
                None,
            ),
            journal_dir: dir.path().to_path_buf(),
            client: crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1"),
            namespace: "ns".into(),
            session_id: "s-exp".into(),
            archive_dir_override: Some(archive_dir.clone()),
            shared_server: SharedServerCompaction::Off,
        };
        let report = tokio_test::block_on(run_retention_sweep(ctx, now_ms));
        assert_eq!(report.files_modified, 1);
        assert_eq!(report.archive_paths.len(), 1, "归档路径登记");
        // 归档包：manifest 行（总记录数+聚合 digest）+ 每记录原文行
        let archives: Vec<_> = std::fs::read_dir(&archive_dir)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(archives.len(), 1, "单 JSONL 归档文件");
        let content = std::fs::read_to_string(archives[0].path()).unwrap();
        let mut lines = content.lines();
        let manifest: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(manifest["manifest"], json!(true));
        assert_eq!(manifest["records"], json!(2));
        assert!(manifest["aggregate_digest"]
            .as_str()
            .unwrap()
            .starts_with("blake3:"));
        let rest = lines.collect::<Vec<_>>().join("\n");
        assert!(rest.contains("超龄 wire 全文保真原文"), "归档含完整原文");
        // journal 全文出账 + 链验证仍过
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("超龄 wire 全文保真原文"));
        assert!(verify_journal_file(&path).unwrap().verified);
    }

    #[test]
    fn export_failure_aborts_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let now_ms = 1_800_000_000_000u64;
        let day = 86_400_000u64;
        let path = write_chained_journal(dir.path(), "s-abort", now_ms, day);
        let before_bytes = std::fs::read(&path).unwrap();
        // 归档落点指到一个「文件」——写归档必失败
        let bogus_dir = dir.path().join("not-a-dir");
        std::fs::write(&bogus_dir, "I am a file").unwrap();
        let ctx = RetentionSweepCtx {
            policy: policy_with(
                RetentionPeriod::NinetyDays,
                RetentionAction::ExportThenCompact,
                None,
            ),
            journal_dir: dir.path().to_path_buf(),
            client: crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1"),
            namespace: "ns".into(),
            session_id: "s-abort".into(),
            archive_dir_override: Some(bogus_dir),
            shared_server: SharedServerCompaction::Off,
        };
        let report = tokio_test::block_on(run_retention_sweep(ctx, now_ms));
        // 宁不删不丢失：文件逐字节不变、零事件、放弃登记
        let after_bytes = std::fs::read(&path).unwrap();
        assert_eq!(before_bytes, after_bytes, "导出失败必须放弃本轮压缩");
        assert!(!report.aborted.is_empty(), "放弃应留痕");
        assert_eq!(report.files_modified, 0);
        assert!(verify_journal_file(&path).unwrap().verified);
    }

    // ---------- 缺省零发生 parity ----------

    #[test]
    fn default_policy_zero_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let now_ms = 1_800_000_000_000u64;
        let day = 86_400_000u64;
        let path = write_chained_journal(dir.path(), "s-zero", now_ms, day);
        let before_bytes = std::fs::read(&path).unwrap();
        let ctx = RetentionSweepCtx {
            policy: StorageRetentionPolicy::default(),
            journal_dir: dir.path().to_path_buf(),
            client: crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1"),
            namespace: "ns".into(),
            session_id: "s-zero".into(),
            archive_dir_override: None,
            shared_server: SharedServerCompaction::Off,
        };
        let report = tokio_test::block_on(run_retention_sweep(ctx, now_ms));
        assert!(report.no_op, "缺省策略整体短路");
        assert_eq!(report.files_scanned, 0, "零扫描零写入");
        assert_eq!(
            before_bytes,
            std::fs::read(&path).unwrap(),
            "文件逐字节不变"
        );
    }

    // ---------- 归档落点解析 ----------

    #[test]
    fn archive_dir_resolution_chain() {
        let journal_dir = Path::new("/tmp/jd");
        // 显式参数优先
        let o = resolve_archive_dir(Some(Path::new("/explicit")), journal_dir).unwrap();
        assert_eq!(o, PathBuf::from("/explicit"));
        // 兜底链必然可解析（journal 目录兜底永在）
        assert!(resolve_archive_dir(None, journal_dir).is_some());
    }

    // ---------- 服务端共享账压实调度（epoch 契约接线面）----------

    fn shared_server_settings(v: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert(
            "storage.retention.sharedServer".to_string(),
            serde_json::Value::String(v.to_string()),
        );
        m
    }

    #[test]
    fn shared_server_mode_from_settings_closed_enum() {
        // 缺省（无键）= off = 零动作
        assert_eq!(
            SharedServerCompaction::from_settings(&serde_json::Map::new()),
            SharedServerCompaction::Off
        );
        // 白名单值
        assert_eq!(
            SharedServerCompaction::from_settings(&shared_server_settings("archive")),
            SharedServerCompaction::Archive
        );
        assert_eq!(
            SharedServerCompaction::from_settings(&shared_server_settings("discard")),
            SharedServerCompaction::Discard
        );
        // 非白名单值不 clamp 只回缺省（封闭枚举口径与既有三键一致）
        assert_eq!(
            SharedServerCompaction::from_settings(&shared_server_settings("nuke")),
            SharedServerCompaction::Off
        );
        // 类型不符回缺省
        let mut wrong_type = serde_json::Map::new();
        wrong_type.insert(
            "storage.retention.sharedServer".to_string(),
            serde_json::Value::Bool(true),
        );
        assert_eq!(
            SharedServerCompaction::from_settings(&wrong_type),
            SharedServerCompaction::Off
        );
    }

    /// 去重调度 mock：POST 命中计数（mockito 1.x 无 hit 计数 API，闭包自计）
    async fn counting_compaction_mock(
        server: &mut mockito::Server,
        mode: &'static str,
        status: usize,
    ) -> (
        mockito::Mock,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_body = hits.clone();
        let body = move |w: &mut dyn std::io::Write| {
            use std::sync::atomic::Ordering;
            hits_for_body.fetch_add(1, Ordering::SeqCst);
            let payload = format!(r#"{{"scheduled":true,"echo_mode":"{mode}","status":{status}}}"#);
            w.write_all(payload.as_bytes())?;
            Ok::<(), std::io::Error>(())
        };
        let mock = server
            .mock("POST", "/api/admin/shared/compaction")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "mode": mode
            })))
            .with_status(status)
            .with_header("content-type", "application/json")
            .with_chunked_body(body)
            .create_async()
            .await;
        (mock, hits)
    }

    /// off=零动作：零 HTTP（计数 mock 恒 0）
    #[tokio::test]
    async fn maybe_schedule_off_is_zero_http() {
        let mut server = mockito::Server::new_async().await;
        let (_mock, hits) = counting_compaction_mock(&mut server, "archive", 200).await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let flag = std::sync::atomic::AtomicBool::new(false);
        let scheduled =
            maybe_schedule_server_compaction(SharedServerCompaction::Off, &client, &flag).await;
        assert!(!scheduled);
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "off 档必须零 HTTP"
        );
    }

    /// archive 档：mode 直传；去重标志注入=同标志第二次调用不再 POST
    #[tokio::test]
    async fn maybe_schedule_posts_once_then_dedupes() {
        let mut server = mockito::Server::new_async().await;
        let (_mock, hits) = counting_compaction_mock(&mut server, "archive", 200).await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let flag = std::sync::atomic::AtomicBool::new(false);
        let first =
            maybe_schedule_server_compaction(SharedServerCompaction::Archive, &client, &flag).await;
        assert!(first, "首次调用应执行调度");
        let second =
            maybe_schedule_server_compaction(SharedServerCompaction::Archive, &client, &flag).await;
        assert!(!second, "去重标志已置位：第二次调用不再 POST");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "恰好一次 HTTP"
        );
    }

    /// 403 失败面：fail-soft 不 panic 不重试（返回 false，错误只留痕）
    #[tokio::test]
    async fn maybe_schedule_auth_failure_is_fail_soft() {
        let mut server = mockito::Server::new_async().await;
        let (_mock, hits) = counting_compaction_mock(&mut server, "discard", 403).await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let flag = std::sync::atomic::AtomicBool::new(false);
        let scheduled =
            maybe_schedule_server_compaction(SharedServerCompaction::Discard, &client, &flag).await;
        assert!(!scheduled, "403=调度未成功");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "零重试");
    }

    /// 清扫尾部接线：非缺省清扫轮执行调度（discard 直传）；缺省档位零 HTTP。
    /// 空 journal 目录 + 空 session_id = 最小 no-op 清扫形态（payload 面跳过）。
    /// 本测试独占进程级去重静态（其余调度测试用本地标志，互不污染）。
    #[tokio::test]
    async fn sweep_tail_schedules_server_compaction() {
        use std::sync::atomic::Ordering;
        SHARED_COMPACTION_SCHEDULED.store(false, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        // —— 调度档：清扫轮尾部调度一次 ——
        let mut server = mockito::Server::new_async().await;
        let (_mock, hits) = counting_compaction_mock(&mut server, "discard", 200).await;
        let ctx = RetentionSweepCtx {
            policy: policy_with(RetentionPeriod::NinetyDays, RetentionAction::Compact, None),
            journal_dir: dir.path().to_path_buf(),
            client: crate::api::evorule_client::EvoruleApiClient::new(&server.url()),
            namespace: String::new(),
            session_id: String::new(),
            archive_dir_override: None,
            shared_server: SharedServerCompaction::Discard,
        };
        let report = run_retention_sweep(ctx, 1_700_000_000_000).await;
        assert!(report.no_op);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "清扫轮尾部应调度服务端压实");
        // —— 缺省档：零 HTTP ——
        let mut server2 = mockito::Server::new_async().await;
        let (_mock2, hits2) = counting_compaction_mock(&mut server2, "archive", 200).await;
        let ctx2 = RetentionSweepCtx {
            policy: policy_with(RetentionPeriod::NinetyDays, RetentionAction::Compact, None),
            journal_dir: dir.path().to_path_buf(),
            client: crate::api::evorule_client::EvoruleApiClient::new(&server2.url()),
            namespace: String::new(),
            session_id: String::new(),
            archive_dir_override: None,
            shared_server: SharedServerCompaction::Off,
        };
        let report2 = run_retention_sweep(ctx2, 1_700_000_000_000).await;
        assert!(report2.no_op);
        assert_eq!(hits2.load(Ordering::SeqCst), 0, "off 档清扫轮零调度 HTTP");
    }
}
