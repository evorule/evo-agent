// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 物化函数（plan-execute 方案 D；实现契约 = Phase 0 交付物 5，已评审通过）
//!
//! 把动态性关进计划生成阶段：双输入源，同一展开 pass（交付物 5 §1.2）——
//! - **PlanFact 形态**（planner LLM 产出：无壳，顶层 `edges` 为依赖权威，J7 双轨）
//! - **手写 DSL 形态**（workflow_dag v1.2 宪法文档：宪法壳，节点内联 `depends_on`）
//!
//! 两形态先归一化为统一中间形态（IR），展开 pass 只对 IR 工作——一份实现两处输入。
//! 展开语义（交付物 5 §3 / 交付物 3 §4）：
//! 1. 副本生成：循环体按 `{loop_id}_iter{k}_{node_id}` 静态复制 max_iterations 份（k 从 0 递增）；
//! 2. 引用改写（R1–R4 文法，交付物 3 §2.3）：同迭代原名 → 当前迭代副本全名；`prev.X` →
//!    上一迭代副本全名（iter0 按三消费面消解——占位符消除 / `ComputeInput::Empty` / 空观察源，
//!    注记一）；全局原名原样；跨循环展开全名校验 k 越界后原样；
//! 3. 隐式依赖逐节点全连（注记二）：iter k 每副本依赖 iter k-1 全部副本，保证迭代串行；
//! 4. 展开后自校验：总量 ≤512、副本名唯一（含与全局名碰撞）、引用存在性、拓扑无环、
//!    引用更早拓扑层、output_node 存在；物化 DAG 中不得残留任何 `prev.` 形态。
//!
//! 纯函数声明（纲领风险 4）：无 IO、无时钟、无随机源、不读引擎状态；同 (输入，
//! [`MATERIALIZER_VERSION`]) 必得同 DAG（逐字节）。破坏性变更（命名/改写/隐式依赖形态）
//! 必须 bump 版本；重放/离线复算按 PlanFact.materializer_version 选择对应版本函数。

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::agent::workflow::{ComputeInput, ComputeSpec, RunWhen, Workflow, WorkflowNode};

/// 物化函数版本（semver；MVP 首发，记入 PlanFact.materializer_version 由外层驱动注入）
pub const MATERIALIZER_VERSION: &str = "1.0.0";

/// 冻结限额（交付物 3 §5 / 交付物 5 §3 步骤 0）
const MAX_PRE_EXPANSION_NODES: usize = 64;
const MAX_LOOPS: usize = 8;
const MAX_BODY_NODES: usize = 8;
const MAX_ITERATIONS: usize = 32;
const MAX_EXPANDED_NODES: usize = 512;

// ============================================================================
// 入口（双形态）
// ============================================================================

/// 物化手写 DSL 形态（workflow_dag v1.2 宪法文档，壳字段忽略只读 body）。
///
/// 前置信任：body 已通过宪法 schema 校验（constitution 门卫）；本函数做防御性
/// 再校验（步骤 0），失败即拒载（fail-fast，无静默修复）。
pub fn materialize_workflow_dag(body: &serde_json::Value) -> Result<Workflow, Vec<String>> {
    let workflow_id = str_field(body, "workflow_id")?.ok_or_else(|| {
        vec!["workflow_dag 文档缺 workflow_id（物化防御性再校验拒绝）".to_string()]
    })?;
    let description = body
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let output_node = str_field(body, "output_node")?.ok_or_else(|| {
        vec!["workflow_dag 文档缺 output_node（物化防御性再校验拒绝）".to_string()]
    })?;

    let nodes = body
        .get("nodes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| vec!["workflow_dag 文档缺 nodes 数组".to_string()])?;
    let mut globals = Vec::with_capacity(nodes.len());
    for n in nodes {
        globals.push(parse_dsl_node(n).map_err(|e| vec![e])?);
    }
    let loops = parse_loops_opt(body.get("loops"))?;

    // 归一化：手写内联 depends_on → dep_edges（隐式边表统一形态；顺序 = 声明序，确定性）
    let mut dep_edges = Vec::new();
    for g in &globals {
        for d in &g.deps {
            dep_edges.push((d.clone(), g.id.clone()));
        }
    }
    for l in &loops {
        for b in &l.body {
            for d in &b.deps {
                dep_edges.push((d.clone(), b.id.clone()));
            }
        }
    }

    expand(Ir {
        workflow_id: workflow_id.to_string(),
        description,
        globals,
        loops,
        dep_edges,
        output_node: Some(output_node.to_string()),
    })
}

/// 物化 PlanFact 形态（planner 节点结果；注入组/plan_source 等壳字段与本 pass 无关，忽略）。
///
/// `workflow_id` 由外层驱动确定性供给（PlanFact 无此字段）。依赖权威 = 顶层 `edges`
/// （J7）；`input_refs` 仅数据声明，非依赖（一致性校验 C3/C4 属代码层校验器，Phase 1-D）。
/// PlanFact 无 `output_node` 字段（交付物 2 schema 封口）：物化器以**展开后唯一汇点**
/// 派生产出节点，多汇点/零汇点即拒载（实现新明确点，随专项档留痕报项目方）。
pub fn materialize_plan_fact(
    plan: &serde_json::Value,
    workflow_id: &str,
) -> Result<Workflow, Vec<String>> {
    let nodes = plan
        .get("nodes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| vec!["PlanFact 缺 nodes 数组（物化防御性再校验拒绝）".to_string()])?;
    let mut globals = Vec::with_capacity(nodes.len());
    for n in nodes {
        globals.push(parse_plan_fact_node(n).map_err(|e| vec![e])?);
    }
    let loops = parse_loops_opt(plan.get("loops"))?;

    let mut dep_edges = Vec::new();
    if let Some(edges) = plan.get("edges").and_then(|v| v.as_array()) {
        for e in edges {
            let pair = e
                .as_array()
                .filter(|p| p.len() == 2)
                .ok_or_else(|| vec![format!("PlanFact edges 元素须为 [from, to] 二元组: {e}")])?;
            let from = pair[0]
                .as_str()
                .ok_or_else(|| vec![format!("PlanFact edges from 须为字符串: {e}")])?;
            let to = pair[1]
                .as_str()
                .ok_or_else(|| vec![format!("PlanFact edges to 须为字符串: {e}")])?;
            dep_edges.push((from.to_string(), to.to_string()));
        }
    }

    expand(Ir {
        workflow_id: workflow_id.to_string(),
        description: String::new(),
        globals,
        loops,
        dep_edges,
        output_node: None,
    })
}

// ============================================================================
// 归一化中间形态（IR）——两形态归一化后逐字段同构，展开 pass 不感知来源
// ============================================================================

struct IrNode {
    id: String,
    agent_type: Option<String>,
    task: Option<String>,
    task_template: Option<String>,
    compute: Option<ComputeSpec>,
    run_when: Option<RunWhen>,
    /// 内联 depends_on（仅 DSL 形态；PlanFact 形态恒空，依赖由顶层 edges 表达）。
    /// 归一化后被收入 Ir.dep_edges，展开 pass 不再读本字段。
    deps: Vec<String>,
}

struct IrLoop {
    id: String,
    max_iterations: usize,
    body: Vec<IrNode>,
}

struct Ir {
    workflow_id: String,
    description: String,
    globals: Vec<IrNode>,
    loops: Vec<IrLoop>,
    /// (from, to)：to 依赖 from（PlanFact edges 与内联 depends_on 的统一形态）
    dep_edges: Vec<(String, String)>,
    /// None = PlanFact 形态（展开后按唯一汇点派生）
    output_node: Option<String>,
}

fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> Result<Option<&'a str>, Vec<String>> {
    match v.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.as_str())),
        Some(other) => Err(vec![format!("字段 '{key}' 须为字符串，实际: {other}")]),
    }
}

fn check_id_whitelist(id: &str, what: &str) -> Result<(), String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "{what} id '{id}' 非法：须匹配 [A-Za-z0-9_-]+（防占位符污染/路径穿越）"
        ));
    }
    Ok(())
}

