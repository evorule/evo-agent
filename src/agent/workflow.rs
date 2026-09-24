// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G9:工作流引擎 —— DAG 拓扑编排多 agent
//!
//! ## 设计
//!
//! 工作流是一个 **DAG**(有向无环图),用 JSON DSL 定义:
//!
//! ```json
//! {
//!   "workflow_id": "research_and_write",
//!   "nodes": [
//!     { "id": "research_rust", "agent_type": "researcher", "task": "...", "depends_on": [] },
//!     { "id": "write_report",  "agent_type": "writer",     "task_template": "基于: {research_rust}", "depends_on": ["research_rust"] }
//!   ],
//!   "output_node": "write_report"
//! }
//! ```
//!
//! ## 执行算法
//!
//! 1. **拓扑排序**(Kahn 分层):按 `depends_on` 把节点分成若干层,同层无互相依赖
//! 2. **逐层规划**:先决定本层哪些节点执行、哪些跳过——
//!    - 节点声明了 `run_when`(workflow_dag v1.1 条件分支)→ 按条件求值决定去留
//!      (豁免级联:即使上游被跳过,条件为真仍执行)
//!    - 未声明 `run_when` 的节点:任一直接依赖被跳过 → 级联跳过
//! 3. **逐层执行**:同层待执行节点并行(`DelegateContext::delegate_parallel`)
//! 4. **模板渲染**:下一层的 `task_template` 中 `{node_id}` 被上游结果替换;
//!    被跳过的上游节点占位符替换为空字符串
//! 5. 任一**执行中**节点失败 → 整个工作流终止,返回 Err(被跳过的节点不算失败)
//! 6. 返回 `output_node` 的结果;`output_node` 被跳过 → 返回 Err(无静默空结果)
//!
//! ## 边界(§9.6)
//!
//! - 条件分支为**节点级静态条件**(v1.1 `run_when`,纯函数求值,谓词最小集
//!   `contains`/`equals`/`not_contains`);动态 plan-and-execute 循环不在本引擎范围
//! - 节点失败默认终止整个工作流(无 `on_failure: skip`)
//! - `run_when` 引用的节点被跳过/无结果时视作空字符串(确定性语义,同输入必同输出)

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::agent::delegate::DelegateContext;

/// 条件谓词(workflow_dag v1.1 `run_when.op` 最小集,冻结于该版本)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOp {
    /// 上游结果包含 `value`
    Contains,
    /// 上游结果完全等于 `value`
    Equals,
    /// 上游结果不包含 `value`
    NotContains,
}

/// 节点级执行条件(workflow_dag v1.1 新增可选字段 `run_when`)
///
/// 语义(纯函数,同输入必同输出):
/// - 被观察节点(`node`)的结果取自已完成结果表;该节点被跳过或尚无结果时视作**空字符串**
/// - 求值为真 → 执行本节点;为假 → 跳过本节点
/// - 声明了 `run_when` 的节点**豁免级联跳过**(去留完全由自身条件决定);
///   未声明的节点在任一直接依赖被跳过时级联跳过
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunWhen {
    /// 被观察节点的 id(必须存在于工作流,且位于本节点的更早拓扑层)
    pub node: String,
    /// 比较谓词
    pub op: ConditionOp,
    /// 期望值(与被观察节点的结果字符串比较)
    pub value: String,
}

/// compute 节点输入引用（workflow_dag v1.2 / 交付物 4 §3；交付物 5 注记一·2）
///
/// 源文档形态只有 `Node`（字符串引用）；`Empty` 为物化器改写产物——iter0 的
/// `prev.X` 输入消解为空输入字面量，执行期视作空字符串（与 run_when 空观察源
/// 同一语义：被引用节点无结果 → 空串）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ComputeInput {
    /// 节点引用（展开后 = 副本全名/全局原名）
    Node(String),
    /// 空输入字面量（物化器产物，源文档无此形态）
    Empty,
}

/// strcmp 比较模式（交付物 4 §4.1）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrcmpMode {
    /// 两输入完全相等（收敛检测主用；结果词表 equal|different）
    Equal,
    /// inputs[0] 包含 inputs[1]（结果词表 contained|not_contained）
    Contains,
}

/// numeric_cmp 比较模式（交付物 4 §4.2）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumericCmpMode {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

/// 纯函数节点声明（workflow_dag v1.2 / D-03 拍板方案③；交付物 4 实现契约）
///
/// 执行形态约束：不经 delegate（不占 max_concurrent/max_depth）、execute 层循环
/// 内同步内联求值、无 IO 无副作用、不产生 IoRequest；函数目录封闭
/// （strcmp/numeric_cmp/regex_match），同输入必同输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "function", rename_all = "snake_case")]
pub enum ComputeSpec {
    /// 字符串比较（恰 2 输入）
    Strcmp {
        inputs: Vec<ComputeInput>,
        mode: StrcmpMode,
    },
    /// 数值比较（IEEE 754 双精度；1..=2 输入，单输入必带 threshold、双输入禁带）
    NumericCmp {
        inputs: Vec<ComputeInput>,
        mode: NumericCmpMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threshold: Option<f64>,
    },
    /// 正则匹配（恰 1 输入；pattern 加载期校验，非法即拒载）
    RegexMatch {
        inputs: Vec<ComputeInput>,
        pattern: String,
    },
}

impl ComputeSpec {
    /// 全部输入引用（按声明序，只读取用）
    pub fn inputs(&self) -> &[ComputeInput] {
        match self {
            ComputeSpec::Strcmp { inputs, .. }
            | ComputeSpec::NumericCmp { inputs, .. }
            | ComputeSpec::RegexMatch { inputs, .. } => inputs,
        }
    }
}

/// 工作流节点
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowNode {
    /// 节点 id(工作流内唯一,被 `depends_on` / `task_template` / `output_node` 引用)
    pub id: String,
    /// 执行该节点的 agent 类型(对应 `agents/<agent_type>.json`);compute 节点无 agent 语义
    pub agent_type: String,
    /// 任务描述(静态文本,与 `task_template` 二选一)
    ///
    /// 若同时提供 `task` 和 `task_template`,优先用 `task_template`。
    #[serde(default)]
    pub task: String,
    /// 任务模板(可含 `{node_id}` 占位符,被上游节点结果替换)
    ///
    /// 例:`"基于以下调研写报告:\nRust: {research_rust}\nPython: {research_python}"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_template: Option<String>,
    /// 依赖的节点 id 列表(必须全部完成后本节点才能执行)
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 执行条件(可选,workflow_dag v1.1):求值为假 → 跳过本节点;不写 = v1.0 现行为
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_when: Option<RunWhen>,
    /// 纯函数节点声明(可选,workflow_dag v1.2 / D-03):存在时本节点为 compute 节点,
    /// 禁止 agent_type/task/task_template(execute 层循环内同步内联求值,不走 delegate)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeSpec>,
}

/// 工作流定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    /// 工作流 id(用于日志/加载)
    pub workflow_id: String,
    /// 人类可读描述
    #[serde(default)]
    pub description: String,
    /// 节点列表
    pub nodes: Vec<WorkflowNode>,
    /// 输出节点 id(其结果作为整个工作流的返回值)
    pub output_node: String,
}

/// 工作流引擎
///
/// 持有 [`DelegateContext`],负责拓扑排序 + 并行执行 + 模板渲染。
#[derive(Debug, Clone)]
pub struct WorkflowEngine {
    ctx: DelegateContext,
}

impl WorkflowEngine {
    /// 创建工作流引擎
    pub fn new(ctx: DelegateContext) -> Self {
        Self { ctx }
    }

    /// 只读访问内部 `DelegateContext`
    pub fn delegate_context(&self) -> &DelegateContext {
        &self.ctx
    }

