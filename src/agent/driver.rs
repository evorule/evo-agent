// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! plan-execute 外层驱动循环（纲领 §8 Phase 1-D 交付物 4 + replan 循环落地；Phase 1-B）
//!
//! ## 职责
//!
//! 编排「计划 → 执行 → （失败/预算）重规划」的应用层主循环：
//!
//! - [`PlanMode::Dsl`]：载入的手写 workflow 即 v1 计划直接执行；失败/预算触发
//!   replan 时由 planner 产出 PlanFact v2+。
//! - [`PlanMode::PlanExecute`]：先执行载入的 **planning probe DAG**（单 planner
//!   节点，纲领 Phase 1-D 交付物 4「跑 planner 单节点 DAG」），其输出解析为
//!   PlanFact v1 → [`materialize_plan_fact`] 物化 → 执行。
//! - replan（丢弃式 D-02，纲领 §9.4）：v(n) 失败或预算耗尽 → 按 §9.4.3 摘要
//!   构造 replan 任务 → planner（走 [`DelegateContext::delegate`] 既有路径）产
//!   PlanFact v(n+1) → 物化 → **全新 execute**（v1 已执行结果不注入）。
//!
//! ## 红线核验（显式留痕）
//!
//! - planner 走 delegate 既有路径 = IoRequest sidecar 审计链，**零新增 Fact
//!   类型、零新增审计通道**（交付物 2 §1 两原则）。
//! - 注入组（`plan_source`/`plan_version`/`parent_plan_hash`）为外层驱动权威
//!   注入（交付物 2 J5：LLM 不产出注入组）；`parent_plan_hash` = BLAKE3
//!   64-hex（与生态哈希纪律一致），锚 = 注入后 PlanFact 的 canonical JSON
//!   （`serde_json::to_string`，键序确定）；Dsl v1 的锚 = workflow 文件原文
//!   hash（由调用方以 `seed_hash` 传入）。
//! - 阈值来自驱动配置（CLI），**禁入 PlanFact**（交付物 6 §5.1）。
//! - 摘要 `completed_nodes`/`executed_side_effects` 丢弃式置空（纲领 §9.4.3
//!   Phase 1–2 口径）；不做 LLM 压缩。
//! - 回放面零改动：replay 沿既有链叙述，v1..vN 的 IoRequest 链即因果证据
//!   （交付物 6 §7 因果链重构表）。
//!
//! ## Phase 2 增强（纲领 §8 Phase 2 交付物 6/7 + R1-T03/T04 + R8-T03 + M3）
//!
//! - **D-01 enforce 判别**：节点失败文本带固定前缀 `enforce violation:`
//!   （runner Violation 分支上抛，见 runner.rs）→ 外层驱动判定后**终止不
//!   replan**（§9.5.1 选项 B——宪法违规是系统性错误，不开「换计划再试」通道）。
//! - **planner 重试面**：PlanFact JSON 提取失败 → 带错误反馈重试 1 次
//!   （R1-T04 重试硬上限），仍失败整体终止。
//! - **静态拦截 R8-T03**：v(n+1) 物化前与已执行注册表比对——同 id 同
//!   agent_type 的重复节点 warn + `repeated_nodes` 埋点（丢弃式 D-02 接受
//!   幂等重复成本，纲领 §9.4.2）；写类工具（file_write/shell_exec，schema
//!   J2 已封，防御深度）出现即拒绝提交。
//! - **replan 成本埋点（交付物 7）**：`repeated_nodes` / `tokens_used`（经
//!   DelegateContext 共享累加器，io_response token_usage 汇总）/ `replan_tokens`
//!   （v2+ 版本执行 + replan planner 调用消耗，D-02 Phase 3 启动判定数据源）/
//!   阈值随统计行输出。

use serde_json::{json, Value};

use crate::agent::delegate::DelegateContext;
use crate::agent::journal::{read_all_tolerant, JournalWriter};
use crate::agent::materializer::materialize_plan_fact;
use crate::agent::replan::{
    lookup_agent_type, lookup_node_atomic, parse_failed_node_id, should_replan, BudgetCounters,
    BudgetThresholds, FailureRoute, FailureRouteInput, ReplanReason, ReplanState,
};
use crate::agent::workflow::{Workflow, WorkflowEngine};
use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handlers::tool_handler::ToolHandler;

/// 驱动模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanMode {
    /// 手写 workflow 直接作为 v1 执行（失败/预算可 replan 产 PlanFact v2+）
    Dsl,
    /// plan-execute：先执行 planning probe DAG（单 planner 节点），
    /// 其输出解析为 PlanFact v1 物化执行（纲领 Phase 1-D 交付物 4）
    PlanExecute,
}

/// 驱动限额（CLI 配置；禁入 PlanFact，交付物 6 §5.1）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverLimits {
    /// replan 硬上限（纲领拍板默认 3）
    pub max_replan: u32,
    /// 原子粒重切预算（分类路由，缺省 3；0 = 原子粒失败即判能力缺口）
    pub max_recuts: u32,
    /// 墙钟预算毫秒（默认 1,800,000 = 30 分钟；None = 不限）
    pub max_wall_ms: Option<u64>,
    /// token 预算上限（累计 tokens_used ≥ 阈值触发 Budget；None = 不限，
    /// 收官遗留 B1 启用——tokens 埋点已随交付物 7 落地，阈值可放开）
    pub max_tokens: Option<u64>,
}

/// 驱动循环统计
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanLoopStats {
    /// 执行过的计划版本数（Dsl 无 replan = 1；PlanExecute = 1 + replans）
    pub plan_versions: u32,
    /// 实际发生的 replan 次数
    pub replans: u32,
    /// 全部版本累计完成节点数
    pub nodes_executed: u64,
    /// 全部版本累计墙钟毫秒
    pub wall_ms: u64,
    /// 全部版本累计 token（tokens 埋点，纲领 §8 Phase 2 交付物 7）
    pub tokens_used: u64,
    /// v2+ 版本消耗 token（v2+ 各版执行差值 + replan planner 调用自身消耗；
    /// replan 重复执行成本的总量上界，D-02 Phase 3 增量式启动判定数据源——
    /// 重复执行节点数 × 对应版本 token）
    pub replan_tokens: u64,
    /// 静态拦截命中的重复执行节点数（R8-T03 告警 + 埋点，幂等重复放行）
    pub repeated_nodes: u64,
}

/// 驱动循环成功产出
#[derive(Debug, Clone)]
pub struct PlanLoopOutcome {
    /// 末版计划 `output_node` 结果（纲领交付物 5「产出结果」）
    pub content: String,
    /// 全程统计（版本数/replan 数/节点数/墙钟）
    pub stats: PlanLoopStats,
    /// run 级账本会话名(恢复面凭此回放续跑;None = 无账域/显式关停。
    /// 调用方[如评测 harness]留存此值即可在进程死亡后显式恢复)
    pub run_session_id: Option<String>,
}

/// 计划循环检查点的账面状态(最新 PlanLoopCheckpointed 的投影;
/// `cur_workflow` 为序列化 Workflow JSON,重建时反序列化)
#[derive(Debug, Clone)]
pub struct PlanLoopCheckpointState {
    /// 当前计划版本(v1 起)
    pub version: u32,
    /// 已发生 replan 次数
    pub replan_count: u32,
    /// 已执行注册表 (node_id, agent_type) 跨版本累积
    pub executed_registry: Vec<(String, String)>,
    /// 预算计数器(跨版本累计,恢复不得清零)
    pub counters: BudgetCounters,
    /// 计划形态锚(注入后 PlanFact canonical JSON 的 64-hex hash;Dsl None)
    pub cur_canonical_hash: Option<String>,
    /// 原始目标文本(replan 任务构造输入)
    pub goal: Option<String>,
    /// 当前版工作流(序列化 JSON)
    pub cur_workflow: String,
    /// 协作标记会话 id(run 的治理身份;恢复必须复用同会话——全新标记会话
    /// 会让下一节点的 phase 前置门查不到前置标记而误拦)
    pub marks_session: Option<String>,
    /// 原子粒记忆集(原子性跨版本持续有效;恢复不归零)
    pub atomic_granules: std::collections::HashSet<String>,
    /// 按节点重切计数(重切预算跨版本累计;恢复不归零)
    pub recut_counts: std::collections::HashMap<String, u32>,
}

/// 驱动循环恢复入参(从 run 级账本回放装配;见 [`build_plan_loop_resume`])
#[derive(Debug, Clone)]
pub struct PlanLoopResume {
    /// 计划状态(版本/计数/注册表/预算计数器/锚/目标/当前版工作流)
    pub state: PlanLoopCheckpointState,
    /// 粒级进度种子(最新计划检查点之后检查点尾段的回放重建;
    /// 已完成粒零重执行)
    pub node_replay: crate::agent::workflow::NodeCheckpointReplay,
    /// run 级账本会话名(恢复续写同一账本)
    pub run_session_id: String,
}

/// 从 run 级账本回放最新计划循环检查点(纯账面投影;不做粒级回放——
/// 调用方拿尾部索引自 [`PlanLoopCheckpointState`] 对应工作流的
/// 计划锚切片后走粒级回放,多版本不混切)。
///
/// 返回 `(状态, 尾段起始行索引)`;账内无计划检查点 = `Ok(None)`
/// (该账本不具备驱动级恢复条件,调用方显式报错)。
pub fn replay_plan_loop_checkpoint(
    lines: &[crate::agent::journal::JournalLine],
) -> Result<Option<(PlanLoopCheckpointState, usize)>, String> {
    use crate::agent::journal::{BudgetCountersSnapshot, JournalEvent};
    let mut found: Option<(PlanLoopCheckpointState, usize)> = None;
    for (idx, line) in lines.iter().enumerate() {
        if let JournalEvent::PlanLoopCheckpointed {
            version,
            replan_count,
            executed_registry,
            counters:
                BudgetCountersSnapshot {
                    nodes_executed,
                    wall_ms,
                    tokens_used,
                },
            cur_canonical_hash,
            goal,
            marks_session,
            atomic_granules,
            recut_counts,
            cur_workflow,
        } = &line.event
        {
            found = Some((
                PlanLoopCheckpointState {
                    version: *version,
                    replan_count: *replan_count,
                    executed_registry: executed_registry.clone(),
                    counters: BudgetCounters {
                        nodes_executed: *nodes_executed,
                        wall_ms: *wall_ms,
                        tokens_used: *tokens_used,
                    },
                    cur_canonical_hash: cur_canonical_hash.clone(),
                    goal: goal.clone(),
                    cur_workflow: cur_workflow.clone(),
                    marks_session: marks_session.clone(),
                    atomic_granules: atomic_granules.iter().cloned().collect(),
                    recut_counts: recut_counts.iter().cloned().collect(),
                },
                idx + 1,
            ));
        }
    }
    Ok(found)
}

/// 从 run 级账本装配驱动恢复入参:最新计划检查点状态 + 其后检查点尾段的
/// 粒级回放(按当前版工作流的计划锚严格校验,锚不匹配/blob 缺失/hash 不过
/// 一律拒绝——宁可重跑不可错续)。
pub fn build_plan_loop_resume(
    dir: &std::path::Path,
    session_id: &str,
) -> Result<PlanLoopResume, String> {
    use crate::agent::journal::{read_all_tolerant, JournalWriter};
    let path = JournalWriter::path_for(dir, session_id);
    let lines = read_all_tolerant(&path)
        .map_err(|e| format!("run journal read failed ('{session_id}'): {e}"))?;
    let (state, tail_start) = replay_plan_loop_checkpoint(&lines)?.ok_or_else(|| {
        format!("no plan loop checkpoint in run journal '{session_id}' - nothing to resume from")
    })?;
    let cur_workflow: Workflow = serde_json::from_str(&state.cur_workflow)
        .map_err(|e| format!("checkpoint workflow deserialize failed: {e}"))?;
    let plan_hash = WorkflowEngine::workflow_plan_hash(&cur_workflow)?;
    // 粒级回放喂全量账本(按锚采纳/异版跳过语义)——不做「最新计划检查点
    // 之后」切片:同锚粒检查点无论落在哪个计划检查点前后都有效,切片会让
    // 二次恢复(恢复后写点成为新最新检查点)丢失此前的粒级进度
    let _ = tail_start;
    let node_replay = crate::agent::workflow::replay_node_checkpoints(&lines, &plan_hash)?;
    Ok(PlanLoopResume {
        state,
        node_replay,
        run_session_id: session_id.to_string(),
    })
}

