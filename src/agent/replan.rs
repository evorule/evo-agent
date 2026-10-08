// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! replan 触发条件纯函数（plan-execute 方案 D Phase 1-A T4；交付物 6 实现契约）
//!
//! ## 定位
//!
//! G9 工作流执行失败或预算耗尽时，外层驱动依据本模块的判定函数决定是否
//! 触发 replan（重调 planner 产出下一版 PlanFact）。判定是**纯函数**：
//! 输入全显式（无环境/时钟读取——墙钟作为计数器**值**由调用方传入）、
//! 同输入必同输出、LLM 不在判定路径上。
//!
//! ## 判定顺序（交付物 6 §2，写死防实现漂移）
//!
//! 1. `replan_count >= max_replan` → `None`（硬上限终止，纲领拍板 3 次）
//! 2. `outcome` 为 Err → `Some(Failure)`（失败优先于预算判定）
//! 3. `counters` 任一维度 >= 阈值 → `Some(Budget)`
//! 4. 其余 → `None`
//!
//! ## 红线核验（交付物 6 §1.3 显式留痕）
//!
//! - 零新增 Fact 类型：失败摘要/预算快照仅在触发 replan 时随 v2 planner
//!   IoRequest 的摘要间接入链（§7 因果链重构表）；本模块全部为应用层内存结构。
//! - 计数器不设独立 Fact（§4.2 降格澄清）：外层驱动内存状态，入链时机 =
//!   replan 摘要 `budget_state` 字段。
//! - `wall_ms` 非确定注记（§4.3）：执行期控制流决策允许非确定输入，确定性
//!   红线约束的是机器事实与协议标识符（不触碰）。
//! - 阈值来自驱动配置禁止进 PlanFact（§5.1）：PlanFact 是 planner LLM 产出，
//!   阈值可入即等于 LLM 可为自己写预算上限。

use serde::{Deserialize, Serialize};

use crate::agent::workflow::WorkflowNode;

/// replan 状态（外层驱动持有；交付物 6 §2 签名）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplanState {
    /// 当前计划版本（v1 起，触发 replan 后由驱动递增）
    pub current_version: u32,
    /// 已发生的 replan 次数（硬上限 = max_replan）
    pub replan_count: u32,
}

/// 预算计数器（外层驱动内存状态，每节点完成时累加；交付物 6 §4.1）
///
/// `tokens_used` 经交付物 7 埋点真实累加（Phase 2 已落地）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetCounters {
    /// 已成功完成节点数（含 compute 与 LLM 节点，跳过不计）
    pub nodes_executed: u64,
    /// 累计墙钟毫秒（非确定源，作为传入计数器值参与判定——§4.3 注记）
    pub wall_ms: u64,
    /// 累计 token（交付物 7 埋点：驱动经 ctx token 计数器真实累加）
    pub tokens_used: u64,
}

/// 预算阈值（驱动配置，交付物 6 §5；禁止进 PlanFact——§5.1 裁决）
///
/// `None` 维度不参与判定：`max_tokens` 缺省 `None`（CLI `--max-tokens` 可启用，
/// 收官遗留 B1；tokens 埋点已随交付物 7 落地）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetThresholds {
    /// replan 硬上限（纲领拍板 3 次）
    pub max_replan: u32,
    /// 最大执行节点数（动态默认 = 展开后节点总数 × 2，防异常重跑）
    pub max_nodes: Option<u64>,
    /// 最大墙钟毫秒（默认 1,800,000 = 30 分钟，对齐引擎会话 TTL 量级）
    pub max_wall_ms: Option<u64>,
    /// 最大 token（CLI `--max-tokens` 可配；缺省 None 不参与判定）
    pub max_tokens: Option<u64>,
}