/// 解析 v1.2 node 形态（手写 DSL；compute/run_when 交 serde 强类型防御性解析）
fn parse_dsl_node(v: &serde_json::Value) -> Result<IrNode, String> {
    let id = str_field(v, "id")
        .map_err(|e| e.join("; "))?
        .ok_or_else(|| "节点缺 id".to_string())?;
    check_id_whitelist(id, "节点")?;
    let compute = match v.get("compute") {
        None => None,
        Some(c) => Some(
            serde_json::from_value::<ComputeSpec>(c.clone())
                .map_err(|e| format!("节点 '{id}' compute 解析失败: {e}"))?,
        ),
    };
    // compute 节点禁止 agent_type/task/task_template（v1.2 schema allOf；防御性复读）
    if compute.is_some() {
        for forbidden in ["agent_type", "task", "task_template"] {
            if v.get(forbidden).is_some() {
                return Err(format!(
                    "节点 '{id}' 含 compute 时禁止提供 {forbidden}（D-03：compute 无 agent 语义）"
                ));
            }
        }
    }
    Ok(IrNode {
        id: id.to_string(),
        agent_type: str_field(v, "agent_type")
            .map_err(|e| e.join("; "))?
            .map(String::from),
        task: str_field(v, "task")
            .map_err(|e| e.join("; "))?
            .map(String::from),
        task_template: str_field(v, "task_template")
            .map_err(|e| e.join("; "))?
            .map(String::from),
        compute,
        run_when: parse_run_when_opt(v, id)?,
        deps: depends_on_of(v, id)?,
    })
}

/// 内联 depends_on 读取（DSL 形态；数组字符串元素，防御性校验元素类型）
fn depends_on_of(v: &serde_json::Value, id: &str) -> Result<Vec<String>, String> {
    match v.get("depends_on") {
        None => Ok(Vec::new()),
        Some(serde_json::Value::Array(arr)) => {
            let mut deps = Vec::with_capacity(arr.len());
            for d in arr {
                let s = d
                    .as_str()
                    .ok_or_else(|| format!("节点 '{id}' depends_on 元素须为字符串: {d}"))?;
                deps.push(s.to_string());
            }
            Ok(deps)
        }
        Some(other) => Err(format!("节点 '{id}' depends_on 须为数组: {other}")),
    }
}

/// 解析 PlanFact node 形态（J1 显式 type 判别；type=tool 按 J3 物化为 agent 节点，
/// tool 字段是意图声明不进 DAG——工具调用与参数经 agent 会话链路入链）
fn parse_plan_fact_node(v: &serde_json::Value) -> Result<IrNode, String> {
    let mut node = parse_dsl_node(v)?;
    // PlanFact 节点无 depends_on（交付物 2 schema additionalProperties 封口）：
    // 依赖权威 = 顶层 edges（J7）。防御性：带了即拒，fail-fast。
    if !node.deps.is_empty() {
        return Err(format!(
            "PlanFact 节点 '{}' 不承载 depends_on：依赖由顶层 edges 表达（J7）",
            node.id
        ));
    }
    node.deps = Vec::new();
    // C9（交付物 2 §6）：计划节点禁止引用 planner 自身（防自指递归——planner
    // 是计划的生产者不是执行者；手写 DSL v1.2 probe 工作流不受此限）
    if node.agent_type.as_deref() == Some("planner") {
        return Err(format!(
            "PlanFact 节点 '{}' 禁止引用 agent_type='planner'（C9 防自指递归）",
            node.id
        ));
    }
    let ty = str_field(v, "type")
        .map_err(|e| e.join("; "))?
        .ok_or_else(|| format!("PlanFact 节点 '{}' 缺 type（J1 显式判别）", node.id))?;
    match ty {
        "llm" => {
            if node.agent_type.is_none() {
                return Err(format!(
                    "PlanFact 节点 '{}' type=llm 须提供 agent_type",
                    node.id
                ));
            }
            if node.task.is_none() && node.task_template.is_none() {
                return Err(format!(
                    "PlanFact 节点 '{}' type=llm 须提供 task 或 task_template",
                    node.id
                ));
            }
            Ok(node)
        }
        "tool" => {
            if node.agent_type.is_none() {
                return Err(format!(
                    "PlanFact 节点 '{}' type=tool 须提供 agent_type（J3 agent 语义）",
                    node.id
                ));
            }
            if v.get("tool").is_none() {
                return Err(format!(
                    "PlanFact 节点 '{}' type=tool 须提供 tool（幂等读白名单）",
                    node.id
                ));
            }
            if node.task.is_none() && node.task_template.is_none() {
                return Err(format!(
                    "PlanFact 节点 '{}' type=tool 须提供 task 或 task_template",
                    node.id
                ));
            }
            Ok(node) // tool 字段到此完成声明职责，不进物化 DAG
        }
        "compute" => {
            if node.compute.is_none() {
                return Err(format!(
                    "PlanFact 节点 '{}' type=compute 须提供 compute",
                    node.id
                ));
            }
            Ok(node)
        }
        other => Err(format!(
            "PlanFact 节点 '{}' type='{other}' 不在白名单（J2：llm|tool|compute）",
            node.id
        )),
    }
}

fn parse_run_when_opt(v: &serde_json::Value, id: &str) -> Result<Option<RunWhen>, String> {
    match v.get("run_when") {
        None => Ok(None),
        Some(r) => Ok(Some(
            serde_json::from_value::<RunWhen>(r.clone())
                .map_err(|e| format!("节点 '{id}' run_when 解析失败: {e}"))?,
        )),
    }
}

fn parse_loops_opt(v: Option<&serde_json::Value>) -> Result<Vec<IrLoop>, Vec<String>> {
    let Some(arr) = v.and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut loops = Vec::with_capacity(arr.len());
    for l in arr {
        let id = str_field(l, "id")?.ok_or_else(|| vec!["loop 缺 id".to_string()])?;
        check_id_whitelist(id, "loop").map_err(|e| vec![e])?;
        let max_iterations = l
            .get("max_iterations")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| vec![format!("loop '{id}' 缺 max_iterations 或非正整数")])?
            as usize;
        let body_arr = l
            .get("body")
            .and_then(|v| v.as_array())
            .ok_or_else(|| vec![format!("loop '{id}' 缺 body 数组")])?;
        let mut body = Vec::with_capacity(body_arr.len());
        for n in body_arr {
            // loop 体节点两形态同构（v1.2 node），手写/PlanFact 均用 DSL node 解析；
            // PlanFact 体节点若带 type 字段也按其校验——统一走 DSL 解析 + 显式 type 复查
            body.push(parse_loop_body_node(n)?);
        }
        loops.push(IrLoop {
            id: id.to_string(),
            max_iterations,
            body,
        });
    }
    Ok(loops)
}

/// loop 体节点解析：兼容两形态（v1.2 node 无 type；PlanFact node 有 type）
fn parse_loop_body_node(v: &serde_json::Value) -> Result<IrNode, Vec<String>> {
    if v.get("type").is_some() {
        parse_plan_fact_node(v).map_err(|e| vec![e])
    } else {
        parse_dsl_node(v).map_err(|e| vec![e])
    }
}

// ============================================================================
// 引用文法分类（R1–R4，交付物 3 §2.3；三消费面共用同一分类器）
// ============================================================================

struct LoopInfo {
    id: String,
    max_iterations: usize,
    body_ids: Vec<String>,
}

/// 引用分类上下文；current = (loop 下标, 迭代 k)，全局节点为 None
struct RefCtx<'a> {
    globals: &'a BTreeSet<String>,
    loops: &'a [LoopInfo],
    current: Option<(usize, usize)>,
}

enum Resolved {
    /// 最终引用名（副本全名 / 全局原名 / R4 展开全名）
    Name(String),
    /// iter0 的 prev.X——三消费面各自消解（注记一）
    Empty,
    /// 非引用 token（v1.0 兼容：模板字面量原样保留；结构面由存在性校验拒绝）
    Keep,
}

fn copy_name(loop_id: &str, k: usize, node_id: &str) -> String {
    format!("{loop_id}_iter{k}_{node_id}")
}

/// 解析 R4 展开全名 `{loop_id}_iter{k}_{node_id}`：loop_id/node_id 可能本身含下划线，
/// 枚举所有 `_iter<数字>_` 切分点，取第一个命中已知 loop + 其体节点的切分（确定性）。
fn parse_r4(token: &str, loops: &[LoopInfo]) -> Option<(usize, usize)> {
    for (i, _) in token.match_indices("_iter") {
        let after = &token[i + 5..];
        let digits_end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if digits_end == 0 || !after[digits_end..].starts_with('_') {
            continue;
        }
        let Ok(k) = after[..digits_end].parse::<usize>() else {
            continue;
        };
        let loop_id = &token[..i];
        let node_id = &after[digits_end + 1..];
        if node_id.is_empty() {
            continue;
        }
        if let Some(idx) = loops.iter().position(|l| l.id == loop_id) {
            if loops[idx].body_ids.iter().any(|b| b == node_id) {
                return Some((idx, k));
            }
        }
    }
    None
}