/// 计划循环检查点落账(内部辅助:账写不掉 = Err 上抛,驱动中止——账面硬义务)
#[allow(clippy::too_many_arguments)]
fn write_plan_loop_checkpoint(
    journal: Option<&JournalWriter>,
    version: u32,
    replan_count: u32,
    executed_registry: &[(String, String)],
    counters: &BudgetCounters,
    cur_canonical_hash: Option<&str>,
    goal: Option<&str>,
    marks_session: Option<&str>,
    atomic_granules: &std::collections::HashSet<String>,
    recut_counts: &std::collections::HashMap<String, u32>,
    cur_wf: &Workflow,
) -> Result<(), String> {
    let Some(journal) = journal else {
        return Ok(());
    };
    let cur_workflow = serde_json::to_string(cur_wf).map_err(|e| e.to_string())?;
    journal
        .plan_loop_checkpointed(
            version,
            replan_count,
            executed_registry.to_vec(),
            crate::agent::journal::BudgetCountersSnapshot {
                nodes_executed: counters.nodes_executed,
                wall_ms: counters.wall_ms,
                tokens_used: counters.tokens_used,
            },
            cur_canonical_hash.map(str::to_string),
            goal.map(str::to_string),
            marks_session.map(str::to_string),
            atomic_granules.iter().cloned().collect::<Vec<_>>(),
            recut_counts
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect::<Vec<_>>(),
            &cur_workflow,
        )
        .map_err(|e| format!("plan loop checkpoint write failed: {e}"))?;
    Ok(())
}

/// plan-execute 外层驱动主循环（见模块文档；每版执行 = 全新 `execute`，丢弃式）
///
/// `seed_hash`：Dsl v1 计划形态 hash 锚（workflow 文件原文 BLAKE3 hex）；
/// PlanExecute 模式可传 `None`（v1 hash 取注入后 PlanFact canonical JSON）。
///
/// `marks_session`：M5-b 协作工作流标记会话（`Some` = 启用）。启用后每个节点
/// 成功完成时，驱动向该会话提交中性完成信号指令
/// `set meta_signal.node_done = <node_id>`（机制层伴生事实，驱动不含任何
/// 标记知识）；任务标记（meta_task.*）由规则面 branch 壳+set 业务规则裁决
/// 写入——节点→标记映射全在规则层。信号提交失败 = fail-fast（留痕是硬义务）。
/// `None` = 既有行为零变更（测试/无 server 场景）。
pub async fn run_plan_loop(
    ctx: DelegateContext,
    initial: Workflow,
    mode: PlanMode,
    limits: DriverLimits,
    seed_hash: Option<String>,
    marks_session: Option<String>,
    judge_container: Option<String>,
) -> Result<PlanLoopOutcome, String> {
    run_plan_loop_with_resume(
        ctx,
        initial,
        mode,
        limits,
        seed_hash,
        marks_session,
        judge_container,
        None,
    )
    .await
}

/// 驱动终态落账守卫:任意出口(Ok/Err/`?` 传播)落地 plan_loop_finished——
/// 扫尾面凭终态事件把已终局 run 排除出可恢复列表(无终态且计划检查点在账
/// = 中断 run)。Ok 路径先 [`Self::finish_ok`] 再返回;其余出口 drop 落
/// "error"。账写失败 best-effort(error 日志):主产物优先不因终态标记失败
/// 而翻报——退化面=该 run 被列可恢复,后续恢复尝试按账面状态快进,不产生
/// 粒级重执行(粒级检查点护栏)。
struct PlanFinishGuard {
    journal: std::sync::Mutex<Option<JournalWriter>>,
    done: std::cell::Cell<bool>,
}

impl PlanFinishGuard {
    fn new() -> Self {
        Self {
            journal: std::sync::Mutex::new(None),
            done: std::cell::Cell::new(false),
        }
    }

    /// 绑定 run 账本句柄(账本开立后调用;Dsl/无账域不绑定=守卫空转)
    fn attach(&self, writer: &JournalWriter) {
        *self.journal.lock().unwrap_or_else(|p| p.into_inner()) = Some(writer.clone());
    }

    /// Ok 出口:落 "ok" 终态并解除 drop 侧写
    fn finish_ok(&self) {
        self.write("ok");
        self.done.set(true);
    }

    fn write(&self, status: &str) {
        // 终态幂等:finish_ok 已落 ok 后,drop 侧不得再补 error(已完成 run
        // 落双终态=账面失真)
        if self.done.get() {
            return;
        }
        let writer = self
            .journal
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(j) = writer {
            if let Err(e) = j.plan_loop_finished(status) {
                tracing::error!("plan loop finish marker write failed: {e}");
            }
        }
    }
}

impl Drop for PlanFinishGuard {
    fn drop(&mut self) {
        self.write("error");
    }
}

/// 可恢复 run 信息(扫尾投影;authoritative:false 投影视图,真相源=run 账本)
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResumableRunInfo {
    /// run 级账本会话名(resume 调用方凭此显式恢复)
    pub session_id: String,
    /// 账本种类:plan=计划级 run(planrun- 前缀)/react=单代理会话(悬挂 turn
    /// 的崩溃会话;react 条目的计划域字段为占位零值)
    pub kind: String,
    /// 计划版本(最新计划检查点)
    pub version: u32,
    /// 已发生 replan 次数
    pub replan_count: u32,
    /// 已执行注册表条目数
    pub executed_registry_len: usize,
    /// 恢复可跳过的已完成粒数(最新计划检查点之后尾段的粒级检查点数)
    pub checkpointed_nodes: usize,
    /// 工作流 id(自检查点工作流全文提取)
    pub workflow_id: String,
    /// 累计墙钟毫秒(恢复面剩余预算判断输入)
    pub wall_ms: u64,
    /// 账本末事件 seq
    pub last_seq: u64,
}

/// 扫尾:扫 run 级账本目录,列可恢复 run(计划检查点在账且无终态标记)。
/// 人类持剑:本函数只列不改——续跑决策权在显式 resume 调用方,不自动续跑。
/// 单文件读败跳过并告警(列表面健壮性优先;账面损坏由打开路径 fail-visible)。
pub fn scan_resumable_runs(dir: &std::path::Path) -> Result<Vec<ResumableRunInfo>, String> {
    use crate::agent::journal::JournalEvent;
    let mut out = Vec::new();
    let mut delegate_children: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut react_candidates: Vec<(String, Vec<crate::agent::journal::JournalLine>)> = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("scan dir failed: {e}"))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".jsonl") {
            continue;
        }
        let session_id = name.trim_end_matches(".jsonl").to_string();
        let path = dir.join(&name);
        // 容忍尾部半行(强杀崩溃的自然产物):整本判废会把崩溃会话排除出
        // 可恢复列表——恰是恢复面最需要覆盖的对象
        let Ok(lines) = read_all_tolerant(&path) else {
            tracing::warn!(session = %session_id, "resumable scan: journal unreadable, skipped");
            continue;
        };
        // 委托子会话排除集:被任何账本的委托锚引用的会话不是独立可恢复主体
        //(其恢复主体是父 run)
        for l in &lines {
            if let JournalEvent::DelegateSpawned {
                child_session_id, ..
            } = &l.event
            {
                delegate_children.insert(child_session_id.clone());
            }
        }
        if !name.starts_with("planrun-") {
            react_candidates.push((session_id, lines));
            continue;
        }
        let has_checkpoint = lines.iter().any(|l| {
            matches!(
                l.event,
                crate::agent::journal::JournalEvent::PlanLoopCheckpointed { .. }
            )
        });
        let finished = lines.iter().any(|l| {
            matches!(
                l.event,
                crate::agent::journal::JournalEvent::PlanLoopFinished { .. }
            )
        });
        if !has_checkpoint || finished {
            continue;
        }
        let Some((state, tail_start)) = replay_plan_loop_checkpoint(&lines)? else {
            continue;
        };
        let workflow_id = serde_json::from_str::<serde_json::Value>(&state.cur_workflow)
            .ok()
            .and_then(|v| {
                v.get("workflow_id")
                    .and_then(|w| w.as_str().map(str::to_string))
            })
            .unwrap_or_default();
        let checkpointed_nodes = lines[tail_start..]
            .iter()
            .filter(|l| {
                matches!(
                    l.event,
                    crate::agent::journal::JournalEvent::NodeCheckpointed { .. }
                )
            })
            .count();
        out.push(ResumableRunInfo {
            kind: "plan".to_string(),
            session_id,
            version: state.version,
            replan_count: state.replan_count,
            executed_registry_len: state.executed_registry.len(),
            checkpointed_nodes,
            workflow_id,
            wall_ms: state.counters.wall_ms,
            last_seq: lines.last().map(|l| l.seq).unwrap_or(0),
        });
    }
    // react 面:悬挂 turn(turn_started 无配对收尾)且末事件非崩溃标记的根
    // 会话 = 崩溃恢复候选;委托子会话排除(其恢复主体是父 run)
    for (session_id, lines) in react_candidates {
        if delegate_children.contains(&session_id) {
            continue;
        }
        let mut turn_open = false;
        let mut last_was_crash = false;
        for l in &lines {
            match &l.event {
                JournalEvent::TurnStarted { .. } => turn_open = true,
                JournalEvent::TurnEnded { .. } => turn_open = false,
                _ => {}
            }
            last_was_crash = matches!(l.event, JournalEvent::SessionCrashed { .. });
        }
        if turn_open && !last_was_crash {
            out.push(ResumableRunInfo {
                kind: "react".to_string(),
                session_id,
                version: 0,
                replan_count: 0,
                executed_registry_len: 0,
                checkpointed_nodes: 0,
                workflow_id: String::new(),
                wall_ms: 0,
                last_seq: lines.last().map(|l| l.seq).unwrap_or(0),
            });
        }
    }
    out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(out)
}

