// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 运行时行为检测规则层——审计流上的确定性检测（批处理先行，零热路径）。
//!
//! ## 评估形态
//!
//! 第一批只有离线/按需批处理：对一份会话 journal 跑规则评估，输出检测报告
//! （纯函数管线，可核验可重放）。inline 实时检测是热路径延迟与故障面代价，
//! 明确后置——首批规则多为频次/窗口类，批处理足够。
//!
//! ## 信任模型
//!
//! 命中只做**发现与留痕**（报告 + tracing 事件），永远不做自动阻断/自动处置
//! ——处置是治理规则执行面的既有职责，语义不混；误报的代价=人工复核，
//! 报告携带完整证据锚（journal 行 seq）供快速裁决。与提案审批同一信任模型：
//! 机器提议/检测，人工批准/处置。
//!
//! ## 规则模型（确定性谓词，数据即语义）
//!
//! 三种谓词原语，全部可确定性重放：
//! - `count_over_window`：滑动时间窗频次（可按工具分组）；
//! - `sequence_contains`：序列形态（scope 过滤流中按序出现——子序列语义，
//!   间隔允许；步骤可约束同工具/结果态/载荷子串）；
//! - `threshold_field`：数值字段越限（size_bytes/attempt/tokens_est）。
//!
//! 规则集版本化 + blake3 内容哈希钉桩（`evorule-hash` 口径，与治理面同一
//! 哈希纪律）；内置基线规则集随二进制发行（运行时不联网、不可变）。
//!
//! ## journal 观测面（诚实边界）
//!
//! 数据源=会话 journal（`data/sessions/{session}.jsonl`）。归一化时做两个
//! 确定性 join：tool_result 经 call_id 回联工具名；approval_resolved 经
//! approval_id 回联工具名与决定。凭据触达观测面=审批命令载荷（工具参数在
//! journal 只存 digest，审批载荷是唯一明文面）；spawn 拒载观测依赖
//! mcp_capability_blocked 事件（能力包绑定任务的启动探测落账）。journal
//! 追加式演进的新事件形态不经归一化面（catch-all），规则引用未支持形态
//! =装载即拒（白名单闸）——不静默零命中。
//!
//! ## 报告完整性
//!
//! `report_hash` = `blake3:` 前缀 [`evorule_hash::json_digest`]，对未含该键
//! 的报告对象计算（与 mcp 扫描报告同一约定）；规则集哈希随报告钉桩，任何
//! 一侧被事后改写均可复核发现。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::journal::{JournalEvent, JournalLine};

/// 规则/报告 schema 版本（形态变更时 bump）。
pub const DETECT_SCHEMA_VERSION: &str = "1";

/// 内置基线规则集版本（任何规则变更必须 bump）。
pub const BASELINE_RULESET_VERSION: &str = "1.0.0";

/// 内置基线规则集（五条，业界运行时滥用形态的确定性化——引规则思路，不引产品）。
const BASELINE_RULES_JSON: &str = include_str!("detect/baseline-rules-v1.json");

/// 证据条截断上限（防报告膨胀；截断时 detail 尾注实数）。
const EVIDENCE_MAX: usize = 50;

/// 检测规则作用域：从哪个数据源过滤出待评估事件流。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetectScope {
    /// 数据源（当前唯一合法值 `session_journal`；其余值装载即拒——fail-visible）
    pub source: String,
    /// 事件形态过滤（count/threshold 谓词必填；sequence 谓词必须缺席——
    /// 序列的形态过滤在步骤级声明）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// 工具名前缀过滤（命中工具名的 event 才进流）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_prefix: Option<String>,
    /// 结果态过滤（仅对携带成败语义的事件生效：tool_result/approval_resolved/policy_judged）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
}

/// 序列步骤约束（sequence_contains 用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SequenceStep {
    /// 事件形态（snake_case，须在受支持清单内）
    pub event: String,
    /// 工具名精确匹配
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// 与上一步命中事件同工具（审批拒绝→同工具重试这类关联面）
    #[serde(default)]
    pub same_tool_as_previous: bool,
    /// 结果态约束（仅对携带成败语义的事件生效）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    /// 载荷子串任一命中（大小写不敏感；仅审批载荷携带明文，其余事件无载荷）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub payload_any: Vec<String>,
}

/// 谓词原语（首批三种；tag=type 与规则样例 JSON 同形）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Predicate {
    /// 滑动时间窗频次：任一窗口内事件数 > threshold 即命中（取计数最大的窗口为证据）。
    CountOverWindow {
        /// 频次阈值（> threshold 触发，正整数）
        threshold: u32,
        /// 窗口宽（秒，正整数）
        window_secs: u64,
        /// 按工具名分组分别评估（单 server 频次异常这类按主体计数用）
        #[serde(default)]
        group_by_tool: bool,
    },
    /// 序列形态：scope 过滤流中按序出现整段序列（子序列语义，间隔允许）
    /// 即命中（首处命中为证据）。
    SequenceContains {
        /// 序列步骤（≥1 步；步骤 1 不得声明 same_tool_as_previous）
        sequence: Vec<SequenceStep>,
    },
    /// 数值字段越限：任一事件该字段 ≥ limit 即命中（全部越限事件为证据）。
    /// 受支持字段：`size_bytes`(tool_result) / `attempt`(tool_retried) /
    /// `tokens_est`(llm_called)；其余事件无该字段则不参与。
    ThresholdField {
        /// 字段名
        field: String,
        /// 越限下界（≥ limit 触发，正整数）
        limit: u64,
    },
}

