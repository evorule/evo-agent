// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 宪法 schema 校验桥（M7-B1/B2，2026-08-27；2026-09-22 收编薄封装化）
//!
//! 判定代码已收编至 [`evorule-constitution`] 共享组件（0.2.0：schema 编译期
//! 内嵌 + `Policy` 双模式降级策略，缺省 Strict）。属地执法义务要求判定代码
//! 必须是组件、禁止复制第二份——本模块自此只是**接入层**：
//! - workflow_dag 双版本分派（按文档形态分派，见 [`detect_workflow_dag_version`]）
//! - 校验结果形态转换（组件 `Violation` → 本仓 `Vec<String>`）
//!
//! 组件以内嵌 schema + 缺省 Strict 运行：v1.x schema 编译期随组件携带，
//! 部署/CI 零磁盘依赖，「宪法仓不可得」的整类环境缺陷不再存在；未知
//! kind/版本在 Strict 下 fail-fast（门禁不可降级）。
//!
//! 接入点：资产（或其 sidecar 标注层）反序列化后先过宪法 schema 全量校验，
//! 再进入业务门卫（`AgentDefinition::validate` 等）。

use std::sync::OnceLock;

use evorule_constitution::Constitution;

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
/// 1. body 显式携带 `$schema` 字段 → 按声明分派（指向 v1.1 用 v1.1，其余按 v1.0）
/// 2. 裸 body（运行时形态，无 `$schema`）→ 能力探测：任一节点含 `run_when`
///    即按 v1.1（条件分支是 v1.1 相对 v1.0 的唯一增量），否则 v1.0
fn detect_workflow_dag_version(body: &serde_json::Value) -> &'static str {
    if let Some(url) = body.get("$schema").and_then(|v| v.as_str()) {
        return if url.ends_with("/workflow_dag/v1.1.json") {
            "v1.1"
        } else {
            "v1.0"
        };
    }
    let uses_run_when = body
        .get("nodes")
        .and_then(|v| v.as_array())
        .is_some_and(|nodes| nodes.iter().any(|n| n.get("run_when").is_some()));
    if uses_run_when {
        "v1.1"
    } else {
        "v1.0"
    }
}

/// 用 workflow_dag 校验裸文档（无壳 body）。
///
/// 按 [`detect_workflow_dag_version`] 分派 v1.0/v1.1 校验器（双版本并存窗口）。
pub fn validate_workflow_dag(body: &serde_json::Value) -> Result<(), Vec<String>> {
    let version = detect_workflow_dag_version(body);
    validate_kind_version("workflow_dag", version, body)
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
}
