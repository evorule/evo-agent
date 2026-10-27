// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! plan_submit —— 计划模式提交工具（交互批）——agent 把结构化计划交用户裁决。
//!
//! 计划模式下工具面收窄为只读族 + 本工具：agent 只读探查，形成计划后经
//! plan_submit 提交；runner 识别计划模式回合内本工具执行成功即收束回合并推
//! PlanSubmitted 帧（前端计划卡：批准 / 拒绝两键，首批不做在线改计划）。
//!
//! ## 形态
//!
//! 入参：`steps` 必填（1-30 项，每项 `title` 必填 + `detail` 可选），
//! `title`（整体标题）与 `notes`（补充说明/风险/权衡）可选。
//! 出参：`{"status":"submitted","plan_id","steps_count"}`；plan_id =
//! 规范化计划 JSON 的 BLAKE3 前 12 hex（同内容同 id，审计可关联）。
//!
//! ## 治理面
//!
//! 纯申报型工具（零文件/网络/子进程触点，与 ask_user 同族）：Standard
//! 免裁决、AutoPolicy 直通、不进 EXFIL_SCAN_TOOLS；本工具自身无副作用，
//! 计划落库走既有消息/工具调用账面（journal tool_invoked + tool_result）。

use serde_json::{json, Value};

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 工具名
pub const PLAN_SUBMIT_TOOL: &str = "plan_submit";

/// 步骤数上限（防 LLM 传超大结构挂住回喂轮）
pub const PLAN_MAX_STEPS: usize = 30;
/// 整体标题字符上限
pub const PLAN_MAX_TITLE_CHARS: usize = 300;
/// 单步标题字符上限
pub const PLAN_MAX_STEP_TITLE_CHARS: usize = 200;
/// 单步说明字符上限
pub const PLAN_MAX_STEP_DETAIL_CHARS: usize = 2000;
/// 补充说明字符上限
pub const PLAN_MAX_NOTES_CHARS: usize = 2000;

/// 计划提交工具（无状态）
#[derive(Clone, Default)]
pub struct PlanSubmitTool;

impl PlanSubmitTool {
    /// 构造（无状态工具，实例等价）
    pub fn new() -> Self {
        Self
    }
}

/// 入参校验 + 规范化（纯函数，fail-visible 一次列明全部问题）
///
/// 返回规范化计划 JSON：`{"title":String,"steps":[{"title","detail"}],"notes":String}`
/// （可选项缺省为空串，形态稳定供 plan_id 与前端渲染）
pub fn validate_plan_args(args: &Value) -> Result<Value, String> {
    let mut problems: Vec<String> = Vec::new();
    if !args.is_object() {
        return Err("args must be a JSON object".to_string());
    }
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
    if title.chars().count() > PLAN_MAX_TITLE_CHARS {
        problems.push(format!("title exceeds {PLAN_MAX_TITLE_CHARS} chars"));
    }
    let notes = args.get("notes").and_then(|v| v.as_str()).unwrap_or("");
    if notes.chars().count() > PLAN_MAX_NOTES_CHARS {
        problems.push(format!("notes exceeds {PLAN_MAX_NOTES_CHARS} chars"));
    }
    let Some(steps) = args.get("steps").and_then(|v| v.as_array()) else {
        return Err("steps is required and must be a non-empty array of {{title, detail?}}"
            .to_string());
    };
    if steps.is_empty() {
        problems.push("steps must contain at least 1 item".to_string());
    }
    if steps.len() > PLAN_MAX_STEPS {
        problems.push(format!("steps exceeds {PLAN_MAX_STEPS} items"));
    }
    let mut norm_steps: Vec<Value> = Vec::with_capacity(steps.len());
    for (i, s) in steps.iter().enumerate() {
        let st = s.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if st.trim().is_empty() {
            problems.push(format!("steps[{}].title is required", i + 1));
            continue;
        }
        if st.chars().count() > PLAN_MAX_STEP_TITLE_CHARS {
            problems.push(format!(
                "steps[{}].title exceeds {PLAN_MAX_STEP_TITLE_CHARS} chars",
                i + 1
            ));
        }
        let detail = s.get("detail").and_then(|v| v.as_str()).unwrap_or("");
        if detail.chars().count() > PLAN_MAX_STEP_DETAIL_CHARS {
            problems.push(format!(
                "steps[{}].detail exceeds {PLAN_MAX_STEP_DETAIL_CHARS} chars",
                i + 1
            ));
        }
        norm_steps.push(json!({"title": st.trim(), "detail": detail}));
    }
    if !problems.is_empty() {
        return Err(format!("invalid plan: {}", problems.join("; ")));
    }
    Ok(json!({
        "title": title.trim(),
        "steps": norm_steps,
        "notes": notes.trim(),
    }))
}