/// 引用分类（纯函数）。错误消息携带 R4 写法指引（A10 友好化，交付物 5 §3.1）。
fn classify(token: &str, ctx: &RefCtx<'_>) -> Result<Resolved, String> {
    // R2 跨迭代（仅循环体内合法；作用域外 = 反模式 A3）
    if let Some(rest) = token.strip_prefix("prev.") {
        let Some((li, k)) = ctx.current else {
            return Err(format!(
                "引用 '{token}'：prev. 前缀仅循环体内合法（R2 作用域，反模式 A3）"
            ));
        };
        if !ctx.loops[li].body_ids.iter().any(|b| b == rest) {
            return Err(format!(
                "引用 '{token}'：prev. 的目标 '{rest}' 须是本循环 '{}' 的体节点",
                ctx.loops[li].id
            ));
        }
        if k == 0 {
            return Ok(Resolved::Empty); // iter0 视作空结果（注记一）
        }
        return Ok(Resolved::Name(copy_name(&ctx.loops[li].id, k - 1, rest)));
    }
    // R1 同迭代原名（当前循环体节点）
    if let Some((li, k)) = ctx.current {
        if ctx.loops[li].body_ids.iter().any(|b| b == token) {
            return Ok(Resolved::Name(copy_name(&ctx.loops[li].id, k, token)));
        }
    }
    // R3 全局原名
    if ctx.globals.contains(token) {
        return Ok(Resolved::Name(token.to_string()));
    }
    // R4 跨循环展开全名（k 越界即拒，反模式 A4）
    if let Some((li, k)) = parse_r4(token, ctx.loops) {
        if k >= ctx.loops[li].max_iterations {
            return Err(format!(
                "引用 '{token}' 越界：k={k} 不小于 loop '{}' 的 max_iterations={}（R4 须 k < max_iterations）",
                ctx.loops[li].id, ctx.loops[li].max_iterations
            ));
        }
        return Ok(Resolved::Name(token.to_string()));
    }
    // 其他循环体节点的原名引用 = 反模式 A10（循环外原名引用循环体节点）
    if let Some(l) = ctx
        .loops
        .iter()
        .find(|l| l.body_ids.iter().any(|b| b == token))
    {
        return Err(format!(
            "'{token}' 是循环 {} 的体节点，循环外引用须写展开全名 {}_iter{{k}}_{token}（R4，反模式 A10）",
            l.id, l.id
        ));
    }
    Ok(Resolved::Keep)
}

// ============================================================================
// 展开算法（交付物 5 §3 步骤 0–6）
// ============================================================================

fn expand(ir: Ir) -> Result<Workflow, Vec<String>> {
    // ---- 步骤 0：防御性再校验（id 白名单/唯一性/loop 冲突/限额）----
    step0_checks(&ir)?;

    // 引用分类 Universe
    let globals: BTreeSet<String> = ir.globals.iter().map(|n| n.id.clone()).collect();
    let loop_infos: Vec<LoopInfo> = ir
        .loops
        .iter()
        .map(|l| LoopInfo {
            id: l.id.clone(),
            max_iterations: l.max_iterations,
            body_ids: l.body.iter().map(|n| n.id.clone()).collect(),
        })
        .collect();

    // ---- 步骤 2/3：副本生成 + 引用改写（无 loop 时同 pass 退化为恒等改写 + A3 检查）----
    let mut nodes: Vec<WorkflowNode> = Vec::new();
    for g in &ir.globals {
        let ctx = RefCtx {
            globals: &globals,
            loops: &loop_infos,
            current: None,
        };
        let deps = rewritten_deps(&ir.dep_edges, &g.id, &ctx).map_err(|e| vec![e])?;
        nodes.push(rewrite_node(g, g.id.clone(), &ctx, deps).map_err(|e| vec![e])?);
    }
    for (li, loop_def) in ir.loops.iter().enumerate() {
        for k in 0..loop_def.max_iterations {
            for body_node in &loop_def.body {
                let ctx = RefCtx {
                    globals: &globals,
                    loops: &loop_infos,
                    current: Some((li, k)),
                };
                let mut deps =
                    rewritten_deps(&ir.dep_edges, &body_node.id, &ctx).map_err(|e| vec![e])?;
                // ---- 步骤 4：隐式依赖逐节点全连（注记二）----
                if k >= 1 {
                    for prev in &loop_def.body {
                        let name = copy_name(&loop_def.id, k - 1, &prev.id);
                        if !deps.contains(&name) {
                            deps.push(name);
                        }
                    }
                }
                let new_id = copy_name(&loop_def.id, k, &body_node.id);
                nodes.push(rewrite_node(body_node, new_id, &ctx, deps).map_err(|e| vec![e])?);
            }
        }
    }

    // ---- PlanFact 形态：output_node 按展开后唯一汇点派生（实现新明确点）----
    let output_node = match &ir.output_node {
        Some(o) => o.clone(),
        None => derive_unique_sink(&nodes)?,
    };

    // ---- 步骤 5：展开后图自校验 ----
    self_check(&nodes, &output_node, &ir)?;

    // ---- 步骤 5.5 计划体检：孤立 compute warn（交付物 4 §5.3 / 1-A M3 裁定，
    //      Phase 2 计划体检面落地）——非拒载，引导作者确认节点存在必要 ----
    let orphans = find_orphan_computes(&nodes, &output_node);
    if !orphans.is_empty() {
        tracing::warn!(
            workflow_id = %ir.workflow_id,
            orphan_computes = ?orphans,
            "plan health check: 孤立 compute 节点（结果无任何下游消费面，工作流结束后不可考）"
        );
    }

    // ---- 步骤 6：输出物化 DAG（R5-T06：日志面，人可核对）----
    let edge_count: usize = nodes.iter().map(|n| n.depends_on.len()).sum();
    tracing::info!(
        workflow_id = %ir.workflow_id,
        materializer_version = MATERIALIZER_VERSION,
        expanded_nodes = nodes.len(),
        dep_edges = edge_count,
        loops = ir.loops.len(),
        "workflow materialized: 展开完成（节点/边清单见物化 DAG，可离线复算核对）"
    );
    for n in &nodes {
        tracing::debug!(
            workflow_id = %ir.workflow_id,
            node_id = %n.id,
            depends_on = ?n.depends_on,
            "materialized node"
        );
    }

    Ok(Workflow {
        workflow_id: ir.workflow_id,
        description: ir.description,
        nodes,
        output_node,
    })
}

/// 步骤 5.5 计划体检纯函数：孤立 compute 节点检测（交付物 4 §5.3）
///
/// 「孤立」= 该 compute 节点 id 不出现在任何节点的 `depends_on` 中、也不是
/// `output_node`——其结果无任何下游消费面。仅检测不拒载（§5.3 建议形态）。
fn find_orphan_computes(nodes: &[WorkflowNode], output_node: &str) -> Vec<String> {
    let mut consumers: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for n in nodes {
        for d in &n.depends_on {
            consumers.insert(d.as_str());
        }
    }
    consumers.insert(output_node);
    nodes
        .iter()
        .filter(|n| n.compute.is_some() && !consumers.contains(n.id.as_str()))
        .map(|n| n.id.clone())
        .collect()
}

/// 步骤 0：限额与命名空间校验（交付物 5 §3 步骤 0 / 交付物 3 §5 冻结值）
fn step0_checks(ir: &Ir) -> Result<(), Vec<String>> {
    // 全局节点域唯一（跨 loop 的体节点同名合法——展开后副本名带 loop 前缀不冲突）
    let mut all_ids: BTreeSet<String> = BTreeSet::new();
    for n in ir.globals.iter() {
        check_id_whitelist(&n.id, "节点").map_err(|e| vec![e])?;
        if !all_ids.insert(n.id.clone()) {
            return Err(vec![format!("节点 id 重复: '{}'", n.id)]);
        }
    }
    let mut loop_ids: BTreeSet<String> = BTreeSet::new();
    for l in &ir.loops {
        check_id_whitelist(&l.id, "loop").map_err(|e| vec![e])?;
        if !loop_ids.insert(l.id.clone()) {
            return Err(vec![format!("loop id 重复: '{}'", l.id)]);
        }
        if all_ids.contains(&l.id) {
            return Err(vec![format!(
                "loop id '{}' 与节点 id 冲突（同一命名空间，反模式 A6）",
                l.id
            )]);
        }
        // 单 loop 内体节点唯一
        let mut body_ids: BTreeSet<String> = BTreeSet::new();
        for b in &l.body {
            check_id_whitelist(&b.id, "节点").map_err(|e| vec![e])?;
            if !body_ids.insert(b.id.clone()) {
                return Err(vec![format!(
                    "loop '{}' 内体节点 id 重复: '{}'",
                    l.id, b.id
                )]);
            }
        }
    }
    let body_total: usize = ir.loops.iter().map(|l| l.body.len()).sum();
    if ir.globals.len() + body_total > MAX_PRE_EXPANSION_NODES {
        return Err(vec![format!(
            "展开前节点总量 {} 超限额 {MAX_PRE_EXPANSION_NODES}（nodes ∪ Σbody，交付物 3 §5）",
            ir.globals.len() + body_total
        )]);
    }
    if ir.loops.len() > MAX_LOOPS {
        return Err(vec![format!(
            "loops 数量 {} 超限额 {MAX_LOOPS}",
            ir.loops.len()
        )]);
    }
    for l in &ir.loops {
        if l.body.is_empty() || l.body.len() > MAX_BODY_NODES {
            return Err(vec![format!(
                "loop '{}' body 大小 {} 越界（1..={MAX_BODY_NODES}；超限引导拆子工作流委托）",
                l.id,
                l.body.len()
            )]);
        }
        if l.max_iterations == 0 || l.max_iterations > MAX_ITERATIONS {
            return Err(vec![format!(
                "loop '{}' max_iterations={} 越界（1..={MAX_ITERATIONS}，冻结限额）",
                l.id, l.max_iterations
            )]);
        }
    }
    // compute 防御性再校验（签名封闭 + regex 加载期合法性，交付物 4 §4.1–4.3）
    for n in ir
        .globals
        .iter()
        .chain(ir.loops.iter().flat_map(|l| l.body.iter()))
    {
        if let Some(c) = &n.compute {
            check_compute_spec(c, &n.id)?;
        }
    }
    Ok(())
}

