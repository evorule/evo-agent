// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! LLM 命名操作（决策点⑦ / 37 号）
//!
//! 供 evorule-rule（数据治理系统）作为客户端契约消费。暴露结构化命名操作端点，
//! 而非裸 prompt 接口——输出约束到 JSON schema、可校验、可审计、模型可插拔。
//!
//! - MVP 只实现三个 op：`draft_rule` / `gen_tests` / `explain_rule`
//!   （`patch_rule` / `query_corpus` 后置，对齐 30 号基线 §B⑦ 与 36 号回写通道后置）；
//! - 同步为主（`status=completed` 直接返回结果），响应体预留 `task_id`（平滑升异步）；
//! - 每次命名的 LLM 产出都带 `llm_generated` 溯源（model/op/timestamp）；
//! - **确定性边界**：只负责"起草/解释/测试生成"，产出为 Draft 草稿，绝不直写执行态
//!   （决策点⑦强约束，由 evorule-rule 侧 validate 的 LLM 边界强制）。

use std::time::SystemTime;

use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tracing::info;

use crate::api::agent_api::AgentApiState;
use crate::io_handler::IoHandler;
use crate::io_handlers::LlmHandler;

/// 命名操作标识
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// 起草：法规文本/需求 + 领域 → 候选规则 JSON（Draft）
    DraftRule,
    /// 测试生成：规则 JSON + 数据依赖契约 → test_cases
    GenTests,
    /// 解释：规则 JSON → 人类可读解释
    ExplainRule,
}

impl Operation {
    /// 从字符串解析（URL 路径段；非法 → Err）
    pub fn parse(s: &str) -> Result<Self, LlmOpsError> {
        match s {
            "draft_rule" => Ok(Operation::DraftRule),
            "gen_tests" => Ok(Operation::GenTests),
            "explain_rule" => Ok(Operation::ExplainRule),
            _ => Err(LlmOpsError::UnknownOperation(s.to_string())),
        }
    }

    /// 对应的动作名（幂等审计用）
    pub fn as_str(&self) -> &'static str {
        match self {
            Operation::DraftRule => "draft_rule",
            Operation::GenTests => "gen_tests",
            Operation::ExplainRule => "explain_rule",
        }
    }

    /// 该 op 要求的入参提示（用于构建 LLM prompt / 校验）
    pub fn prompt_instruction(&self) -> &'static str {
        match self {
            Operation::DraftRule => {
                "根据提供的法规文本/需求与领域，起草一条 evorule 原生 JSON 规则（rule_body）。\
                 输出必须是 JSON 对象，结构为 {\"rule_id\":\"...\",\"version\":\"0.1.0\",\"description\":\"...\",\"transform\":[...]}，\
                 不要输出任何解释文字，只输出 JSON。"
            }
            Operation::GenTests => {
                "根据提供的 evorule 原生规则 JSON 与其 data_dependencies（inputs/services 契约），\
                 生成 test_cases。输出必须是 JSON 对象 {\"test_cases\":[...]}，\
                 每条 test_case 形如 {\"name\":\"...\",\"input\":{...},\"expect\":{...}}。\
                 不要输出任何解释文字，只输出 JSON。"
            }
            Operation::ExplainRule => {
                "用中文清晰解释给定 evorule 规则 JSON 的行为与意图，供审计人/审批者/消费者理解。\
                 输出 JSON 对象 {\"explanation\":\"...\"}（可含 \n 换行，保存在字符串里）。\
                 不要输出 JSON 以外的文字。"
            }
        }
    }
}

/// 请求参数（各 op 共享骨架，37 号 §4）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmOpRequest {
    /// 模型标识（可插拔，模型标识随请求传）
    #[serde(default)]
    pub model: Option<String>,
    /// 幂等 / 审计
    #[serde(default)]
    pub request_id: Option<String>,
    /// 各 op 专属参数（draft_rule/gen_tests/explain_rule 各自字段）
    #[serde(default)]
    pub params: Value,
}