    /// 执行工作流
    ///
    /// # 算法
    ///
    /// 1. 校验(`output_node` 存在、无重复 id、依赖合法、无环、`run_when` 引用合法)
    /// 2. 拓扑排序成分层结构
    /// 3. 逐层规划:按 `run_when` / 级联规则决定本层执行与跳过(见 [`plan_layer`])
    /// 4. 本层待执行节点并行执行(`delegate_parallel`)
    /// 5. 上游结果填入下游 `task_template`(被跳过的上游占位符替换为空字符串)
    /// 6. 返回 `output_node` 的结果
    ///
    /// # 错误
    ///
    /// - `output_node` 不存在
    /// - 节点 id 重复
    /// - `depends_on` 引用不存在的节点
    /// - 自依赖
    /// - 检测到环
    /// - `run_when` 引用不存在节点 / 自身 / 同层或下游节点
    /// - 任一**执行中**节点失败(终止整个工作流;被跳过的节点不算失败)
    /// - `output_node` 被跳过(无静默空结果)
    pub async fn execute(&self, wf: &Workflow) -> Result<String, String> {
        // 1. 校验
        self.validate(wf)?;

        // 2. 拓扑排序
        let layers = self.topological_sort(&wf.nodes)?;

        // 2.5 run_when 上游层检查:被观察节点必须位于更早拓扑层,
        //     保证条件求值时其结果已就绪(同层并行节点的结果在规划期不可得)
        Self::check_run_when_layers(wf, &layers)?;

        // 2.6 compute inputs 层序检查(交付物 4 §3.4,复用同族校验):
        //     输入仅可引用更早拓扑层节点,自引用/同层/下游引用一律拒绝
        Self::check_compute_layers(wf, &layers)?;

        // 3. 逐层规划 + 执行
        let mut results: BTreeMap<String, String> = BTreeMap::new();
        let mut skipped: HashSet<String> = HashSet::new();
        for (layer_idx, layer) in layers.iter().enumerate() {
            let (to_run, newly_skipped) = plan_layer(layer, &results, &skipped);
            for id in &newly_skipped {
                tracing::info!(
                    workflow_id = %wf.workflow_id,
                    layer = layer_idx,
                    node_id = %id,
                    "workflow node skipped"
                );
            }
            skipped.extend(newly_skipped);

            if to_run.is_empty() {
                continue;
            }

            // compute 节点同步内联求值(交付物 4 §5.1,不经 delegate:不占并发槽、
            // 不耗 max_depth、无 IoRequest;同层按层内序依次求值,纯函数微秒级)
            // LLM/工具节点照常并行(delegate_parallel,零改动)
            let mut llm_nodes: Vec<&WorkflowNode> = Vec::with_capacity(to_run.len());
            for node in to_run {
                match &node.compute {
                    Some(spec) => {
                        let output = eval_compute(spec, &results)
                            .map_err(|e| format!("workflow node '{}' failed: {}", node.id, e))?;
                        tracing::info!(
                            workflow_id = %wf.workflow_id,
                            layer = layer_idx,
                            node_id = %node.id,
                            content_len = output.len(),
                            "workflow compute node evaluated"
                        );
                        results.insert(node.id.clone(), output);
                    }
                    None => llm_nodes.push(node),
                }
            }

            if llm_nodes.is_empty() {
                continue;
            }

            let tasks: Vec<(String, String)> = llm_nodes
                .iter()
                .map(|node| {
                    let task = self.render_task(node, &results, &skipped);
                    (node.agent_type.clone(), task)
                })
                .collect();

            tracing::info!(
                workflow_id = %wf.workflow_id,
                layer = layer_idx,
                node_count = llm_nodes.len(),
                "executing workflow layer"
            );

            let layer_results = self.ctx.delegate_parallel(tasks).await;

            for (node, result) in llm_nodes.iter().zip(layer_results.iter()) {
                match result {
                    Ok(content) => {
                        tracing::info!(
                            workflow_id = %wf.workflow_id,
                            node_id = %node.id,
                            content_len = content.len(),
                            "workflow node succeeded"
                        );
                        results.insert(node.id.clone(), content.clone());
                    }
                    Err(e) => {
                        tracing::warn!(
                            workflow_id = %wf.workflow_id,
                            node_id = %node.id,
                            error = %e,
                            "workflow node failed, aborting workflow"
                        );
                        return Err(format!("workflow node '{}' failed: {}", node.id, e));
                    }
                }
            }
        }

        // 4. 返回 output_node 结果
        results
            .get(&wf.output_node)
            .cloned()
            .ok_or_else(|| format!("output node '{}' produced no result", wf.output_node))
    }

    /// 校验工作流定义(不检查环,环在拓扑排序阶段检测)
    fn validate(&self, wf: &Workflow) -> Result<(), String> {
        if wf.nodes.is_empty() {
            return Err(format!("workflow '{}' has no nodes", wf.workflow_id));
        }

        let mut id_set: HashSet<&str> = HashSet::new();
        for n in &wf.nodes {
            // 门卫(P2-M7 前置补丁):node id 必须是标识符 [A-Za-z0-9_-]+——
            // id 会被拼进 task_template 占位符 `{id}`,含 {} / 空格等字符会
            // 污染模板机制;强制白名单消除该隐性边角。
            if n.id.is_empty()
                || !n
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(format!(
                    "workflow '{}': node id '{}' is not a valid identifier ([A-Za-z0-9_-]+)",
                    wf.workflow_id, n.id
                ));
            }
            // 门卫(路径穿越防护):agent_type 最终会进入 AgentDefinition 加载器,
            // 此处提前拦截并给出工作流上下文的可读错误。
            // compute 节点无 agent 语义(D-03:不经 delegate),豁免本门卫。
            if n.compute.is_none() {
                if let Err(e) =
                    crate::agent::definition::AgentDefinition::validate_agent_type(&n.agent_type)
                {
                    return Err(format!("workflow '{}': {}", wf.workflow_id, e));
                }
            }
            if !id_set.insert(n.id.as_str()) {
                return Err(format!(
                    "workflow '{}' has duplicate node id: '{}'",
                    wf.workflow_id, n.id
                ));
            }
        }

        // output_node 必须存在
        if !id_set.contains(wf.output_node.as_str()) {
            return Err(format!(
                "workflow '{}' output_node '{}' not found in nodes",
                wf.workflow_id, wf.output_node
            ));
        }

        // 依赖合法性 + 自依赖 + run_when 引用合法性
        for n in &wf.nodes {
            for dep in &n.depends_on {
                if dep == &n.id {
                    return Err(format!(
                        "workflow '{}' node '{}' depends on itself",
                        wf.workflow_id, n.id
                    ));
                }
                if !id_set.contains(dep.as_str()) {
                    return Err(format!(
                        "workflow '{}' node '{}' depends on unknown node '{}'",
                        wf.workflow_id, n.id, dep
                    ));
                }
            }
            // run_when 引用合法性(层序检查在拓扑分层后进行,见 check_run_when_layers)
            if let Some(cond) = &n.run_when {
                if cond.node == n.id {
                    return Err(format!(
                        "workflow '{}' node '{}' run_when references itself",
                        wf.workflow_id, n.id
                    ));
                }
                // 空观察源豁免(交付物 5 注记一·3):物化器把 iter0 的 prev.X 观察
                // 消解为空观察源(node=""),执行期求值 results.get("") → None → 空串,
                // 与既有「被观察节点无结果 → 空串」语义天然一致;手写 DSL 路径
                // schema pattern 本就拒空串,不受影响。
                if cond.node.is_empty() {
                    continue;
                }
                if !id_set.contains(cond.node.as_str()) {
                    return Err(format!(
                        "workflow '{}' node '{}' run_when references unknown node '{}'",
                        wf.workflow_id, n.id, cond.node
                    ));
                }
            }
            // compute 形态校验(封闭目录代码层校验,交付物 4 §4.6 双保险):
            // 输入数量/threshold 互斥/pattern 编译/inputs 引用存在性
            if let Some(spec) = &n.compute {
                validate_compute_spec(&wf.workflow_id, &n.id, spec, &id_set)?;
            }
        }