/// compute 签名防御性校验：输入数、threshold 互斥（schema oneOf 已表达，代码层双保险）
fn check_compute_spec(c: &ComputeSpec, node_id: &str) -> Result<(), Vec<String>> {
    match c {
        ComputeSpec::Strcmp { inputs, .. } => {
            if inputs.len() != 2 {
                return Err(vec![format!(
                    "compute 节点 '{node_id}' strcmp 须恰 2 个 inputs，实际 {}",
                    inputs.len()
                )]);
            }
        }
        ComputeSpec::NumericCmp {
            inputs, threshold, ..
        } => {
            if inputs.is_empty() || inputs.len() > 2 {
                return Err(vec![format!(
                    "compute 节点 '{node_id}' numeric_cmp 须 1..=2 个 inputs，实际 {}",
                    inputs.len()
                )]);
            }
            if inputs.len() == 1 && threshold.is_none() {
                return Err(vec![format!(
                    "compute 节点 '{node_id}' numeric_cmp 单输入形态必填 threshold"
                )]);
            }
            if inputs.len() == 2 && threshold.is_some() {
                return Err(vec![format!(
                    "compute 节点 '{node_id}' numeric_cmp 双输入形态禁止 threshold"
                )]);
            }
        }
        ComputeSpec::RegexMatch { inputs, pattern } => {
            if inputs.len() != 1 {
                return Err(vec![format!(
                    "compute 节点 '{node_id}' regex_match 须恰 1 个 inputs，实际 {}",
                    inputs.len()
                )]);
            }
            if regex::Regex::new(pattern).is_err() {
                return Err(vec![format!(
                    "compute 节点 '{node_id}' regex_match pattern 非法（Rust regex 语法，加载期拒载）: '{pattern}'"
                )]);
            }
        }
    }
    Ok(())
}

/// 重写一个节点的显式依赖边（deps 面禁止 R2：跨迭代依赖由展开器隐式插入，反模式 A2）
fn rewritten_deps(
    dep_edges: &[(String, String)],
    owner_id: &str,
    ctx: &RefCtx<'_>,
) -> Result<Vec<String>, String> {
    let mut deps = Vec::new();
    for (from, to) in dep_edges {
        if to != owner_id {
            continue;
        }
        if from.starts_with("prev.") {
            return Err(format!(
                "节点 '{owner_id}' depends_on '{from}'：depends_on 禁止 prev. 前缀（反模式 A2，跨迭代依赖由展开器隐式插入）"
            ));
        }
        let name = match classify(from, ctx)? {
            Resolved::Name(n) => n,
            Resolved::Empty => {
                return Err(format!(
                    "节点 '{owner_id}' depends_on '{from}'：iter0 空消解不适用于依赖面"
                ))
            }
            Resolved::Keep => from.clone(), // 存在性由步骤 5 拒绝
        };
        if !deps.contains(&name) {
            deps.push(name);
        }
    }
    Ok(deps)
}

/// 步骤 3：按三消费面改写一个节点（模板占位符 / run_when 观察源 / compute inputs）
fn rewrite_node(
    src: &IrNode,
    new_id: String,
    ctx: &RefCtx<'_>,
    deps: Vec<String>,
) -> Result<WorkflowNode, String> {
    let task_template = match &src.task_template {
        Some(t) => Some(
            rewrite_template(t, ctx)
                .map_err(|e| format!("节点 '{}' 模板引用改写失败: {e}", src.id))?,
        ),
        None => None,
    };
    let run_when = match &src.run_when {
        None => None,
        Some(rw) => {
            let node_ref = match classify(&rw.node, ctx)
                .map_err(|e| format!("节点 '{}' run_when 观察源改写失败: {e}", src.id))?
            {
                Resolved::Name(n) => n,
                // iter0 空观察源（注记一·3）：node=""，执行期 results.get("") → None → 空串
                Resolved::Empty => String::new(),
                Resolved::Keep => rw.node.clone(),
            };
            Some(RunWhen {
                node: node_ref,
                op: rw.op,
                value: rw.value.clone(),
            })
        }
    };
    let compute = match &src.compute {
        None => None,
        Some(c) => Some(
            rewrite_compute(c, ctx)
                .map_err(|e| format!("节点 '{}' compute 输入改写失败: {e}", src.id))?,
        ),
    };
    Ok(WorkflowNode {
        id: new_id,
        agent_type: src.agent_type.clone().unwrap_or_default(),
        task: src.task.clone().unwrap_or_default(),
        task_template,
        depends_on: deps,
        run_when,
        compute,
    })
}

fn rewrite_compute(c: &ComputeSpec, ctx: &RefCtx<'_>) -> Result<ComputeSpec, String> {
    let map_input = |i: &ComputeInput| -> Result<ComputeInput, String> {
        match i {
            // 物化器源形态不应出现 Empty（Empty 是物化产物）；防御性原样保留
            ComputeInput::Empty => Ok(ComputeInput::Empty),
            ComputeInput::Node(t) => match classify(t, ctx)? {
                Resolved::Name(n) => Ok(ComputeInput::Node(n)),
                // iter0 空输入字面量（注记一·2；交付物 4 §3.3 缺失=空串语义落位）
                Resolved::Empty => Ok(ComputeInput::Empty),
                Resolved::Keep => Ok(ComputeInput::Node(t.clone())), // 步骤 5 存在性拒绝
            },
        }
    };
    Ok(match c {
        ComputeSpec::Strcmp { inputs, mode } => ComputeSpec::Strcmp {
            inputs: inputs.iter().map(map_input).collect::<Result<_, _>>()?,
            mode: *mode,
        },
        ComputeSpec::NumericCmp {
            inputs,
            mode,
            threshold,
        } => ComputeSpec::NumericCmp {
            inputs: inputs.iter().map(map_input).collect::<Result<_, _>>()?,
            mode: *mode,
            threshold: *threshold,
        },
        ComputeSpec::RegexMatch { inputs, pattern } => ComputeSpec::RegexMatch {
            inputs: inputs.iter().map(map_input).collect::<Result<_, _>>()?,
            pattern: pattern.clone(),
        },
    })
}

