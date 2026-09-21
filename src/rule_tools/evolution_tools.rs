// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 进化信号只读消费面 ＋ 约束层晋升提名（2 个工具）
//!
//! - evolution_signals：拉取指定会话的违规信号聚合摘要
//!   （GET /api/sessions/{id}/evolution-signals，只读）；
//! - rule_promote：把起草产物提名为约束层晋升（POST /api/publish/queue，
//!   kind 硬编码 meta_promotion；进人审队列，本工具不提供任何审批能力）；
//! - [`render_evolution_signals_summary`]：信号 → 人类可读摘要的渲染函数，
//!   evolution_signals 工具与前馈感知段共用同一模板源。
//!
//! 边界纪律：evolution_signals 只做信号的**读取与陈述**（展示层）；rule_promote
//! 只做**提名转发**（kind 硬编码 meta_promotion，LLM 无法改道 normal 通道），
//! 审批/落盘全部在治理链人审闭环内，agent 面不存在审批通道。

use std::sync::Arc;

use serde_json::Value;

use crate::api::evorule_client::EvoruleApiClient;
use crate::api::workspace_client::{SubmitPublishRequest, WorkspaceApiClient};
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};

/// 无进化信号时的明示文本（工具面不返回空串）
pub const NO_SIGNALS_TEXT: &str = "当前无进化信号。";

/// 渲染进化信号摘要（evolution_signals 工具的单一模板源）
///
/// 输入为 `GET /api/sessions/{id}/evolution-signals` 响应。返回 `None` =
/// 无信号，调用方按语义处理（工具返回明示文本）。
pub fn render_evolution_signals_summary(resp: &Value) -> Option<String> {
    let signals = resp.get("signals").and_then(|s| s.as_array())?;
    if signals.is_empty() {
        return None;
    }
    let total = resp
        .get("total_violations")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let session_id = resp.get("session_id").and_then(|s| s.as_u64()).unwrap_or(0);
    let mut out = String::new();
    out.push_str(&format!("【进化信号（会话 {session_id}）】\n"));
    out.push_str(&format!(
        "审计链共记录 {total} 条违规拦截，聚合为以下信号：\n"
    ));
    for s in signals {
        let rule_ref = s.get("rule_ref").and_then(|r| r.as_str()).unwrap_or("?");
        let count = s.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
        let last_version = s.get("last_version").and_then(|v| v.as_u64()).unwrap_or(0);
        let instr = s
            .get("last_instr_type")
            .and_then(|i| i.as_str())
            .unwrap_or("unknown");
        let reason = s
            .get("reason_summary")
            .and_then(|r| r.as_str())
            .unwrap_or("");
        out.push_str(&format!(
            "- {rule_ref} ×{count}（末次: v{last_version}，指令: {instr}）— {reason}\n"
        ));
    }
    if let Some(queue) = resp.get("queue") {
        let normal = queue
            .get("pending_normal")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let promo = queue
            .get("pending_meta_promotion")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        out.push_str(&format!(
            "治理队列现状：待审普通规则 {normal} 条；待审约束层晋升 {promo} 条。\n"
        ));
    }
    out.push_str(
        "提示：起草改进规则使用 rule_create；约束层变更须经治理链提名并人工审批，不得旁路。\n",
    );
    Some(out)
}

/// 解析 session_id 参数（宽容数字/数字字符串，对齐会话端点 u64 口径）
fn parse_session_id(args: &Value) -> Result<u64, String> {
    match args.get("session_id") {
        None | Some(Value::Null) => Err("missing required parameter: session_id".to_string()),
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| "session_id must be a non-negative integer".to_string()),
        Some(Value::String(s)) => s
            .parse::<u64>()
            .map_err(|_| "session_id must be a valid integer string".to_string()),
        Some(_) => Err("session_id must be a non-negative integer".to_string()),
    }
}

// =============================================================================
// evolution_signals —— 会话违规信号聚合摘要
// =============================================================================

#[derive(Clone)]
pub struct EvolutionSignalsTool {
    client: EvoruleApiClient,
}