/// 驱动循环恢复入口:`resume = Some` 时从 run 级账本回放态续跑(计划状态+
/// 粒级进度,已完成粒零重执行);`None` = 既有行为零变更。计划级检查点仅
/// PlanExecute 模式落账(Dsl 工作流由入参可复建,粒级检查点已覆盖)。
///
/// 恢复态剩余预算闸:恢复态累计墙钟已 ≥ 本跑限额 = 预算已耗尽,恢复无意义
/// ——快速失败(errored 但省 token,诚实面:中断时点已注定无法在限内完成)。
#[allow(clippy::too_many_arguments)] // 恢复入口编排参数平铺,收拢反损调用点对位可读性
pub async fn run_plan_loop_with_resume(
    ctx: DelegateContext,
    initial: Workflow,
    mode: PlanMode,
    limits: DriverLimits,
    seed_hash: Option<String>,
    marks_session: Option<String>,
    judge_container: Option<String>,
    resume: Option<PlanLoopResume>,
) -> Result<PlanLoopOutcome, String> {
    // tokens 埋点累加器（纲领 §8 Phase 2 交付物 7）：驱动注入 ctx，随每个
    // 子 runner 共享；仅供观测统计，不改变任何控制流。
    let token_arc = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ctx = ctx.with_token_counter(token_arc.clone());
    // 恢复态解包(先行;marks_session 以恢复态为准——run 的治理身份必须
    // 跨进程复用,见 PlanLoopCheckpointState 注)
    let (resumed_state, node_replay, run_session_id) = match resume {
        Some(r) => (Some(r.state), Some(r.node_replay), Some(r.run_session_id)),
        None => (None, None, None),
    };
    // 恢复态剩余预算闸(账先于一切推进:预算耗尽的恢复=注定超限,快速失败)
    if let (Some(state), Some(max_wall)) = (resumed_state.as_ref(), limits.max_wall_ms) {
        if state.counters.wall_ms >= max_wall {
            return Err(format!(
                "resumed run wall budget already exhausted (wall_ms={} >= limit {}) \
                 - failing fast (resuming cannot finish within budget)",
                state.counters.wall_ms, max_wall
            ));
        }
    }
    // 驱动终态守卫:任意出口(Ok/Err/传播)落地 plan_loop_finished——扫尾面
    // 凭终态事件把已终局 run 排除出可恢复列表。Ok 路径先 finish_ok 再返回,
    // 其余出口 drop 落 "error"。
    let finish = PlanFinishGuard::new();
    let marks_session = resumed_state
        .as_ref()
        .and_then(|s| s.marks_session.clone())
        .or(marks_session);
    // M5-c:标记会话启用时同步注入阶段前置裁决通道(每个 LLM 节点 delegate
    // 前提交 phase 信号,由 00_constraint enforce 裁决前置条件——引擎不含
    // 协作纪律知识);None = 零变更
    let mut engine = WorkflowEngine::new(ctx.clone()).with_judge_container(judge_container); // 判据 v0:None = 宿主直执行（CLI 既有语义）
    if let Some(sid) = marks_session.as_deref() {
        engine = engine.with_phase_gate(crate::agent::workflow::PhaseGate {
            marks_session: sid.to_string(),
            client: ctx.evorule_client.clone(),
        });
    }
    let initial_id = initial.workflow_id.clone();

    // probe 阶段不入 run 账:probe 粒检查点的计划锚=probe DAG,对恢复面无效
    //(恢复锚恒为最新计划检查点的工作流)且会污染粒级回放的异版判据——
    // probe 前显式关停引擎记账,plan v1 物化后才开账(见下)
    if mode == PlanMode::PlanExecute && resumed_state.is_none() {
        engine = engine.without_run_journal();
    }

    // 当前计划版本（v1 起；Dsl v1 = 手写 workflow，PlanExecute v1 = probe 产出 PlanFact）
    // 恢复态在场时全量取自账面(计数器/注册表/版本不得清零——恢复即续算)
    let mut version: u32 = resumed_state.as_ref().map_or(1, |s| s.version);
    let mut replan_count: u32 = resumed_state.as_ref().map_or(0, |s| s.replan_count);
    let mut counters = resumed_state
        .as_ref()
        .map(|s| s.counters.clone())
        .unwrap_or_default();
    let mut cur_wf = match resumed_state.as_ref() {
        Some(s) => serde_json::from_str::<Workflow>(&s.cur_workflow)
            .map_err(|e| format!("checkpoint workflow deserialize failed: {e}"))?,
        None => initial,
    };
    // 当前版计划形态锚(注入后 PlanFact canonical JSON 的 64-hex;Dsl v1 = None,
    // replan 时锚回退 seed_hash。恢复态直接取账面 hash——原 canonical 串仅被
    // 求哈希,存 hash 即等价)
    let mut cur_canonical_hash: Option<String> = resumed_state
        .as_ref()
        .and_then(|s| s.cur_canonical_hash.clone());
    // 原始目标（PlanExecute = probe planner 节点 task；Dsl = None，摘要引导）
    let mut goal: Option<String> = resumed_state.as_ref().and_then(|s| s.goal.clone());
    // 已执行注册表（跨版本累积 (node_id, agent_type)；R8-T03 静态拦截比对源；
    // 恢复态取账面——不重置不重复）
    let mut executed_registry: Vec<(String, String)> = resumed_state
        .as_ref()
        .map(|s| s.executed_registry.clone())
        .unwrap_or_default();
    // 原子粒工作事实记忆（分类路由）：曾携带 atomic 标记的节点 id 集合——
    // 原子性一经标记跨版本持续有效（replan 重产计划不带 atomic 字段，记忆集
    // 合补事实连续性）
    let mut atomic_granules: std::collections::HashSet<String> = resumed_state
        .as_ref()
        .map(|s| s.atomic_granules.clone())
        .unwrap_or_default();
    // 按节点重切计数（分类路由：原子粒重切预算判定输入；恢复不归零——
    // 归零=恢复 run 重切预算重新起算，预算面方差）
    let mut recut_counts: std::collections::HashMap<String, u32> = resumed_state
        .as_ref()
        .map(|s| s.recut_counts.clone())
        .unwrap_or_default();
    // v2+ 版本 token 消耗（replan 重复执行成本埋点，交付物 7 / D-02 判定源;
    // 跨进程成本统计不入账——恢复后从本进程增量起算,诚实面:成本统计为部分值)
    let mut replan_tokens: u64 = 0;
    // 静态拦截命中累计（R8-T03 幂等重复告警计数）
    let mut repeated_nodes: u64 = 0;
    // 恢复首版标记(首版执行带粒级种子;replan 后版本全新执行)
    let mut first_version_node_replay = node_replay;
    // 借用先行（replan 站点闭包 async move 按值捕获会 move 非 Copy 的 ctx；
    // 借引用则 Copy 复制进 future，跨 loop 轮次复用同一借用）
    let ctx_ref = &ctx;

    if mode == PlanMode::PlanExecute && resumed_state.is_none() {
        // ① planning probe：跑载入 DAG（单 planner 节点），产出 PlanFact v1。
        //    probe 失败 = 尚无计划可 replan，直接失败（R1-T03 重试面在
        //    call_planner_with_retry 内：提取失败带反馈重试 1 次）。
        //    恢复态在场 = 计划状态取自账面,跳过 probe(probe 产 v1 依赖非确定
        //    LLM 输出,重跑即漂移;账面 cur_workflow 是唯一确定性重建源)。
        let probe_out = engine
            .execute(&cur_wf)
            .await
            .map_err(|e| format!("planning probe failed: {}", e))?;
        goal = cur_wf
            .nodes
            .iter()
            .find(|n| n.agent_type == "planner")
            .map(|n| n.task.clone());
        let probe_task = goal.clone().unwrap_or_default();
        // 引用先行（async move 按值捕获会 move 非 Copy 的 engine；借引用则 Copy 复制）
        let engine_ref = &engine;
        let mut plan = call_planner_with_retry(
            |t: String| async move { engine_ref.delegate_context().delegate("planner", &t).await },
            probe_task,
            Some(probe_out),
        )
        .await
        .map_err(|e| format!("planning probe failed: {}", e))?;
        inject_plan_meta(&mut plan, "initial_planning", 1, None);
        let canonical = serde_json::to_string(&plan).map_err(|e| e.to_string())?;
        cur_wf = materialize_plan_fact(&plan, &format!("{}_plan_v1", initial_id))
            .map_err(|errs| format!("plan v1 materialization failed: {}", errs.join("; ")))?;
        cur_canonical_hash = Some(plan_canonical_hash(&canonical));
        tracing::info!(plan_version = 1, "plan-execute: plan v1 materialized");
    }

    // run 级账本开立(PlanExecute;位置在 probe 之后——probe 粒不入账,probe
    // 粒的计划锚=probe DAG,对恢复面无效且污染粒级回放的异版判据;恢复路径
    // 跳过 probe 直达此处,续写同一会话):计划级检查点+引擎粒级检查点同账;
    // Dsl/无账域 = 引擎自举语义不变(粒级检查点仍落,计划级检查点不落)。
    // 开账失败 = fail-fast。
    let run_journal: Option<JournalWriter> = if mode == PlanMode::PlanExecute {
        match ctx.journal_dir.clone() {
            Some(dir) => {
                let sid = match &run_session_id {
                    Some(sid) => sid.clone(),
                    None => WorkflowEngine::new_run_session_id(&initial_id),
                };
                let writer = JournalWriter::open(&dir, &sid)
                    .map_err(|e| format!("run ledger open failed (session '{sid}'): {e}"))?;
                finish.attach(&writer);
                engine = engine.with_run_journal(writer.clone());
                Some(writer)
            }
            None => None,
        }
    } else {
        None
    };

    if mode == PlanMode::PlanExecute && resumed_state.is_none() {
        // 计划级检查点(写点 A:每版计划物化后——账先于状态,此点之后任意
        // 时刻崩溃,v1 计划可从账面确定性重建)
        write_plan_loop_checkpoint(
            run_journal.as_ref(),
            version,
            replan_count,
            &executed_registry,
            &counters,
            cur_canonical_hash.as_deref(),
            goal.as_deref(),
            marks_session.as_deref(),
            &atomic_granules,
            &recut_counts,
            &cur_wf,
        )?;
    }

    loop {
        // 每版 = 全新 execute（丢弃式 D-02：v(n) 结果不注入 v(n+1)，results 表随栈帧析构）
        // 恢复首版 = execute_with_resume(粒级种子):已完成粒零重执行
        let tokens_before = token_arc.load(std::sync::atomic::Ordering::Relaxed);
        let started = std::time::Instant::now();
        let result = match first_version_node_replay.take() {
            Some(replay) => engine.execute_with_resume(&cur_wf, replay).await,
            None => engine.execute(&cur_wf).await,
        };
        counters.wall_ms += started.elapsed().as_millis() as u64;
        counters.nodes_executed += engine.executed_nodes();

        // tokens 埋点增量（交付物 7）：本版真实消耗 = 累加器前后差值；v2+ 版本
        // 差值累计进 replan_tokens（replan 重复执行成本上界，D-02 Phase 3 判定源）。
        let version_tokens = token_arc
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(tokens_before);
        counters.tokens_used += version_tokens;
        if version > 1 {
            replan_tokens += version_tokens;
        }

        // 已执行注册表 drain（R8-T03 比对源）：本版成功节点 (id, agent_type)
        // 跨版本累积；agent_type 按 id 反查（执行过必有定义，查不到兜底空串）。
        // M5-b：同批 drained 节点逐个向标记会话提交中性完成信号（submit 失败
        // fail-fast——留痕是硬义务，引擎不忘；信号幂等 set，replan 重执行安全）。
        for node_id in engine.take_executed_node_ids() {
            let agent_type = lookup_agent_type(&cur_wf.nodes, &node_id).unwrap_or_default();
            executed_registry.push((node_id.clone(), agent_type));
            if let Some(sid) = marks_session.as_deref() {
                submit_node_signal(ctx_ref.evorule_client.clone(), sid, &node_id)
                    .await
                    .map_err(|e| {
                        format!("workflow mark signal submit failed (node '{node_id}'): {e}")
                    })?;
            }
        }

        // 计划级检查点(写点 C:每轮计数器/注册表落定处——账先于状态,此点
        // 之后 replan 决策/planner 调用/物化全链崩溃均可恢复;预算阈值越限
        // 形态亦被本点覆盖)
        write_plan_loop_checkpoint(
            run_journal.as_ref(),
            version,
            replan_count,
            &executed_registry,
            &counters,
            cur_canonical_hash.as_deref(),
            goal.as_deref(),
            marks_session.as_deref(),
            &atomic_granules,
            &recut_counts,
            &cur_wf,
        )?;

        // D-01 enforce 判别（§9.5.1 选项 B）：宪法违规是系统性错误，一票否决
        // ——终止整个循环且不 replan（不开「换计划再试」通道）。
        if let Err(err_text) = &result {
            if is_enforce_violation(err_text) {
                return Err(format!(
                    "halted by enforce violation (no replan per §9.5.1-B; versions={} replans={}): {}",
                    version, replan_count, err_text
                ));
            }
        }

        // 阈值逐版重算（max_nodes = 当前版展开节点数 × 2）；计数器跨版累计——
        // 预算判定对累计值做，保守方向（早触发），Phase 1-B 实现新明确点
        let mut thresholds = BudgetThresholds::defaults(cur_wf.nodes.len());
        thresholds.max_replan = limits.max_replan;
        thresholds.max_wall_ms = limits.max_wall_ms;
        thresholds.max_tokens = limits.max_tokens;

        // 分类路由输入：失败节点 id 反解 → atomic 判定（结构标记 ∨ 记忆
        // 集合——原子性一经标记跨版本持续有效，replan 重产计划不带该字段）→
        // 该节点重切计数。route_input 是 should_replan 纯函数的第 5 参。
        let failed_node_id = result.as_ref().err().and_then(|e| parse_failed_node_id(e));
        let route_input = FailureRouteInput {
            atomic: failed_node_id
                .as_ref()
                .map(|id| lookup_node_atomic(&cur_wf.nodes, id) || atomic_granules.contains(id))
                .unwrap_or(false),
            recut_count: failed_node_id
                .as_ref()
                .map(|id| recut_counts.get(id).copied().unwrap_or(0))
                .unwrap_or(0),
            max_recuts: limits.max_recuts,
        };

        let Some(mut decision) = should_replan(
            &result,
            &counters,
            &thresholds,
            &ReplanState {
                current_version: version,
                replan_count,
            },
            &route_input,
        ) else {
            // 到此必为： outcome Ok（无触发）或 replan 硬上限已耗尽（含 Err 情形——
            // 交付物 6 §2 判定序第 1 步优先返回 None）。Err 必须显式传播不静默。
            let content = match result {
                Ok(c) => c,
                Err(e) => {
                    return Err(format!(
                        "workflow failed with replan budget exhausted (versions={} replans={}): {}",
                        version, replan_count, e
                    ))
                }
            };
            // 终态落账(ok):完成 run 从可恢复列表排除
            finish.finish_ok();
            return Ok(PlanLoopOutcome {
                content,
                stats: PlanLoopStats {
                    plan_versions: version,
                    replans: replan_count,
                    nodes_executed: counters.nodes_executed,
                    wall_ms: counters.wall_ms,
                    tokens_used: token_arc.load(std::sync::atomic::Ordering::Relaxed),
                    replan_tokens,
                    repeated_nodes,
                },
                run_session_id: run_session_id.or_else(|| engine.run_ledger_session_id()),
            });
        };

        // should_replan 第 1 步（硬上限）已挡超额 replan；此处只是防御性断言
        if replan_count >= limits.max_replan {
            return Err("internal: replan decided beyond hard cap".to_string());
        }

        // 失败记录回填计划形态 hash（锚定失败到具体计划版本，交付物 6 §3.1）
        let cur_hash = cur_canonical_hash.clone().or_else(|| seed_hash.clone());
        if let Some(rec) = decision.failure_record.as_mut() {
            rec.failed_plan_hash = cur_hash.clone();
            // agent_type 外层反查回填（交付物 6 §3.1；查不到保持 None 不猜测）
            if rec.agent_type.is_none() {
                if let Some(node_id) = &rec.failed_node_id {
                    rec.agent_type = lookup_agent_type(&cur_wf.nodes, node_id);
                }
            }
        }

        // 分类路由分派：Failure 决策按 FailureRoute 处置——Split 走既有
        // replan（原子粒附「同 id 保留 + 换维度重切」指令，重切计数递进）；
        // RepairContract 走 replan 但注入「不重拆 + 契约修复」指令（契约缺口
        // 描述已随 failure_record 入 trigger 回喂）；两类 Halt 显式终止不 replan
        // （漂移走治漂独立线不并入；能力缺口=切法空间内不可解，上报后终止）。
        let err_text = match &result {
            Err(e) => e.clone(),
            Ok(_) => String::new(), // Budget 触发：无失败文本（防御占位）
        };
        let mut route_guidance: Option<String> = None;
        match (&decision.reason, decision.route) {
            (ReplanReason::Budget, _) => {} // 预算触发无路由（既有行为零变更）
            (ReplanReason::Failure, Some(FailureRoute::ReplanSplit)) => {
                if let (true, Some(node_id)) = (route_input.atomic, failed_node_id.as_ref()) {
                    atomic_granules.insert(node_id.clone()); // 记忆集：跨版本持续有效
                    let k = recut_counts.entry(node_id.clone()).or_insert(0);
                    let hint = recut_dimension_hint(*k);
                    *k += 1;
                    let attempt = *k;
                    route_guidance = Some(format!(
                        "ROUTING: failed node '{node_id}' is an ATOMIC granule (recut \
                         {attempt}/{max}). Keep a node with the SAME id '{node_id}' covering \
                         the same work in the new plan, but re-cut it along a DIFFERENT \
                         dimension. Suggested dimension: {hint}.",
                        max = limits.max_recuts,
                    ));
                }
            }
            (ReplanReason::Failure, Some(FailureRoute::ReplanRepairContract)) => {
                if let Some(node_id) = &failed_node_id {
                    route_guidance = Some(format!(
                        "ROUTING: failed node '{node_id}' hit an interface failure — its \
                         OUTPUT violates the inter-granule contract. Do NOT split this node \
                         into smaller granules (the granule itself is correct); repair the \
                         node's output to satisfy the contract described in the failure \
                         record below."
                    ));
                }
            }
            (ReplanReason::Failure, Some(FailureRoute::DriftHalt)) => {
                return Err(format!(
                    "halted by drift-classified node failure (no resplit; drift handling is a \
                     separate track; versions={} replans={}): {}",
                    version, replan_count, err_text
                ));
            }
            (ReplanReason::Failure, Some(FailureRoute::CapabilityGapHalt)) => {
                let node_id = failed_node_id
                    .clone()
                    .unwrap_or_else(|| "(unknown)".to_string());
                // marks 信号 fail-fast（留痕是硬义务，与 node_done 信号同纪律）
                if let Some(sid) = marks_session.as_deref() {
                    submit_signal(
                        ctx_ref.evorule_client.clone(),
                        sid,
                        &capability_gap_signal(&node_id),
                    )
                    .await
                    .map_err(|e| {
                        format!("capability gap signal submit failed (node '{node_id}'): {e}")
                    })?;
                }
                // writeback fail-soft：env 旗标开 → 直连 rule 收件端点上报能力缺口
                // （不可达/非 2xx 仅 warn 不阻断——上报是观测面，终止语义不依赖送达）
                match crate::agent::writeback::config_from_env() {
                    Some(cfg) => {
                        let agent_type_str = lookup_agent_type(&cur_wf.nodes, &node_id);
                        let event = crate::agent::writeback::capability_gap_event(
                            &cfg,
                            &node_id,
                            agent_type_str.as_deref(),
                            version,
                            route_input.recut_count,
                            &err_text,
                        );
                        if let Err(e) = crate::agent::writeback::report_event(&cfg, &event).await {
                            tracing::warn!(
                                error = %e,
                                "capability gap writeback report failed (fail-soft)"
                            );
                        }
                    }
                    None => {
                        tracing::debug!("capability gap writeback disabled (env flag off)");
                    }
                }
                return Err(format!(
                    "halted by capability gap: atomic granule '{}' is unsolvable within the \
                     recut space (budget {} exhausted); no further replanning (versions={} \
                     replans={}): {}",
                    node_id, limits.max_recuts, version, replan_count, err_text
                ));
            }
            (ReplanReason::Failure, None) => {
                return Err("internal: failure decision without route".to_string());
            }
        }

        // ② replan：调 planner（delegate 既有路径，IoRequest sidecar 入链）产 v(n+1)
        let next = version + 1;
        let trigger_json = match decision.reason {
            ReplanReason::Failure => decision
                .failure_record
                .as_ref()
                .map(|rec| serde_json::to_string_pretty(rec).unwrap_or_default())
                .unwrap_or_default(),
            ReplanReason::Budget => decision
                .budget_snapshot
                .as_ref()
                .map(|snap| serde_json::to_string_pretty(snap).unwrap_or_default())
                .unwrap_or_default(),
        };
        let task = build_replan_task(
            goal.as_deref(),
            &build_plan_summary(&cur_wf, version),
            &trigger_json,
            next,
            route_guidance.as_deref(),
        );
        // replan planner 调用（delegate 既有路径，IoRequest sidecar 入链）产 v(n+1)；
        // 调用自身 token 消耗也计入 replan_tokens（否则会夹在两版差值采样之间丢失）
        let planner_before = token_arc.load(std::sync::atomic::Ordering::Relaxed);
        let mut plan = call_planner_with_retry(
            |t: String| async move { ctx_ref.delegate("planner", &t).await },
            task,
            None,
        )
        .await
        .map_err(|e| format!("replan planner call failed: {}", e))?;
        replan_tokens += token_arc
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(planner_before);
        let source = match decision.reason {
            ReplanReason::Failure => "replan_after_failure",
            ReplanReason::Budget => "replan_after_budget",
        };
        let parent =
            cur_hash.ok_or_else(|| "internal: replan without plan hash anchor".to_string())?;
        inject_plan_meta(&mut plan, source, next, Some(&parent));
        let canonical = serde_json::to_string(&plan).map_err(|e| e.to_string())?;
        cur_wf = materialize_plan_fact(&plan, &format!("{}_plan_v{}", initial_id, next)).map_err(
            |errs| format!("plan v{} materialization failed: {}", next, errs.join("; ")),
        )?;
        // R8-T03 静态拦截（物化后、提交前）：
        // ① 写类工具节点（file_write/shell_exec）→ 拒绝提交。schema J3 幂等读
        //    白名单在物化时已拒一轮；此处是 replan 产物提交前的防御深度二道闸。
        let writes = find_non_idempotent_writes(&plan);
        if !writes.is_empty() {
            return Err(format!(
                "plan v{} rejected: non-idempotent write-tool nodes present (R8-T03 defense-in-depth): {}",
                next,
                writes.join(", ")
            ));
        }
        // ② 幂等重复（同 id 同 agent_type 已执行）→ warn + 埋点放行：丢弃式
        //    D-02 接受重复执行成本（纲领 §9.4.2），验收标准只要求写类拒绝。
        let repeats = detect_repeated_nodes(&cur_wf.nodes, &executed_registry);
        if !repeats.is_empty() {
            repeated_nodes += repeats.len() as u64;
            tracing::warn!(
                plan_version = next,
                repeated = ?repeats,
                "plan repeats already-executed nodes (idempotent repetition accepted under D-02)"
            );
        }

        cur_canonical_hash = Some(plan_canonical_hash(&canonical));
        version = next;
        replan_count += 1;
        tracing::info!(
            plan_version = next,
            "plan-execute: replan materialized, re-executing"
        );
        // 计划级检查点(写点 B:每次 replan 物化后——此后到下轮写点 C 之间
        // 崩溃,恢复取本点,新版本从头执行,已完成粒由粒级检查点护栏)
        write_plan_loop_checkpoint(
            run_journal.as_ref(),
            version,
            replan_count,
            &executed_registry,
            &counters,
            cur_canonical_hash.as_deref(),
            goal.as_deref(),
            marks_session.as_deref(),
            &atomic_granules,
            &recut_counts,
            &cur_wf,
        )?;
    }
}