/// 模板占位符改写：扫描 `{token}`，逐 token 分类替换。
/// iter0 的 `{prev.X}` 整体消除（占位符消除 → 模板成为静态文本，注记一·1）；
/// 未知 token 原样保留（v1.0 兼容：render_task 对无结果占位符的行为不变）。
fn rewrite_template(tmpl: &str, ctx: &RefCtx<'_>) -> Result<String, String> {
    let mut out = String::with_capacity(tmpl.len());
    let mut rest = tmpl;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let token = &after[..end];
                match classify(token, ctx)? {
                    Resolved::Name(n) => {
                        out.push('{');
                        out.push_str(&n);
                        out.push('}');
                    }
                    Resolved::Empty => {} // 占位符消除
                    Resolved::Keep => out.push_str(&rest[start..=start + end + 1]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// PlanFact 形态产出节点派生：展开后依赖图唯一汇点（出度 0）。
/// 多汇点/零汇点（零汇点仅可能因环，步骤 5 先行报错）即拒载。
fn derive_unique_sink(nodes: &[WorkflowNode]) -> Result<String, Vec<String>> {
    let mut has_out: HashSet<&str> = HashSet::new();
    for n in nodes {
        for d in &n.depends_on {
            has_out.insert(d.as_str());
        }
    }
    let sinks: Vec<&str> = nodes
        .iter()
        .map(|n| n.id.as_str())
        .filter(|id| !has_out.contains(id))
        .collect();
    if sinks.len() == 1 {
        return Ok(sinks[0].to_string());
    }
    Err(vec![format!(
        "PlanFact 形态无 output_node 字段：展开后依赖图须存在唯一汇点作为产出节点，实际 {} 个: {:?}\
         （实现新明确点：如需多产出请在计划中收敛到单一 finalize 节点）",
        sinks.len(),
        sinks
    )])
}

/// 步骤 5：展开后图自校验（总量/唯一性/prev. 残留/存在性/无环/层序/output_node）
fn self_check(nodes: &[WorkflowNode], output_node: &str, ir: &Ir) -> Result<(), Vec<String>> {
    if nodes.len() > MAX_EXPANDED_NODES {
        return Err(vec![format!(
            "展开后总节点数 {} 超硬上限 {MAX_EXPANDED_NODES}（schema 不可表达乘积，代码层兜底）",
            nodes.len()
        )]);
    }
    // 唯一性（副本名与全局节点名碰撞在此拒绝，报错指明冲突 id——交付物 5 §6-3）
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    for n in nodes {
        if !ids.insert(n.id.as_str()) {
            return Err(vec![format!(
                "展开后节点 id 冲突: '{}'（若为副本名与全局节点名碰撞——交付物 5 §6-3——\
                 请改全局节点名或 loop/体节点名以消除 _iter{{k}}_ 中缀歧义）",
                n.id
            )]);
        }
    }
    // prev. 残留断言（含模板文本扫描，交付物 5 §6-1）
    for n in nodes {
        if let Some(t) = &n.task_template {
            if t.contains("prev.") {
                return Err(vec![format!(
                    "物化 DAG 残留 prev. 形态：节点 '{}' 模板含 prev.（内部断言失败）",
                    n.id
                )]);
            }
        }
        if let Some(rw) = &n.run_when {
            if rw.node.starts_with("prev.") {
                return Err(vec![format!(
                    "物化 DAG 残留 prev. 形态：节点 '{}' run_when 观察 '{}'",
                    n.id, rw.node
                )]);
            }
        }
        if let Some(c) = &n.compute {
            let inputs = match c {
                ComputeSpec::Strcmp { inputs, .. }
                | ComputeSpec::NumericCmp { inputs, .. }
                | ComputeSpec::RegexMatch { inputs, .. } => inputs,
            };
            for i in inputs {
                if let ComputeInput::Node(t) = i {
                    if t.starts_with("prev.") {
                        return Err(vec![format!(
                            "物化 DAG 残留 prev. 形态：节点 '{}' compute 输入 '{t}'",
                            n.id
                        )]);
                    }
                }
            }
        }
    }
    // 引用存在性
    for n in nodes {
        for d in &n.depends_on {
            if !ids.contains(d.as_str()) {
                return Err(vec![format!(
                    "节点 '{}' depends_on 未知节点 '{}'（展开后引用存在性校验；\
                     若为循环体节点请写 R4 展开全名）",
                    n.id, d
                )]);
            }
        }
        if let Some(rw) = &n.run_when {
            if !rw.node.is_empty() && !ids.contains(rw.node.as_str()) {
                return Err(vec![format!(
                    "节点 '{}' run_when 引用未知节点 '{}'",
                    n.id, rw.node
                )]);
            }
        }
        if let Some(c) = &n.compute {
            let inputs = match c {
                ComputeSpec::Strcmp { inputs, .. }
                | ComputeSpec::NumericCmp { inputs, .. }
                | ComputeSpec::RegexMatch { inputs, .. } => inputs,
            };
            for i in inputs {
                if let ComputeInput::Node(t) = i {
                    if !ids.contains(t.as_str()) {
                        return Err(vec![format!(
                            "节点 '{}' compute 输入引用未知节点 '{t}'（交付物 4 §3.5 展开后统一校验）",
                            n.id
                        )]);
                    }
                }
            }
        }
    }
    // 拓扑分层（Kahn，与引擎同构）：无环断言 + compute/run_when 引用更早拓扑层
    let layers = kahn_layers(nodes)?;
    let mut layer_of: HashMap<&str, usize> = HashMap::new();
    for (i, layer) in layers.iter().enumerate() {
        for id in layer {
            layer_of.insert(id.as_str(), i);
        }
    }
    let earlier = |owner: &str, target: &str| -> Result<(), Vec<String>> {
        let (lo, lt) = match (layer_of.get(owner), layer_of.get(target)) {
            (Some(a), Some(b)) => (*a, *b),
            _ => return Ok(()), // 存在性已校验，防御性放行
        };
        if lt >= lo {
            return Err(vec![format!(
                "节点 '{owner}' 引用的 '{target}' 须位于更早拓扑层（同层/下游引用拒绝，\
                 交付物 4 §3.4 / check_run_when_layers 同族）"
            )]);
        }
        Ok(())
    };
    for n in nodes {
        if let Some(rw) = &n.run_when {
            if !rw.node.is_empty() {
                earlier(&n.id, &rw.node)?;
            }
        }
        if let Some(c) = &n.compute {
            let inputs = match c {
                ComputeSpec::Strcmp { inputs, .. }
                | ComputeSpec::NumericCmp { inputs, .. }
                | ComputeSpec::RegexMatch { inputs, .. } => inputs,
            };
            for i in inputs {
                if let ComputeInput::Node(t) = i {
                    earlier(&n.id, t)?;
                }
            }
        }
    }
    // output_node 存在性
    if !ids.contains(output_node) {
        return Err(vec![format!(
            "workflow '{}' output_node '{}' 不存在于展开后节点集",
            ir.workflow_id, output_node
        )]);
    }
    Ok(())
}

/// Kahn 分层（与 workflow.rs 引擎同构；确定性：按展开序选层）
fn kahn_layers(nodes: &[WorkflowNode]) -> Result<Vec<Vec<String>>, Vec<String>> {
    let mut processed: HashSet<String> = HashSet::new();
    let mut layers: Vec<Vec<String>> = Vec::new();
    loop {
        let layer: Vec<String> = nodes
            .iter()
            .filter(|n| !processed.contains(n.id.as_str()))
            .filter(|n| n.depends_on.iter().all(|d| processed.contains(d.as_str())))
            .map(|n| n.id.clone())
            .collect();
        if layer.is_empty() {
            break;
        }
        for id in &layer {
            processed.insert(id.clone());
        }
        layers.push(layer);
    }
    if processed.len() != nodes.len() {
        let cycle: Vec<&str> = nodes
            .iter()
            .map(|n| n.id.as_str())
            .filter(|id| !processed.contains(*id))
            .collect();
        return Err(vec![format!(
            "物化后检测到环（防御性断言，迭代单向链应保证无环）; 节点: {cycle:?}"
        )]);
    }
    Ok(layers)
}

// ============================================================================
// 测试（三 golden = 交付物 5 §4 推演表；R5/R2/R4 族用例按交付物 9 归属实现）
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::workflow::{ConditionOp, StrcmpMode};

    // ----- R5-T01 / golden 示例 1：简单线性（无 loop，PlanFact 形态）-----

    #[test]
    fn golden_example_1_linear_plan_fact() {
        let plan = serde_json::json!({
            "plan_version": 1, "parent_plan_hash": null, "plan_source": "initial_planning",
            "materializer_version": "1.0.0",
            "nodes": [
                { "id": "fetch",    "type": "llm", "agent_type": "researcher", "task": "获取并整理输入材料" },
                { "id": "analyze",  "type": "llm", "agent_type": "analyst",
                  "task_template": "分析以下材料：\n{fetch}", "input_refs": ["fetch"] },
                { "id": "report",   "type": "llm", "agent_type": "writer",
                  "task_template": "撰写报告：\n{analyze}", "input_refs": ["analyze"] }
            ],
            "edges": [ ["fetch", "analyze"], ["analyze", "report"] ]
        });
        let wf = materialize_plan_fact(&plan, "golden_linear").expect("线性计划应物化成功");
        assert_eq!(wf.workflow_id, "golden_linear");
        assert_eq!(wf.nodes.len(), 3);
        assert_eq!(wf.nodes[0].id, "fetch");
        assert_eq!(wf.nodes[0].depends_on, Vec::<String>::new());
        assert_eq!(wf.nodes[1].id, "analyze");
        assert_eq!(wf.nodes[1].depends_on, vec!["fetch".to_string()]);
        assert_eq!(
            wf.nodes[1].task_template.as_deref(),
            Some("分析以下材料：\n{fetch}")
        );
        assert_eq!(wf.nodes[2].id, "report");
        assert_eq!(wf.nodes[2].depends_on, vec!["analyze".to_string()]);
        // 核对点：无 loop 时物化 = 归一化 + 恒等透传；output_node 按唯一汇点派生
        assert_eq!(wf.output_node, "report");
    }

    // ----- R5-T01 / golden 示例 2：单 loop（bounded_refine 同构，手写 DSL 形态）-----

    fn bounded_refine_doc() -> serde_json::Value {
        serde_json::json!({
            "workflow_id": "bounded_refine",
            "description": "准备 → 最多 3 轮（搜索→提炼→收敛检查）→ 定稿",
            "nodes": [
                { "id": "prepare", "agent_type": "planner", "task": "明确调研主题与检索词" },
                { "id": "finalize", "agent_type": "writer",
                  "task_template": "基于最终提炼结果撰写报告：\n{research_iter2_refine}",
                  "depends_on": ["research_iter2_refine"] }
            ],
            "loops": [
                { "id": "research", "max_iterations": 3, "body": [
                    { "id": "search", "agent_type": "researcher",
                      "task_template": "围绕主题搜集资料：\n{prepare}\n上一轮提炼（首轮无）：{prev.refine}",
                      "run_when": { "node": "prev.check", "op": "not_contains", "value": "equal" } },
                    { "id": "refine", "agent_type": "analyst",
                      "task_template": "提炼本轮搜索结果为要点：\n{search}",
                      "depends_on": ["search"] },
                    { "id": "check",
                      "compute": { "function": "strcmp", "inputs": ["refine", "prev.refine"], "mode": "equal" },
                      "depends_on": ["refine"] }
                ] }
            ],
            "output_node": "finalize"
        })
    }

    #[test]
    fn golden_example_2_bounded_refine() {
        let wf =
            materialize_workflow_dag(&bounded_refine_doc()).expect("bounded_refine 应物化成功");

        assert_eq!(wf.nodes.len(), 11); // 9 副本 + 2 全局
        let by_id = |id: &str| -> WorkflowNode {
            wf.nodes
                .iter()
                .find(|n| n.id == id)
                .unwrap_or_else(|| panic!("缺节点 {id}"))
                .clone()
        };

        // iter0_search：run_when 空观察源 + prev 占位符消除（注记一·1/·3）
        let s0 = by_id("research_iter0_search");
        let rw = s0
            .run_when
            .as_ref()
            .expect("iter0_search 保留 run_when 声明");
        assert_eq!(rw.node, "", "iter0 prev.check → 空观察源");
        assert_eq!(rw.op, ConditionOp::NotContains);
        assert_eq!(
            s0.task_template.as_deref(),
            Some("围绕主题搜集资料：\n{prepare}\n上一轮提炼（首轮无）："),
            "iter0 prev 占位符消除，prepare(R3) 原名保留"
        );
        assert!(s0.depends_on.is_empty());

        // iter0_refine：R1 同迭代改写
        let r0 = by_id("research_iter0_refine");
        assert_eq!(r0.depends_on, vec!["research_iter0_search".to_string()]);
        assert_eq!(
            r0.task_template.as_deref(),
            Some("提炼本轮搜索结果为要点：\n{research_iter0_search}")
        );

        // iter0_check：inputs [refine, prev.refine] → [副本名, Empty]（注记一·2）
        let c0 = by_id("research_iter0_check");
        match &c0.compute {
            Some(ComputeSpec::Strcmp { inputs, mode }) => {
                assert_eq!(*mode, StrcmpMode::Equal);
                assert_eq!(
                    inputs,
                    &vec![
                        ComputeInput::Node("research_iter0_refine".to_string()),
                        ComputeInput::Empty
                    ]
                );
            }
            other => panic!("iter0_check compute 形态错误: {other:?}"),
        }
        assert_eq!(c0.depends_on, vec!["research_iter0_refine".to_string()]);

        // iter1_search：R2 → 上一迭代副本全名；R1 不误绑 k-1（R5-T02）
        let s1 = by_id("research_iter1_search");
        assert_eq!(
            s1.run_when.as_ref().expect("rw").node,
            "research_iter0_check"
        );
        assert!(s1
            .task_template
            .as_deref()
            .unwrap_or("")
            .contains("{research_iter0_refine}"));
        assert!(s1
            .task_template
            .as_deref()
            .unwrap_or("")
            .contains("{prepare}"));
        // 隐式全连：iter1 各副本 ∪= iter0 全部副本
        assert_eq!(
            s1.depends_on,
            vec![
                "research_iter0_search".to_string(),
                "research_iter0_refine".to_string(),
                "research_iter0_check".to_string()
            ]
        );

        // iter1_refine：显式边在前（R1 iter1_search），隐式全连在后（契约推演表第 3 步）
        let r1 = by_id("research_iter1_refine");
        assert_eq!(
            r1.depends_on,
            vec![
                "research_iter1_search".to_string(),
                "research_iter0_search".to_string(),
                "research_iter0_refine".to_string(),
                "research_iter0_check".to_string()
            ]
        );
        assert_eq!(
            r1.task_template.as_deref(),
            Some("提炼本轮搜索结果为要点：\n{research_iter1_search}"),
            "R1 绑定当前迭代，不得误绑 k-1"
        );

        // iter1_check：inputs → [iter1_refine, iter0_refine]
        let c1 = by_id("research_iter1_check");
        match &c1.compute {
            Some(ComputeSpec::Strcmp { inputs, .. }) => {
                assert_eq!(
                    inputs,
                    &vec![
                        ComputeInput::Node("research_iter1_refine".to_string()),
                        ComputeInput::Node("research_iter0_refine".to_string())
                    ]
                );
            }
            other => panic!("iter1_check compute 形态错误: {other:?}"),
        }

        // finalize：R4 全名原样透传；经全连隐式边传递闭包覆盖整个循环
        let f = by_id("finalize");
        assert_eq!(f.depends_on, vec!["research_iter2_refine".to_string()]);
        assert_eq!(
            f.task_template.as_deref(),
            Some("基于最终提炼结果撰写报告：\n{research_iter2_refine}")
        );

        // 物化 DAG 无 prev. 残留
        for n in &wf.nodes {
            if let Some(t) = &n.task_template {
                assert!(!t.contains("prev."), "节点 {} 模板残留 prev.", n.id);
            }
        }
        assert_eq!(wf.output_node, "finalize");
    }

    // ----- R5-T01 / golden 示例 3：双 loop 链式 + R4 跨循环（手写 DSL 形态）-----

    #[test]
    fn golden_example_3_dual_loop() {
        let doc = serde_json::json!({
            "workflow_id": "dual_loop",
            "nodes": [
                { "id": "gather", "agent_type": "researcher", "task": "收集原始清单" },
                { "id": "bridge", "agent_type": "analyst",
                  "task_template": "汇总两轮：\n{scan_iter0_pick}\n{scan_iter1_pick}",
                  "depends_on": ["scan_iter0_pick", "scan_iter1_pick"] }
            ],
            "loops": [
                { "id": "scan", "max_iterations": 2, "body": [
                    { "id": "pick", "agent_type": "researcher",
                      "task_template": "第 k 轮挑重点：\n{gather}\n上一轮（首轮无）：\n{prev.pick}" }
                ] },
                { "id": "verify", "max_iterations": 2, "body": [
                    { "id": "check", "agent_type": "verifier",
                      "task_template": "核验汇总与末轮重点：\n{bridge}\n{scan_iter1_pick}",
                      "depends_on": ["bridge"] }
                ] }
            ],
            "output_node": "verify_iter1_check"
        });
        let wf = materialize_workflow_dag(&doc).expect("双 loop 应物化成功");
        assert_eq!(wf.nodes.len(), 6);
        let by_id = |id: &str| -> WorkflowNode {
            wf.nodes
                .iter()
                .find(|n| n.id == id)
                .unwrap_or_else(|| panic!("缺节点 {id}"))
                .clone()
        };

        let p0 = by_id("scan_iter0_pick");
        assert!(p0.depends_on.is_empty());
        assert_eq!(
            p0.task_template.as_deref(),
            Some("第 k 轮挑重点：\n{gather}\n上一轮（首轮无）：\n"),
            "iter0 prev.pick 占位符消除"
        );

        let p1 = by_id("scan_iter1_pick");
        assert_eq!(
            p1.task_template.as_deref(),
            Some("第 k 轮挑重点：\n{gather}\n上一轮（首轮无）：\n{scan_iter0_pick}")
        );
        assert_eq!(
            p1.depends_on,
            vec!["scan_iter0_pick".to_string()],
            "隐式全连"
        );

        let b = by_id("bridge");
        assert_eq!(
            b.depends_on,
            vec!["scan_iter0_pick".to_string(), "scan_iter1_pick".to_string()],
            "R4 全名原样透传（k=0,1 均 < 2）"
        );

        let v0 = by_id("verify_iter0_check");
        assert_eq!(v0.depends_on, vec!["bridge".to_string()]);
        assert_eq!(
            v0.task_template.as_deref(),
            Some("核验汇总与末轮重点：\n{bridge}\n{scan_iter1_pick}"),
            "loopB 体内混合 R3（bridge）与 R4（scan_iter1_pick）"
        );

        let v1 = by_id("verify_iter1_check");
        assert_eq!(
            v1.depends_on,
            vec!["bridge".to_string(), "verify_iter0_check".to_string()],
            "显式 bridge 在前，隐式 iter0 副本在后"
        );
        assert_eq!(wf.output_node, "verify_iter1_check");
    }

    // ----- R4-T01：同输入同 DAG（字节级确定性）-----

    #[test]
    fn determinism_same_input_same_dag_bytes() {
        let a = materialize_workflow_dag(&bounded_refine_doc()).expect("ok");
        let b = materialize_workflow_dag(&bounded_refine_doc()).expect("ok");
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap(),
            "同输入两次物化须逐字节一致（含节点序/边序/run_when）"
        );
    }

    // ----- R2-T02：限额反例（步骤 0）-----

    fn minimal_loop_doc(loops: serde_json::Value, extra_globals: usize) -> serde_json::Value {
        let mut globals = vec![serde_json::json!({ "id": "out", "agent_type": "w", "task": "t" })];
        for i in 0..extra_globals {
            globals
                .push(serde_json::json!({ "id": format!("g{i}"), "agent_type": "w", "task": "t" }));
        }
        serde_json::json!({
            "workflow_id": "limits",
            "nodes": globals,
            "loops": loops,
            "output_node": "out"
        })
    }

    #[test]
    fn step0_limit_rejections() {
        // loops ≤ 8
        let nine: Vec<_> = (0..9)
            .map(|i| {
                serde_json::json!({ "id": format!("l{i}"), "max_iterations": 1,
                    "body": [{ "id": "b", "agent_type": "w", "task": "t" }] })
            })
            .collect();
        let errs = materialize_workflow_dag(&minimal_loop_doc(serde_json::json!(nine), 0))
            .expect_err("9 loops 须拒");
        assert!(errs[0].contains("loops 数量"), "{errs:?}");
        // body ≤ 8
        let big_body: Vec<_> = (0..9)
            .map(|i| serde_json::json!({ "id": format!("b{i}"), "agent_type": "w", "task": "t" }))
            .collect();
        let errs = materialize_workflow_dag(&minimal_loop_doc(
            serde_json::json!([{ "id": "l", "max_iterations": 1, "body": big_body }]),
            0,
        ))
        .expect_err("body 9 须拒");
        assert!(errs[0].contains("body 大小"), "{errs:?}");
        // max_iterations ≤ 32
        let errs = materialize_workflow_dag(&minimal_loop_doc(
            serde_json::json!([{ "id": "l", "max_iterations": 33,
                "body": [{ "id": "b", "agent_type": "w", "task": "t" }] }]),
            0,
        ))
        .expect_err("max_iterations 33 须拒");
        assert!(errs[0].contains("max_iterations"), "{errs:?}");
        // 展开前总量 ≤ 64
        let errs = materialize_workflow_dag(&minimal_loop_doc(
            serde_json::json!([{ "id": "l", "max_iterations": 1,
                "body": [{ "id": "b", "agent_type": "w", "task": "t" }] }]),
            63,
        ))
        .expect_err("64 全局 + 1 体节点 = 65 须拒");
        assert!(errs[0].contains("展开前节点总量"), "{errs:?}");
        // loop.id 与节点 id 冲突（A6）
        let doc = serde_json::json!({
            "workflow_id": "w",
            "nodes": [{ "id": "clash", "agent_type": "w", "task": "t" }],
            "loops": [{ "id": "clash", "max_iterations": 1,
                "body": [{ "id": "b", "agent_type": "w", "task": "t" }] }],
            "output_node": "clash"
        });
        let errs = materialize_workflow_dag(&doc).expect_err("loop.id 冲突须拒");
        assert!(errs[0].contains("冲突"), "{errs:?}");
    }

    // ----- R2-T03：展开总量 512 拦截 -----

    #[test]
    fn expanded_512_boundary() {
        // 恰 512：L1 8×32=256 + L2 7×32=224 + L3 1×31=31 + 1 全局 = 512
        let l1: Vec<_> = (0..8)
            .map(|i| serde_json::json!({ "id": format!("a{i}"), "agent_type": "w", "task": "t" }))
            .collect();
        let l2: Vec<_> = (0..7)
            .map(|i| serde_json::json!({ "id": format!("b{i}"), "agent_type": "w", "task": "t" }))
            .collect();
        let l3 = serde_json::json!([{ "id": "c", "agent_type": "w", "task": "t" }]);
        let ok_doc = serde_json::json!({
            "workflow_id": "boundary",
            "nodes": [{ "id": "out", "agent_type": "w", "task": "t" }],
            "loops": [
                { "id": "l1", "max_iterations": 32, "body": l1 },
                { "id": "l2", "max_iterations": 32, "body": l2 },
                { "id": "l3", "max_iterations": 31, "body": l3 }
            ],
            "output_node": "out"
        });
        let wf = materialize_workflow_dag(&ok_doc).expect("恰 512 须放行");
        assert_eq!(wf.nodes.len(), 512);
        // 再加一节点 → 513 → 拒
        let over_doc = serde_json::json!({
            "workflow_id": "boundary",
            "nodes": [
                { "id": "out", "agent_type": "w", "task": "t" },
                { "id": "extra", "agent_type": "w", "task": "t" }
            ],
            "loops": [
                { "id": "l1", "max_iterations": 32, "body": l1 },
                { "id": "l2", "max_iterations": 32, "body": l2 },
                { "id": "l3", "max_iterations": 31, "body": l3 }
            ],
            "output_node": "out"
        });
        let errs = materialize_workflow_dag(&over_doc).expect_err("513 须拒");
        assert!(errs[0].contains("512"), "{errs:?}");
    }

    // ----- R5-T04：副本名与全局节点名碰撞 -----

    #[test]
    fn copy_name_collision_rejected_with_pair() {
        let doc = serde_json::json!({
            "workflow_id": "collide",
            "nodes": [{ "id": "research_iter0_search", "agent_type": "w", "task": "t" }],
            "loops": [{ "id": "research", "max_iterations": 2,
                "body": [{ "id": "search", "agent_type": "w", "task": "t" }] }],
            "output_node": "research_iter0_search"
        });
        let errs = materialize_workflow_dag(&doc).expect_err("碰撞须拒");
        assert!(errs[0].contains("research_iter0_search"), "{errs:?}");
        assert!(errs[0].contains("冲突"), "{errs:?}");
    }

    // ----- R5-T05：循环外原名引用循环体节点（A10 友好报错）-----

    #[test]
    fn a10_original_name_reference_rejected_with_r4_hint() {
        let doc = serde_json::json!({
            "workflow_id": "a10",
            "nodes": [
                { "id": "fin", "agent_type": "w", "task": "t", "depends_on": ["refine"] }
            ],
            "loops": [{ "id": "research", "max_iterations": 2, "body": [
                { "id": "refine", "agent_type": "w", "task": "t" }
            ] }],
            "output_node": "fin"
        });
        let errs = materialize_workflow_dag(&doc).expect_err("A10 须拒");
        assert!(
            errs[0].contains("展开全名"),
            "报错须给 R4 写法指引: {errs:?}"
        );
        assert!(errs[0].contains("research"), "{errs:?}");
    }

    // ----- A3：循环体外 prev. / A2：depends_on 承载 prev. -----

    #[test]
    fn prev_outside_loop_rejected() {
        let doc = serde_json::json!({
            "workflow_id": "a3",
            "nodes": [
                { "id": "fin", "agent_type": "w", "task_template": "x {prev.refine}" }
            ],
            "loops": [{ "id": "research", "max_iterations": 2, "body": [
                { "id": "refine", "agent_type": "w", "task": "t" }
            ] }],
            "output_node": "fin"
        });
        let errs = materialize_workflow_dag(&doc).expect_err("A3 须拒");
        assert!(
            errs[0].contains("prev.") && errs[0].contains("循环体内"),
            "{errs:?}"
        );

        let doc2 = serde_json::json!({
            "workflow_id": "a2",
            "nodes": [{ "id": "g", "agent_type": "w", "task": "t" }],
            "loops": [{ "id": "research", "max_iterations": 2, "body": [
                { "id": "b", "agent_type": "w", "task": "t", "depends_on": ["prev.b"] }
            ] }],
            "output_node": "g"
        });
        let errs = materialize_workflow_dag(&doc2).expect_err("A2 须拒");
        assert!(errs[0].contains("depends_on 禁止 prev."), "{errs:?}");
    }

    // ----- A4：R4 越界 -----

    #[test]
    fn r4_out_of_bounds_rejected() {
        let doc = serde_json::json!({
            "workflow_id": "a4",
            "nodes": [
                { "id": "fin", "agent_type": "w", "task": "t",
                  "depends_on": ["research_iter3_refine"] }
            ],
            "loops": [{ "id": "research", "max_iterations": 3, "body": [
                { "id": "refine", "agent_type": "w", "task": "t" }
            ] }],
            "output_node": "fin"
        });
        let errs = materialize_workflow_dag(&doc).expect_err("R4 k=3 >= max 3 须拒");
        assert!(errs[0].contains("越界"), "{errs:?}");
    }

    // ----- R5-T03：展开后层校验（compute 输入引用更晚拓扑层 → 拒）-----

    #[test]
    fn compute_input_layer_violation_rejected() {
        // loop a body [x, y]：iter0 的 y 经 R4 引用 a_iter1_x（更晚层）→ 层校验拒
        let doc = serde_json::json!({
            "workflow_id": "layer",
            "nodes": [{ "id": "out", "agent_type": "w", "task": "t",
                        "depends_on": ["a_iter1_y"] }],
            "loops": [{ "id": "a", "max_iterations": 2, "body": [
                { "id": "x", "agent_type": "w", "task": "t" },
                { "id": "y",
                  "compute": { "function": "strcmp", "inputs": ["a_iter1_x", "x"], "mode": "equal" },
                  "depends_on": ["x"] }
            ] }],
            "output_node": "out"
        });
        let errs = materialize_workflow_dag(&doc).expect_err("同层/下游引用须拒");
        assert!(errs[0].contains("更早拓扑层"), "{errs:?}");
    }

    // ----- regex 加载期校验（交付物 4 §4.3）-----

    #[test]
    fn invalid_regex_pattern_rejected_at_load() {
        let doc = serde_json::json!({
            "workflow_id": "re",
            "nodes": [
                { "id": "g", "agent_type": "w", "task": "t" },
                { "id": "c", "compute": { "function": "regex_match",
                    "inputs": ["g"], "pattern": "(" } }
            ],
            "output_node": "c"
        });
        let errs = materialize_workflow_dag(&doc).expect_err("非法 pattern 须拒载");
        assert!(errs[0].contains("pattern 非法"), "{errs:?}");
    }

    // ----- PlanFact 唯一汇点派生（实现新明确点）-----

    #[test]
    fn plan_fact_rejects_planner_agent_type() {
        // C9（交付物 2 §6）：计划节点禁止引用 planner 自身（防自指递归）
        let plan = serde_json::json!({
            "nodes": [
                { "id": "a", "type": "llm", "agent_type": "planner", "task": "t" }
            ]
        });
        let errs = materialize_plan_fact(&plan, "wf").expect_err("C9 须拒");
        assert!(errs[0].contains("planner"), "{errs:?}");
    }

    #[test]
    fn plan_fact_requires_unique_sink() {
        let two_sinks = serde_json::json!({
            "nodes": [
                { "id": "a", "type": "llm", "agent_type": "w", "task": "t" },
                { "id": "b", "type": "llm", "agent_type": "w", "task": "t" }
            ]
        });
        let errs = materialize_plan_fact(&two_sinks, "wf").expect_err("双汇点须拒");
        assert!(errs[0].contains("唯一汇点"), "{errs:?}");

        let chained = serde_json::json!({
            "nodes": [
                { "id": "a", "type": "llm", "agent_type": "w", "task": "t" },
                { "id": "b", "type": "tool", "agent_type": "w", "tool": "file_read",
                  "task_template": "读 {a}" }
            ],
            "edges": [["a", "b"]]
        });
        let wf = materialize_plan_fact(&chained, "wf").expect("链式唯一汇点应通过");
        assert_eq!(wf.output_node, "b");
        assert_eq!(wf.nodes[1].depends_on, vec!["a".to_string()]);
        // J3：tool 节点物化为 agent 节点（agent 语义），tool 字段不进 DAG
        assert!(wf.nodes[1].compute.is_none());
    }

    // ----- PlanFact loop 体（type 判别形态）+ edges 指向体节点 -----

    #[test]
    fn plan_fact_loop_with_edges() {
        let plan = serde_json::json!({
            "nodes": [
                { "id": "g", "type": "llm", "agent_type": "w", "task": "t" },
                { "id": "fin", "type": "llm", "agent_type": "w", "task_template": "{scan_iter1_p}" }
            ],
            "edges": [["g", "p"], ["scan_iter1_p", "fin"]],
            "loops": [
                { "id": "scan", "max_iterations": 2, "body": [
                    { "id": "p", "type": "llm", "agent_type": "w",
                      "task_template": "轮次 {g} 上轮 {prev.p}" }
                ] }
            ]
        });
        let wf = materialize_plan_fact(&plan, "wf").expect("ok");
        assert_eq!(wf.nodes.len(), 4); // g, fin, scan_iter0_p, scan_iter1_p
        let p1 = wf.nodes.iter().find(|n| n.id == "scan_iter1_p").unwrap();
        assert_eq!(
            p1.task_template.as_deref(),
            Some("轮次 {g} 上轮 {scan_iter0_p}")
        );
        // edges [g, p] → 每个副本依赖 g；[scan_iter1_p, fin] → R4 名直用
        assert!(p1.depends_on.contains(&"g".to_string()));
        assert!(p1.depends_on.contains(&"scan_iter0_p".to_string()));
        let fin = wf.nodes.iter().find(|n| n.id == "fin").unwrap();
        assert_eq!(fin.depends_on, vec!["scan_iter1_p".to_string()]);
        assert_eq!(wf.output_node, "fin");
    }

    // ----- R6-T01 PT：物化器大规模展开计时（#[ignore] 手动跑，不进 CI）-----

    /// PT 基线（R6-T01，收官遗留 B5）：上限 512 节点展开 ×10 次计时。
    ///
    /// 定位 = 可观测性能基线（打印每次耗时供对账），非 CI 门禁——物化是
    /// 纯函数（无 IO/无墙钟依赖），耗时只随输入规模线性波动；粗上限断言
    /// 仅防数量级回归（如误加 O(n²) 全对全依赖）。手动跑：
    /// `cargo test -p evo-agent --lib pt_materialize_512 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn pt_materialize_512_boundary_timing() {
        let l1: Vec<_> = (0..8)
            .map(|i| serde_json::json!({ "id": format!("a{i}"), "agent_type": "w", "task": "t" }))
            .collect();
        let l2: Vec<_> = (0..7)
            .map(|i| serde_json::json!({ "id": format!("b{i}"), "agent_type": "w", "task": "t" }))
            .collect();
        let l3 = serde_json::json!([{ "id": "c", "agent_type": "w", "task": "t" }]);
        let doc = serde_json::json!({
            "workflow_id": "pt_boundary",
            "nodes": [{ "id": "out", "agent_type": "w", "task": "t" }],
            "loops": [
                { "id": "l1", "max_iterations": 32, "body": l1 },
                { "id": "l2", "max_iterations": 32, "body": l2 },
                { "id": "l3", "max_iterations": 31, "body": l3 }
            ],
            "output_node": "out"
        });
        let runs = 10;
        let mut total = std::time::Duration::ZERO;
        for i in 0..runs {
            let t0 = std::time::Instant::now();
            let wf = materialize_workflow_dag(&doc).expect("512 展开须成功");
            let dt = t0.elapsed();
            assert_eq!(wf.nodes.len(), 512, "run {i}: 展开数须恰 512");
            println!("PT run {i}: {} nodes in {:?}", wf.nodes.len(), dt);
            total += dt;
        }
        let avg = total / runs;
        println!("PT avg over {runs} runs: {avg:?}");
        // 数量级防回归（512 节点展开应为毫秒级；此阈值按环境宽放 100 倍余量）
        assert!(
            avg < std::time::Duration::from_secs(10),
            "物化 512 节点平均耗时 {avg:?} 异常（疑似复杂度回归）"
        );
    }
}