impl BudgetThresholds {
    /// 驱动缺省阈值（交付物 6 §5.2；max_nodes 随物化结果动态计算）
    pub fn defaults(expanded_node_count: usize) -> Self {
        Self {
            max_replan: 3,
            max_nodes: Some(expanded_node_count as u64 * 2),
            max_wall_ms: Some(1_800_000),
            max_tokens: None,
        }
    }
}

/// 触发原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplanReason {
    /// G9 execute 返回 Err（失败优先于预算判定）
    Failure,
    /// 预算计数器任一维度达到阈值
    Budget,
}

/// 节点失败分类（分类路由）：接口失败区别于粒失败；漂移为分类学预留位
/// （执行面标注源后置——v0 分类判定只产粒/接口两类，路由面预留其处置位）；
/// 能力缺口为路由判定产物（非执行面标注——原子粒重切预算耗尽时由路由面改标）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeFailureClass {
    /// 粒失败：agent/compute 执行本身失败
    Granule,
    /// 接口失败：产出违反粒间契约（非 JSON / schema 校验不过，契约 v0）
    Interface,
    /// 漂移：节点产出偏离任务语义（治漂手段为独立实验线，不并入本路由——
    /// v0 无执行面标注源，路由面预留「不重拆」处置位）
    Drift,
    /// 原子粒-能力缺口：原子粒重切预算耗尽（切法空间内不可解——路由判定
    /// 改标，随失败摘要与上报事件落账）
    CapabilityGap,
}

/// 失败路由决策（分类路由表：由失败分类 + 原子粒标记 + 重切预算共同决定）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureRoute {
    /// 粒失败 → replan 再拆（常规恢复）
    ReplanSplit,
    /// 接口失败 → replan 但不重拆：粒本身正确，拆小无益——契约缺口描述随
    /// 失败摘要回喂，指令约束 planner 修复产出以满足粒间契约
    ReplanRepairContract,
    /// 漂移 → 不重拆（v0 显式终止不静默；治漂走独立实验线）
    DriftHalt,
    /// 原子粒-能力缺口 → 终止不再拆 + 上报（marks 链入账 + writeback 事件）
    CapabilityGapHalt,
}

/// 失败路由输入（外层驱动按失败节点反查回填；纯函数显式输入——判定面
/// 不引入 LLM/时钟/环境）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureRouteInput {
    /// 失败节点是否原子粒（当前版节点标记，或原子粒记忆命中——原子性是
    /// 工作事实，一经标记跨版本持续有效）
    pub atomic: bool,
    /// 该节点已发生的重切次数（驱动按失败节点 id 计数）
    pub recut_count: u32,
    /// 重切预算 N（驱动配置，缺省 3）
    pub max_recuts: u32,
}

/// 失败路由表（纯函数；同输入必同输出）
///
/// 判定顺序：分类优先（接口/漂移有专属路由），原子粒重切预算其次——粒失败
/// 且原子粒标记且重切计数已达预算 → 能力缺口（不再拆）；其余粒失败 → 常规
/// 再拆。能力缺口分类直接映射终止路由（防御性直通，正常流中该分类由本表
/// 产出而非输入）。
pub fn route_for_failure_class(
    class: NodeFailureClass,
    atomic: bool,
    recut_count: u32,
    max_recuts: u32,
) -> FailureRoute {
    match class {
        NodeFailureClass::Interface => FailureRoute::ReplanRepairContract,
        NodeFailureClass::Drift => FailureRoute::DriftHalt,
        NodeFailureClass::CapabilityGap => FailureRoute::CapabilityGapHalt,
        NodeFailureClass::Granule => {
            if atomic && recut_count >= max_recuts {
                FailureRoute::CapabilityGapHalt
            } else {
                FailureRoute::ReplanSplit
            }
        }
    }
}

/// 按失败文本分类（纯函数）：携带接口契约标记 → Interface，否则 Granule
pub fn classify_node_failure(error_message: &str) -> NodeFailureClass {
    if error_message.contains(crate::agent::workflow::INTERFACE_CONTRACT_MARKER) {
        NodeFailureClass::Interface
    } else {
        NodeFailureClass::Granule
    }
}