        Ok(())
    }

    /// 渲染 `task_template`:把 `{node_id}` 替换为上游结果
    ///
    /// 无 `task_template` 时返回 `node.task`。上游结果未就绪时占位符保留
    /// (但拓扑排序保证执行时上游已完成,不会出现未就绪)。
    /// 被跳过的上游节点无结果:占位符替换为**空字符串**(可预测,不会把
    /// `{id}` 字面量漏进下游任务文本)。
    fn render_task(
        &self,
        node: &WorkflowNode,
        results: &BTreeMap<String, String>,
        skipped: &HashSet<String>,
    ) -> String {
        if let Some(tmpl) = &node.task_template {
            let mut task = tmpl.clone();
            for (id, content) in results {
                task = task.replace(&format!("{{{}}}", id), content);
            }
            for id in skipped {
                task = task.replace(&format!("{{{}}}", id), "");
            }
            task
        } else {
            node.task.clone()
        }
    }

    /// run_when 上游层检查:被观察节点必须位于本节点的**更早拓扑层**
    ///
    /// Kahn 分层保证更早层的节点在本层规划前已完成;同层并行节点的结果在
    /// 规划期不可得,下游节点同理。引用同层/下游节点几乎必然是定义错误,
    /// 在执行前 fail-fast。validate 已保证引用节点存在,此处查不到层视为内部错误。
    fn check_run_when_layers(wf: &Workflow, layers: &[Vec<&WorkflowNode>]) -> Result<(), String> {
        let mut layer_of: HashMap<&str, usize> = HashMap::new();
        for (i, layer) in layers.iter().enumerate() {
            for n in layer {
                layer_of.insert(n.id.as_str(), i);
            }
        }
        for node in &wf.nodes {
            if let Some(cond) = &node.run_when {
                // 空观察源豁免(交付物 5 注记一·3,同 validate):iter0 空观察源
                // 天然早于一切层,无需层序检查
                if cond.node.is_empty() {
                    continue;
                }
                let target = layer_of.get(cond.node.as_str()).copied().ok_or_else(|| {
                    format!(
                        "workflow '{}': node '{}' run_when references unknown node '{}'",
                        wf.workflow_id, node.id, cond.node
                    )
                })?;
                let own = layer_of
                    .get(node.id.as_str())
                    .copied()
                    .unwrap_or(usize::MAX);
                if target >= own {
                    return Err(format!(
                        "workflow '{}': node '{}' run_when must reference an upstream node \
                         from an earlier layer ('{}' is at same-or-later layer)",
                        wf.workflow_id, node.id, cond.node
                    ));
                }
            }
        }
        Ok(())
    }

    /// compute inputs 上游层检查(交付物 4 §3.4):输入引用必须位于本节点的
    /// **更早拓扑层**——保证求值时输入已就绪;与 `check_run_when_layers` 同族。
    /// validate 已保证引用存在,此处查不到层视为内部错误。
    fn check_compute_layers(wf: &Workflow, layers: &[Vec<&WorkflowNode>]) -> Result<(), String> {
        let mut layer_of: HashMap<&str, usize> = HashMap::new();
        for (i, layer) in layers.iter().enumerate() {
            for n in layer {
                layer_of.insert(n.id.as_str(), i);
            }
        }
        for node in &wf.nodes {
            let Some(spec) = &node.compute else {
                continue;
            };
            for input in spec.inputs() {
                let ComputeInput::Node(name) = input else {
                    continue; // Empty 为物化产物空输入字面量,无层序概念
                };
                let target = layer_of.get(name.as_str()).copied().ok_or_else(|| {
                    format!(
                        "workflow '{}': compute node '{}' references unknown node '{}'",
                        wf.workflow_id, node.id, name
                    )
                })?;
                let own = layer_of
                    .get(node.id.as_str())
                    .copied()
                    .unwrap_or(usize::MAX);
                if target >= own {
                    return Err(format!(
                        "workflow '{}': compute node '{}' input '{}' must reference an \
                         upstream node from an earlier layer",
                        wf.workflow_id, node.id, name
                    ));
                }
            }
        }
        Ok(())
    }

    /// 拓扑排序:返回按依赖层级分组的节点列表
    ///
    /// 同层节点无互相依赖,可并行执行。检测到环时返回 Err。
    ///
    /// 算法:Kahn 分层 —— 反复选出"所有依赖都已处理"的节点作为下一层,
    /// 直到全部处理完;若中途无进展但仍有未处理节点,说明存在环。
    fn topological_sort<'a>(
        &self,
        nodes: &'a [WorkflowNode],
    ) -> Result<Vec<Vec<&'a WorkflowNode>>, String> {
        let mut processed: HashSet<&str> = HashSet::new();
        let mut layers: Vec<Vec<&WorkflowNode>> = Vec::new();

        loop {
            // 选出:未处理 + 所有依赖均已处理
            let layer: Vec<&WorkflowNode> = nodes
                .iter()
                .filter(|n| !processed.contains(n.id.as_str()))
                .filter(|n| n.depends_on.iter().all(|d| processed.contains(d.as_str())))
                .collect();

            if layer.is_empty() {
                break;
            }

            for n in &layer {
                processed.insert(n.id.as_str());
            }
            layers.push(layer);
        }

        // 环检测:若有未处理节点,说明它们互相依赖成环
        if processed.len() != nodes.len() {
            let cycle_nodes: Vec<&str> = nodes
                .iter()
                .map(|n| n.id.as_str())
                .filter(|id| !processed.contains(*id))
                .collect();
            return Err(format!(
                "cycle detected in workflow; nodes involved: {:?}",
                cycle_nodes
            ));
        }

        Ok(layers)
    }
}

/// 条件求值(纯函数):同输入必同输出
///
/// 被观察节点无结果(被跳过/未完成)时视作**空字符串**——这是跳过语义的
/// 确定性根基:同一执行轨迹下,条件结果只依赖已完成结果表的内容。
fn eval_condition(cond: &RunWhen, results: &BTreeMap<String, String>) -> bool {
    let actual = results.get(&cond.node).map(String::as_str).unwrap_or("");
    match cond.op {
        ConditionOp::Contains => actual.contains(cond.value.as_str()),
        ConditionOp::Equals => actual == cond.value,
        ConditionOp::NotContains => !actual.contains(cond.value.as_str()),
    }
}

/// compute 输入解析(纯函数):Node 引用读 results 表——被引用节点被跳过/尚无结果
/// 时视作**空字符串**(与 run_when 既有语义逐字一致,交付物 4 §3.3);Empty 为
/// 物化器产物(iter0 的 prev.X 消解),执行期即空输入字面量
fn resolve_compute_input(input: &ComputeInput, results: &BTreeMap<String, String>) -> String {
    match input {
        ComputeInput::Node(name) => results
            .get(name)
            .map(String::as_str)
            .unwrap_or("")
            .to_string(),
        ComputeInput::Empty => String::new(),
    }
}