/// serve 面 plan-execute 装配入口（serve 挂 driver）：单 planner 节点 probe DAG
/// → constitution 校验链（fail-fast 拒载）→ DelegateContext（toolkit/治理段由
/// 调用方注入）→ marks_session 创建（fail-fast——留痕是硬义务）→ run_plan_loop。
///
/// 与 CLI workflow 装配段（cmd_workflow）同构，差异仅两点：
/// - toolkit 由调用方传入（serve 面 `build_filtered_toolkit_with_switches`
///   产物，E1 安全隔离语义优先），不在本函数重建 CLI union toolkit；
/// - 治理门禁段随上下文下放（serve 构造点与主路径同源——治理纪律无豁免面；
///   CLI 面传 None 与 CLI 主路径同口径）。
///
/// 返回 `(PlanLoopOutcome, marks_session_id)`——session id 供响应面会话关联
/// 收口（消费者可凭此查询权威面或工作台回放）。
///
/// `max_depth` / `max_concurrent` 与 CLI workflow 子命令缺省一致（3 / 5），
/// 由调用方传入；驱动限额 `limits` 语义见 [`DriverLimits`]（阈值禁入 PlanFact）。
///
/// `resume_session`:恢复此前中断的 plan-execute run(值为 run 级账本会话名,
/// 由 `PlanLoopOutcome.run_session_id` 透出或扫尾端点列出)。恢复仅经显式
/// 入参,不自动续跑(人类持剑);恢复路径:计划状态/治理标记身份取自账面,
/// probe 不重跑(非确定 LLM 输出,重跑即漂移),恢复路径入口发射续跑标记
/// (账面序列=崩溃标记→续跑标记),剩余预算闸在驱动入口判定。
#[allow(clippy::too_many_arguments)]
pub async fn run_plan_execute(
    evorule_client: EvoruleApiClient,
    definitions: crate::agent::AgentDefinitionManager,
    toolkit: ToolHandler,
    workdir: &std::path::Path,
    governance_segment: Option<String>,
    goal: &str,
    limits: DriverLimits,
    max_depth: usize,
    max_concurrent: usize,
    container: Option<String>,
    resume_session: Option<&str>,
    step_timeout_override: Option<std::time::Duration>,
) -> Result<(PlanLoopOutcome, String), String> {
    let sessions_dir = workdir.join("data").join("sessions");
    // 恢复装配:回放最新计划检查点+其后检查点尾段;锚不齐/blob 缺失/hash
    // 不过一律拒绝(fail-closed)。发射续跑标记(先开-发射-落:驱动随后自开
    // 同会话续写,写者注册表在落时释放;open 内崩溃检测先补写崩溃标记,
    // 账面序列=crashed→resumed)
    let plan_resume = match resume_session {
        Some(sid) => {
            let r = build_plan_loop_resume(&sessions_dir, sid)?;
            let writer = JournalWriter::open(&sessions_dir, sid)
                .map_err(|e| format!("run journal reopen failed ('{sid}'): {e}"))?;
            let replay_seq = writer
                .read_lines()
                .ok()
                .and_then(|ls| ls.last().map(|l| l.seq))
                .unwrap_or(0);
            writer
                .session_resumed(
                    replay_seq,
                    vec![
                        format!("plan_state:v{}", r.state.version),
                        format!("registry:{}", r.state.executed_registry.len()),
                        format!("nodes:{}", r.node_replay.results.len()),
                    ],
                )
                .map_err(|e| format!("session resumed marker write failed: {e}"))?;
            drop(writer);
            Some(r)
        }
        None => None,
    };

    let (wf, marks_session, seed_hash) = match &plan_resume {
        Some(r) => {
            // 恢复路径:计划与治理身份取自账面;标记会话缺席=治理身份不可
            // 恢复,显式失败(全新标记会话会让阶段前置门误拦)
            let marks = r.state.marks_session.clone().ok_or_else(|| {
                "resumed run checkpoint lacks marks session - governance identity unrestorable"
                    .to_string()
            })?;
            let wf = serde_json::from_str::<Workflow>(&r.state.cur_workflow)
                .map_err(|e| format!("checkpoint workflow deserialize failed: {e}"))?;
            (wf, marks, None)
        }
        None => {
            // ① probe DAG（单 planner 节点，task=goal）→ constitution 校验链（与 CLI
            //    同一加载入口：schema 全量校验 + 物化，失败 fail-fast 拒载）
            let probe = probe_workflow_value(goal);
            let wf = crate::agent::constitution::load_workflow(&probe).map_err(|violations| {
                format!(
                    "plan-execute probe workflow failed constitution validation/materialization: {}",
                    violations.join("; ")
                )
            })?;
            // ③ marks_session 创建（fail-fast，与 CLI 同语义；workflow_run kind 锚定
            //    本链路——节点完成信号与协作标记都落此会话）
            let marks = evorule_client
                .create_session(
                    Some(&json!({
                        "kind": "workflow_run",
                        "workflow_id": "serve_plan_execute",
                    })),
                    Some("llm"),
                )
                .await
                .map_err(|e| format!("failed to create workflow marks session: {}", e))?;
            // ④ seed_hash 锚 = probe DAG canonical JSON 的 BLAKE3（与 CLI「workflow 文件
            //    原文 hash」同口径；serde_json BTreeMap 键序保证 canonical 确定性）
            let probe_canonical = serde_json::to_string(&probe).map_err(|e| e.to_string())?;
            (wf, marks, Some(plan_canonical_hash(&probe_canonical)))
        }
    };

    // ② DelegateContext 构造（对齐 CLI：toolkit+workdir 成对注入——delegate 按
    //    各子代理 def.tools 白名单过滤挂载；journal 同目录落账）
    let mut ctx = DelegateContext::new("workflow_root", definitions, evorule_client.clone())
        .with_toolkit(toolkit, workdir)
        .with_max_depth(max_depth)
        .with_journal_dir(sessions_dir)
        .with_governance_segment(governance_segment)
        .with_step_timeout_override(step_timeout_override);
    if max_concurrent > 0 {
        ctx = ctx.with_max_concurrent_delegates(max_concurrent);
    }

    let outcome = run_plan_loop_with_resume(
        ctx,
        wf,
        PlanMode::PlanExecute,
        limits,
        seed_hash,
        Some(marks_session.clone()),
        container, // 判据 v0：run 请求容器名透传（None = 宿主直执行）
        plan_resume,
    )
    .await?;
    Ok((outcome, marks_session))
}

