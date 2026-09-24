// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 宪法 schema 校验桥（M7-B1/B2，2026-08-27；2026-09-22 收编薄封装化）
//!
//! 判定代码已收编至 [`evorule-constitution`] 共享组件（0.2.0：schema 编译期
//! 内嵌 + `Policy` 双模式降级策略，缺省 Strict）。属地执法义务要求判定代码
//! 必须是组件、禁止复制第二份——本模块自此只是**接入层**：
//! - workflow_dag 三版本分派（按文档形态分派，见 [`detect_workflow_dag_version`]）
//! - 校验结果形态转换（组件 `Violation` → 本仓 `Vec<String>`）
//! - 统一加载入口 [`load_workflow`]（schema 校验 → v1.2 物化 / v1.0-v1.1 反序列化）
//!
//! 组件以内嵌 schema + 缺省 Strict 运行：v1.x schema 编译期随组件携带，
//! 部署/CI 零磁盘依赖，「宪法仓不可得」的整类环境缺陷不再存在；未知
//! kind/版本在 Strict 下 fail-fast（门禁不可降级）。
//!
//! 接入点：资产（或其 sidecar 标注层）反序列化后先过宪法 schema 全量校验，
//! 再进入业务门卫（`AgentDefinition::validate` 等）。

use std::sync::OnceLock;

use evorule_constitution::Constitution;

use crate::agent::materializer;
use crate::agent::workflow::Workflow;

/// 进程级宪法校验器（组件内嵌模式 + 缺省 Strict 策略）
fn constitution() -> &'static Constitution {
    static C: OnceLock<Constitution> = OnceLock::new();
    C.get_or_init(Constitution::new)
}

/// 校验 agent_def v1.0 裸文档（无壳 body）。校验不通过即 Err（fail-fast 收口，
/// 错误消息含违规路径与原因，由加载方拒绝资产并给出指引）。
pub fn validate_agent_def(body: &serde_json::Value) -> Result<(), Vec<String>> {
    validate_kind_version("agent_def", "v1.0", body)
}

/// 判定 workflow_dag 裸文档应按哪个版本校验（并存窗口）
///
/// 分派规则（确定性：同输入必同分派）：
/// 1. body 显式携带 `$schema` 字段 → 按声明分派（v1.2/v1.1，其余按 v1.0）
/// 2. 裸 body（运行时形态，无 `$schema`）→ 能力探测（有序）：
///    顶层 `loops` 非空或任一节点含 `compute` → v1.2（v1.2 相对 v1.1 的增量特征）；
///    任一节点含 `run_when` → v1.1（v1.1 相对 v1.0 的唯一增量）；否则 v1.0
fn detect_workflow_dag_version(body: &serde_json::Value) -> &'static str {
    if let Some(url) = body.get("$schema").and_then(|v| v.as_str()) {
        if url.ends_with("/workflow_dag/v1.2.json") {
            return "v1.2";
        }
        return if url.ends_with("/workflow_dag/v1.1.json") {
            "v1.1"
        } else {
            "v1.0"
        };
    }
    let nodes = body.get("nodes").and_then(|v| v.as_array());
    let uses_v12 = body
        .get("loops")
        .and_then(|v| v.as_array())
        .is_some_and(|loops| !loops.is_empty())
        || nodes.is_some_and(|nodes| nodes.iter().any(|n| n.get("compute").is_some()));
    if uses_v12 {
        "v1.2"
    } else if nodes.is_some_and(|nodes| nodes.iter().any(|n| n.get("run_when").is_some())) {
        "v1.1"
    } else {
        "v1.0"
    }
}