/// 37 号 §4 响应骨架
#[derive(Debug, Serialize)]
pub struct LlmOpResponse {
    pub operation: String,
    pub request_id: Option<String>,
    /// MVP 预留：将来异步任务的句柄（同步模式下为 `completed`，无实际队列）
    pub task_id: Option<String>,
    pub status: String,
    pub result: Value,
    pub errors: Option<String>,
    /// LLM 溯源（决策点⑦）：model/op/timestamp
    pub llm_generated: Value,
}

/// 命名操作错误
#[derive(Debug, Error)]
pub enum LlmOpsError {
    #[error("未知命名操作: {0}")]
    UnknownOperation(String),
    #[error("操作入参非法: {0}")]
    InvalidParams(String),
    #[error("LLM 调用失败: {0}")]
    LlmFailure(String),
    #[error("LLM 输出解析失败: {0}")]
    OutputParse(String),
}

impl LlmOpsError {
    fn status_code(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            LlmOpsError::UnknownOperation(_) => StatusCode::NOT_FOUND,
            LlmOpsError::InvalidParams(_) => StatusCode::BAD_REQUEST,
            LlmOpsError::LlmFailure(_) => StatusCode::BAD_GATEWAY,
            LlmOpsError::OutputParse(_) => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }
}

/// `POST /ops/{operation}` 处理器
///
/// 通用骨架：解析 op → 走到对应 handler。所有 op 共用同一套请求/响应契约（37 号 §4）。
pub async fn run_operation(
    State(state): State<AgentApiState>,
    Path(operation): Path<String>,
    axum::Json(req): axum::Json<LlmOpRequest>,
) -> Result<axum::Json<LlmOpResponse>, (axum::http::StatusCode, String)> {
    let op = Operation::parse(&operation).map_err(|e| (e.status_code(), e.to_string()))?;
    info!(operation = op.as_str(), request_id = ?req.request_id, "LLM named operation invoked");

    let model = req.model.as_deref().unwrap_or("default");
    let result = run_llm_op(&op, &req, model)
        .await
        .map_err(|e| (e.status_code(), e.to_string()))?;

    let timestamp = iso_now();
    Ok(axum::Json(LlmOpResponse {
        operation: op.as_str().to_string(),
        request_id: req.request_id.clone(),
        task_id: None, // MVP 同步主路径，异步预留在响应体
        status: "completed".to_string(),
        result,
        errors: None,
        llm_generated: json!({
            "model": model,
            "operation": op.as_str(),
            "timestamp": timestamp,
        }),
    }))
}

/// 按 op 分发：构建 LLM prompt，调用 `LlmHandler::execute`（同步 + mock 兼容），解析输出。
async fn run_llm_op(op: &Operation, req: &LlmOpRequest, model: &str) -> Result<Value, LlmOpsError> {
    // 构建给 LLM 的用户消息（含 op 指令 + 入参序列化）
    let payload = serde_json::to_string(&req.params).unwrap_or_else(|_| "{}".to_string());
    let user_prompt = format!("{}\n\n入参 JSON：\n{}", op.prompt_instruction(), payload);

    // 冒烟/离线测试开关：`EVO_AGENT_LLM_MOCK_CONTENT` 非空时用 mock handler，
    // 不访问外部 LLM（供端到端契约验证；未设置时走真实 `with_defaults`）。
    let llm = match std::env::var("EVO_AGENT_LLM_MOCK_CONTENT") {
        Ok(mock) => LlmHandler::mock(&mock),
        Err(_) => LlmHandler::with_defaults(),
    };
    let params = json!({
        "model": model,
        "prompt": user_prompt,
        "temperature": 0.2,
    });
    let tcb_params = crate::json_convert::serde_to_tcb(&params);

    let io = llm
        .execute(&tcb_params)
        .await
        .map_err(|e| LlmOpsError::LlmFailure(e))?;
    let content =
        io_to_content(&io).ok_or_else(|| LlmOpsError::LlmFailure("空响应".to_string()))?;

    // 各 op 的输出解析（去掉可能的 ```json / ``` 围栏）
    let cleaned = strip_code_fence(&content);
    let parsed: Value =
        serde_json::from_str(&cleaned).map_err(|e| LlmOpsError::OutputParse(e.to_string()))?;
    normalize_output(*op, parsed)
        .ok_or_else(|| LlmOpsError::OutputParse("输出结构不符合预期".to_string()))
}