/// serve 面 plan-execute probe DAG（单 planner 节点，task=goal）——构造与
/// 校验分离（纯函数可测）；形态与 `rules/workflows/research_plan.json` 一致
fn probe_workflow_value(goal: &str) -> Value {
    json!({
        "workflow_id": "serve_plan_execute",
        "description": "serve plan-execute planning probe: single planner node, output PlanFact v1",
        "nodes": [
            {
                "id": "planner",
                "agent_type": "planner",
                "task": goal,
                "depends_on": []
            }
        ],
        "output_node": "planner"
    })
}

/// M5-b：协作节点完成信号指令形态（纯函数）
///
/// 中性事件：`set meta_signal.node_done = <node_id>`。驱动只报告「某节点完成了」，
/// 不含任何标记知识——节点→任务标记的映射由规则面 branch 壳+set 业务规则裁决。
pub fn node_done_signal(node_id: &str) -> Value {
    json!({
        "type": "set",
        "params": {
            "attr": "meta_signal.node_done",
            "operation": "set",
            "value": node_id
        }
    })
}

/// 分类路由：原子粒重切维度提示（四维度轮换纯函数）
///
/// 重切预算内逐次换维度，避免 planner 原地踏步（同维度重切 = 切法空间内重复
/// 采样）；index 对 4 取模轮换，切法空间描述与「同 id 保留」指令一并注入
/// replan 任务。
const RECUT_DIMENSIONS: [&str; 4] = [
    "by sequential steps of the work",
    "by distinct objects or entities the work touches",
    "by abstraction layers (goal, sub-tasks, concrete actions)",
    "by verification surface (what can be independently checked)",
];

/// 原子粒重切维度提示（纯函数）：按重切序号轮换返回建议切法维度文案
/// （四维度轮换取模；序号从 0 起——首次重切建议第一维度）
pub fn recut_dimension_hint(recut_index: u32) -> &'static str {
    RECUT_DIMENSIONS[(recut_index % RECUT_DIMENSIONS.len() as u32) as usize]
}

/// 分类路由：能力缺口信号指令形态（纯函数；中性事件同 node_done 纪律——
/// 驱动只报告「某原子粒在切法空间内不可解」，处置知识在规则层/能力线）
pub fn capability_gap_signal(node_id: &str) -> Value {
    json!({
        "type": "set",
        "params": {
            "attr": "meta_signal.capability_gap",
            "operation": "set",
            "value": node_id
        }
    })
}

/// 向标记会话提交信号指令（通用形态；node_done / capability_gap 信号共用
/// submit_command 既有通道，成功即落链为 StateTransition 事实）
async fn submit_signal(
    client: crate::api::evorule_client::EvoruleApiClient,
    session_id: &str,
    instruction: &Value,
) -> Result<(), String> {
    client
        .submit_command(session_id, instruction)
        .await
        .map_err(|e| e.to_string())
}

/// M5-b：向标记会话提交节点完成信号（[`submit_signal`] 的节点完成信号 wrapper）
async fn submit_node_signal(
    client: crate::api::evorule_client::EvoruleApiClient,
    session_id: &str,
    node_id: &str,
) -> Result<(), String> {
    submit_signal(client, session_id, &node_done_signal(node_id)).await
}

/// D-01 enforce 违规判别（纯函数；§9.5.1 选项 B）
///
/// runner Violation 分支上抛的失败文本带固定前缀 `enforce violation:`
/// （workflow 层包装为 `workflow node '<id>' failed: <error>`，故按子串判别）。
pub fn is_enforce_violation(err_text: &str) -> bool {
    err_text.contains("enforce violation:")
}

/// planner 调用带重试面（R1-T03/T04：提取失败带错误反馈重试 1 次，硬上限 2 次调用）
///
/// `first_output`：首次调用的既有输出——probe 站点已跑过 planning probe DAG
/// 传 `Some`（不再重复调 planner）；replan 站点尚无输出传 `None`（函数内先补
/// 第 1 次调用）。提取失败 → 原任务附提取错误反馈重试 1 次 → 仍失败整体报错
/// （调用方终止；R1-T04 重试硬上限 = 全程最多 2 次 planner 调用）。
pub async fn call_planner_with_retry<F, Fut>(
    call: F,
    task: String,
    first_output: Option<String>,
) -> Result<Value, String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let first_err = match first_output {
        Some(text) => match extract_plan_json(&text) {
            Ok(plan) => return Ok(plan),
            Err(e) => e,
        },
        None => {
            let text = call(task.clone())
                .await
                .map_err(|e| format!("planner call failed: {}", e))?;
            match extract_plan_json(&text) {
                Ok(plan) => return Ok(plan),
                Err(e) => e,
            }
        }
    };
    tracing::warn!(
        error = %first_err,
        "planner output unparsable; retrying once with error feedback (R1-T04 hard cap: 2 calls)"
    );
    let retry_task = format!(
        "{task}\n\nIMPORTANT: your previous response was not a valid PlanFact JSON object \
         (extraction error: {err}). Output ONLY a single valid PlanFact JSON object — \
         no markdown fences, no prose before or after.",
        err = first_err,
    );
    let retry_text = call(retry_task)
        .await
        .map_err(|e| format!("planner retry call failed: {}", e))?;
    extract_plan_json(&retry_text).map_err(|second_err| {
        format!(
            "planner output unparsable after retry (R1-T04 hard cap reached): first: {}; second: {}",
            first_err, second_err
        )
    })
}

/// R8-T03 静态拦截②比对：检测物化后 DAG 中与已执行注册表重复的节点（纯函数）
///
/// 重复判据 = (id, agent_type) 双字段相同（同 id 不同 agent_type 视为计划
/// 结构性变更，不算重复）。命中 → warn + `repeated_nodes` 埋点后放行执行
/// （丢弃式 D-02 接受幂等重复成本，纲领 §9.4.2）。
pub fn detect_repeated_nodes(
    nodes: &[crate::agent::workflow::WorkflowNode],
    registry: &[(String, String)],
) -> Vec<String> {
    nodes
        .iter()
        .filter(|n| {
            registry
                .iter()
                .any(|(id, agent_type)| id == &n.id && agent_type == &n.agent_type)
        })
        .map(|n| n.id.clone())
        .collect()
}

/// R8-T03 静态拦截①闸门：递归扫描 PlanFact 中声明写类工具的 tool 节点（纯函数）
///
/// 写类 = `file_write` / `shell_exec`（schema J3 幂等读白名单之外的危险面；
/// J2 封锁 type 白名单）。schema 物化时已拒一轮，本函数是 replan 产物提交前
/// 的防御深度二道闸——递归覆盖顶层 nodes 与 loops[].body 嵌套。命中即拒绝。
pub fn find_non_idempotent_writes(plan: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            Value::Object(map) => {
                if map.get("type").and_then(|t| t.as_str()) == Some("tool") {
                    if let Some(tool) = map.get("tool").and_then(|t| t.as_str()) {
                        if matches!(tool, "file_write" | "shell_exec") {
                            let id = map.get("id").and_then(|i| i.as_str()).unwrap_or("(no id)");
                            out.push(format!("{id} ({tool})"));
                        }
                    }
                }
                map.values().for_each(|c| walk(c, out));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(plan, &mut out);
    out
}

/// 从 planner 输出文本提取 PlanFact JSON（纯函数）
///
/// 容忍三类常见形态：裸 JSON / markdown 代码栅栏包裹 / 前后带说明文字——
/// 依次尝试直接解析、首个 `{` 到末个 `}` 切片解析。非对象即拒（fail-fast，
/// 不猜测；planner 重试逻辑归 Phase 2 R1-T03）。
pub fn extract_plan_json(text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return v
            .is_object()
            .then_some(v)
            .ok_or_else(|| "planner output JSON is not an object".to_string());
    }
    let start = trimmed
        .find('{')
        .ok_or_else(|| "no JSON object found in planner output".to_string())?;
    let end = trimmed
        .rfind('}')
        .ok_or_else(|| "no JSON object found in planner output".to_string())?;
    if end <= start {
        return Err("no JSON object found in planner output".to_string());
    }
    let slice = &trimmed[start..=end];
    let v: Value = serde_json::from_str(slice)
        .map_err(|e| format!("planner output is not valid JSON: {}", e))?;
    v.is_object()
        .then_some(v)
        .ok_or_else(|| "planner output JSON is not an object".to_string())
}