/// 一条检测规则。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetectRule {
    /// 规则 id（报告与告警事件的引用键）
    pub rule_id: String,
    /// 作用域
    pub scope: DetectScope,
    /// 谓词
    pub predicate: Predicate,
    /// 严重度（info|warning|critical，其余值装载即拒）
    pub severity: String,
    /// 立场说明（人读；为何这条规则值得看）
    pub rationale: String,
}

/// 规则集：版本 + 规则列表（哈希钉桩由 [`ruleset_hash`] 计算，不入文件）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetectRuleset {
    /// 规则集版本（基线集随 bump 演进）
    pub version: String,
    /// 规则列表
    pub rules: Vec<DetectRule>,
}

/// 归一化事件（检测评估的唯一输入形态；join 已完成）。
#[derive(Debug, Clone, PartialEq)]
pub struct DetectEvent {
    /// journal 行 seq（证据锚）
    pub seq: u64,
    /// journal 行时间戳（unix ms）
    pub ts: u64,
    /// 事件形态（snake_case；未受支持形态="unmapped"，不可被规则引用）
    pub kind: &'static str,
    /// 关联工具/服务名（join 产物；无关联为 None）
    pub tool: Option<String>,
    /// 成败语义（ok|error、approved|rejected、allowed|blocked 归一；无成败语义为 None）
    pub ok: Option<bool>,
    /// 审批命令载荷明文（仅 approval_requested；其余事件 None）
    pub payload: Option<String>,
}

/// 装载期校验错误（fail-visible：规则文件损坏必须大声）。
#[derive(Debug, PartialEq)]
pub enum RulesetError {
    /// JSON 解析失败
    Parse(String),
    /// 形状/语义校验失败（携带人读理由）
    Invalid(String),
}

impl std::fmt::Display for RulesetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RulesetError::Parse(e) => write!(f, "规则集解析失败: {e}"),
            RulesetError::Invalid(e) => write!(f, "规则集校验失败: {e}"),
        }
    }
}

impl std::error::Error for RulesetError {}

/// 谓词评估受支持的事件形态清单（装载期白名单；journal 新增形态须显式
/// 扩此清单并补归一化臂——规则引用未知形态=装载即拒，不静默零命中）。
const SUPPORTED_EVENT_KINDS: &[&str] = &[
    "tool_invoked",
    "tool_result",
    "tool_retried",
    "approval_requested",
    "approval_resolved",
    "policy_judged",
    "mcp_capability_blocked",
    "llm_called",
];

/// 合法严重度。
const SEVERITIES: &[&str] = &["info", "warning", "critical"];

/// 规则集哈希钉桩：canonical JSON 的 `blake3:` 前缀摘要（evorule-hash 口径，
/// 与治理面规则资产同一哈希纪律）。
pub fn ruleset_hash(ruleset: &DetectRuleset) -> Result<String, String> {
    let value = serde_json::to_value(ruleset).map_err(|e| format!("serialize ruleset: {e}"))?;
    Ok(evorule_hash::prefixed(&evorule_hash::json_digest(&value)))
}

/// 装载内置基线规则集（版本常量 + 随二进制发行的规则 JSON；不可变）。
pub fn baseline_ruleset() -> DetectRuleset {
    // 随二进制发行的内容，解析失败=编译期就该被黄金样本测试抓住的缺陷；
    // 运行时 panic 优于静默空规则集（fail-visible）。
    parse_ruleset(BASELINE_RULES_JSON).expect("内置基线规则集必须合法")
}

/// 从 JSON 文本装载并校验规则集。
pub fn parse_ruleset(raw: &str) -> Result<DetectRuleset, RulesetError> {
    let ruleset: DetectRuleset =
        serde_json::from_str(raw).map_err(|e| RulesetError::Parse(e.to_string()))?;
    validate_ruleset(&ruleset)?;
    Ok(ruleset)
}

/// 装载期校验（形状+语义；错误信息携带规则 id 定位）。
pub fn validate_ruleset(rs: &DetectRuleset) -> Result<(), RulesetError> {
    if rs.version.trim().is_empty() {
        return Err(RulesetError::Invalid("规则集 version 为空".into()));
    }
    if rs.rules.is_empty() {
        return Err(RulesetError::Invalid("规则列表为空".into()));
    }
    let mut seen = std::collections::HashSet::new();
    for rule in &rs.rules {
        if rule.rule_id.trim().is_empty() {
            return Err(RulesetError::Invalid("存在空 rule_id".into()));
        }
        if !seen.insert(rule.rule_id.as_str()) {
            return Err(RulesetError::Invalid(format!(
                "rule_id 重复: {}",
                rule.rule_id
            )));
        }
        if rule.scope.source != "session_journal" {
            return Err(RulesetError::Invalid(format!(
                "{}: 不支持的数据源 {}（当前唯一合法值 session_journal）",
                rule.rule_id, rule.scope.source
            )));
        }
        if !SEVERITIES.contains(&rule.severity.as_str()) {
            return Err(RulesetError::Invalid(format!(
                "{}: 非法 severity {}（合法值 info|warning|critical）",
                rule.rule_id, rule.severity
            )));
        }
        match &rule.predicate {
            Predicate::CountOverWindow {
                threshold,
                window_secs,
                ..
            } => {
                if *threshold == 0 {
                    return Err(RulesetError::Invalid(format!(
                        "{}: threshold 必须为正整数",
                        rule.rule_id
                    )));
                }
                if *window_secs == 0 {
                    return Err(RulesetError::Invalid(format!(
                        "{}: window_secs 必须为正整数",
                        rule.rule_id
                    )));
                }
                let event = rule.scope.event.as_deref().ok_or_else(|| {
                    RulesetError::Invalid(format!(
                        "{}: count 谓词要求 scope.event 必填",
                        rule.rule_id
                    ))
                })?;
                ensure_known_kind(&rule.rule_id, event)?;
            }
            Predicate::SequenceContains { sequence } => {
                if sequence.is_empty() {
                    return Err(RulesetError::Invalid(format!(
                        "{}: 序列至少一步",
                        rule.rule_id
                    )));
                }
                if rule.scope.event.is_some() {
                    return Err(RulesetError::Invalid(format!(
                        "{}: 序列谓词的形态过滤在步骤级声明，scope.event 必须缺席",
                        rule.rule_id
                    )));
                }
                for (idx, step) in sequence.iter().enumerate() {
                    ensure_known_kind(&rule.rule_id, &step.event)?;
                    if idx == 0 && step.same_tool_as_previous {
                        return Err(RulesetError::Invalid(format!(
                            "{}: 序列第 1 步无「上一步」可比",
                            rule.rule_id
                        )));
                    }
                }
            }
            Predicate::ThresholdField { field, limit } => {
                if *limit == 0 {
                    return Err(RulesetError::Invalid(format!(
                        "{}: limit 必须为正整数",
                        rule.rule_id
                    )));
                }
                let event = rule.scope.event.as_deref().ok_or_else(|| {
                    RulesetError::Invalid(format!(
                        "{}: threshold 谓词要求 scope.event 必填",
                        rule.rule_id
                    ))
                })?;
                ensure_known_kind(&rule.rule_id, event)?;
                if numeric_field(event, field).is_none() {
                    return Err(RulesetError::Invalid(format!(
                        "{}: 事件 {event} 无数值字段 {field}（受支持组合：tool_result.size_bytes / tool_retried.attempt / llm_called.tokens_est）",
                        rule.rule_id
                    )));
                }
            }
        }
    }
    Ok(())
}