/// 用 workflow_dag 校验裸文档（无壳 body）。
///
/// 按 [`detect_workflow_dag_version`] 分派 v1.0/v1.1/v1.2 校验器（三版本并存窗口）。
///
/// **v1.2 物化门**（原防呆拒载门，引擎 loop/compute 能力落地后翻转）：
/// v1.2 文档通过 schema 校验后还须通过物化器 [`materializer::materialize_workflow_dag`]
/// 的静态展开自检（引用文法 R1–R4、冻结限额、展开后 DAG 合法性）——
/// `serde_json::from_value` 会静默忽略 `loops` 字段，物化门保证 v1.2 增量
/// 语义被真正消费而非静默丢弃。
pub fn validate_workflow_dag(body: &serde_json::Value) -> Result<(), Vec<String>> {
    let version = detect_workflow_dag_version(body);
    validate_kind_version("workflow_dag", version, body)?;
    if version == "v1.2" {
        // schema 已过；物化成功 = v1.2 增量语义可被完整消费
        materializer::materialize_workflow_dag(body).map(|_| ())
    } else {
        Ok(())
    }
}

/// 校验并加载 workflow_dag 裸文档为可执行 [`Workflow`]。
///
/// 统一入口（v1.0/v1.1/v1.2 三版本并存）：
/// - schema 校验（宪法内嵌 Strict）→ 失败即 Err
/// - v1.2 → 物化器静态展开（loop 展开为线性副本链 + compute 节点就位）
/// - v1.0/v1.1 → 直接反序列化
pub fn load_workflow(body: &serde_json::Value) -> Result<Workflow, Vec<String>> {
    let version = detect_workflow_dag_version(body);
    validate_kind_version("workflow_dag", version, body)?;
    match version {
        "v1.2" => materializer::materialize_workflow_dag(body),
        _ => serde_json::from_value(body.clone())
            .map_err(|e| vec![format!("workflow_dag {version} 文档反序列化失败: {e}")]),
    }
}