/// 注入 PlanFact 元数据组（外层驱动权威注入，交付物 2 J5；LLM 不产出注入组）
///
/// `parent_hash=None` 时写 JSON null（`initial_planning` 形态）。
pub fn inject_plan_meta(plan: &mut Value, source: &str, version: u32, parent_hash: Option<&str>) {
    if let Some(obj) = plan.as_object_mut() {
        obj.insert("plan_source".to_string(), json!(source));
        obj.insert("plan_version".to_string(), json!(version));
        obj.insert(
            "parent_plan_hash".to_string(),
            parent_hash.map(|h| json!(h)).unwrap_or(Value::Null),
        );
    }
}

/// PlanFact canonical JSON 的 BLAKE3 hex（64 位；parent_plan_hash/failed_plan_hash 锚）
pub fn plan_canonical_hash(canonical: &str) -> String {
    blake3::hash(canonical.as_bytes()).to_hex().to_string()
}

/// v(n) 计划摘要（纲领 §9.4.3 `v1_plan_summary`；从展开后 DAG 生成——
/// loop 副本全名天然携带循环结构，无需单独字段）
pub fn build_plan_summary(wf: &Workflow, version: u32) -> Value {
    json!({
        "plan_version": version,
        "nodes": wf
            .nodes
            .iter()
            .map(|n| {
                json!({
                    "id": n.id,
                    "agent_type": n.agent_type,
                    "depends_on": n.depends_on,
                })
            })
            .collect::<Vec<_>>(),
        "output_node": wf.output_node,
    })
}