/// 工作流失败事件记录（外层驱动内存结构 + 摘要 JSON 形态；交付物 6 §3.1）
///
/// 本结构是**索引与摘要载体，不是新 Fact 类型**（§3.1 链上事实澄清）：失败节点
/// 的事实已在其子 agent 会话链上（Error 事件/IoRequest 链），`failed_plan_hash`
/// 把失败锚定到具体计划版本——审计者经 PlanFact → 物化 DAG → 失败节点会话链，
/// 因果完整可重构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkflowFailureRecord {
    /// 触发类型（固定 "node_failure"）
    pub trigger: String,
    /// 失败的计划版本号
    pub failed_plan_version: u32,
    /// 失败计划的入链形态 hash（64-hex；骨架阶段外层未持有 PlanFact 入链 hash 时为 None）
    pub failed_plan_hash: Option<String>,
    /// 失败节点 id（从 G9 Err 文本解析；格式不符时 None——不猜测、不静默丢弃）
    pub failed_node_id: Option<String>,
    /// 失败节点 agent 类型（外层按 failed_node_id 反查；反查不到为 None）
    pub agent_type: Option<String>,
    /// 失败分类（分类路由：粒/接口/漂移/能力缺口；接口失败由执行面标注，
    /// 能力缺口由路由判定改标——见 [`route_for_failure_class`]）
    pub failure_class: NodeFailureClass,
    /// G9 execute Err 全文
    pub error_message: String,
}

/// replan 决策（交付物 6 §2：None | { reason, failure_record?, budget_snapshot }）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplanDecision {
    /// 触发原因（Failure 失败优先于 Budget 预算）
    pub reason: ReplanReason,
    /// 仅 Failure 触发时携带
    pub failure_record: Option<WorkflowFailureRecord>,
    /// 仅 Budget 触发时携带（触发时计数器快照）
    pub budget_snapshot: Option<BudgetCounters>,
    /// 失败路由（分类路由表产物；Budget 触发无失败路由 = None）
    pub route: Option<FailureRoute>,
}

/// replan 触发判定总函数（交付物 6 §2；纯函数，判定顺序写死）
pub fn should_replan(
    outcome: &Result<String, String>,
    counters: &BudgetCounters,
    thresholds: &BudgetThresholds,
    replan_state: &ReplanState,
    route_input: &FailureRouteInput,
) -> Option<ReplanDecision> {
    // 1. 硬上限终止（优先于一切，即使本次执行失败也不再 replan）
    if replan_state.replan_count >= thresholds.max_replan {
        return None;
    }
    // 2. 失败优先于预算判定；失败分类路由（粒→再拆 / 接口→修契约不重拆 /
    //    漂移→终止 / 原子粒重切预算耗尽→能力缺口终止）
    if let Err(err_text) = outcome {
        let failure_class = classify_node_failure(err_text);
        let route = route_for_failure_class(
            failure_class,
            route_input.atomic,
            route_input.recut_count,
            route_input.max_recuts,
        );
        // 路由判为能力缺口 → 失败分类改标（摘要/上报事件随路由一致）
        let record_class = if route == FailureRoute::CapabilityGapHalt {
            NodeFailureClass::CapabilityGap
        } else {
            failure_class
        };
        return Some(ReplanDecision {
            reason: ReplanReason::Failure,
            failure_record: Some(WorkflowFailureRecord {
                trigger: "node_failure".to_string(),
                failed_plan_version: replan_state.current_version,
                failed_plan_hash: None,
                failed_node_id: parse_failed_node_id(err_text),
                agent_type: None, // 由外层驱动按 failed_node_id 反查后回填
                failure_class: record_class,
                error_message: err_text.clone(),
            }),
            budget_snapshot: None,
            route: Some(route),
        });
    }
    // 3. 预算任一维度达到阈值（None 维度不参与）
    let budget_exhausted = counters.nodes_executed >= thresholds.max_nodes.unwrap_or(u64::MAX)
        || counters.wall_ms >= thresholds.max_wall_ms.unwrap_or(u64::MAX)
        || counters.tokens_used >= thresholds.max_tokens.unwrap_or(u64::MAX);
    if budget_exhausted {
        return Some(ReplanDecision {
            reason: ReplanReason::Budget,
            failure_record: None,
            budget_snapshot: Some(counters.clone()),
            route: None,
        });
    }
    // 4. 其余不触发
    None
}