fn ensure_known_kind(rule_id: &str, kind: &str) -> Result<(), RulesetError> {
    if SUPPORTED_EVENT_KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(RulesetError::Invalid(format!(
            "{rule_id}: 未支持的事件形态 {kind}"
        )))
    }
}

/// journal 事件流 → 归一化事件流（含确定性 join：call_id/approval_id 回联）。
/// 保序 1:1（输出[i] 对应输入行[i]，证据索引直通）。
pub fn normalize(lines: &[JournalLine]) -> Vec<DetectEvent> {
    // 回联表：call_id → 工具名；approval_id → 工具名
    let mut call_tool: HashMap<&str, &str> = HashMap::new();
    let mut approval_tool: HashMap<&str, &str> = HashMap::new();
    for line in lines {
        match &line.event {
            JournalEvent::ToolInvoked { call_id, tool, .. } => {
                call_tool.insert(call_id.as_str(), tool.as_str());
            }
            JournalEvent::ApprovalRequested {
                approval_id, tool, ..
            } => {
                approval_tool.insert(approval_id.as_str(), tool.as_str());
            }
            _ => {}
        }
    }
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let kind = event_kind(&line.event);
        let (tool, ok, payload) = match &line.event {
            JournalEvent::ToolInvoked { tool, .. } => (Some(tool.clone()), None, None),
            JournalEvent::ToolResult {
                call_id, status, ..
            } => (
                call_tool.get(call_id.as_str()).map(|s| (*s).to_string()),
                Some(status == "ok"),
                None,
            ),
            JournalEvent::ToolRetried { tool, .. } => (Some(tool.clone()), None, None),
            JournalEvent::ApprovalRequested { tool, payload, .. } => {
                (Some(tool.clone()), None, Some(payload.clone()))
            }
            JournalEvent::ApprovalResolved {
                approval_id,
                decision,
                ..
            } => (
                approval_tool
                    .get(approval_id.as_str())
                    .map(|s| (*s).to_string()),
                Some(decision == "approved"),
                None,
            ),
            JournalEvent::PolicyJudged { verdict, .. } => (None, Some(verdict == "allowed"), None),
            JournalEvent::McpCapabilityBlocked { server, .. } => (Some(server.clone()), None, None),
            _ => (None, None, None),
        };
        out.push(DetectEvent {
            seq: line.seq,
            ts: line.ts,
            kind,
            tool,
            ok,
            payload,
        });
    }
    out
}

/// 事件形态标签（与 serde snake_case 序列化形态一致；受支持清单之外的
/// 形态一律 "unmapped"——不可被规则引用，白名单闸兜底）。
fn event_kind(event: &JournalEvent) -> &'static str {
    match event {
        JournalEvent::ToolInvoked { .. } => "tool_invoked",
        JournalEvent::ToolResult { .. } => "tool_result",
        JournalEvent::ToolRetried { .. } => "tool_retried",
        JournalEvent::ApprovalRequested { .. } => "approval_requested",
        JournalEvent::ApprovalResolved { .. } => "approval_resolved",
        JournalEvent::PolicyJudged { .. } => "policy_judged",
        JournalEvent::McpCapabilityBlocked { .. } => "mcp_capability_blocked",
        JournalEvent::LlmCalled { .. } => "llm_called",
        _ => "unmapped",
    }
}

/// 数值字段解析（threshold_field 谓词的受支持面；按 (形态, 字段) 组合白名单）。
fn numeric_field(kind: &str, field: &str) -> Option<fn(&JournalEvent) -> Option<u64>> {
    match (kind, field) {
        ("tool_result", "size_bytes") => Some(|e| match e {
            JournalEvent::ToolResult { size_bytes, .. } => Some(*size_bytes as u64),
            _ => None,
        }),
        ("tool_retried", "attempt") => Some(|e| match e {
            JournalEvent::ToolRetried { attempt, .. } => Some(*attempt),
            _ => None,
        }),
        ("llm_called", "tokens_est") => Some(|e| match e {
            JournalEvent::LlmCalled { tokens_est, .. } => *tokens_est,
            _ => None,
        }),
        _ => None,
    }
}