/// 从 `IoResult`（JsonValue 包装的 LlmResponse JSON）提取 `content` 字符串。
fn io_to_content(io: &evorule_tcb::JsonValue) -> Option<String> {
    let serde_val = crate::json_convert::tcb_to_serde(io);
    serde_val
        .get("content")
        .and_then(|c| c.as_str())
        .map(ToOwned::to_owned)
}

/// 剥离常见的 ```json ... ``` 代码块围栏。
fn strip_code_fence(s: &str) -> String {
    let t = s.trim();
    let start_markers = ["```json", "```JSON", "```"];
    let mut body = t.to_string();
    for m in start_markers {
        if let Some(rest) = t.strip_prefix(m) {
            body = rest.trim_start().to_string();
            break;
        }
    }
    if body.ends_with("```") {
        body = body[..body.len() - 3].trim_end().to_string();
    }
    body
}

/// 归一化各 op 输出为契约结构。
fn normalize_output(op: Operation, v: Value) -> Option<Value> {
    match op {
        Operation::DraftRule => match v {
            Value::Object(_) => Some(json!({ "rule": v })),
            _ => None,
        },
        Operation::GenTests => match v.get("test_cases") {
            Some(Value::Array(_)) => Some(v),
            _ => None,
        },
        Operation::ExplainRule => match v.get("explanation") {
            Some(Value::String(_)) => Some(v),
            _ => None,
        },
    }
}

/// ISO-8601 UTC 时间戳（LLM 溯源）
fn iso_now() -> String {
    use std::time::UNIX_EPOCH;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}Z", secs) // MVP：以 epoch 秒 + 'Z' 占位，避免引第三方时间库
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_operation_parse() {
        assert_eq!(
            Operation::parse("draft_rule").unwrap(),
            Operation::DraftRule
        );
        assert_eq!(Operation::parse("gen_tests").unwrap(), Operation::GenTests);
        assert_eq!(
            Operation::parse("explain_rule").unwrap(),
            Operation::ExplainRule
        );
        assert!(Operation::parse("patch_rule").is_err());
        assert!(Operation::parse("nope").is_err());
    }

    #[test]
    fn test_strip_code_fence() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```\n[1]\n```"), "[1]");
    }

    #[test]
    fn test_normalize_output() {
        // draft_rule：对象 → 包成 {rule: ...}
        let r = normalize_output(
            Operation::DraftRule,
            json!({"rule_id": "x", "transform": []}),
        );
        assert!(r.unwrap().get("rule").is_some());

        // gen_tests：必须含 test_cases 数组
        let g = normalize_output(Operation::GenTests, json!({"test_cases": []}));
        assert!(g.is_some());
        assert!(normalize_output(Operation::GenTests, json!({"x": 1})).is_none());

        // explain_rule：必须含 explanation 字符串
        let e = normalize_output(Operation::ExplainRule, json!({"explanation": "解释"}));
        assert!(e.is_some());
        assert!(normalize_output(Operation::ExplainRule, json!({"x": 1})).is_none());
    }

    #[test]
    fn test_io_to_content() {
        let payload = json!({"content": "hello", "finish_reason": "stop"});
        let tcb = crate::json_convert::serde_to_tcb(&payload);
        assert_eq!(io_to_content(&tcb).as_deref(), Some("hello"));
    }
}