impl EvolutionSignalsTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for EvolutionSignalsTool {
    async fn call(&self, args: &Value) -> IoResult {
        let session_id = parse_session_id(args)?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        if args.get("limit").is_some() && args.get("limit") != Some(&Value::Null) && limit.is_none()
        {
            return Err("limit must be a non-negative integer".to_string());
        }
        let resp = self
            .client
            .get_evolution_signals(session_id, limit)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Value::String(
            render_evolution_signals_summary(&resp).unwrap_or_else(|| NO_SIGNALS_TEXT.to_string()),
        ))
    }
}

// =============================================================================
// rule_promote —— 约束层晋升提名（治理链 enqueue,人审闭环）
// =============================================================================

/// 解析 rule_version_ids 参数（宽容单字符串/字符串数组两种形态）
fn parse_rule_version_ids(args: &Value) -> Result<Vec<String>, String> {
    match args.get("rule_version_ids") {
        None | Some(Value::Null) => Err("missing required parameter: rule_version_ids".to_string()),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(arr)) => {
            let ids: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            if ids.is_empty() || ids.len() != arr.len() {
                return Err("rule_version_ids must be a non-empty array of strings".to_string());
            }
            Ok(ids)
        }
        Some(_) => Err("rule_version_ids must be a string or an array of strings".to_string()),
    }
}

fn require_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required parameter: {key}"))
}

#[derive(Clone)]
pub struct RulePromoteTool {
    client: WorkspaceApiClient,
}

impl RulePromoteTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RulePromoteTool {
    async fn call(&self, args: &Value) -> IoResult {
        let workspace_id = require_str(args, "workspace_id")?;
        let rule_version_ids = parse_rule_version_ids(args)?;
        // 转写产物必填：约束层内容 JSON 字符串（服务端做 schema/门禁前置校验）
        let meta_rule_content = require_str(args, "meta_rule_content")?;
        let submitted_by = require_str(args, "submitted_by")?;
        let role = require_str(args, "role")?;
        // 闸门一硬约束：meta_promotion 必须在提交时关联已关闭的沙盒测试
        // （服务端审批 fail-closed，缺失必死路）。容忍 LLM 将数字字符串化的常见形态。
        let raw = args
            .get("test_report_sandbox_id")
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                "missing required parameter: test_report_sandbox_id (gate-one evidence: run \
                 sandbox_start + sandbox_close on the source rule version first, then pass \
                 the closed sandbox id here)"
                    .to_string()
            })?;
        let test_report_sandbox_id = match raw {
            Value::Number(n) => n.as_i64().ok_or_else(|| {
                "test_report_sandbox_id must be an integer sandbox id".to_string()
            })?,
            Value::String(s) => s
                .trim()
                .parse::<i64>()
                .map_err(|_| "test_report_sandbox_id must be an integer sandbox id".to_string())?,
            _ => return Err("test_report_sandbox_id must be an integer sandbox id".to_string()),
        };
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // kind 硬编码 meta_promotion：提名工具无改道 normal 通道的口子（防旁路）
        let req = SubmitPublishRequest {
            workspace_id,
            rule_version_ids,
            test_report_sandbox_id: Some(test_report_sandbox_id),
            description,
            kind: Some("meta_promotion".to_string()),
            meta_rule_content: Some(meta_rule_content),
        };
        let item = self
            .client
            .submit_publish(req, &submitted_by, &role)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&item).unwrap_or_default();
        Ok(v)
    }
}

// =============================================================================
// register / specs
// =============================================================================