/// scope 过滤（source 已在装载期保证；此处只做形态/前缀/结果态）。
fn in_scope(rule: &DetectRule, ev: &DetectEvent) -> bool {
    if let Some(event) = &rule.scope.event {
        if ev.kind != event {
            return false;
        }
    }
    if let Some(prefix) = &rule.scope.tool_prefix {
        match &ev.tool {
            Some(t) if t.starts_with(prefix) => {}
            _ => return false,
        }
    }
    if let Some(ok) = rule.scope.ok {
        if ev.ok != Some(ok) {
            return false;
        }
    }
    true
}

/// 证据条（报告内指向原始 journal 行的最小锚）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvidenceRef {
    /// journal 行 seq
    pub seq: u64,
    /// unix ms
    pub ts: u64,
    /// 事件形态
    pub kind: String,
    /// 关联工具/服务名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

/// 一条检测命中。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetectHit {
    /// 规则 id
    pub rule_id: String,
    /// 严重度
    pub severity: String,
    /// 规则立场说明
    pub rationale: String,
    /// 人读命中说明（窗口/序列/越限的具体数值）
    pub detail: String,
    /// 证据锚（指向原始 journal 行；上限 [`EVIDENCE_MAX`] 条防报告膨胀）
    pub evidence: Vec<EvidenceRef>,
}

/// 单规则在归一化流上评估（raws 与 events 按 seq 1:1 对齐——normalize 保序）。
/// 命中返回 [`DetectHit`]，未命中返回 None。
pub fn evaluate_rule(
    rule: &DetectRule,
    events: &[DetectEvent],
    raws: &[JournalLine],
) -> Option<DetectHit> {
    let scoped: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, ev)| in_scope(rule, ev))
        .map(|(i, _)| i)
        .collect();
    match &rule.predicate {
        Predicate::CountOverWindow {
            threshold,
            window_secs,
            group_by_tool,
        } => count_over_window_hit(
            rule,
            events,
            &scoped,
            *threshold,
            *window_secs,
            *group_by_tool,
        ),
        Predicate::SequenceContains { sequence } => sequence_hit(rule, events, &scoped, sequence),
        Predicate::ThresholdField { field, limit } => {
            threshold_field_hit(rule, events, raws, &scoped, field, *limit)
        }
    }
}

/// 证据条构造（超限截断并在 detail 尾注实数）。
fn evidence_of(events: &[DetectEvent], idxs: &[usize], detail: &mut String) -> Vec<EvidenceRef> {
    if idxs.len() > EVIDENCE_MAX {
        detail.push_str(&format!(
            "（证据截断：共 {} 条，示前 {EVIDENCE_MAX} 条）",
            idxs.len()
        ));
    }
    idxs.iter()
        .take(EVIDENCE_MAX)
        .map(|&i| EvidenceRef {
            seq: events[i].seq,
            ts: events[i].ts,
            kind: events[i].kind.to_string(),
            tool: events[i].tool.clone(),
        })
        .collect()
}

fn count_over_window_hit(
    rule: &DetectRule,
    events: &[DetectEvent],
    scoped: &[usize],
    threshold: u32,
    window_secs: u64,
    group_by_tool: bool,
) -> Option<DetectHit> {
    // 分组（group_by_tool=false 时单组）；journal 保序 → 组内 ts 非降，双指针滑窗。
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for &i in scoped {
        let key = if group_by_tool {
            events[i].tool.clone().unwrap_or_default()
        } else {
            String::new()
        };
        groups.entry(key).or_default().push(i);
    }
    let window_ms = window_secs.saturating_mul(1000);
    let mut best: Option<(usize, Vec<usize>, String)> = None;
    for (group, idxs) in &groups {
        let mut start = 0usize;
        for end in 0..idxs.len() {
            while events[idxs[end]].ts.saturating_sub(events[idxs[start]].ts) > window_ms {
                start += 1;
            }
            let count = end - start + 1;
            if count > threshold as usize
                && best.as_ref().map(|(c, _, _)| count > *c).unwrap_or(true)
            {
                best = Some((count, idxs[start..=end].to_vec(), group.clone()));
            }
        }
    }
    let (count, idxs, group) = best?;
    let mut detail = format!("峰值窗口 {count} 次（阈值 {threshold}，窗口 {window_secs}s）");
    if group_by_tool && !group.is_empty() {
        detail.push_str(&format!("，主体 {group}"));
    }
    let evidence = evidence_of(events, &idxs, &mut detail);
    Some(DetectHit {
        rule_id: rule.rule_id.clone(),
        severity: rule.severity.clone(),
        rationale: rule.rationale.clone(),
        detail,
        evidence,
    })
}