/// 从 G9 失败消息解析失败节点 id（纯函数；交付物 6 §3.1）
///
/// G9 失败消息格式固定：`workflow node '<id>' failed: <error>`——解析失败
/// （格式不符，如 validate 错误/cycle detected/output_node 无结果）时返回
/// `None`，调用方将 Err 全文置入 error_message（不猜测、不静默丢弃）。
pub fn parse_failed_node_id(err_text: &str) -> Option<String> {
    let rest = err_text.strip_prefix("workflow node '")?;
    let end = rest.find('\'')?;
    let id = &rest[..end];
    if id.is_empty() || !rest[end + 1..].starts_with(" failed: ") {
        return None;
    }
    Some(id.to_string())
}

/// 按失败节点 id 反查 agent 类型（纯函数查表；交付物 6 §3.1）
///
/// 输入为**展开后**节点表（G9 Err 中的 node id 是物化产物副本全名）；
/// 查不到（Err 解析失败场景）返回 None。
pub fn lookup_agent_type(nodes: &[WorkflowNode], node_id: &str) -> Option<String> {
    nodes
        .iter()
        .find(|n| n.id == node_id)
        .map(|n| n.agent_type.clone())
}

/// 按失败节点 id 反查原子粒标记（纯函数查表）
///
/// 查不到 = false 不猜测——能力缺口判定要求显式标记，缺标记按常规粒处理。
pub fn lookup_node_atomic(nodes: &[WorkflowNode], node_id: &str) -> bool {
    nodes
        .iter()
        .find(|n| n.id == node_id)
        .map(|n| n.atomic)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counters(nodes: u64, wall: u64) -> BudgetCounters {
        BudgetCounters {
            nodes_executed: nodes,
            wall_ms: wall,
            tokens_used: 0,
        }
    }

    fn thresholds() -> BudgetThresholds {
        BudgetThresholds {
            max_replan: 3,
            max_nodes: Some(10),
            max_wall_ms: Some(1_000),
            max_tokens: None,
        }
    }

    fn state(count: u32) -> ReplanState {
        ReplanState {
            current_version: 1,
            replan_count: count,
        }
    }

    fn route_input(atomic: bool, recut: u32, max: u32) -> FailureRouteInput {
        FailureRouteInput {
            atomic,
            recut_count: recut,
            max_recuts: max,
        }
    }

    // ----- 失败路由表（分类路由：四类各一 UT；纯函数同输入同输出）-----

    #[test]
    fn route_granule_replans_split() {
        // 粒失败（非原子粒 / 预算内）→ 常规再拆
        assert_eq!(
            route_for_failure_class(NodeFailureClass::Granule, false, 0, 3),
            FailureRoute::ReplanSplit
        );
        assert_eq!(
            route_for_failure_class(NodeFailureClass::Granule, true, 2, 3),
            FailureRoute::ReplanSplit
        );
    }

    #[test]
    fn route_interface_repairs_contract_without_resplit() {
        // 接口失败 → 不重拆，契约缺口回喂修复
        assert_eq!(
            route_for_failure_class(NodeFailureClass::Interface, false, 0, 3),
            FailureRoute::ReplanRepairContract
        );
        assert_eq!(
            route_for_failure_class(NodeFailureClass::Interface, true, 9, 3),
            FailureRoute::ReplanRepairContract
        );
    }

    #[test]
    fn route_drift_halts_without_resplit() {
        // 漂移 → 不重拆终止（治漂独立线，v0 显式不静默）
        assert_eq!(
            route_for_failure_class(NodeFailureClass::Drift, false, 0, 3),
            FailureRoute::DriftHalt
        );
    }

    #[test]
    fn route_capability_gap_when_atomic_recut_budget_exhausted() {
        // 原子粒 + 重切计数达预算 → 能力缺口（不再拆）；>= 语义等值触发
        assert_eq!(
            route_for_failure_class(NodeFailureClass::Granule, true, 3, 3),
            FailureRoute::CapabilityGapHalt
        );
        // 非原子粒 → 永远常规再拆（能力缺口要求显式标记）
        assert_eq!(
            route_for_failure_class(NodeFailureClass::Granule, false, 9, 3),
            FailureRoute::ReplanSplit
        );
    }

    #[test]
    fn route_is_deterministic() {
        // 纯函数：同输入必同输出
        let a = route_for_failure_class(NodeFailureClass::Granule, true, 1, 3);
        let b = route_for_failure_class(NodeFailureClass::Granule, true, 1, 3);
        assert_eq!(a, b);
    }

    #[test]
    fn lookup_node_atomic_from_expanded_nodes() {
        let nodes = vec![WorkflowNode {
            id: "atomic_x".to_string(),
            agent_type: "researcher".to_string(),
            task: String::new(),
            task_template: None,
            depends_on: Vec::new(),
            run_when: None,
            compute: None,
            output_schema: None,
            judge: None,
            atomic: true,
        }];
        assert!(lookup_node_atomic(&nodes, "atomic_x"));
        assert!(!lookup_node_atomic(&nodes, "ghost")); // 查不到 = false 不猜测
    }

    // ----- 判定顺序四分支（交付物 6 §2，顺序写死）-----

    #[test]
    fn decision_order_hard_limit_terminates_even_on_failure() {
        // 1. replan_count >= max_replan → None（即使本次执行失败也不再 replan）
        let outcome: Result<String, String> = Err("workflow node 'a' failed: boom".to_string());
        assert_eq!(
            should_replan(
                &outcome,
                &counters(99, 99_999),
                &thresholds(),
                &state(3),
                &route_input(false, 0, 3)
            ),
            None
        );
    }

    #[test]
    fn decision_order_failure_precedes_budget() {
        // 2. Err 优先于预算判定：Err + 计数器双超 → Failure 而非 Budget
        let outcome: Result<String, String> = Err("workflow node 'a' failed: boom".to_string());
        let d = should_replan(
            &outcome,
            &counters(99, 99_999),
            &thresholds(),
            &state(0),
            &route_input(false, 0, 3),
        )
        .expect("failure must trigger");
        assert_eq!(d.reason, ReplanReason::Failure);
        assert!(d.budget_snapshot.is_none());
        assert_eq!(d.route, Some(FailureRoute::ReplanSplit));
        let record = d.failure_record.expect("failure record present");
        assert_eq!(record.trigger, "node_failure");
        assert_eq!(record.failed_plan_version, 1);
        assert_eq!(record.failed_node_id.as_deref(), Some("a"));
        assert_eq!(record.error_message, "workflow node 'a' failed: boom");
    }

    #[test]
    fn decision_budget_per_dimension() {
        // 3. 计数器各维度独立触发（Ok 但预算耗尽 → Budget）
        let ok: Result<String, String> = Ok("done".to_string());
        for c in [counters(10, 0), counters(0, 1_000)] {
            let d = should_replan(&ok, &c, &thresholds(), &state(0), &route_input(false, 0, 3))
                .expect("budget trigger");
            assert_eq!(d.reason, ReplanReason::Budget);
            assert!(d.failure_record.is_none());
            assert!(d.route.is_none());
            assert_eq!(d.budget_snapshot, Some(c.clone()));
        }
        // 恰好低于阈值 → 不触发（>= 语义：等值触发）
        assert_eq!(
            should_replan(
                &ok,
                &counters(9, 999),
                &thresholds(),
                &state(0),
                &route_input(false, 0, 3)
            ),
            None
        );
    }

    #[test]
    fn decision_none_dimension_never_triggers() {
        // None 维度不参与判定（含缺省 max_tokens = None）
        let ok: Result<String, String> = Ok("done".to_string());
        let t = BudgetThresholds {
            max_replan: 3,
            max_nodes: None,
            max_wall_ms: None,
            max_tokens: None,
        };
        assert_eq!(
            should_replan(
                &ok,
                &counters(u64::MAX / 2, u64::MAX / 2),
                &t,
                &state(0),
                &route_input(false, 0, 3)
            ),
            None
        );
    }

    #[test]
    fn decision_budget_tokens_dimension() {
        // tokens 维度独立触发（B1 启用：CLI --max-tokens 可配；>= 语义等值触发）
        let ok: Result<String, String> = Ok("done".to_string());
        let t = BudgetThresholds {
            max_replan: 3,
            max_nodes: None,
            max_wall_ms: None,
            max_tokens: Some(50),
        };
        let c = BudgetCounters {
            nodes_executed: 0,
            wall_ms: 0,
            tokens_used: 50,
        };
        let d = should_replan(&ok, &c, &t, &state(0), &route_input(false, 0, 3))
            .expect("tokens budget trigger");
        assert_eq!(d.reason, ReplanReason::Budget);
        assert!(d.failure_record.is_none());
        assert_eq!(d.budget_snapshot, Some(c));
        // 低于阈值 → 不触发
        let under = BudgetCounters {
            nodes_executed: 0,
            wall_ms: 0,
            tokens_used: 49,
        };
        assert_eq!(
            should_replan(&ok, &under, &t, &state(0), &route_input(false, 0, 3)),
            None
        );
    }

    #[test]
    fn decision_is_deterministic() {
        // 纯函数：同输入必同输出
        let outcome: Result<String, String> = Err("workflow node 'x' failed: e".to_string());
        let a = should_replan(
            &outcome,
            &counters(1, 1),
            &thresholds(),
            &state(0),
            &route_input(true, 1, 3),
        );
        let b = should_replan(
            &outcome,
            &counters(1, 1),
            &thresholds(),
            &state(0),
            &route_input(true, 1, 3),
        );
        assert_eq!(a, b);
    }

    // ----- Err 文本解析 / agent_type 反查（交付物 6 §3.1）-----

    #[test]
    fn parse_failed_node_id_formats() {
        assert_eq!(
            parse_failed_node_id("workflow node 'research_iter1_search' failed: timeout"),
            Some("research_iter1_search".to_string())
        );
        // 非节点失败形态（validate/cycle/output_node）→ None（不猜测）
        assert_eq!(
            parse_failed_node_id("cycle detected in workflow; nodes involved: []"),
            None
        );
        assert_eq!(
            parse_failed_node_id("output node 'x' produced no result"),
            None
        );
        assert_eq!(parse_failed_node_id("workflow node '' failed: e"), None);
        assert_eq!(parse_failed_node_id(""), None);
    }

    #[test]
    fn lookup_agent_type_expanded_nodes() {
        let nodes = vec![WorkflowNode {
            id: "lp_iter0_s".to_string(),
            agent_type: "researcher".to_string(),
            task: String::new(),
            task_template: None,
            depends_on: Vec::new(),
            run_when: None,
            compute: None,
            output_schema: None,
            judge: None,
            atomic: false,
        }];
        assert_eq!(
            lookup_agent_type(&nodes, "lp_iter0_s"),
            Some("researcher".to_string())
        );
        assert_eq!(lookup_agent_type(&nodes, "ghost"), None);
    }

    // ----- 阈值缺省 / 摘要 JSON 形态（交付物 6 §5.2 / §3.1）-----

    #[test]
    fn thresholds_defaults_dynamic_max_nodes() {
        let t = BudgetThresholds::defaults(8);
        assert_eq!(t.max_replan, 3);
        assert_eq!(t.max_nodes, Some(16)); // 展开后节点总数 × 2
        assert_eq!(t.max_wall_ms, Some(1_800_000));
        assert_eq!(t.max_tokens, None); // Phase 2 启用
    }

    #[test]
    fn failure_record_summary_json_shape() {
        let record = WorkflowFailureRecord {
            trigger: "node_failure".to_string(),
            failed_plan_version: 1,
            failed_plan_hash: Some("a".repeat(64)),
            failed_node_id: Some("research_iter1_search".to_string()),
            agent_type: Some("researcher".to_string()),
            failure_class: NodeFailureClass::Granule,
            error_message: "workflow node 'research_iter1_search' failed: boom".to_string(),
        };
        let json = serde_json::to_value(&record).unwrap();
        // §3.1 摘要 JSON 形态：snake_case 字段名逐字对齐
        for key in [
            "trigger",
            "failed_plan_version",
            "failed_plan_hash",
            "failed_node_id",
            "agent_type",
            "failure_class",
            "error_message",
        ] {
            assert!(json.get(key).is_some(), "missing key {key}: {json}");
        }
        assert_eq!(json["failure_class"], "granule");
    }

    // ----- 失败分类（契约 v0：接口失败区别于粒失败）-----

    #[test]
    fn classify_node_failure_by_marker() {
        let interface_text =
            crate::agent::workflow::interface_contract_error("a", "output is not valid JSON: boom");
        assert_eq!(
            classify_node_failure(&interface_text),
            NodeFailureClass::Interface
        );
        assert_eq!(
            classify_node_failure("workflow node 'a' failed: boom"),
            NodeFailureClass::Granule
        );
    }

    #[test]
    fn should_replan_records_interface_class() {
        let outcome: Result<String, String> =
            Err(crate::agent::workflow::interface_contract_error(
                "s",
                "0: \"x\" is not of type \"object\"",
            ));
        let d = should_replan(
            &outcome,
            &counters(0, 0),
            &thresholds(),
            &state(0),
            &route_input(false, 0, 3),
        )
        .expect("failure must trigger");
        let record = d.failure_record.expect("record present");
        assert_eq!(record.failure_class, NodeFailureClass::Interface);
        assert_eq!(record.failed_node_id.as_deref(), Some("s"));
        assert_eq!(d.route, Some(FailureRoute::ReplanRepairContract));
        let json = serde_json::to_string_pretty(&record).unwrap();
        assert!(
            json.contains("\"failure_class\": \"interface\""),
            "摘要 JSON 须携带 interface 分类: {json}"
        );
    }

    #[test]
    fn should_replan_halts_capability_gap_and_relabels_class() {
        // 原子粒重切预算耗尽 → 终止路由 + 失败分类改标 capability_gap（随摘要
        // JSON 形态落账——上报事件与摘要分类一致）
        let outcome: Result<String, String> = Err("workflow node 'a' failed: boom".to_string());
        let d = should_replan(
            &outcome,
            &counters(0, 0),
            &thresholds(),
            &state(0),
            &route_input(true, 3, 3),
        )
        .expect("halt decision must trigger");
        assert_eq!(d.reason, ReplanReason::Failure);
        assert_eq!(d.route, Some(FailureRoute::CapabilityGapHalt));
        let record = d.failure_record.expect("record present");
        assert_eq!(record.failure_class, NodeFailureClass::CapabilityGap);
        assert_eq!(record.failed_node_id.as_deref(), Some("a"));
        let json = serde_json::to_string_pretty(&record).unwrap();
        assert!(
            json.contains("\"failure_class\": \"capability_gap\""),
            "摘要 JSON 须携带 capability_gap 分类: {json}"
        );
    }
}
