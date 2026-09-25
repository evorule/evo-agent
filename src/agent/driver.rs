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
use crate::agent::materializer::materialize_plan_fact;
use crate::agent::replan::{
    lookup_agent_type, should_replan, BudgetCounters, BudgetThresholds, ReplanReason, ReplanState,
};
use crate::agent::workflow::{Workflow, WorkflowEngine};

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
) -> Result<PlanLoopOutcome, String> {
    // tokens 埋点累加器（纲领 §8 Phase 2 交付物 7）：驱动注入 ctx，随每个
    // 子 runner 共享；仅供观测统计，不改变任何控制流。
    let token_arc = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ctx = ctx.with_token_counter(token_arc.clone());
    // M5-c:标记会话启用时同步注入阶段前置裁决通道(每个 LLM 节点 delegate
    // 前提交 phase 信号,由 00_constraint enforce 裁决前置条件——引擎不含
    // 协作纪律知识);None = 零变更
    let mut engine = WorkflowEngine::new(ctx.clone());
    if let Some(sid) = marks_session.as_deref() {
        engine = engine.with_phase_gate(crate::agent::workflow::PhaseGate {
            marks_session: sid.to_string(),
            client: ctx.evorule_client.clone(),
        });
    }
    let initial_id = initial.workflow_id.clone();

    // 当前计划版本（v1 起；Dsl v1 = 手写 workflow，PlanExecute v1 = probe 产出 PlanFact）
    let mut version: u32 = 1;
    let mut replan_count: u32 = 0_u32;
    let mut counters = BudgetCounters::default();
    let mut cur_wf = initial;
    // 当前 PlanFact 注入后 canonical JSON（Dsl v1 = None，replan 时锚回退 seed_hash）
    let mut cur_canonical: Option<String> = None;
    // 原始目标（PlanExecute = probe planner 节点 task；Dsl = None，摘要引导）
    let mut goal: Option<String> = None;
    // 已执行注册表（跨版本累积 (node_id, agent_type)；R8-T03 静态拦截比对源）
    let mut executed_registry: Vec<(String, String)> = Vec::new();
    // v2+ 版本 token 消耗（replan 重复执行成本埋点，交付物 7 / D-02 判定源）
    let mut replan_tokens: u64 = 0;
    // 静态拦截命中累计（R8-T03 幂等重复告警计数）
    let mut repeated_nodes: u64 = 0;
    // 借用先行（replan 站点闭包 async move 按值捕获会 move 非 Copy 的 ctx；
    // 借引用则 Copy 复制进 future，跨 loop 轮次复用同一借用）
    let ctx_ref = &ctx;

    if mode == PlanMode::PlanExecute {
        // ① planning probe：跑载入 DAG（单 planner 节点），产出 PlanFact v1。
        //    probe 失败 = 尚无计划可 replan，直接失败（R1-T03 重试面在
        //    call_planner_with_retry 内：提取失败带反馈重试 1 次）。
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
        cur_canonical = Some(canonical);
        tracing::info!(plan_version = 1, "plan-execute: plan v1 materialized");
    }

    loop {
        // 每版 = 全新 execute（丢弃式 D-02：v(n) 结果不注入 v(n+1)，results 表随栈帧析构）
        let tokens_before = token_arc.load(std::sync::atomic::Ordering::Relaxed);
        let started = std::time::Instant::now();
        let result = engine.execute(&cur_wf).await;
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

        let Some(mut decision) = should_replan(
            &result,
            &counters,
            &thresholds,
            &ReplanState {
                current_version: version,
                replan_count,
            },
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
            });
        };

        // should_replan 第 1 步（硬上限）已挡超额 replan；此处只是防御性断言
        if replan_count >= limits.max_replan {
            return Err("internal: replan decided beyond hard cap".to_string());
        }

        // 失败记录回填计划形态 hash（锚定失败到具体计划版本，交付物 6 §3.1）
        let cur_hash = cur_canonical
            .as_deref()
            .map(plan_canonical_hash)
            .or_else(|| seed_hash.clone());
        if let Some(rec) = decision.failure_record.as_mut() {
            rec.failed_plan_hash = cur_hash.clone();
            // agent_type 外层反查回填（交付物 6 §3.1；查不到保持 None 不猜测）
            if rec.agent_type.is_none() {
                if let Some(node_id) = &rec.failed_node_id {
                    rec.agent_type = lookup_agent_type(&cur_wf.nodes, node_id);
                }
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

        cur_canonical = Some(canonical);
        version = next;
        replan_count += 1;
        tracing::info!(
            plan_version = next,
            "plan-execute: replan materialized, re-executing"
        );
    }
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

/// M5-b：向标记会话提交节点完成信号（submit_command 既有通道；成功即落链
/// 为 StateTransition 事实，规则面在同一转换上求值 branch 壳并裁决标记）
async fn submit_node_signal(
    client: crate::api::evorule_client::EvoruleApiClient,
    session_id: &str,
    node_id: &str,
) -> Result<(), String> {
    let cmd = node_done_signal(node_id);
    client
        .submit_command(session_id, &cmd)
        .await
        .map_err(|e| e.to_string())
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
pub fn build_replan_task(
    goal: Option<&str>,
    summary: &Value,
    trigger_json: &str,
    next_version: u32,
) -> String {
    format!(
        "REPLAN REQUEST — produce plan v{next} as a single PlanFact JSON object.\n\n\
         Original goal:\n{goal}\n\n\
         Previous plan summary (v{prev}):\n{summary}\n\n\
         Trigger (why replanning):\n{trigger}\n\n\
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
        );
        assert!(task.contains("plan v2"));
        assert!(task.contains("研究 X"));
        assert!(task.contains("node_failure"));
        assert!(task.contains("COMPLETE new plan"));
        // 无目标时的引导语
        let task2 = build_replan_task(None, &summary, "{}", 3);
        assert!(task2.contains("infer it from the previous plan summary"));
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
}