fn sequence_hit(
    rule: &DetectRule,
    events: &[DetectEvent],
    scoped: &[usize],
    sequence: &[SequenceStep],
) -> Option<DetectHit> {
    let step_ok = |ev: &DetectEvent, step: &SequenceStep| -> bool {
        if ev.kind != step.event {
            return false;
        }
        if let Some(ok) = step.ok {
            if ev.ok != Some(ok) {
                return false;
            }
        }
        if let Some(tool) = &step.tool {
            if ev.tool.as_deref() != Some(tool.as_str()) {
                return false;
            }
        }
        if !step.payload_any.is_empty() {
            let Some(payload) = &ev.payload else {
                return false;
            };
            let lower = payload.to_lowercase();
            if !step
                .payload_any
                .iter()
                .any(|needle| lower.contains(&needle.to_lowercase()))
            {
                return false;
            }
        }
        true
    };
    // 子序列语义：scope 流中按序匹配各步（间隔允许）；同工具约束对齐上一步
    // 命中事件。首处完整命中即返回（确定性：流序即遍历序）。
    for (pos, &i) in scoped.iter().enumerate() {
        if !step_ok(&events[i], &sequence[0]) {
            continue;
        }
        let mut matched = vec![i];
        let mut cursor = pos + 1;
        for step in &sequence[1..] {
            let mut found = None;
            while cursor < scoped.len() {
                let j = scoped[cursor];
                if step_ok(&events[j], step)
                    && (!step.same_tool_as_previous
                        || events[j].tool == events[matched[matched.len() - 1]].tool)
                {
                    found = Some(j);
                    cursor += 1;
                    break;
                }
                cursor += 1;
            }
            match found {
                Some(j) => matched.push(j),
                None => break,
            }
        }
        if matched.len() == sequence.len() {
            let mut detail = sequence
                .iter()
                .zip(matched.iter())
                .map(|(s, &j)| format!("{}@seq{}", s.event, events[j].seq))
                .collect::<Vec<_>>()
                .join(" -> ");
            let evidence = evidence_of(events, &matched, &mut detail);
            return Some(DetectHit {
                rule_id: rule.rule_id.clone(),
                severity: rule.severity.clone(),
                rationale: rule.rationale.clone(),
                detail,
                evidence,
            });
        }
    }
    None
}

fn threshold_field_hit(
    rule: &DetectRule,
    events: &[DetectEvent],
    raws: &[JournalLine],
    scoped: &[usize],
    field: &str,
    limit: u64,
) -> Option<DetectHit> {
    let event_kind = rule.scope.event.as_deref().unwrap_or_default();
    let extractor = numeric_field(event_kind, field)?;
    let mut hits = Vec::new();
    let mut max_seen = 0u64;
    for &i in scoped {
        if let Some(v) = extractor(&raws[i].event) {
            if v >= limit {
                hits.push(i);
                max_seen = max_seen.max(v);
            }
        }
    }
    if hits.is_empty() {
        return None;
    }
    let mut detail = format!(
        "{field} 峰值 {max_seen}（下界 {limit}，越限 {} 次）",
        hits.len()
    );
    let evidence = evidence_of(events, &hits, &mut detail);
    Some(DetectHit {
        rule_id: rule.rule_id.clone(),
        severity: rule.severity.clone(),
        rationale: rule.rationale.clone(),
        detail,
        evidence,
    })
}

/// 检测报告（命中=发现不是失败；报告完整性可由 report_hash 复核）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetectReport {
    /// schema 版本
    pub schema_version: String,
    /// 评估的会话标识（人读回显）
    pub session: String,
    /// 扫过的 journal 行数
    pub events_scanned: usize,
    /// 规则集钉桩：版本
    pub ruleset_version: String,
    /// 规则集钉桩：blake3 内容哈希
    pub ruleset_hash: String,
    /// 命中列表（规则声明序）
    pub hits: Vec<DetectHit>,
    /// 零命中规则 id（评估覆盖面回显——「看过且没发现」也要可核）
    pub rules_clean: Vec<String>,
    /// 报告完整性哈希（对未含本键的报告对象计算；报告落盘前由调用方钉桩）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_hash: Option<String>,
}

/// 全量评估：规则集 × journal → 报告（纯函数；tracing 告警由调用方按命中发）。
pub fn evaluate(
    ruleset: &DetectRuleset,
    session: &str,
    lines: &[JournalLine],
) -> Result<DetectReport, String> {
    validate_ruleset(ruleset).map_err(|e| e.to_string())?;
    let events = normalize(lines);
    let mut hits = Vec::new();
    let mut clean = Vec::new();
    for rule in &ruleset.rules {
        match evaluate_rule(rule, &events, lines) {
            Some(hit) => hits.push(hit),
            None => clean.push(rule.rule_id.clone()),
        }
    }
    let ruleset_hash = ruleset_hash(ruleset)?;
    Ok(DetectReport {
        schema_version: DETECT_SCHEMA_VERSION.to_string(),
        session: session.to_string(),
        events_scanned: lines.len(),
        ruleset_version: ruleset.version.clone(),
        ruleset_hash,
        hits,
        rules_clean: clean,
        report_hash: None,
    })
}

/// 报告完整性钉桩（与 mcp 扫描报告同一 digest 约定：对未含 report_hash 键
/// 的对象计算 canonical JSON 摘要，随后插入该键）。
pub fn report_with_hash(report: &DetectReport) -> Result<Value, String> {
    let mut value =
        serde_json::to_value(report).map_err(|e| format!("serialize detect report: {e}"))?;
    let digest = evorule_hash::prefixed(&evorule_hash::json_digest(&value));
    if let Some(obj) = value.as_object_mut() {
        obj.insert("report_hash".to_string(), Value::String(digest));
    }
    Ok(value)
}