fn validate_kind_version(
    kind: &str,
    version: &str,
    body: &serde_json::Value,
) -> Result<(), Vec<String>> {
    constitution()
        .validate_version(kind, version, body)
        .map_err(|violations| violations.iter().map(|v| v.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_agent_def_rejects_bad_temperature() {
        // 内嵌模式:无需磁盘宪法仓,校验恒可用
        let bad = serde_json::json!({
            "agent_type": "x", "version": "1", "description": "", "system_prompt": "s",
            "model": "m", "temperature": 99.0, "max_steps": 1,
            "step_timeout_secs": 1, "tools": []
        });
        let errs = validate_agent_def(&bad).expect_err("out-of-range must reject");
        assert!(
            errs.iter().any(|e| e.contains("temperature")),
            "违规须指明 temperature: {errs:?}"
        );
    }

    #[test]
    fn test_validate_accepts_real_shapes() {
        let ok_agent = serde_json::json!({
            "agent_type": "researcher", "version": "0.1.0", "description": "d",
            "system_prompt": "s", "model": "m", "temperature": 0.3,
            "max_steps": 20, "step_timeout_secs": 60,
            "tools": ["file_read"]
        });
        assert!(validate_agent_def(&ok_agent).is_ok());
        let ok_wf = serde_json::json!({
            "workflow_id": "wf", "description": "",
            "nodes": [{"id": "a", "agent_type": "researcher", "task": "t"}],
            "output_node": "a"
        });
        assert!(validate_workflow_dag(&ok_wf).is_ok());
    }

    // ----- workflow_dag v1.0/v1.1 双版本分派 -----

    #[test]
    fn test_detect_workflow_dag_version() {
        // 显式 $schema 按声明分派
        let explicit_v11 = serde_json::json!({
            "$schema": "https://evorule.org/schemas/workflow_dag/v1.1.json",
            "workflow_id": "w", "nodes": [], "output_node": "x"
        });
        assert_eq!(detect_workflow_dag_version(&explicit_v11), "v1.1");
        let explicit_v10 = serde_json::json!({
            "$schema": "https://evorule.org/schemas/workflow_dag/v1.0.json",
            "workflow_id": "w", "nodes": [], "output_node": "x"
        });
        assert_eq!(detect_workflow_dag_version(&explicit_v10), "v1.0");
        // 裸 body（运行时形态）能力探测：含 run_when → v1.1
        let bare_v11 = serde_json::json!({
            "workflow_id": "w",
            "nodes": [
                {"id": "a", "agent_type": "x", "run_when": {"node": "a", "op": "contains", "value": "v"}}
            ],
            "output_node": "a"
        });
        assert_eq!(detect_workflow_dag_version(&bare_v11), "v1.1");
        // 裸 body 无 run_when → v1.0
        let bare_v10 = serde_json::json!({
            "workflow_id": "w", "nodes": [{"id": "a", "agent_type": "x"}], "output_node": "a"
        });
        assert_eq!(detect_workflow_dag_version(&bare_v10), "v1.0");
    }

    #[test]
    fn test_validate_workflow_v11_accepts_run_when() {
        let ok = serde_json::json!({
            "workflow_id": "w", "description": "",
            "nodes": [
                {"id": "a", "agent_type": "researcher", "task": "t"},
                {"id": "b", "agent_type": "writer", "task": "t", "depends_on": ["a"],
                 "run_when": {"node": "a", "op": "contains", "value": "APPROVE"}}
            ],
            "output_node": "b"
        });
        assert!(validate_workflow_dag(&ok).is_ok());
    }

    #[test]
    fn test_validate_workflow_v11_rejects_unknown_op() {
        // 含 run_when 的 body 走 v1.1 严格校验：op 枚举外取值被拒。
        // （v1.0 schema 未定义 run_when、默认放行未知字段——本用例同时证明分派到了 v1.1）
        let bad = serde_json::json!({
            "workflow_id": "w", "description": "",
            "nodes": [
                {"id": "a", "agent_type": "researcher", "task": "t"},
                {"id": "b", "agent_type": "writer", "task": "t", "depends_on": ["a"],
                 "run_when": {"node": "a", "op": "starts_with", "value": "APPROVE"}}
            ],
            "output_node": "b"
        });
        assert!(validate_workflow_dag(&bad).is_err());
    }

    #[test]
    fn test_validate_workflow_v11_rejects_non_string_value() {
        let bad = serde_json::json!({
            "workflow_id": "w", "description": "",
            "nodes": [
                {"id": "a", "agent_type": "researcher", "task": "t"},
                {"id": "b", "agent_type": "writer", "task": "t", "depends_on": ["a"],
                 "run_when": {"node": "a", "op": "equals", "value": 42}}
            ],
            "output_node": "b"
        });
        assert!(validate_workflow_dag(&bad).is_err());
    }

    #[test]
    fn test_validate_workflow_v11_rejects_extra_keys_in_run_when() {
        // run_when 子对象封口（additionalProperties: false）：防拼写漂移
        let bad = serde_json::json!({
            "workflow_id": "w", "description": "",
            "nodes": [
                {"id": "a", "agent_type": "researcher", "task": "t"},
                {"id": "b", "agent_type": "writer", "task": "t", "depends_on": ["a"],
                 "run_when": {"node": "a", "op": "equals", "value": "v", "extra": 1}}
            ],
            "output_node": "b"
        });
        assert!(validate_workflow_dag(&bad).is_err());
    }

    // ----- workflow_dag v1.2 三版本分派 + 防呆门 -----

    #[test]
    fn test_detect_workflow_dag_v12() {
        // 显式 $schema 按声明分派
        let explicit_v12 = serde_json::json!({
            "$schema": "https://evorule.org/schemas/workflow_dag/v1.2.json",
            "workflow_id": "w", "nodes": [], "output_node": "x"
        });
        assert_eq!(detect_workflow_dag_version(&explicit_v12), "v1.2");
        // 裸 body：顶层 loops 非空 → v1.2
        let bare_loops = serde_json::json!({
            "workflow_id": "w", "nodes": [{"id": "a", "agent_type": "x"}],
            "loops": [{"id": "lp", "max_iterations": 3, "body": [{"id": "s", "agent_type": "x"}]}],
            "output_node": "a"
        });
        assert_eq!(detect_workflow_dag_version(&bare_loops), "v1.2");
        // 裸 body：节点含 compute → v1.2
        let bare_compute = serde_json::json!({
            "workflow_id": "w",
            "nodes": [
                {"id": "a", "agent_type": "x"},
                {"id": "c", "compute": {"function": "strcmp", "inputs": ["a", "a"], "mode": "equal"}}
            ],
            "output_node": "c"
        });
        assert_eq!(detect_workflow_dag_version(&bare_compute), "v1.2");
        // 空 loops 数组不构成 v1.2 特征（无其他特征 → v1.0）
        let empty_loops = serde_json::json!({
            "workflow_id": "w", "nodes": [{"id": "a", "agent_type": "x"}],
            "loops": [], "output_node": "a"
        });
        assert_eq!(detect_workflow_dag_version(&empty_loops), "v1.0");
    }

    #[test]
    fn test_validate_workflow_v12_materializes_after_engine_support() {
        // v1.2 物化门翻转：schema + 物化双门通过即放行（loop 展开为副本链）
        let doc = serde_json::json!({
            "workflow_id": "w",
            "nodes": [
                {"id": "a", "agent_type": "researcher", "task": "t",
                 "depends_on": ["lp_iter1_s"]}
            ],
            "loops": [{"id": "lp", "max_iterations": 2, "body": [
                {"id": "s", "agent_type": "researcher", "task": "t"}
            ]}],
            "output_node": "a"
        });
        assert_eq!(detect_workflow_dag_version(&doc), "v1.2");
        assert!(
            validate_workflow_dag(&doc).is_ok(),
            "v1.2 通过 schema + 物化双门后必须放行"
        );
        // load_workflow：v1.2 走物化器，loop 展开为 {loop_id}_iter{k}_{node_id} 副本链
        // （展开序 = 规格步骤 2→3：全局节点在前，循环副本在后）
        let wf = load_workflow(&doc).expect("v1.2 must materialize");
        let ids: Vec<&str> = wf.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "lp_iter0_s", "lp_iter1_s"]);
    }

    #[test]
    fn test_load_workflow_v10_v11_direct_deserialize() {
        // load_workflow 对 v1.0/v1.1 = schema 校验 + 直接反序列化（不物化）
        let v10 = serde_json::json!({
            "workflow_id": "w",
            "nodes": [{"id": "a", "agent_type": "researcher", "task": "t"}],
            "output_node": "a"
        });
        let wf = load_workflow(&v10).expect("v1.0 must load directly");
        assert_eq!(wf.nodes.len(), 1);
        assert_eq!(wf.output_node, "a");
    }

    #[test]
    fn test_validate_workflow_v12_schema_errors_surface() {
        // v1.2 schema 违规直接浮出（compute 节点带 agent_type 应报 schema 违规）
        let bad = serde_json::json!({
            "workflow_id": "w",
            "nodes": [
                {"id": "c", "agent_type": "x",
                 "compute": {"function": "strcmp", "inputs": ["a", "b"], "mode": "equal"}}
            ],
            "output_node": "c"
        });
        let errs = validate_workflow_dag(&bad).expect_err("schema violation must reject");
        assert!(!errs.is_empty(), "应报 schema 违规而非静默放行: {errs:?}");
    }

    #[test]
    fn test_validate_workflow_v10_v11_unchanged() {
        // 回归：v1.0/v1.1 文档不受 v1.2 分派与防呆门影响
        let v10 = serde_json::json!({
            "workflow_id": "w",
            "nodes": [{"id": "a", "agent_type": "x", "task": "t"}],
            "output_node": "a"
        });
        assert!(validate_workflow_dag(&v10).is_ok());
        let v11 = serde_json::json!({
            "workflow_id": "w",
            "nodes": [
                {"id": "a", "agent_type": "researcher", "task": "t"},
                {"id": "b", "agent_type": "writer", "task": "t", "depends_on": ["a"],
                 "run_when": {"node": "a", "op": "contains", "value": "APPROVE"}}
            ],
            "output_node": "b"
        });
        assert!(validate_workflow_dag(&v11).is_ok());
    }
}