/// 确定性 plan_id：规范化计划 JSON 的 BLAKE3 前 12 hex（同内容同 id）
pub fn plan_id_for(canonical: &Value) -> String {
    let canonical_str =
        serde_json::to_string(canonical).unwrap_or_else(|_| "unserializable".to_string());
    let digest = blake3::hash(canonical_str.as_bytes());
    let hex = digest.to_hex();
    hex.as_str()[..12].to_string()
}

#[async_trait::async_trait]
impl ToolFunction for PlanSubmitTool {
    async fn call(&self, args: &Value) -> IoResult {
        let canonical = match validate_plan_args(args) {
            Ok(c) => c,
            Err(e) => return Ok(json!({"status": "error", "error": e})),
        };
        let plan_id = plan_id_for(&canonical);
        let steps_count = canonical
            .get("steps")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        Ok(json!({
            "status": "submitted",
            "plan_id": plan_id,
            "steps_count": steps_count,
            "note": "计划已提交给用户裁决;回合在此收束,等待用户批准或拒绝后再继续执行。",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_plan_normalizes_and_ids_deterministically() {
        let args = json!({
            "title": " 重构 ",
            "steps": [
                {"title": "读代码", "detail": "先读 runner.rs"},
                {"title": "改", }
            ],
            "notes": " 风险低 "
        });
        let canonical = validate_plan_args(&args).unwrap();
        assert_eq!(canonical["title"], "重构");
        assert_eq!(canonical["steps"][1]["detail"], "");
        let id1 = plan_id_for(&canonical);
        let id2 = plan_id_for(&validate_plan_args(&args).unwrap());
        assert_eq!(id1, id2, "same content must yield same plan_id");
        assert_eq!(id1.len(), 12);
    }

    #[test]
    fn empty_steps_rejected() {
        let err = validate_plan_args(&json!({"steps": []})).unwrap_err();
        assert!(err.contains("at least 1"));
        assert!(validate_plan_args(&json!({})).is_err());
        assert!(validate_plan_args(&json!({"steps": [{"title": ""}]})).is_err());
    }

    #[test]
    fn oversized_fields_rejected() {
        let big = "x".repeat(PLAN_MAX_STEP_TITLE_CHARS + 1);
        assert!(validate_plan_args(&json!({"steps": [{"title": big}]})).is_err());
        let many: Vec<Value> = (0..=PLAN_MAX_STEPS)
            .map(|i| json!({"title": format!("s{i}")}))
            .collect();
        assert!(validate_plan_args(&json!({"steps": many})).is_err());
    }

    #[tokio::test]
    async fn call_returns_submitted_shape() {
        let tool = PlanSubmitTool::new();
        let out = tool
            .call(&json!({"steps": [{"title": "a"}, {"title": "b"}]}))
            .await
            .unwrap();
        assert_eq!(out["status"], "submitted");
        assert_eq!(out["steps_count"], 2);
        assert!(out["plan_id"].as_str().is_some());
    }
}