/// 复核报告完整性（镜像 digest 约定；任何事后改写在此被拒）。
pub fn verify_report_hash(report: &Value) -> Result<(), String> {
    let obj = report
        .as_object()
        .ok_or_else(|| "detect report must be an object".to_string())?;
    let given = obj
        .get("report_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing report_hash".to_string())?;
    let mut stripped = obj.clone();
    stripped.remove("report_hash");
    let digest = evorule_hash::prefixed(&evorule_hash::json_digest(&Value::Object(stripped)));
    if digest == given {
        Ok(())
    } else {
        Err(format!("report_hash 不匹配（期望 {digest}，实得 {given}）"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::journal::JournalEvent as JE;

    fn line(seq: u64, ts: u64, event: JE) -> JournalLine {
        JournalLine {
            seq,
            ts,
            event,
            hashes: None,
        }
    }

    fn invoked(seq: u64, ts: u64, call: &str, tool: &str) -> JournalLine {
        line(
            seq,
            ts,
            JE::ToolInvoked {
                call_id: call.to_string(),
                tool: tool.to_string(),
                args_digest: "aa".to_string(),
                evorule_request_id: None,
            },
        )
    }

    fn result(seq: u64, ts: u64, call: &str, status: &str, size: usize) -> JournalLine {
        line(
            seq,
            ts,
            JE::ToolResult {
                call_id: call.to_string(),
                status: status.to_string(),
                size_bytes: size,
                content_digest: "bb".to_string(),
                execution_profile: None,
            },
        )
    }

    fn count_rule(threshold: u32, window: u64, group: bool) -> DetectRule {
        serde_json::from_value(serde_json::json!({
            "rule_id": "r-count",
            "scope": {"source": "session_journal", "event": "tool_invoked", "tool_prefix": "mcp_"},
            "predicate": {"type": "count_over_window", "threshold": threshold, "window_secs": window, "group_by_tool": group},
            "severity": "warning",
            "rationale": "测试规则"
        })).expect("rule json")
    }

    fn seq_rule(steps: serde_json::Value) -> DetectRule {
        serde_json::from_value(serde_json::json!({
            "rule_id": "r-seq",
            "scope": {"source": "session_journal"},
            "predicate": {"type": "sequence_contains", "sequence": steps},
            "severity": "info",
            "rationale": "测试规则"
        }))
        .expect("rule json")
    }

    #[test]
    fn baseline_golden_shape() {
        let rs = baseline_ruleset();
        assert_eq!(rs.version, BASELINE_RULESET_VERSION);
        assert_eq!(rs.version, "1.0.0");
        let ids: Vec<&str> = rs.rules.iter().map(|r| r.rule_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "mcp-call-rate",
                "tool-failure-cluster",
                "approval-denied-retry",
                "credential-field-touch",
                "spawn-reject-retry-storm",
            ]
        );
        // 全部规则通过装载校验（severity/白名单/谓词形状）
        validate_ruleset(&rs).expect("baseline validates");
        // 重复装载同哈希（钉桩形态面：blake3: 前缀 + 64 hex）
        let h1 = ruleset_hash(&rs).unwrap();
        let h2 = ruleset_hash(&baseline_ruleset()).unwrap();
        assert_eq!(h1, h2);
        assert!(
            h1.starts_with("blake3:") && h1.len() == 7 + 64,
            "hash form: {h1}"
        );
    }

    #[test]
    fn baseline_hash_pinned_golden_vector() {
        // 基线内容是随二进制发行的契约——哈希被测试锁死；改规则必须同步
        // bump 版本并重钉本向量（黄金向量纪律，与治理面 server_key 同法）。
        let rs = baseline_ruleset();
        let h = ruleset_hash(&rs).unwrap();
        assert_eq!(
            h, "blake3:0a60b0da0d09b75ea0b65e5d4eac00486d73effef9c87601eec1b5e31745d6c1",
            "baseline ruleset hash drift - bump version and re-pin: actual={h}"
        );
    }

    #[test]
    fn parse_rejects_bad_shapes() {
        let bad = [
            r#"{"version":"1","rules":[{"rule_id":"a","scope":{"source":"elsewhere","event":"tool_invoked"},"predicate":{"type":"count_over_window","threshold":1,"window_secs":60,"group_by_tool":false},"severity":"info","rationale":"x"}]}"#,
            r#"{"version":"1","rules":[{"rule_id":"a","scope":{"source":"session_journal","event":"tool_invoked"},"predicate":{"type":"count_over_window","threshold":1,"window_secs":60,"group_by_tool":false},"severity":"loud","rationale":"x"}]}"#,
            r#"{"version":"1","rules":[{"rule_id":"a","scope":{"source":"session_journal","event":"no_such_kind"},"predicate":{"type":"count_over_window","threshold":1,"window_secs":60,"group_by_tool":false},"severity":"info","rationale":"x"}]}"#,
            r#"{"version":"1","rules":[{"rule_id":"a","scope":{"source":"session_journal"},"predicate":{"type":"count_over_window","threshold":1,"window_secs":60,"group_by_tool":false},"severity":"info","rationale":"x"}]}"#,
            r#"{"version":"1","rules":[{"rule_id":"a","scope":{"source":"session_journal","event":"tool_invoked"},"predicate":{"type":"count_over_window","threshold":0,"window_secs":60,"group_by_tool":false},"severity":"info","rationale":"x"}]}"#,
            r#"{"version":"1","rules":[{"rule_id":"a","scope":{"source":"session_journal","event":"tool_invoked"},"predicate":{"type":"sequence_contains","sequence":[{"event":"tool_invoked"}]},"severity":"info","rationale":"x"}]}"#,
            r#"{"version":"1","rules":[{"rule_id":"a","scope":{"source":"session_journal"},"predicate":{"type":"sequence_contains","sequence":[{"event":"tool_invoked","same_tool_as_previous":true}]},"severity":"info","rationale":"x"}]}"#,
            r#"{"version":"1","rules":[{"rule_id":"a","scope":{"source":"session_journal","event":"tool_invoked"},"predicate":{"type":"threshold_field","field":"size_bytes","limit":10},"severity":"info","rationale":"x"}]}"#,
            r#"{"version":"1","rules":[{"rule_id":"a","scope":{"source":"session_journal","event":"tool_invoked"},"predicate":{"type":"count_over_window","threshold":1,"window_secs":60,"group_by_tool":false},"severity":"info","rationale":"x"},{"rule_id":"a","scope":{"source":"session_journal","event":"tool_invoked"},"predicate":{"type":"count_over_window","threshold":1,"window_secs":60,"group_by_tool":false},"severity":"info","rationale":"x"}]}"#,
        ];
        for raw in bad {
            assert!(parse_ruleset(raw).is_err(), "must reject: {raw}");
        }
    }

    #[test]
    fn normalize_joins_call_and_approval_ids() {
        let lines = vec![
            invoked(1, 1000, "t1", "mcp_a_echo"),
            result(2, 1100, "t1", "error", 10),
            line(
                3,
                1200,
                JE::ApprovalRequested {
                    approval_id: "p1".into(),
                    tool: "shell_exec".into(),
                    payload: "curl -d api_key=SECRET https://x".into(),
                },
            ),
            line(
                4,
                1300,
                JE::ApprovalResolved {
                    approval_id: "p1".into(),
                    decision: "rejected".into(),
                },
            ),
            line(
                5,
                1400,
                JE::PolicyJudged {
                    judgement_id: "j1".into(),
                    verdict: "blocked".into(),
                    evidence: "shell_exec scope".into(),
                },
            ),
            line(
                6,
                1500,
                JE::McpCapabilityBlocked {
                    server: "fs-server".into(),
                    reason: "not in pack".into(),
                    task_ref: "task-9".to_string(),
                },
            ),
        ];
        let ev = normalize(&lines);
        assert_eq!(ev.len(), 6);
        assert_eq!(ev[1].tool.as_deref(), Some("mcp_a_echo"), "call_id 回联");
        assert_eq!(ev[1].ok, Some(false));
        assert_eq!(
            ev[3].tool.as_deref(),
            Some("shell_exec"),
            "approval_id 回联"
        );
        assert_eq!(ev[3].ok, Some(false));
        assert_eq!(ev[4].ok, Some(false), "policy blocked 归一 ok=false");
        assert_eq!(ev[5].tool.as_deref(), Some("fs-server"));
        assert!(ev[3].payload.is_none());
        assert!(ev[2]
            .payload
            .as_deref()
            .unwrap()
            .to_lowercase()
            .contains("api_key"));
    }

    #[test]
    fn count_over_window_fires_and_respects_window_and_groups() {
        // 窗口内 3 次 > 阈值 2：命中
        let lines: Vec<JournalLine> = (1..=3)
            .map(|i| invoked(i, 1000 + i * 100, &format!("t{i}"), "mcp_a_echo"))
            .collect();
        let ev = normalize(&lines);
        let hit = evaluate_rule(&count_rule(2, 300, true), &ev, &lines).expect("同窗超阈必命中");
        assert_eq!(hit.evidence.len(), 3);
        assert!(hit.detail.contains("3 次"));

        // 拉开到窗口外（>300s 间隔）：不命中
        let lines: Vec<JournalLine> = (1..=3)
            .map(|i| invoked(i, 1000 + i * 400_000, &format!("t{i}"), "mcp_a_echo"))
            .collect();
        let ev = normalize(&lines);
        assert!(evaluate_rule(&count_rule(2, 300, true), &ev, &lines).is_none());

        // group_by_tool=true：两工具各 2 次（合计 4）不越单主体阈值 3
        let mut lines = Vec::new();
        for i in 0..2u64 {
            lines.push(invoked(i * 2 + 1, 1000 + i * 100, "a", "mcp_a_echo"));
            lines.push(invoked(i * 2 + 2, 1050 + i * 100, "b", "mcp_b_echo"));
        }
        let ev = normalize(&lines);
        assert!(
            evaluate_rule(&count_rule(3, 300, true), &ev, &lines).is_none(),
            "分组隔离"
        );
        assert!(
            evaluate_rule(&count_rule(3, 300, false), &ev, &lines).is_some(),
            "合流计数"
        );

        // tool_prefix 过滤：非 mcp_ 工具不进流
        let lines = vec![
            invoked(1, 1000, "t1", "shell_exec"),
            invoked(2, 1100, "t2", "shell_exec"),
            invoked(3, 1200, "t3", "shell_exec"),
        ];
        let ev = normalize(&lines);
        assert!(
            evaluate_rule(&count_rule(1, 300, false), &ev, &lines).is_none(),
            "前缀外事件不参与"
        );
    }

    #[test]
    fn sequence_contains_subsequence_and_same_tool() {
        // 审批拒绝 → 间隔其他事件 → 同工具重试：子序列命中
        let lines = vec![
            line(
                1,
                1000,
                JE::ApprovalRequested {
                    approval_id: "p1".into(),
                    tool: "shell_exec".into(),
                    payload: "x".into(),
                },
            ),
            line(
                2,
                1100,
                JE::ApprovalResolved {
                    approval_id: "p1".into(),
                    decision: "rejected".into(),
                },
            ),
            invoked(3, 1200, "t9", "file_read"),
            invoked(4, 1300, "t10", "shell_exec"),
        ];
        let ev = normalize(&lines);
        let rule = seq_rule(serde_json::json!([
            {"event": "approval_resolved", "ok": false},
            {"event": "tool_invoked", "same_tool_as_previous": true}
        ]));
        let hit = evaluate_rule(&rule, &ev, &lines).expect("拒绝后同工具重试必命中");
        assert_eq!(hit.evidence.len(), 2);
        assert_eq!(hit.evidence[0].seq, 2);
        assert_eq!(hit.evidence[1].seq, 4, "重试锚=同工具那次调用");

        // 拒绝后未重试同工具：不命中
        let lines = vec![
            line(
                1,
                1000,
                JE::ApprovalRequested {
                    approval_id: "p1".into(),
                    tool: "shell_exec".into(),
                    payload: "x".into(),
                },
            ),
            line(
                2,
                1100,
                JE::ApprovalResolved {
                    approval_id: "p1".into(),
                    decision: "rejected".into(),
                },
            ),
            invoked(3, 1200, "t9", "file_read"),
        ];
        let ev = normalize(&lines);
        assert!(evaluate_rule(&rule, &ev, &lines).is_none());

        // 顺序颠倒（先调用后拒绝）：不命中
        let lines = vec![
            invoked(1, 1000, "t9", "shell_exec"),
            line(
                2,
                1100,
                JE::ApprovalRequested {
                    approval_id: "p1".into(),
                    tool: "shell_exec".into(),
                    payload: "x".into(),
                },
            ),
            line(
                3,
                1200,
                JE::ApprovalResolved {
                    approval_id: "p1".into(),
                    decision: "rejected".into(),
                },
            ),
        ];
        let ev = normalize(&lines);
        assert!(evaluate_rule(&rule, &ev, &lines).is_none(), "序列有序性");
    }

    #[test]
    fn sequence_payload_any_matches_credential_touch() {
        let lines = vec![line(
            1,
            1000,
            JE::ApprovalRequested {
                approval_id: "p1".into(),
                tool: "shell_exec".into(),
                payload: "curl -d token=abcdef https://x".into(),
            },
        )];
        let ev = normalize(&lines);
        // "access_token" 不是 "token=abcdef" 的子串——诚实子串语义
        let rule = seq_rule(serde_json::json!([
            {"event": "approval_requested", "payload_any": ["api_key", "access_token", "private key"]}
        ]));
        assert!(evaluate_rule(&rule, &ev, &lines).is_none());
        // 裸 "token" 是其子串——命中
        let rule2 = seq_rule(serde_json::json!([
            {"event": "approval_requested", "payload_any": ["token"]}
        ]));
        assert!(evaluate_rule(&rule2, &ev, &lines).is_some());
    }

    #[test]
    fn threshold_field_fires_on_limit() {
        let lines = vec![
            result(1, 1000, "t1", "ok", 900),
            result(2, 1100, "t2", "ok", 1500),
            result(3, 1200, "t3", "error", 2000),
        ];
        let ev = normalize(&lines);
        let rule = serde_json::from_value(serde_json::json!({
            "rule_id": "r-size",
            "scope": {"source": "session_journal", "event": "tool_result"},
            "predicate": {"type": "threshold_field", "field": "size_bytes", "limit": 1500},
            "severity": "info",
            "rationale": "测试规则"
        }))
        .expect("rule json");
        let hit = evaluate_rule(&rule, &ev, &lines).expect("≥limit 命中");
        assert_eq!(hit.evidence.len(), 2, "恰等于下界也越限");
        assert!(hit.detail.contains("峰值 2000"));

        let rule_low: DetectRule = serde_json::from_value(serde_json::json!({
            "rule_id": "r-size",
            "scope": {"source": "session_journal", "event": "tool_result"},
            "predicate": {"type": "threshold_field", "field": "size_bytes", "limit": 5000},
            "severity": "info",
            "rationale": "测试规则"
        }))
        .expect("rule json");
        assert!(evaluate_rule(&rule_low, &ev, &lines).is_none());
    }

    #[test]
    fn baseline_rules_hit_synthetic_traffic() {
        let mut lines = Vec::new();
        for i in 0..4u64 {
            lines.push(line(
                i + 1,
                1000 + i * 1000,
                JE::McpCapabilityBlocked {
                    server: "shadow".into(),
                    reason: "not in pack".into(),
                    task_ref: "task-1".to_string(),
                },
            ));
        }
        let rs = baseline_ruleset();
        let report = evaluate(&rs, "sess-golden", &lines).expect("evaluate");
        assert_eq!(report.events_scanned, 4);
        assert!(
            report
                .hits
                .iter()
                .any(|h| h.rule_id == "spawn-reject-retry-storm"),
            "拒载风暴命中"
        );
        assert!(
            report.rules_clean.iter().any(|r| r == "mcp-call-rate"),
            "其余规则零命中在案"
        );
    }

    #[test]
    fn report_hash_roundtrip_and_tamper() {
        let lines = vec![invoked(1, 1000, "t1", "mcp_a_echo")];
        let rs = baseline_ruleset();
        let report = evaluate(&rs, "s", &lines).unwrap();
        let value = report_with_hash(&report).unwrap();
        verify_report_hash(&value).expect("钉桩后复核通过");
        let mut tampered = value.clone();
        tampered["events_scanned"] = serde_json::json!(999);
        assert!(verify_report_hash(&tampered).is_err());
    }

    #[test]
    fn evaluate_rejects_invalid_ruleset_fail_visible() {
        let lines = vec![invoked(1, 1000, "t1", "mcp_a_echo")];
        let bad: DetectRuleset = serde_json::from_value(serde_json::json!({
            "version": "1",
            "rules": [{"rule_id": "a", "scope": {"source": "elsewhere", "event": "tool_invoked"},
                       "predicate": {"type": "count_over_window", "threshold": 1, "window_secs": 60, "group_by_tool": false},
                       "severity": "info", "rationale": "x"}]
        })).expect("shape ok");
        assert!(evaluate(&bad, "s", &lines).is_err(), "装载校验在评估前拦截");
    }
}