/// 构造 replan 任务文本（纲领 §9.4.3 摘要格式 + 丢弃式指令；`completed_nodes`
/// 与 `executed_side_effects` 丢弃式置空，不进本任务文本）
///
/// `route_guidance`（分类路由）：Failure 路由的处置指令段（原子粒换维度
/// 重切 / 接口失败契约修复）；`None`（Budget 触发或非原子粒）时不加段——
/// 既有任务文本逐字节零变化。
pub fn build_replan_task(
    goal: Option<&str>,
    summary: &Value,
    trigger_json: &str,
    next_version: u32,
    route_guidance: Option<&str>,
) -> String {
    let guidance_section = route_guidance
        .map(|g| format!("Routing guidance:\n{g}\n\n"))
        .unwrap_or_default();
    format!(
        "REPLAN REQUEST — produce plan v{next} as a single PlanFact JSON object.\n\n\
         Original goal:\n{goal}\n\n\
         Previous plan summary (v{prev}):\n{summary}\n\n\
         Trigger (why replanning):\n{trigger}\n\n\
         {guidance_section}\
         Instructions:\n\
         - Discard semantics: the previous run is abandoned; produce a COMPLETE new plan from scratch.\n\
         - Avoid the failure cause shown in the trigger; you may drop or restructure nodes.\n\
         - Follow the PlanFact schema and rules from your system prompt (node id grammar, agent_type whitelist, single sink, limits).\n\
         - Output ONLY the PlanFact JSON object. No markdown fences, no explanations.\n",
        next = next_version,
        prev = next_version - 1,
        goal = goal.unwrap_or(
            "(no explicit goal; infer it from the previous plan summary and the trigger)",
        ),
        summary = serde_json::to_string_pretty(summary).unwrap_or_default(),
        trigger = if trigger_json.is_empty() {
            "(unspecified)"
        } else {
            trigger_json
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::workflow::WorkflowNode;

    // ----- extract_plan_json（R1 非法输出处置的 MVP 形态：fail-fast 不猜测）-----

    #[test]
    fn extract_bare_json_object() {
        let v = extract_plan_json(r#"{"nodes":[],"edges":[]}"#).expect("bare object parses");
        assert!(v.is_object());
    }

    #[test]
    fn extract_json_in_markdown_fence_with_prose() {
        let text = "Here is the plan:\n```json\n{\"nodes\":[],\"edges\":[]}\n```\nHope it helps.";
        let v = extract_plan_json(text).expect("fenced object extracts");
        assert!(v.get("nodes").is_some());
    }

    #[test]
    fn extract_rejects_non_object_and_garbage() {
        assert!(extract_plan_json("[1,2,3]").is_err());
        assert!(extract_plan_json("no json at all").is_err());
        assert!(extract_plan_json("{ broken").is_err());
        assert!(extract_plan_json("").is_err());
    }

    #[test]
    fn extract_prose_wrapped_single_object() {
        // 切片策略服务「散文包裹单对象」形态（planner 被指示只输出一个 JSON 对象）
        let text = "Sure! Here it is: {\"nodes\":[],\"edges\":[]} — done.";
        let v = extract_plan_json(text).expect("prose-wrapped object extracts");
        assert!(v.get("nodes").is_some());
    }

    // ----- inject_plan_meta（交付物 2 J5 注入组权威注入）-----

    #[test]
    fn inject_initial_planning_writes_null_parent() {
        let mut plan = json!({"nodes": []});
        inject_plan_meta(&mut plan, "initial_planning", 1, None);
        assert_eq!(plan["plan_source"], "initial_planning");
        assert_eq!(plan["plan_version"], 1);
        assert!(plan["parent_plan_hash"].is_null());
    }

    #[test]
    fn inject_replan_writes_parent_hash() {
        let mut plan = json!({"nodes": []});
        let parent = plan_canonical_hash(r#"{"nodes":[]}"#);
        assert_eq!(parent.len(), 64);
        assert!(parent.chars().all(|c| c.is_ascii_hexdigit()));
        inject_plan_meta(&mut plan, "replan_after_failure", 2, Some(&parent));
        assert_eq!(plan["plan_source"], "replan_after_failure");
        assert_eq!(plan["plan_version"], 2);
        assert_eq!(plan["parent_plan_hash"], parent.as_str());
    }

    // ----- serve 挂 driver：probe DAG 构造 + constitution 校验链 -----

    #[test]
    fn probe_workflow_passes_constitution_and_shape() {
        let goal = "research the topic 'X' and produce a digest";
        let probe = probe_workflow_value(goal);
        let wf = crate::agent::constitution::load_workflow(&probe)
            .expect("probe DAG passes constitution validation/materialization");
        assert_eq!(wf.workflow_id, "serve_plan_execute");
        assert_eq!(wf.nodes.len(), 1);
        assert_eq!(wf.nodes[0].id, "planner");
        assert_eq!(wf.nodes[0].agent_type, "planner");
        assert_eq!(wf.nodes[0].task, goal);
        assert!(wf.nodes[0].depends_on.is_empty());
        assert_eq!(wf.output_node, "planner");
        // canonical 确定性：同输入必同输出（键序由 BTreeMap 保证，seed_hash 锚稳定）
        assert_eq!(probe, probe_workflow_value(goal));
    }

    // ----- build_plan_summary / build_replan_task（纲领 §9.4.3 摘要格式）-----

    #[test]
    fn plan_summary_lists_nodes_deterministically() {
        let wf = Workflow {
            workflow_id: "wf".to_string(),
            description: String::new(),
            nodes: vec![WorkflowNode {
                id: "a".to_string(),
                agent_type: "researcher".to_string(),
                task: "t".to_string(),
                task_template: None,
                depends_on: Vec::new(),
                run_when: None,
                compute: None,
                output_schema: None,
                judge: None,
                atomic: false,
            }],
            output_node: "a".to_string(),
        };
        let summary = build_plan_summary(&wf, 1);
        assert_eq!(summary["plan_version"], 1);
        assert_eq!(summary["nodes"][0]["id"], "a");
        assert_eq!(summary["nodes"][0]["agent_type"], "researcher");
        assert_eq!(summary["output_node"], "a");
        // 同输入同输出（纯函数确定性）
        assert_eq!(summary, build_plan_summary(&wf, 1));
    }

    #[test]
    fn replan_task_contains_summary_trigger_and_version() {
        let summary = json!({"plan_version": 1, "nodes": []});
        let task = build_replan_task(
            Some("研究 X"),
            &summary,
            "{\"trigger\":\"node_failure\"}",
            2,
            None,
        );
        assert!(task.contains("plan v2"));
        assert!(task.contains("研究 X"));
        assert!(task.contains("node_failure"));
        assert!(task.contains("COMPLETE new plan"));
        // 无目标时的引导语
        let task2 = build_replan_task(None, &summary, "{}", 3, None);
        assert!(task2.contains("infer it from the previous plan summary"));
    }

    // ----- 分类路由：重切维度轮换 + 能力缺口信号 + replan 任务路由指令段 -----

    #[test]
    fn recut_dimension_hint_rotates_deterministically() {
        // 四维度轮换：连续 index 各不相同，回绕后与首轮一致（确定性纯函数）
        let h = |i| recut_dimension_hint(i);
        assert_ne!(h(0), h(1));
        assert_ne!(h(1), h(2));
        assert_ne!(h(2), h(3));
        assert_ne!(h(3), h(0));
        assert_eq!(h(4), h(0));
        assert_eq!(h(9), h(1));
        assert_eq!(h(2), h(2));
    }

    #[test]
    fn capability_gap_signal_shape_is_neutral_set() {
        // 中性信号形态：set meta_signal.capability_gap=<node_id>；处置知识不在驱动
        let sig = capability_gap_signal("granule_x");
        assert_eq!(sig["type"], "set");
        assert_eq!(sig["params"]["attr"], "meta_signal.capability_gap");
        assert_eq!(sig["params"]["operation"], "set");
        assert_eq!(sig["params"]["value"], "granule_x");
        assert_eq!(sig, capability_gap_signal("granule_x"));
    }

    #[test]
    fn replan_task_route_guidance_inserted_only_when_present() {
        let summary = json!({"plan_version": 1, "nodes": []});
        // None：无路由指令段（Budget 触发形态，既有文本零变化）
        let base = build_replan_task(Some("g"), &summary, "{}", 2, None);
        assert!(!base.contains("Routing guidance"));
        // Some：指令段在 Trigger 之后、Instructions 之前（顺序稳定）
        let guided = build_replan_task(
            Some("g"),
            &summary,
            "{}",
            2,
            Some("ROUTING: keep node 'a' with the same id."),
        );
        assert!(guided.contains("Routing guidance"));
        assert!(guided.contains("ROUTING: keep node 'a' with the same id."));
        let trigger_pos = guided
            .find("Trigger (why replanning)")
            .expect("trigger present");
        let guidance_pos = guided.find("Routing guidance").expect("guidance present");
        let instr_pos = guided.find("Instructions:").expect("instructions present");
        assert!(trigger_pos < guidance_pos && guidance_pos < instr_pos);
    }

    // ----- M5-b：协作工作流完成信号（node_done_signal / submit_node_signal）-----

    #[test]
    fn node_done_signal_shape_is_neutral_set() {
        // 中性信号形态：set meta_signal.node_done=<node_id>；驱动不含标记知识
        let sig = node_done_signal("due_diligence");
        assert_eq!(sig["type"], "set");
        assert_eq!(sig["params"]["attr"], "meta_signal.node_done");
        assert_eq!(sig["params"]["operation"], "set");
        assert_eq!(sig["params"]["value"], "due_diligence");
        // 同输入必同输出（确定性）
        assert_eq!(sig, node_done_signal("due_diligence"));
    }

    #[tokio::test]
    async fn submit_node_signal_posts_signal_command() {
        let mut server = mockito::Server::new_async().await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let m = server
            .mock("POST", "/api/sessions/42/command")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "instruction": node_done_signal("closure")
            })))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        submit_node_signal(client, "42", "closure")
            .await
            .expect("signal submit must succeed");
        m.assert_async().await;
    }

    #[tokio::test]
    async fn submit_node_signal_fails_fast_on_transport_error() {
        // 不可达端口 → Err（留痕是硬义务，fail-fast 由调用方上抛终止工作流）
        let client = crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1");
        assert!(
            submit_node_signal(client, "42", "closure").await.is_err(),
            "unreachable server must yield transport error"
        );
    }

    // ----- D-01 enforce 判别（§9.5.1-B：终止不 replan）-----

    #[test]
    fn enforce_violation_prefix_detected() {
        assert!(is_enforce_violation(
            "workflow node 'n1' failed: enforce violation: rule_index=Some(11), reason=Model not allowed"
        ));
        assert!(is_enforce_violation("enforce violation: direct"));
        assert!(!is_enforce_violation(
            "workflow node 'n1' failed: connection timeout"
        ));
        assert!(!is_enforce_violation(""));
    }

    // ----- planner 重试面（R1-T03/T04：重试 1 次，硬上限 2 次调用）-----

    #[tokio::test]
    async fn planner_retry_first_output_parses_without_recall() {
        // 首次输出已合法 → 不再调用 planner（closure 计数 = 0）
        let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let c2 = calls.clone();
        let call = move |_t: String| {
            c2.set(c2.get() + 1);
            async move { Ok::<String, String>(r#"{"nodes":[]}"#.to_string()) }
        };
        let plan = call_planner_with_retry(
            call,
            "task".to_string(),
            Some(r#"{"nodes":[{"id":"a"}]}"#.to_string()),
        )
        .await
        .expect("first output parses");
        assert!(plan.get("nodes").is_some());
        assert_eq!(calls.get(), 0);
    }

    #[tokio::test]
    async fn planner_retry_recovers_on_second_call() {
        // None 起步（replan 站点形态）：首次非法 → 带反馈重试 1 次 → 合法
        let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let c2 = calls.clone();
        let call = move |_t: String| {
            let n = c2.get() + 1;
            c2.set(n);
            async move {
                if n == 1 {
                    Ok::<String, String>("sorry, here is my plan...".to_string())
                } else {
                    Ok(r#"{"nodes":[]}"#.to_string())
                }
            }
        };
        let plan = call_planner_with_retry(call, "task".to_string(), None)
            .await
            .expect("retry recovers");
        assert!(plan.get("nodes").is_some());
        assert_eq!(calls.get(), 2); // 首次 + 重试 = 硬上限内
    }

    #[tokio::test]
    async fn planner_retry_hard_cap_terminates() {
        // 两次输出均非法 → 硬上限报错（R1-T04）
        let call = |_t: String| async { Ok::<String, String>("no json here".to_string()) };
        let err =
            call_planner_with_retry(call, "task".to_string(), Some("still no json".to_string()))
                .await
                .expect_err("hard cap must terminate");
        assert!(err.contains("hard cap"), "err: {err}");
    }

    // ----- R8-T03 静态拦截（detect_repeated_nodes / find_non_idempotent_writes）-----

    fn node(id: &str, agent_type: &str) -> WorkflowNode {
        WorkflowNode {
            id: id.to_string(),
            agent_type: agent_type.to_string(),
            task: "t".to_string(),
            task_template: None,
            depends_on: Vec::new(),
            run_when: None,
            compute: None,
            output_schema: None,
            judge: None,
            atomic: false,
        }
    }

    #[test]
    fn repeated_nodes_matched_on_id_and_agent_type() {
        let registry = vec![
            ("a".to_string(), "researcher".to_string()),
            ("b".to_string(), "analyst".to_string()),
        ];
        let nodes = vec![
            node("a", "researcher"), // 双匹配 → 命中
            node("a", "analyst"),    // 同 id 不同类型 → 结构性变更，不命中
            node("c", "researcher"), // 新节点 → 不命中
        ];
        assert_eq!(
            detect_repeated_nodes(&nodes, &registry),
            vec!["a".to_string()]
        );
        assert!(detect_repeated_nodes(&nodes, &[]).is_empty());
    }

    #[test]
    fn non_idempotent_writes_scanned_recursively() {
        // 顶层 + loops body 嵌套均覆盖；写类命中、幂等读与 LLM 节点不误报
        let plan = json!({
            "nodes": [
                { "id": "read1", "type": "tool", "tool": "search_files",
                  "agent_type": "fetcher", "task": "t" },
                { "id": "write1", "type": "tool", "tool": "file_write",
                  "agent_type": "writer", "task": "t" }
            ],
            "loops": [
                { "id": "lp", "body": { "nodes": [
                    { "id": "sh1", "type": "tool", "tool": "shell_exec",
                      "agent_type": "runner", "task": "t" }
                ] } }
            ]
        });
        let hits = find_non_idempotent_writes(&plan);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.contains("write1")));
        assert!(hits.iter().any(|h| h.contains("sh1")));
        let clean = json!({ "nodes": [
            { "id": "r", "type": "tool", "tool": "http_get",
              "agent_type": "f", "task": "t" },
            { "id": "l", "type": "llm", "agent_type": "w", "task": "t" }
        ]});
        assert!(find_non_idempotent_writes(&clean).is_empty());
    }

    // ----- 计划循环检查点与恢复(驱动级) -----

    use crate::agent::delegate::DelegateContext;
    use crate::agent::journal::{
        read_all, BudgetCountersSnapshot, JournalEvent, JournalLine, JournalWriter,
    };
    use crate::agent::workflow::{ComputeInput, ComputeSpec};
    use crate::api::evorule_client::EvoruleApiClient;

    fn resume_test_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("drv-r4-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn resume_ctx(dir: &std::path::Path) -> DelegateContext {
        DelegateContext::new(
            "parent",
            crate::agent::definition::AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new("http://127.0.0.1:1"),
        )
        .with_journal_dir(dir.to_path_buf())
    }

    fn compute_node(id: &str, deps: &[&str], spec: ComputeSpec) -> WorkflowNode {
        WorkflowNode {
            id: id.to_string(),
            agent_type: String::new(),
            task: String::new(),
            task_template: None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            run_when: None,
            compute: Some(spec),
            output_schema: None,
            judge: None,
            atomic: false,
        }
    }

    fn resume_chain_wf() -> Workflow {
        // 三粒全 compute 链:n1=""(空拼接) → n2=Length(n1)="0" →
        // n3=Replace(n2,"0"→"zero")="zero";无 LLM,驱动循环全程离线可跑
        Workflow {
            workflow_id: "resume_wf".to_string(),
            description: String::new(),
            nodes: vec![
                compute_node(
                    "n1",
                    &[],
                    ComputeSpec::Concat {
                        inputs: vec![ComputeInput::Empty, ComputeInput::Empty],
                    },
                ),
                compute_node(
                    "n2",
                    &["n1"],
                    ComputeSpec::Length {
                        inputs: vec![ComputeInput::Node("n1".to_string())],
                    },
                ),
                compute_node(
                    "n3",
                    &["n2"],
                    ComputeSpec::Replace {
                        inputs: vec![ComputeInput::Node("n2".to_string())],
                        find: "0".to_string(),
                        replacement: "zero".to_string(),
                    },
                ),
            ],
            output_node: "n3".to_string(),
        }
    }

    #[test]
    fn plan_checkpoint_replay_latest_state_not_reset() {
        // 计划级回放:最新 PlanLoopCheckpointed 胜出;计数器/注册表/版本取账面
        // (不重置);尾段起始行 = 最新事件之后(粒级切片位)
        let lines = vec![
            JournalLine {
                seq: 1,
                ts: 1,
                event: JournalEvent::NodeCheckpointed {
                    workflow_id: "wf".into(),
                    plan_hash: "ph".into(),
                    node_id: "n0".into(),
                    status: "completed".into(),
                    result_ref: crate::agent::journal::CheckpointResultRef {
                        hash: "blake3:x".into(),
                        len: 0,
                        inline: None,
                    },
                },
            },
            JournalLine {
                seq: 2,
                ts: 2,
                event: JournalEvent::PlanLoopCheckpointed {
                    version: 1,
                    replan_count: 0,
                    executed_registry: vec![],
                    counters: BudgetCountersSnapshot {
                        nodes_executed: 1,
                        wall_ms: 10,
                        tokens_used: 20,
                    },
                    cur_canonical_hash: None,
                    goal: None,
                    marks_session: None,
                    atomic_granules: vec![],
                    recut_counts: vec![],
                    cur_workflow: "{}".into(),
                },
            },
            JournalLine {
                seq: 3,
                ts: 3,
                event: JournalEvent::PlanLoopCheckpointed {
                    version: 2,
                    replan_count: 1,
                    executed_registry: vec![
                        ("n1".into(), "researcher".into()),
                        ("n2".into(), "researcher".into()),
                    ],
                    counters: BudgetCountersSnapshot {
                        nodes_executed: 5,
                        wall_ms: 600,
                        tokens_used: 700,
                    },
                    cur_canonical_hash: Some("blake3:abc".into()),
                    goal: Some("goal text".into()),
                    marks_session: Some("marks-1".into()),
                    atomic_granules: vec!["ghost".into()],
                    recut_counts: vec![("ghost".into(), 2)],
                    cur_workflow: r#"{"workflow_id":"wf"}"#.into(),
                },
            },
            JournalLine {
                seq: 4,
                ts: 4,
                event: JournalEvent::NodeCheckpointed {
                    workflow_id: "wf".into(),
                    plan_hash: "ph2".into(),
                    node_id: "n3".into(),
                    status: "completed".into(),
                    result_ref: crate::agent::journal::CheckpointResultRef {
                        hash: "blake3:y".into(),
                        len: 0,
                        inline: None,
                    },
                },
            },
        ];
        let (state, tail_start) = replay_plan_loop_checkpoint(&lines)
            .unwrap()
            .expect("latest checkpoint must be found");
        assert_eq!(state.version, 2, "最新版本胜出");
        assert_eq!(state.replan_count, 1, "replan 计数取账面(不重置)");
        assert_eq!(state.counters.nodes_executed, 5, "计数器取账面(不清零)");
        assert_eq!(state.counters.tokens_used, 700);
        assert_eq!(state.executed_registry.len(), 2, "注册表取账面(不重建)");
        assert_eq!(state.cur_canonical_hash.as_deref(), Some("blake3:abc"));
        assert_eq!(state.goal.as_deref(), Some("goal text"));
        assert_eq!(state.marks_session.as_deref(), Some("marks-1"));
        assert!(
            state.atomic_granules.contains("ghost"),
            "原子粒记忆集取账面(恢复不归零)"
        );
        assert_eq!(
            state.recut_counts.get("ghost"),
            Some(&2),
            "重切计数取账面(恢复不归零)"
        );
        assert_eq!(tail_start, 3, "尾段自最新计划检查点之后起(含粒级检查点)");
    }

    #[test]
    fn plan_checkpoint_replay_none_when_absent() {
        let lines: Vec<JournalLine> = vec![];
        assert!(replay_plan_loop_checkpoint(&lines).unwrap().is_none());
    }

    #[tokio::test]
    async fn dsl_resume_equivalence_zero_reexecution_and_counter_continuity() {
        // Dsl 三粒链双跑:连续跑出基线;手工装配恢复态(计数器=崩溃前真值 2,
        // 注册表含 n1)从截断账本恢复——终结果与 nodes_executed 与连续路径
        // 等价,同会话账本合并后每粒恰一条检查点(零重执行)
        let dir = resume_test_dir("dsl");
        let wf = resume_chain_wf();
        let limits = DriverLimits {
            max_replan: 3,
            max_recuts: 3,
            max_wall_ms: None,
            max_tokens: None,
        };
        let outcome1 = run_plan_loop(
            resume_ctx(&dir),
            wf.clone(),
            PlanMode::Dsl,
            limits,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome1.content, "zero");
        assert_eq!(outcome1.stats.nodes_executed, 3);
        let sid = outcome1
            .run_session_id
            .expect("engine bootstrapped session id surfaced");
        let path = JournalWriter::path_for(&dir, &sid);
        let lines = read_all(&path).unwrap();
        assert_eq!(lines.len(), 3, "连续跑:三粒各一条检查点");

        // 崩溃模拟:截断到 n3 检查点之前(粒 2 完成后进程死亡)
        let cut = lines
            .iter()
            .position(
                |l| matches!(&l.event, JournalEvent::NodeCheckpointed { node_id, .. } if node_id == "n3"),
            )
            .unwrap();
        let truncated = &lines[..cut];
        let plan_hash = WorkflowEngine::workflow_plan_hash(&wf).unwrap();
        let node_replay =
            crate::agent::workflow::replay_node_checkpoints(truncated, &plan_hash).unwrap();
        assert_eq!(node_replay.results.len(), 2);

        // 恢复态:计数器/注册表 = 崩溃前真值(粒 1/2 已计入),工作流全文入参
        let state = PlanLoopCheckpointState {
            version: 1,
            replan_count: 0,
            executed_registry: vec![("n1".to_string(), String::new())],
            counters: BudgetCounters {
                nodes_executed: 2,
                wall_ms: 5,
                tokens_used: 0,
            },
            cur_canonical_hash: None,
            goal: None,
            cur_workflow: serde_json::to_string(&wf).unwrap(),
            marks_session: None,
            atomic_granules: std::collections::HashSet::new(),
            recut_counts: std::collections::HashMap::new(),
        };
        let resume = PlanLoopResume {
            state,
            node_replay,
            run_session_id: sid.clone(),
        };
        let outcome2 = run_plan_loop_with_resume(
            resume_ctx(&dir),
            wf.clone(),
            PlanMode::Dsl,
            limits,
            None,
            None,
            None,
            Some(resume),
        )
        .await
        .unwrap();
        assert_eq!(
            outcome2.content, outcome1.content,
            "恢复跑终结果与连续路径等价"
        );
        assert_eq!(
            outcome2.stats.nodes_executed, 3,
            "恢复计数器=账面 2+本进程 1,与连续路径等价(不重置不重复)"
        );
        assert_eq!(outcome2.run_session_id.as_deref(), Some(sid.as_str()));

        // 零重执行:同会话账本合并后三粒各恰一条检查点
        let merged = read_all(&path).unwrap();
        let mut ckpt_nodes: Vec<String> = merged
            .iter()
            .filter_map(|l| match &l.event {
                JournalEvent::NodeCheckpointed { node_id, .. } => Some(node_id.clone()),
                _ => None,
            })
            .collect();
        ckpt_nodes.sort();
        assert_eq!(
            ckpt_nodes,
            vec!["n1".to_string(), "n2".to_string(), "n3".to_string()],
            "三粒各恰一条检查点=零重执行"
        );

        // 注册表恢复后静态拦截仍生效:恢复态注册表含 n1 → 重复提案被点名
        let repeats = detect_repeated_nodes(&wf.nodes, &[("n1".to_string(), String::new())]);
        assert_eq!(repeats, vec!["n1".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_resumable_lists_interrupted_and_excludes_finished() {
        // 扫尾判据:计划检查点在账且无终态=可恢复;有终态/无计划检查点/
        // 非 planrun 文件一律排除;坏文件跳过不炸
        let dir = resume_test_dir("scan");
        // A:中断 run(计划检查点在账,无终态)
        {
            let w = JournalWriter::open(&dir, "planrun-wf-aaa").unwrap();
            w.plan_loop_checkpointed(
                2,
                1,
                vec![("n1".to_string(), "researcher".to_string())],
                BudgetCountersSnapshot {
                    nodes_executed: 3,
                    wall_ms: 120,
                    tokens_used: 45,
                },
                Some("blake3:ph".into()),
                Some("goal".into()),
                Some("marks-1".into()),
                vec![],
                vec![],
                r#"{"workflow_id":"wf_scan"}"#,
            )
            .unwrap();
        }
        // B:已完成 run(有终态)→ 排除
        {
            let w = JournalWriter::open(&dir, "planrun-wf-bbb").unwrap();
            w.plan_loop_checkpointed(
                1,
                0,
                vec![],
                BudgetCountersSnapshot::default(),
                None,
                None,
                None,
                vec![],
                vec![],
                r#"{"workflow_id":"wf_done"}"#,
            )
            .unwrap();
            w.plan_loop_finished("ok").unwrap();
        }
        // C:无计划检查点(仅粒级)→ 排除
        {
            let w = JournalWriter::open(&dir, "planrun-wf-ccc").unwrap();
            w.node_checkpointed("wf", "ph", "n1", "completed", "x")
                .unwrap();
        }
        // 非 planrun 文件 → 忽略;坏文件 → 跳过
        std::fs::write(
            dir.join("s-plain.jsonl"),
            "{}
",
        )
        .unwrap();
        std::fs::write(
            dir.join("planrun-wf-bad.jsonl"),
            "not a journal line
",
        )
        .unwrap();

        let runs = scan_resumable_runs(&dir).unwrap();
        assert_eq!(runs.len(), 1, "恰一个可恢复 run");
        let a = &runs[0];
        assert_eq!(a.session_id, "planrun-wf-aaa");
        assert_eq!(a.version, 2);
        assert_eq!(a.replan_count, 1);
        assert_eq!(a.executed_registry_len, 1);
        assert_eq!(a.workflow_id, "wf_scan");
        assert_eq!(a.wall_ms, 120);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_wall_budget_exhaustion_fails_fast() {
        // 剩余预算闸:恢复态累计墙钟已 ≥ 限额 = 快速失败(errored 但省 token)
        let dir = resume_test_dir("wallgate");
        let wf = resume_chain_wf();
        let state = PlanLoopCheckpointState {
            version: 1,
            replan_count: 0,
            executed_registry: vec![],
            counters: BudgetCounters {
                nodes_executed: 1,
                wall_ms: 1_000,
                tokens_used: 0,
            },
            cur_canonical_hash: None,
            goal: None,
            cur_workflow: serde_json::to_string(&wf).unwrap(),
            marks_session: None,
            atomic_granules: std::collections::HashSet::new(),
            recut_counts: std::collections::HashMap::new(),
        };
        let resume = PlanLoopResume {
            state,
            node_replay: crate::agent::workflow::NodeCheckpointReplay::default(),
            run_session_id: "planrun-gate".to_string(),
        };
        let limits = DriverLimits {
            max_replan: 3,
            max_recuts: 3,
            max_wall_ms: Some(500),
            max_tokens: None,
        };
        let err = run_plan_loop_with_resume(
            resume_ctx(&dir),
            wf,
            PlanMode::Dsl,
            limits,
            None,
            None,
            None,
            Some(resume),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("wall budget already exhausted"),
            "快速失败: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn probe_failure_leaves_no_run_ledger() {
        // probe 阶段不入 run 账:planner 不可达 → 显式失败,且零 planrun 账本
        //(probe 粒的计划锚=probe DAG,对恢复面无效——不入账是终态判据的前提)
        let dir = resume_test_dir("finish");
        let wf = resume_chain_wf();
        let limits = DriverLimits {
            max_replan: 3,
            max_recuts: 3,
            max_wall_ms: None,
            max_tokens: None,
        };
        let result = run_plan_loop(
            resume_ctx(&dir),
            wf,
            PlanMode::PlanExecute,
            limits,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_err(), "planner 不可达必须显式失败");
        let ledgers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("planrun-"))
            .collect();
        assert!(ledgers.is_empty(), "probe 失败零 run 账本: {ledgers:?}");
        assert!(scan_resumable_runs(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_resumable_covers_react_hung_tails_and_excludes_delegates() {
        // react 扫尾判定:根会话悬挂 turn=可恢复(kind=react);委托子会话排除;
        // 干净收尾排除。悬挂夹具=泄漏轮守卫(drop 会补写 aborted 收尾);
        // 泄漏的写者句柄使目录清理失败属预期(清理容忍)
        let dir = resume_test_dir("scanreact");
        // A: react 根会话悬挂 turn → 列出
        {
            let w = JournalWriter::open(&dir, "sess-root").unwrap();
            let guard = w.begin_turn("g").unwrap();
            std::mem::forget(guard);
        }
        // B: 委托子会话悬挂 → 排除(恢复主体是父 run)
        {
            let p = JournalWriter::open(&dir, "sess-parent").unwrap();
            p.begin_turn("g").unwrap();
            p.delegate_spawned("sess-child", "researcher", 1, "digest")
                .unwrap();
            p.end_turn("ok", 1, 1).unwrap();
            let c = JournalWriter::open(&dir, "sess-child").unwrap();
            let child_guard = c.begin_turn("sub").unwrap();
            std::mem::forget(child_guard);
        }
        // C: 干净收尾 → 排除
        {
            let w = JournalWriter::open(&dir, "sess-clean").unwrap();
            w.begin_turn("g").unwrap();
            w.end_turn("ok", 1, 1).unwrap();
        }
        let runs = scan_resumable_runs(&dir).unwrap();
        let react: Vec<_> = runs.iter().filter(|r| r.kind == "react").collect();
        assert_eq!(react.len(), 1, "恰一个 react 恢复候选: {runs:?}");
        assert_eq!(react[0].session_id, "sess-root");
        assert!(
            !runs.iter().any(|r| r.session_id == "sess-child"),
            "委托子会话排除"
        );
        assert!(
            !runs.iter().any(|r| r.session_id == "sess-clean"),
            "干净收尾排除"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plan_loop_finish_marker_written_on_error_exit() {
        // 终态守卫 Err 路径(开账后):恢复态工作流含必败粒(除零)→ 首版执行
        // Err → replan planner 不可达 → Err 终止 → error 终态落地,扫尾排除
        let dir = resume_test_dir("finish2");
        // 必败工作流:n1=""(空拼接) → n2=Div(n1,n1)(空串非整数=节点失败)
        let failing_wf = Workflow {
            workflow_id: "failing_wf".to_string(),
            description: String::new(),
            nodes: vec![
                compute_node(
                    "n1",
                    &[],
                    ComputeSpec::Concat {
                        inputs: vec![ComputeInput::Empty, ComputeInput::Empty],
                    },
                ),
                compute_node(
                    "n2",
                    &["n1"],
                    ComputeSpec::Div {
                        inputs: vec![
                            ComputeInput::Node("n1".to_string()),
                            ComputeInput::Node("n1".to_string()),
                        ],
                    },
                ),
            ],
            output_node: "n2".to_string(),
        };
        let state = PlanLoopCheckpointState {
            version: 1,
            replan_count: 0,
            executed_registry: vec![],
            counters: BudgetCounters::default(),
            cur_canonical_hash: None,
            goal: None,
            cur_workflow: serde_json::to_string(&failing_wf).unwrap(),
            marks_session: None,
            atomic_granules: std::collections::HashSet::new(),
            recut_counts: std::collections::HashMap::new(),
        };
        let resume = PlanLoopResume {
            state,
            node_replay: crate::agent::workflow::NodeCheckpointReplay::default(),
            run_session_id: "planrun-finish-mark".to_string(),
        };
        let limits = DriverLimits {
            max_replan: 0,
            max_recuts: 3,
            max_wall_ms: None,
            max_tokens: None,
        };
        let result = run_plan_loop_with_resume(
            resume_ctx(&dir),
            failing_wf,
            PlanMode::PlanExecute,
            limits,
            None,
            None,
            None,
            Some(resume),
        )
        .await;
        assert!(result.is_err(), "必败粒+replan 预算耗尽必须显式失败");
        let lines = read_all(&JournalWriter::path_for(&dir, "planrun-finish-mark")).unwrap();
        let finished = lines.iter().any(|l| {
            matches!(
                l.event,
                JournalEvent::PlanLoopFinished { ref status } if status == "error"
            )
        });
        assert!(finished, "Err 出口必须落地 error 终态");
        let runs = scan_resumable_runs(&dir).unwrap();
        assert!(runs.is_empty(), "已终局 run 不列可恢复: {runs:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
