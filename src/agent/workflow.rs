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
//! 2. **逐层执行**:同层节点并行(`DelegateContext::delegate_parallel`)
//! 3. **模板渲染**:下一层的 `task_template` 中 `{node_id}` 被上游结果替换
//! 4. 任一节点失败 → 整个工作流终止,返回 Err
//! 5. 返回 `output_node` 的结果
//!
//! ## 边界(§9.6)
//!
//! - 不支持条件分支(只支持静态 DAG);条件分支需 P2 的 plan-and-execute
//! - 节点失败默认终止整个工作流(无 `on_failure: skip`)
//! - `task_template` 引用失败节点会是空字符串(但失败即终止,不会走到这)

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::agent::delegate::DelegateContext;

/// 工作流节点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowNode {
    /// 节点 id(工作流内唯一,被 `depends_on` / `task_template` / `output_node` 引用)
    pub id: String,
    /// 执行该节点的 agent 类型(对应 `agents/<agent_type>.json`)
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
    /// 1. 校验(`output_node` 存在、无重复 id、依赖合法、无环)
    /// 2. 拓扑排序成分层结构
    /// 3. 逐层并行执行(`delegate_parallel`)
    /// 4. 上游结果填入下游 `task_template`
    /// 5. 返回 `output_node` 的结果
    ///
    /// # 错误
    ///
    /// - `output_node` 不存在
    /// - 节点 id 重复
    /// - `depends_on` 引用不存在的节点
    /// - 自依赖
    /// - 检测到环
    /// - 任一节点执行失败(终止整个工作流)
    pub async fn execute(&self, wf: &Workflow) -> Result<String, String> {
        // 1. 校验
        self.validate(wf)?;

        // 2. 拓扑排序
        let layers = self.topological_sort(&wf.nodes)?;

        // 3. 逐层执行
        let mut results: BTreeMap<String, String> = BTreeMap::new();
        for (layer_idx, layer) in layers.iter().enumerate() {
            let tasks: Vec<(String, String)> = layer
                .iter()
                .map(|node| {
                    let task = self.render_task(node, &results);
                    (node.agent_type.clone(), task)
                })
                .collect();

            tracing::info!(
                workflow_id = %wf.workflow_id,
                layer = layer_idx,
                node_count = layer.len(),
                "executing workflow layer"
            );

            let layer_results = self.ctx.delegate_parallel(tasks).await;

            for (node, result) in layer.iter().zip(layer_results.iter()) {
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
            if let Err(e) =
                crate::agent::definition::AgentDefinition::validate_agent_type(&n.agent_type)
            {
                return Err(format!("workflow '{}': {}", wf.workflow_id, e));
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

        // 依赖合法性 + 自依赖
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
        }

        Ok(())
    }

    /// 渲染 `task_template`:把 `{node_id}` 替换为上游结果
    ///
    /// 无 `task_template` 时返回 `node.task`。上游结果未就绪时占位符保留
    /// (但拓扑排序保证执行时上游已完成,不会出现未就绪)。
    fn render_task(&self, node: &WorkflowNode, results: &BTreeMap<String, String>) -> String {
        if let Some(tmpl) = &node.task_template {
            let mut task = tmpl.clone();
            for (id, content) in results {
                task = task.replace(&format!("{{{}}}", id), content);
            }
            task
        } else {
            node.task.clone()
        }
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
        };
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "rust-result".to_string());
        results.insert("b".to_string(), "python-result".to_string());
        let rendered = engine.render_task(&n, &results);
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
        };
        let results = BTreeMap::new();
        let rendered = engine.render_task(&n, &results);
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
        };
        let mut results = BTreeMap::new();
        results.insert("x".to_string(), "VAL".to_string());
        let rendered = engine.render_task(&n, &results);
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
        };
        let mut results = BTreeMap::new();
        results.insert("a".to_string(), "A".to_string());
        // b 未就绪
        let rendered = engine.render_task(&n, &results);
        assert_eq!(rendered, "A and {b}");
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
}
