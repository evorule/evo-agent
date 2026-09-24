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
pub async fn run_plan_loop(
    ctx: DelegateContext,
    initial: Workflow,
    mode: PlanMode,
    limits: DriverLimits,
    seed_hash: Option<String>,
) -> Result<PlanLoopOutcome, String> {
    let engine = WorkflowEngine::new(ctx.clone());
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

    if mode == PlanMode::PlanExecute {
        // ① planning probe：跑载入 DAG（单 planner 节点），产出 PlanFact v1。
        //    probe 失败 = 尚无计划可 replan（planner 重试归 Phase 2 R1-T03），直接失败。
        let probe_out = engine
            .execute(&cur_wf)
            .await
            .map_err(|e| format!("planning probe failed: {}", e))?;
        goal = cur_wf
            .nodes
            .iter()
            .find(|n| n.agent_type == "planner")
            .map(|n| n.task.clone());
        let mut plan = extract_plan_json(&probe_out)?;
        inject_plan_meta(&mut plan, "initial_planning", 1, None);
        let canonical = serde_json::to_string(&plan).map_err(|e| e.to_string())?;
        cur_wf = materialize_plan_fact(&plan, &format!("{}_plan_v1", initial_id))
            .map_err(|errs| format!("plan v1 materialization failed: {}", errs.join("; ")))?;
        cur_canonical = Some(canonical);
        tracing::info!(plan_version = 1, "plan-execute: plan v1 materialized");
    }

    loop {
        // 每版 = 全新 execute（丢弃式 D-02：v(n) 结果不注入 v(n+1)，results 表随栈帧析构）
        let started = std::time::Instant::now();
        let result = engine.execute(&cur_wf).await;
        counters.wall_ms += started.elapsed().as_millis() as u64;
        counters.nodes_executed += engine.executed_nodes();

        // 阈值逐版重算（max_nodes = 当前版展开节点数 × 2）；计数器跨版累计——
        // 预算判定对累计值做，保守方向（早触发），Phase 1-B 实现新明确点
        let mut thresholds = BudgetThresholds::defaults(cur_wf.nodes.len());
        thresholds.max_replan = limits.max_replan;
        thresholds.max_wall_ms = limits.max_wall_ms;

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
        let plan_text = ctx
            .delegate("planner", &task)
            .await
            .map_err(|e| format!("replan planner call failed: {}", e))?;
        let mut plan = extract_plan_json(&plan_text)?;
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
        cur_canonical = Some(canonical);
        version = next;
        replan_count += 1;
        tracing::info!(
            plan_version = next,
            "plan-execute: replan materialized, re-executing"
        );
    }
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
}