/// compute 纯函数求值(封闭目录三函数,交付物 4 §4;同输入必同输出)
///
/// 输入数量/threshold 互斥/pattern 合法性已在 validate 期拦截,此处兜底防御
/// (返回 Err 而非 panic);数值解析失败 = 节点失败 = 工作流终止(§4.5,无静默回退)。
/// 结果词表:strcmp→equal|different / contained|not_contained;
/// numeric_cmp→true|false;regex_match→match|no_match(§4.4 总表)。
fn eval_compute(spec: &ComputeSpec, results: &BTreeMap<String, String>) -> Result<String, String> {
    let resolved: Vec<String> = spec
        .inputs()
        .iter()
        .map(|i| resolve_compute_input(i, results))
        .collect();
    match spec {
        ComputeSpec::Strcmp { mode, .. } => {
            let [a, b] = resolved.as_slice() else {
                return Err(format!("strcmp 需恰 2 输入,实得 {}", resolved.len()));
            };
            Ok(match mode {
                StrcmpMode::Equal => {
                    if a == b {
                        "equal"
                    } else {
                        "different"
                    }
                }
                StrcmpMode::Contains => {
                    if a.contains(b.as_str()) {
                        "contained"
                    } else {
                        "not_contained"
                    }
                }
            }
            .to_string())
        }
        ComputeSpec::NumericCmp {
            mode, threshold, ..
        } => {
            let parse = |s: &str| {
                s.trim().parse::<f64>().map_err(|_| {
                    format!("numeric_cmp 输入 '{s}' 数值解析失败(节点失败,无静默回退)")
                })
            };
            let (result, _) = match resolved.as_slice() {
                [a, b] => {
                    if threshold.is_some() {
                        return Err("numeric_cmp 双输入形态禁止携带 threshold".to_string());
                    }
                    let (x, y) = (parse(a)?, parse(b)?);
                    (
                        match mode {
                            NumericCmpMode::Lt => x < y,
                            NumericCmpMode::Le => x <= y,
                            NumericCmpMode::Gt => x > y,
                            NumericCmpMode::Ge => x >= y,
                            NumericCmpMode::Eq => x == y,
                        },
                        y,
                    )
                }
                [a] => {
                    let t = threshold
                        .ok_or_else(|| "numeric_cmp 单输入形态必带 threshold".to_string())?;
                    let x = parse(a)?;
                    (
                        match mode {
                            NumericCmpMode::Lt => x < t,
                            NumericCmpMode::Le => x <= t,
                            NumericCmpMode::Gt => x > t,
                            NumericCmpMode::Ge => x >= t,
                            NumericCmpMode::Eq => x == t,
                        },
                        t,
                    )
                }
                other => return Err(format!("numeric_cmp 需 1..=2 输入,实得 {}", other.len())),
            };
            Ok(if result { "true" } else { "false" }.to_string())
        }
        ComputeSpec::RegexMatch { pattern, .. } => {
            let [a] = resolved.as_slice() else {
                return Err(format!("regex_match 需恰 1 输入,实得 {}", resolved.len()));
            };
            // pattern 合法性加载期已校验(validate_compute_spec),此处编译失败兜底
            let re = regex::Regex::new(pattern)
                .map_err(|e| format!("regex pattern '{pattern}' 编译失败: {e}"))?;
            Ok(if re.is_match(a) { "match" } else { "no_match" }.to_string())
        }
    }
}

/// compute 节点形态校验(封闭目录代码层校验,交付物 4 §4.6 双保险——schema oneOf
/// 已表达 + 本校验兜底):输入数量、threshold 互斥、pattern 编译、inputs 引用存在性
fn validate_compute_spec(
    wf_id: &str,
    node_id: &str,
    spec: &ComputeSpec,
    id_set: &HashSet<&str>,
) -> Result<(), String> {
    let check_inputs = |spec_inputs: &[ComputeInput], expect: &str| -> Result<(), String> {
        for i in spec_inputs {
            if let ComputeInput::Node(name) = i {
                if !id_set.contains(name.as_str()) {
                    return Err(format!(
                        "workflow '{wf_id}': compute node '{node_id}' input references unknown node '{name}'"
                    ));
                }
            }
        }
        if spec_inputs.is_empty() {
            return Err(format!(
                "workflow '{wf_id}': compute node '{node_id}' ({expect}) requires inputs"
            ));
        }
        Ok(())
    };
    match spec {
        ComputeSpec::Strcmp { inputs, .. } => {
            check_inputs(inputs, "strcmp")?;
            if inputs.len() != 2 {
                return Err(format!(
                    "workflow '{wf_id}': compute node '{node_id}' strcmp requires exactly 2 inputs, got {}",
                    inputs.len()
                ));
            }
        }
        ComputeSpec::NumericCmp {
            inputs, threshold, ..
        } => {
            check_inputs(inputs, "numeric_cmp")?;
            if inputs.len() > 2 {
                return Err(format!(
                    "workflow '{wf_id}': compute node '{node_id}' numeric_cmp requires 1..=2 inputs, got {}",
                    inputs.len()
                ));
            }
            let single = inputs.len() == 1;
            if single && threshold.is_none() {
                return Err(format!(
                    "workflow '{wf_id}': compute node '{node_id}' numeric_cmp single-input form requires threshold"
                ));
            }
            if !single && threshold.is_some() {
                return Err(format!(
                    "workflow '{wf_id}': compute node '{node_id}' numeric_cmp two-input form forbids threshold"
                ));
            }
        }
        ComputeSpec::RegexMatch { inputs, pattern } => {
            check_inputs(inputs, "regex_match")?;
            if inputs.len() != 1 {
                return Err(format!(
                    "workflow '{wf_id}': compute node '{node_id}' regex_match requires exactly 1 input, got {}",
                    inputs.len()
                ));
            }
            if pattern.is_empty() {
                return Err(format!(
                    "workflow '{wf_id}': compute node '{node_id}' regex pattern must be non-empty"
                ));
            }
            regex::Regex::new(pattern).map_err(|e| {
                format!(
                    "workflow '{wf_id}': compute node '{node_id}' invalid regex pattern '{pattern}': {e}"
                )
            })?;
        }
    }
    Ok(())
}