pub fn register(h: &mut ToolHandler, ws: &WorkspaceApiClient, ev: &EvoruleApiClient) {
    h.register_tool(
        "evolution_signals",
        Arc::new(EvolutionSignalsTool::new(ev.clone())),
    );
    h.register_tool("rule_promote", Arc::new(RulePromoteTool::new(ws.clone())));
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "evolution_signals".to_string(),
            description: "Fetch the aggregated violation signals of a session (read-only) — \
                          which rules keep causing enforce-blocked violations, how often, and \
                          what is pending in the governance queue."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "session_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Session id (integer or integer string).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "limit".to_string(),
                    r#type: "integer".to_string(),
                    description: "Max number of signals to return (non-negative; 0 = unbounded)."
                        .to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "rule_promote".to_string(),
            description: "Nominate a drafted rule for meta-promotion into the L2 constraint \
                          layer via the governance publish queue (kind is fixed to \
                          meta_promotion; a human reviewer must approve before it takes \
                          effect — no bypass). The reviewer's gate requires sandbox \
                          evidence: run sandbox_start + sandbox_close on the source rule \
                          version first, then pass the closed sandbox id here."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace the source rule belongs to.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_version_ids".to_string(),
                    r#type: "array".to_string(),
                    description: "Source rule version id(s) (string or array of strings); \
                                  recorded as promotion provenance."
                        .to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "meta_rule_content".to_string(),
                    r#type: "string".to_string(),
                    description: "Translated meta-rule JSON (string) with metadata.tier and \
                                  transforms; server validates and authority-fills provenance."
                        .to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "submitted_by".to_string(),
                    r#type: "string".to_string(),
                    description: "Identity of the nominator (recorded for audit).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "role".to_string(),
                    r#type: "string".to_string(),
                    description: "Submitter role accepted by the governance queue \
                                  (\"department_head\" or \"admin\")."
                        .to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "test_report_sandbox_id".to_string(),
                    r#type: "integer".to_string(),
                    description: "Sandbox id (i64) of a CLOSED sandbox test run on the source \
                                  rule version; gate-one review fails closed without it. \
                                  Accepts an integer or numeric string."
                        .to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "description".to_string(),
                    r#type: "string".to_string(),
                    description: "Optional nomination note for the reviewer.".to_string(),
                    required: false,
                },
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ev_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:0")
    }

    fn make_ws_client() -> WorkspaceApiClient {
        WorkspaceApiClient::new("http://localhost:0")
    }

    fn sample_response() -> Value {
        serde_json::json!({
            "session_id": 42,
            "total_violations": 5,
            "signals": [
                {
                    "kind": "violation",
                    "rule_ref": "rule_index=0",
                    "reason_summary": "运动指令缺少安全清场前置",
                    "count": 5,
                    "last_version": 23,
                    "last_instr_type": "branch"
                }
            ],
            "queue": { "pending_normal": 1, "pending_meta_promotion": 0 }
        })
    }

    #[test]
    fn test_render_contains_signals_and_queue() {
        let text = render_evolution_signals_summary(&sample_response()).expect("有信号应渲染");
        assert!(text.contains("会话 42"));
        assert!(text.contains("5 条违规拦截"));
        assert!(text.contains("rule_index=0"));
        assert!(text.contains("×5"));
        assert!(text.contains("v23"));
        assert!(text.contains("branch"));
        assert!(text.contains("运动指令缺少安全清场前置"));
        assert!(text.contains("待审普通规则 1 条"));
        assert!(text.contains("待审约束层晋升 0 条"));
        assert!(text.contains("人工审批"));
    }

    #[test]
    fn test_render_empty_signals_returns_none() {
        let empty = serde_json::json!({
            "session_id": 7,
            "total_violations": 0,
            "signals": [],
            "queue": { "pending_normal": 0, "pending_meta_promotion": 0 }
        });
        assert!(render_evolution_signals_summary(&empty).is_none());
    }

    #[test]
    fn test_render_no_internal_numbering_tokens() {
        // 公开面纪律：模板文本禁内部编号字样（方案号/批次号模式）
        let text = render_evolution_signals_summary(&sample_response()).unwrap();
        for token in ["UV-", "P0", "P1", "P2", "W1", "W2", "W3", "号文", "批 "] {
            assert!(!text.contains(token), "模板文本不得含内部编号字样: {token}");
        }
    }

    #[test]
    fn test_specs_shape() {
        let specs = specs();
        assert_eq!(specs.len(), 2, "evolution_tools 应含 2 个工具 spec");
        assert_eq!(specs[0].name, "evolution_signals");
        assert_eq!(specs[0].parameters.len(), 2);
        assert!(specs[0].parameters[0].required);
        assert!(!specs[0].parameters[1].required);
        // rule_promote：kind 不暴露给 LLM（防旁路 normal 通道），人审必需参数齐全
        assert_eq!(specs[1].name, "rule_promote");
        let names: Vec<&str> = specs[1]
            .parameters
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        for expected in [
            "workspace_id",
            "rule_version_ids",
            "meta_rule_content",
            "submitted_by",
            "role",
        ] {
            assert!(names.contains(&expected), "rule_promote 缺参数 {expected}");
        }
        assert!(!names.contains(&"kind"), "kind 不得暴露给 LLM（防旁路）");
    }

    #[test]
    fn test_register_tools() {
        let mut h = ToolHandler::new();
        register(&mut h, &make_ws_client(), &make_ev_client());
        assert!(h.has_tool("evolution_signals"));
        assert!(h.has_tool("rule_promote"));
    }

    #[test]
    fn test_missing_session_id_is_error() {
        let err = parse_session_id(&serde_json::json!({})).unwrap_err();
        assert!(err.contains("missing required parameter: session_id"));
    }

    #[test]
    fn test_parse_session_id_accepts_number_and_string() {
        assert_eq!(
            parse_session_id(&serde_json::json!({"session_id": 42})).unwrap(),
            42
        );
        assert_eq!(
            parse_session_id(&serde_json::json!({"session_id": "42"})).unwrap(),
            42
        );
        assert!(parse_session_id(&serde_json::json!({"session_id": "abc"})).is_err());
        assert!(parse_session_id(&serde_json::json!({"session_id": -1})).is_err());
    }

    #[test]
    fn test_parse_rule_version_ids_flex_shapes() {
        assert_eq!(
            parse_rule_version_ids(&serde_json::json!({"rule_version_ids": "rv7"})).unwrap(),
            vec!["rv7".to_string()]
        );
        assert_eq!(
            parse_rule_version_ids(&serde_json::json!({"rule_version_ids": ["rv1", "rv2"]}))
                .unwrap(),
            vec!["rv1".to_string(), "rv2".to_string()]
        );
        assert!(parse_rule_version_ids(&serde_json::json!({})).is_err());
        assert!(parse_rule_version_ids(&serde_json::json!({"rule_version_ids": []})).is_err());
        assert!(parse_rule_version_ids(&serde_json::json!({"rule_version_ids": [1, 2]})).is_err());
    }

    #[tokio::test]
    async fn test_rule_promote_missing_required_params_fail_fast() {
        // 缺任一必需参数 → 错误文本，不发请求
        let tool = RulePromoteTool::new(make_ws_client());
        let base = serde_json::json!({
            "workspace_id": "ws1",
            "rule_version_ids": ["rv1"],
            "meta_rule_content": "{\"metadata\":{\"tier\":\"constraint\"}}",
            "submitted_by": "agent-01",
            "role": "DepartmentHead"
        });
        for key in [
            "workspace_id",
            "rule_version_ids",
            "meta_rule_content",
            "submitted_by",
            "role",
        ] {
            let mut args = base.clone();
            args.as_object_mut().unwrap().remove(key);
            let err = tool.call(&args).await.unwrap_err();
            assert!(
                err.contains(&format!("missing required parameter: {key}")),
                "缺 {key} 应报缺参错误，实际: {err}"
            );
        }
    }

    #[tokio::test]
    async fn test_evolution_signals_fail_soft_on_client_error() {
        // 端点不可达（localhost:0 连接失败）→ 工具返回 Err 文本，不 panic
        let tool = EvolutionSignalsTool::new(make_ev_client());
        let args = serde_json::json!({"session_id": 42});
        let result = tool.call(&args).await;
        assert!(result.is_err(), "拉取失败应向调用方透出错误而非空结果");
    }
}