/// 一层的执行/跳过分划(纯函数,决定论:同输入必同输出)
///
/// 规则:
/// 1. 节点声明了 `run_when` → 按条件求值决定去留(**豁免级联**:即使上游被跳过,
///    条件为真仍执行)
/// 2. 未声明 `run_when` → 任一直接依赖已被跳过即级联跳过
///
/// 层内规划不依赖层内执行结果(run_when 只允许引用更早拓扑层,见
/// `check_run_when_layers`),因此层内节点顺序不影响分划结果。
///
/// 返回 (本层待执行节点[保持原序], 新增跳过节点 id 列表)
fn plan_layer<'a>(
    layer: &[&'a WorkflowNode],
    results: &BTreeMap<String, String>,
    skipped: &HashSet<String>,
) -> (Vec<&'a WorkflowNode>, Vec<String>) {
    let mut to_run = Vec::new();
    let mut newly_skipped = Vec::new();
    for &node in layer {
        let run = match &node.run_when {
            Some(cond) => eval_condition(cond, results),
            None => !node.depends_on.iter().any(|d| skipped.contains(d.as_str())),
        };
        if run {
            to_run.push(node);
        } else {
            newly_skipped.push(node.id.clone());
        }
    }
    (to_run, newly_skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::definition::AgentDefinitionManager;
    use crate::agent::delegate::DelegateContext;
    use crate::api::evorule_client::EvoruleApiClient;

    fn make_ctx() -> DelegateContext {
        let definitions = AgentDefinitionManager::with_default_dir();
        DelegateContext::new(
            "parent",
            definitions,
            EvoruleApiClient::new("http://localhost:8080"),
        )
    }

    fn node(id: &str, agent: &str, deps: &[&str]) -> WorkflowNode {
        WorkflowNode {
            id: id.to_string(),
            agent_type: agent.to_string(),
            task: format!("task for {}", id),
            task_template: None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            run_when: None,
            compute: None,
        }
    }

    // ===== 反序列化 =====

    #[test]
    fn test_workflow_deserialize_from_json() {
        let json = r#"{
            "workflow_id": "research_and_write",
            "description": "并行调研 + 串行写作",
            "nodes": [
                {"id": "research_rust", "agent_type": "researcher", "task": "调研 Rust", "depends_on": []},
                {"id": "research_python", "agent_type": "researcher", "task": "调研 Python", "depends_on": []},
                {"id": "write_report", "agent_type": "writer", "task_template": "Rust: {research_rust}\nPython: {research_python}", "depends_on": ["research_rust", "research_python"]}
            ],
            "output_node": "write_report"
        }"#;
        let wf: Workflow = serde_json::from_str(json).unwrap();
        assert_eq!(wf.workflow_id, "research_and_write");
        assert_eq!(wf.nodes.len(), 3);
        assert_eq!(wf.output_node, "write_report");
        assert!(wf.nodes[2].task_template.is_some());
        assert!(wf.nodes[2].task.is_empty()); // task 默认空
        assert_eq!(wf.nodes[0].depends_on, Vec::<String>::new());
    }

    #[test]
    fn test_workflow_deserialize_minimal() {
        let json = r#"{
            "workflow_id": "single",
            "nodes": [{"id": "only", "agent_type": "worker", "task": "do it"}],
            "output_node": "only"
        }"#;
        let wf: Workflow = serde_json::from_str(json).unwrap();
        assert_eq!(wf.nodes.len(), 1);
        assert_eq!(wf.description, ""); // default
    }

    // ===== compute 节点(交付物 4;D-03 拍板方案③,Phase 1-A T3) =====

    fn compute_node(id: &str, deps: &[&str], spec: ComputeSpec) -> WorkflowNode {
        WorkflowNode {
            id: id.to_string(),
            agent_type: String::new(), // compute 无 agent 语义
            task: String::new(),
            task_template: None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            run_when: None,
            compute: Some(spec),
        }
    }

    fn strcmp_equal(a: &str, b: &str) -> ComputeSpec {
        ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node(a.to_string()),
                ComputeInput::Node(b.to_string()),
            ],
            mode: StrcmpMode::Equal,
        }
    }

    fn pure_compute_wf(nodes: Vec<WorkflowNode>, output: &str) -> Workflow {
        Workflow {
            workflow_id: "compute_wf".to_string(),
            description: String::new(),
            nodes,
            output_node: output.to_string(),
        }
    }

    #[test]
    fn test_eval_compute_result_vocabularies() {
        // strcmp/equal 词表 equal|different(交付物 4 §4.1;different 不含 equal 子串)
        let results: BTreeMap<String, String> = [
            ("x".to_string(), "abc".to_string()),
            ("y".to_string(), "abc".to_string()),
            ("z".to_string(), "abd".to_string()),
        ]
        .into_iter()
        .collect();
        let same = ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node("x".into()),
                ComputeInput::Node("y".into()),
            ],
            mode: StrcmpMode::Equal,
        };
        assert_eq!(eval_compute(&same, &results).unwrap(), "equal");
        let diff = ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node("x".into()),
                ComputeInput::Node("z".into()),
            ],
            mode: StrcmpMode::Equal,
        };
        assert_eq!(eval_compute(&diff, &results).unwrap(), "different");

        // strcmp/contains 词表 contained|not_contained
        let b_results: BTreeMap<String, String> = [
            ("b".to_string(), "bc".to_string()),
            ("x".to_string(), "abc".to_string()),
            ("z".to_string(), "abd".to_string()),
        ]
        .into_iter()
        .collect();
        let contains = ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node("x".into()),
                ComputeInput::Node("b".into()),
            ],
            mode: StrcmpMode::Contains,
        };
        assert_eq!(eval_compute(&contains, &b_results).unwrap(), "contained");
        let not_contains = ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node("z".into()),
                ComputeInput::Node("b".into()),
            ],
            mode: StrcmpMode::Contains,
        };
        assert_eq!(
            eval_compute(&not_contains, &b_results).unwrap(),
            "not_contained"
        );

        // numeric_cmp 词表 true|false(双输入 + 单输入 threshold 两形态)
        let num_results: BTreeMap<String, String> = [
            ("n1".to_string(), "3".to_string()),
            ("n2".to_string(), "5".to_string()),
        ]
        .into_iter()
        .collect();
        let two = ComputeSpec::NumericCmp {
            inputs: vec![
                ComputeInput::Node("n1".into()),
                ComputeInput::Node("n2".into()),
            ],
            mode: NumericCmpMode::Lt,
            threshold: None,
        };
        assert_eq!(eval_compute(&two, &num_results).unwrap(), "true");
        let single = ComputeSpec::NumericCmp {
            inputs: vec![ComputeInput::Node("n1".into())],
            mode: NumericCmpMode::Ge,
            threshold: Some(3.0),
        };
        assert_eq!(eval_compute(&single, &num_results).unwrap(), "true");

        // regex_match 词表 match|no_match
        let re = ComputeSpec::RegexMatch {
            inputs: vec![ComputeInput::Node("x".into())],
            pattern: "^a.c$".to_string(),
        };
        assert_eq!(eval_compute(&re, &results).unwrap(), "match");
        let re_no = ComputeSpec::RegexMatch {
            inputs: vec![ComputeInput::Node("z".into())],
            pattern: "^x".to_string(),
        };
        assert_eq!(eval_compute(&re_no, &results).unwrap(), "no_match");
    }

    #[test]
    fn test_eval_compute_deterministic_byte_identical() {
        // 同输入重复求值逐字节一致(交付物 4 §2.5-4)
        let results: BTreeMap<String, String> = [
            ("a".to_string(), "hello world".to_string()),
            ("b".to_string(), "world".to_string()),
        ]
        .into_iter()
        .collect();
        let spec = ComputeSpec::Strcmp {
            inputs: vec![
                ComputeInput::Node("a".into()),
                ComputeInput::Node("b".into()),
            ],
            mode: StrcmpMode::Contains,
        };
        let first = eval_compute(&spec, &results).unwrap();
        for _ in 0..10 {
            assert_eq!(eval_compute(&spec, &results).unwrap(), first);
        }
    }

    #[test]
    fn test_eval_compute_numeric_parse_failure_is_node_failure() {
        // 数值解析失败 = 节点失败(交付物 4 §4.5,无静默回退:不当 0/空串继续跑)
        let results: BTreeMap<String, String> = [
            ("bad".to_string(), "not-a-number".to_string()),
            ("n".to_string(), "2".to_string()),
        ]
        .into_iter()
        .collect();
        let spec = ComputeSpec::NumericCmp {
            inputs: vec![
                ComputeInput::Node("bad".into()),
                ComputeInput::Node("n".into()),
            ],
            mode: NumericCmpMode::Lt,
            threshold: None,
        };
        let err = eval_compute(&spec, &results).unwrap_err();
        assert!(err.contains("数值解析失败"), "{err}");
    }

    #[test]
    fn test_eval_compute_input_missing_is_empty_string() {
        // 缺失语义(交付物 4 §3.3):被跳过/无结果节点 → 空串;Empty 字面量 → 空串
        let results: BTreeMap<String, String> =
            [("a".to_string(), "v".to_string())].into_iter().collect();
        let spec = ComputeSpec::Strcmp {
            inputs: vec![ComputeInput::Node("missing".into()), ComputeInput::Empty],
            mode: StrcmpMode::Equal,
        };
        assert_eq!(eval_compute(&spec, &results).unwrap(), "equal");
    }

    #[test]
    fn test_validate_compute_form_rejections() {
        // 封闭目录代码层校验(交付物 4 §4.6 双保险):数量/threshold 互斥/pattern/引用
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let cases: Vec<(Workflow, &str)> = vec![
            // strcmp 1 输入
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::Strcmp {
                            inputs: vec![ComputeInput::Node("c".into())],
                            mode: StrcmpMode::Equal,
                        },
                    )],
                    "c",
                ),
                "exactly 2 inputs",
            ),
            // numeric_cmp 双输入带 threshold(禁止)
            (
                pure_compute_wf(
                    vec![
                        node("a", "worker", &[]),
                        node("b", "worker", &[]),
                        WorkflowNode {
                            compute: Some(ComputeSpec::NumericCmp {
                                inputs: vec![
                                    ComputeInput::Node("a".into()),
                                    ComputeInput::Node("b".into()),
                                ],
                                mode: NumericCmpMode::Lt,
                                threshold: Some(1.0),
                            }),
                            ..compute_node(
                                "c",
                                &["a", "b"],
                                ComputeSpec::RegexMatch {
                                    inputs: vec![ComputeInput::Node("a".into())],
                                    pattern: "x".to_string(),
                                },
                            )
                        },
                    ],
                    "c",
                ),
                "forbids threshold",
            ),
            // numeric_cmp 单输入缺 threshold(必带)
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::NumericCmp {
                            inputs: vec![ComputeInput::Node("c".into())],
                            mode: NumericCmpMode::Lt,
                            threshold: None,
                        },
                    )],
                    "c",
                ),
                "requires threshold",
            ),
            // regex 非法 pattern(加载期校验,交付物 4 §4.3)
            (
                pure_compute_wf(
                    vec![compute_node(
                        "c",
                        &[],
                        ComputeSpec::RegexMatch {
                            inputs: vec![ComputeInput::Node("c".into())],
                            pattern: "([unclosed".to_string(),
                        },
                    )],
                    "c",
                ),
                "invalid regex pattern",
            ),
            // inputs 引用未知节点
            (
                pure_compute_wf(
                    vec![compute_node("c", &[], strcmp_equal("ghost", "c"))],
                    "c",
                ),
                "unknown node 'ghost'",
            ),
        ];
        for (wf, expect) in cases {
            let err = engine.validate(&wf).unwrap_err();
            assert!(err.contains(expect), "expect '{expect}', got: {err}");
        }
    }

    #[tokio::test]
    async fn test_execute_compute_chain_with_skip_semantics() {
        // 全 compute 链集成(不经 delegate,无 LLM/无会话/无网络):
        // a(strcmp Empty 对)= "equal";b run_when 恒假 → skip;
        // c run_when 恒真(豁免级联)执行,输入含被跳过的 b → 空串(§3.3);
        // d regex("^$") 观察被跳过的 b → 空串 → "match"
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = pure_compute_wf(
            vec![
                WorkflowNode {
                    compute: Some(ComputeSpec::Strcmp {
                        inputs: vec![ComputeInput::Empty, ComputeInput::Empty],
                        mode: StrcmpMode::Equal,
                    }),
                    ..compute_node("a", &[], strcmp_equal("a", "a"))
                },
                WorkflowNode {
                    run_when: Some(RunWhen {
                        node: "a".to_string(),
                        op: ConditionOp::Equals,
                        value: "equal_never".to_string(), // 恒假 → b 被跳过
                    }),
                    ..compute_node("b", &["a"], strcmp_equal("a", "a"))
                },
                WorkflowNode {
                    run_when: Some(RunWhen {
                        node: "a".to_string(),
                        op: ConditionOp::Equals,
                        value: "equal".to_string(), // 恒真 → c 执行(豁免级联)
                    }),
                    ..compute_node("c", &["b"], strcmp_equal("b", "a"))
                },
                WorkflowNode {
                    compute: Some(ComputeSpec::RegexMatch {
                        inputs: vec![ComputeInput::Node("b".into())],
                        pattern: "^$".to_string(), // b 被跳过 → 空串 → match
                    }),
                    ..compute_node("d", &["c"], strcmp_equal("c", "c"))
                },
            ],
            "d",
        );
        let out = engine
            .execute(&wf)
            .await
            .expect("pure compute workflow must run");
        assert_eq!(out, "match");
    }

    // ===== validate =====

    #[test]
    fn test_validate_empty_nodes() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![],
            output_node: "x".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("no nodes"));
    }

    #[test]
    fn test_validate_duplicate_id() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &[]), node("a", "worker", &[])],
            output_node: "a".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("duplicate node id"));
        assert!(err.contains("'a'"));
    }

    // ===== 门卫负向用例(P2-M7 前置补丁,2026-08-27) =====

    #[test]
    fn test_validate_node_id_rejects_non_identifier() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        // 含花括号的 id 会污染 task_template 占位符机制
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("bad{id}", "worker", &[])],
            output_node: "bad{id}".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("not a valid identifier"), "got: {}", err);
    }

    #[test]
    fn test_validate_node_id_rejects_empty_and_space() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        for bad_id in ["", "has space", "dot.dot"] {
            let wf = Workflow {
                workflow_id: "w".to_string(),
                description: String::new(),
                nodes: vec![node(bad_id, "worker", &[])],
                output_node: bad_id.to_string(),
            };
            let err = engine.validate(&wf).unwrap_err();
            assert!(
                err.contains("not a valid identifier"),
                "id {:?} not rejected, got: {}",
                bad_id,
                err
            );
        }
    }

    #[test]
    fn test_validate_agent_type_rejects_path_traversal() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        for bad in ["../researcher", "a/b", "a\\b", ".."] {
            let wf = Workflow {
                workflow_id: "w".to_string(),
                description: String::new(),
                nodes: vec![node("n1", bad, &[])],
                output_node: "n1".to_string(),
            };
            let err = engine.validate(&wf).unwrap_err();
            assert!(
                err.contains("invalid agent_type") || err.contains("path traversal"),
                "agent_type {:?} not rejected, got: {}",
                bad,
                err
            );
        }
    }

    #[test]
    fn test_validate_accepts_valid_identifiers() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![
                node("alpha_1-beta", "general-2_x", &[]),
                node("n2", "researcher", &["alpha_1-beta"]),
            ],
            output_node: "n2".to_string(),
        };
        assert!(engine.validate(&wf).is_ok());
    }

    #[test]
    fn test_validate_output_node_missing() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &[])],
            output_node: "nonexistent".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("output_node"));
        assert!(err.contains("not found"));
    }

    #[test]
    fn test_validate_unknown_dependency() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &["ghost"])],
            output_node: "a".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("unknown node"));
        assert!(err.contains("'ghost'"));
    }

    #[test]
    fn test_validate_self_dependency() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &["a"])],
            output_node: "a".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("depends on itself"));
    }

    #[test]
    fn test_validate_valid_dag() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![
                node("a", "worker", &[]),
                node("b", "worker", &["a"]),
                node("c", "worker", &["a", "b"]),
            ],
            output_node: "c".to_string(),
        };
        assert!(engine.validate(&wf).is_ok());
    }

    // ===== topological_sort =====

    #[test]
    fn test_topo_sort_linear_chain() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![
            node("a", "w", &[]),
            node("b", "w", &["a"]),
            node("c", "w", &["b"]),
        ];
        let layers = engine.topological_sort(&nodes).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0][0].id, "a");
        assert_eq!(layers[1][0].id, "b");
        assert_eq!(layers[2][0].id, "c");
    }

    #[test]
    fn test_topo_sort_parallel_layer() {
        // a, b 无依赖 → 同层;c 依赖两者 → 下一层
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![
            node("a", "w", &[]),
            node("b", "w", &[]),
            node("c", "w", &["a", "b"]),
        ];
        let layers = engine.topological_sort(&nodes).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].len(), 2); // a, b 同层
        assert_eq!(layers[1].len(), 1); // c 单独
                                        // 第一层包含 a 和 b
        let layer0_ids: Vec<&str> = layers[0].iter().map(|n| n.id.as_str()).collect();
        assert!(layer0_ids.contains(&"a"));
        assert!(layer0_ids.contains(&"b"));
        assert_eq!(layers[1][0].id, "c");
    }

    #[test]
    fn test_topo_sort_diamond() {
        // a → b, a → c, b → d, c → d
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![
            node("a", "w", &[]),
            node("b", "w", &["a"]),
            node("c", "w", &["a"]),
            node("d", "w", &["b", "c"]),
        ];
        let layers = engine.topological_sort(&nodes).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0].len(), 1); // a
        assert_eq!(layers[1].len(), 2); // b, c
        assert_eq!(layers[2].len(), 1); // d
    }

    #[test]
    fn test_topo_sort_cycle_detected() {
        // a → b → a(环)
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![node("a", "w", &["b"]), node("b", "w", &["a"])];
        let err = engine.topological_sort(&nodes).unwrap_err();
        assert!(err.contains("cycle detected"));
        assert!(err.contains("a"));
        assert!(err.contains("b"));
    }

    #[test]
    fn test_topo_sort_self_loop_via_dep() {
        // validate 拦截自依赖,但 topological_sort 直接调用时应能处理
        // (自依赖会被环检测捕获,因为节点永远无法满足"依赖已处理")
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![node("a", "w", &["a"])];
        let err = engine.topological_sort(&nodes).unwrap_err();
        assert!(err.contains("cycle detected"));
    }

    #[test]
    fn test_topo_sort_single_node() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let nodes = vec![node("a", "w", &[])];
        let layers = engine.topological_sort(&nodes).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].len(), 1);
    }

    // ===== render_task =====

    #[test]
    fn test_render_task_with_template() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let n = WorkflowNode {
            id: "c".to_string(),
            agent_type: "writer".to_string(),
            task: String::new(),
            task_template: Some("Rust: {a}\nPython: {b}".to_string()),
            depends_on: vec!["a".to_string(), "b".to_string()],
            run_when: None,
            compute: None,
        };
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "rust-result".to_string());
        results.insert("b".to_string(), "python-result".to_string());
        let rendered = engine.render_task(&n, &results, &HashSet::new());
        assert_eq!(rendered, "Rust: rust-result\nPython: python-result");
    }

    #[test]
    fn test_render_task_without_template_uses_task() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let n = WorkflowNode {
            id: "a".to_string(),
            agent_type: "researcher".to_string(),
            task: "调研 Rust".to_string(),
            task_template: None,
            depends_on: vec![],
            run_when: None,
            compute: None,
        };
        let results = BTreeMap::new();
        let rendered = engine.render_task(&n, &results, &HashSet::new());
        assert_eq!(rendered, "调研 Rust");
    }

    #[test]
    fn test_render_task_template_prefers_over_task() {
        // 同时提供 task 和 task_template → 用 template
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let n = WorkflowNode {
            id: "a".to_string(),
            agent_type: "w".to_string(),
            task: "plain task".to_string(),
            task_template: Some("template {x}".to_string()),
            depends_on: vec![],
            run_when: None,
            compute: None,
        };
        let mut results = BTreeMap::new();
        results.insert("x".to_string(), "VAL".to_string());
        let rendered = engine.render_task(&n, &results, &HashSet::new());
        assert_eq!(rendered, "template VAL");
    }

    #[test]
    fn test_render_task_unresolved_placeholder_preserved() {
        // 占位符无对应结果时保留(拓扑排序保证不会发生,但行为应可预测)
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let n = WorkflowNode {
            id: "c".to_string(),
            agent_type: "w".to_string(),
            task: String::new(),
            task_template: Some("{a} and {b}".to_string()),
            depends_on: vec!["a".to_string()],
            run_when: None,
            compute: None,
        };
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "A".to_string());
        // b 未就绪
        let rendered = engine.render_task(&n, &results, &HashSet::new());
        assert_eq!(rendered, "A and {b}");
    }

    #[test]
    fn test_render_task_skipped_upstream_replaced_with_empty() {
        // 被跳过的上游无结果:占位符替换为空字符串(不把 {id} 字面量漏进下游任务)
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let n = WorkflowNode {
            id: "c".to_string(),
            agent_type: "w".to_string(),
            task: String::new(),
            task_template: Some("review: {b}done".to_string()),
            depends_on: vec!["b".to_string()],
            run_when: None,
            compute: None,
        };
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "A".to_string());
        let skipped: HashSet<String> = ["b".to_string()].into_iter().collect();
        let rendered = engine.render_task(&n, &results, &skipped);
        assert_eq!(rendered, "review: done");
    }

    // ===== execute(用深度超限避免实际 evorule 调用)=====

    #[tokio::test]
    async fn test_execute_validates_before_running() {
        // output_node 不存在 → execute 在校验阶段就返回 Err,不触达 delegate
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &[])],
            output_node: "missing".to_string(),
        };
        let err = engine.execute(&wf).await.unwrap_err();
        assert!(err.contains("output_node"));
    }

    #[tokio::test]
    async fn test_execute_first_layer_node_failure_aborts() {
        // max_depth=0:第一层子任务 depth=1 >= 0 → 全部失败
        // → execute 在第一个节点失败时终止,返回 Err
        let ctx = make_ctx().with_max_depth(0);
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "worker", &[])],
            output_node: "a".to_string(),
        };
        let err = engine.execute(&wf).await.unwrap_err();
        assert!(err.contains("node 'a' failed"));
        assert!(err.contains("max delegate depth exceeded"));
    }

    #[tokio::test]
    async fn test_execute_cycle_returns_err() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![node("a", "w", &["b"]), node("b", "w", &["a"])],
            output_node: "a".to_string(),
        };
        // validate 通过(无自依赖、依赖均存在),topological_sort 检测到环
        let err = engine.execute(&wf).await.unwrap_err();
        assert!(err.contains("cycle detected"));
    }

    #[tokio::test]
    async fn test_execute_empty_nodes() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![],
            output_node: "x".to_string(),
        };
        let err = engine.execute(&wf).await.unwrap_err();
        assert!(err.contains("no nodes"));
    }

    // ===== run_when 条件分支(workflow_dag v1.1)=====

    fn cond(node_id: &str, op: ConditionOp, value: &str) -> RunWhen {
        RunWhen {
            node: node_id.to_string(),
            op,
            value: value.to_string(),
        }
    }

    fn with_run_when(mut n: WorkflowNode, c: RunWhen) -> WorkflowNode {
        n.run_when = Some(c);
        n
    }

    // ----- 反序列化 -----

    #[test]
    fn test_workflow_deserialize_run_when() {
        let json = r#"{
            "workflow_id": "conditional_publish",
            "nodes": [
                {"id": "review", "agent_type": "reviewer", "task": "r"},
                {"id": "publish", "agent_type": "publisher", "task": "p",
                 "depends_on": ["review"],
                 "run_when": {"node": "review", "op": "contains", "value": "APPROVE"}}
            ],
            "output_node": "publish"
        }"#;
        let wf: Workflow = serde_json::from_str(json).unwrap();
        let rw = wf.nodes[1].run_when.as_ref().unwrap();
        assert_eq!(rw.node, "review");
        assert_eq!(rw.op, ConditionOp::Contains);
        assert_eq!(rw.value, "APPROVE");
        // 不含 run_when 的节点默认 None(v1.0 兼容)
        assert!(wf.nodes[0].run_when.is_none());
    }

    #[test]
    fn test_workflow_without_run_when_serializes_identically() {
        // 语义等价实证:v1.0 形态往返后不含 run_when 键
        let wf: Workflow = serde_json::from_str(
            r#"{"workflow_id":"w","nodes":[{"id":"a","agent_type":"x","task":"t"}],"output_node":"a"}"#,
        )
        .unwrap();
        let s = serde_json::to_string(&wf).unwrap();
        assert!(!s.contains("run_when"));
    }

    // ----- validate -----

    #[test]
    fn test_validate_run_when_unknown_node() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![with_run_when(
                node("a", "w", &[]),
                cond("ghost", ConditionOp::Contains, "X"),
            )],
            output_node: "a".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(
            err.contains("run_when references unknown node"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_validate_run_when_self_reference() {
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![with_run_when(
                node("a", "w", &[]),
                cond("a", ConditionOp::Contains, "X"),
            )],
            output_node: "a".to_string(),
        };
        let err = engine.validate(&wf).unwrap_err();
        assert!(err.contains("run_when references itself"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_execute_rejects_same_layer_run_when_reference() {
        // run_when 引用同层节点 → 校验期拒绝(其结果在规划期不可得),
        // execute 在触达 delegate 前返回 Err
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![
                with_run_when(node("a", "w", &[]), cond("b", ConditionOp::Contains, "X")),
                with_run_when(node("b", "w", &[]), cond("a", ConditionOp::Contains, "Y")),
            ],
            output_node: "a".to_string(),
        };
        let err = engine.execute(&wf).await.unwrap_err();
        assert!(err.contains("earlier layer"), "got: {}", err);
    }

    #[test]
    fn test_run_when_coexists_with_cycle_detection() {
        // 环检测共存:带 run_when 的图仍能检出环(validate 放行,拓扑排序拒绝)
        let ctx = make_ctx();
        let engine = WorkflowEngine::new(ctx);
        let wf = Workflow {
            workflow_id: "w".to_string(),
            description: String::new(),
            nodes: vec![
                with_run_when(
                    node("a", "w", &["b"]),
                    cond("b", ConditionOp::Contains, "X"),
                ),
                with_run_when(
                    node("b", "w", &["a"]),
                    cond("a", ConditionOp::Contains, "Y"),
                ),
            ],
            output_node: "a".to_string(),
        };
        assert!(engine.validate(&wf).is_ok());
        let err = engine.topological_sort(&wf.nodes).unwrap_err();
        assert!(err.contains("cycle detected"));
    }

    // ----- eval_condition(纯函数)-----

    #[test]
    fn test_eval_condition_ops() {
        let mut results = BTreeMap::new();
        results.insert("review".to_string(), "APPROVE: looks good".to_string());
        assert!(eval_condition(
            &cond("review", ConditionOp::Contains, "APPROVE"),
            &results
        ));
        assert!(!eval_condition(
            &cond("review", ConditionOp::Contains, "REJECT"),
            &results
        ));
        assert!(eval_condition(
            &cond("review", ConditionOp::Equals, "APPROVE: looks good"),
            &results
        ));
        assert!(!eval_condition(
            &cond("review", ConditionOp::Equals, "approve"),
            &results
        ));
        assert!(eval_condition(
            &cond("review", ConditionOp::NotContains, "REJECT"),
            &results
        ));
        assert!(!eval_condition(
            &cond("review", ConditionOp::NotContains, "APPROVE"),
            &results
        ));
    }

    #[test]
    fn test_eval_condition_missing_node_treated_as_empty() {
        // 被观察节点被跳过/无结果 → 视作空字符串(确定性语义)
        let results = BTreeMap::new();
        assert!(eval_condition(
            &cond("gone", ConditionOp::Equals, ""),
            &results
        ));
        assert!(eval_condition(
            &cond("gone", ConditionOp::NotContains, "X"),
            &results
        ));
        assert!(!eval_condition(
            &cond("gone", ConditionOp::Contains, "X"),
            &results
        ));
        assert!(!eval_condition(
            &cond("gone", ConditionOp::Equals, "X"),
            &results
        ));
    }

    #[test]
    fn test_eval_condition_deterministic() {
        // 确定性实证:同一输入重复求值 N 次,结果一致
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "hello world".to_string());
        let c = cond("a", ConditionOp::Contains, "world");
        let expected = eval_condition(&c, &results);
        for _ in 0..100 {
            assert_eq!(eval_condition(&c, &results), expected);
        }
    }

    // ----- plan_layer(纯函数:跳过/级联/豁免/混合)-----

    #[test]
    fn test_plan_layer_run_when_false_skips() {
        // 混合层:无条件的 review 执行,条件为假的 publish 跳过
        let nodes = [
            node("review", "reviewer", &[]),
            with_run_when(
                node("publish", "publisher", &["review"]),
                cond("review", ConditionOp::Contains, "APPROVE"),
            ),
        ];
        let layer: Vec<&WorkflowNode> = nodes.iter().collect();
        let mut results = BTreeMap::new();
        results.insert("review".to_string(), "REJECT: bad".to_string());
        let (to_run, newly_skipped) = plan_layer(&layer, &results, &HashSet::new());
        assert_eq!(to_run.len(), 1);
        assert_eq!(to_run[0].id, "review");
        assert_eq!(newly_skipped, vec!["publish".to_string()]);
    }

    #[test]
    fn test_plan_layer_run_when_true_runs() {
        let nodes = [
            node("review", "reviewer", &[]),
            with_run_when(
                node("publish", "publisher", &["review"]),
                cond("review", ConditionOp::Contains, "APPROVE"),
            ),
        ];
        let layer: Vec<&WorkflowNode> = nodes.iter().collect();
        let mut results = BTreeMap::new();
        results.insert("review".to_string(), "APPROVE: ok".to_string());
        let (to_run, newly_skipped) = plan_layer(&layer, &results, &HashSet::new());
        assert!(newly_skipped.is_empty());
        assert_eq!(to_run.len(), 2);
    }

    #[test]
    fn test_plan_layer_cascades_to_downstream() {
        // 级联:b 已被跳过 → 无 run_when 的下游 c 级联跳过;
        // 豁免:d 有自己的 run_when 且条件为真 → 照常执行
        let nodes = [
            node("c", "w", &["b"]),
            with_run_when(
                node("d", "w", &["b"]),
                cond("a", ConditionOp::Contains, "GO"),
            ),
        ];
        let layer: Vec<&WorkflowNode> = nodes.iter().collect();
        let skipped: HashSet<String> = ["b".to_string()].into_iter().collect();
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "GO".to_string());
        let (to_run, newly_skipped) = plan_layer(&layer, &results, &skipped);
        assert_eq!(to_run.len(), 1);
        assert_eq!(to_run[0].id, "d");
        assert_eq!(newly_skipped, vec!["c".to_string()]);
    }

    #[test]
    fn test_plan_layer_no_run_when_v10_behavior_identical() {
        // 语义等价实证:无 run_when 且无跳过 → 分划与 v1.0 逐层执行完全一致
        // (全执行、保持原序、跳过集为空)
        let nodes = [node("a", "w", &[]), node("b", "w", &[])];
        let layer: Vec<&WorkflowNode> = nodes.iter().collect();
        let (to_run, newly_skipped) = plan_layer(&layer, &BTreeMap::new(), &HashSet::new());
        assert!(newly_skipped.is_empty());
        let ids: Vec<&str> = to_run.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn test_plan_layer_deterministic() {
        // 确定性实证:同一含条件层重复规划 N 次,分划一致
        let nodes = [with_run_when(
            node("notify", "w", &["publish"]),
            cond("review", ConditionOp::Contains, "APPROVE"),
        )];
        let layer: Vec<&WorkflowNode> = nodes.iter().collect();
        let skipped: HashSet<String> = ["publish".to_string()].into_iter().collect();
        let mut results = BTreeMap::new();
        results.insert("review".to_string(), "APPROVE".to_string());
        let first = plan_layer(&layer, &results, &skipped);
        assert_eq!(first.0.len(), 1); // 豁免级联:review 含 APPROVE → notify 照常执行
        assert_eq!(first.0[0].id, "notify");
        for _ in 0..100 {
            let again = plan_layer(&layer, &results, &skipped);
            assert_eq!(first.0.len(), again.0.len());
            assert_eq!(first.0[0].id, again.0[0].id);
            assert_eq!(first.1, again.1);
        }
    }
}
