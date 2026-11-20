// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 临界机验证工具（工具面统一架构，rule_tools 族新件）：
//! `criticality_verify` —— 测量命题 → server `POST /v1/criticality/verify`
//! 确定性四态判定回路。
//!
//! # 语义纪律（判定只读铁律的 agent 侧镜像）
//! - 工具**只读回判定**：`stable` = 数值主张与参照系相容可放行（至少一次
//!   实际容差/区间判定执行）；`alert` / `contradiction` = 主张与常量/定律
//!   参照系不符，须回炉重生成或转人工提案；`unverifiable` = 零次实际评估
//!   （全部跳过或无候选），如实上报「无法验证」**不得**当作已验证放行，
//!   也不得将 verdict 写入任何持久状态替代人工闸；
//! - server 未装配参照系（503 `law_host_not_configured`）时如实告知
//!   「本环境无参照系」不猜不编造；422 结构化拒因原样透传（agent 据此
//!   修正主张形态后可重试）；
//! - 溯源铁律 2 由参数面前置：`source` / `measured_at` 必填，缺失即拒。
//!
//! # 安全面
//! 无 shell、无 fs 面；仅消费既有受保护端点（鉴权沿用 EvoruleApiClient
//! 现行 token 机制）；steps 截断帽防上下文爆炸（审计证据链保头截尾，
//! 截断事实显式标注）。

use serde_json::{json, Value};

use crate::api::evorule_client::EvoruleApiClient;
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use std::sync::Arc;

/// verify_request v1.0 命题文档 `$schema`（SSOT = evorule-server
/// core/criticality 内嵌 schema；此为请求面常量镜像）
const VERIFY_REQUEST_SCHEMA: &str =
    "https://evorule.org/schemas/criticality/verify_request/v1.0.json";

/// steps 截断帽：超出即保头截尾并显式标注（防海量证据链撑爆上下文；
/// 64 主张 × 64 定律的极端证据链可到数百步，全量回灌无增益）
const MAX_STEPS_RETURNED: usize = 40;

/// 单条主张最小形态校验（深层结构/单位/数值边界交 server 422 结构化报因，
/// 这里只挡明显畸形，降低无谓往返）
fn validate_claim_shape(claims: &[Value]) -> Result<(), String> {
    for (i, c) in claims.iter().enumerate() {
        let obj = c
            .as_object()
            .ok_or_else(|| format!("claims[{i}] must be an object"))?;
        for key in ["symbol", "value", "si_unit"] {
            if !obj.contains_key(key) {
                return Err(format!("claims[{i}] missing required field `{key}`"));
            }
        }
        if obj.get("symbol").and_then(|v| v.as_str()).is_none() {
            return Err(format!("claims[{i}].symbol must be a string"));
        }
    }
    Ok(())
}

/// server 错误体（`{"error":{code,message,detail}}`）→ 可读结构化输出。
///
/// 503（law_host 未装配）与 422（结构化拒因）都是业务结果：如实透传 +
/// 附处置提示（agent 据此修正形态或如实告知用户），不猜不编造。
fn error_report(report: &Value) -> Value {
    let err = report.get("error").cloned().unwrap_or(Value::Null);
    let code = err
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let message = err
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("verification endpoint returned an error")
        .to_string();
    let hint = if code == "law_host_not_configured" {
        "law_host 未配置：本环境未装配 law_set/constant_set 参照系，无法验证——\
         如实告知用户当前环境无参照系，不得编造判定。"
    } else {
        "命题被拒（422）：按 detail 逐条修正主张形态（symbol/value/si_unit/\
         uncertainty）后可重试。"
    };
    json!({
        "verified": false,
        "error_code": code,
        "error": message,
        "detail": err.get("detail").cloned().unwrap_or(Value::Null),
        "hint": hint,
    })
}

/// 临界机验证工具（只读判定回路）
pub struct CriticalityVerifyTool {
    ev: EvoruleApiClient,
}

impl CriticalityVerifyTool {
    pub fn new(ev: EvoruleApiClient) -> Self {
        Self { ev }
    }
}

#[async_trait::async_trait]
impl ToolFunction for CriticalityVerifyTool {
    async fn call(&self, args: &Value) -> IoResult {
        // 溯源铁律 2 前置：source / measured_at 必填（缺失即拒，不代填不猜）
        let subject = args
            .get("subject")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or("missing required parameter: subject (what the proposition is about)")?
            .to_string();
        let source = args
            .get("source")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or("missing required parameter: source (measurement provenance)")?
            .to_string();
        let measured_at = args
            .get("measured_at")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or("missing required parameter: measured_at (when the measurement happened)")?
            .to_string();
        let claims = args
            .get("claims")
            .and_then(|v| v.as_array())
            .filter(|a| !a.is_empty())
            .ok_or("missing required parameter: claims (non-empty array)")?;
        validate_claim_shape(claims)?;

        let mut context = json!({ "source": source, "measured_at": measured_at });
        if let Some(m) = args.get("method").and_then(|v| v.as_str()) {
            context["method"] = json!(m);
        }
        if let Some(k) = args.get("k_sigma") {
            if !k.is_null() {
                context["k_sigma"] = k.clone();
            }
        }
        if let Some(p) = args.get("propagation") {
            if !p.is_null() {
                context["propagation"] = p.clone();
            }
        }
        let body = json!({
            "$schema": VERIFY_REQUEST_SCHEMA,
            "subject": subject,
            "claims": claims,
            "context": context,
        });

        let report = self
            .ev
            .verify_criticality(&body)
            .await
            .map_err(|e| format!("criticality verify failed: {e}"))?;

        if report.get("error").is_some() {
            return Ok(error_report(&report));
        }

        // steps 截断帽（保头截尾 + 截断事实显式标注，审计面可知不完整）
        let mut out = report;
        if let Some(steps) = out.get_mut("steps").and_then(|v| v.as_array_mut()) {
            let total = steps.len();
            if total > MAX_STEPS_RETURNED {
                steps.truncate(MAX_STEPS_RETURNED);
                out["steps_total"] = json!(total);
                out["steps_truncated"] = json!(true);
            }
        }
        Ok(out)
    }
}

/// 注册临界机验证工具（full_rule_toolkit 装配点）
pub fn register(h: &mut ToolHandler, ev: &EvoruleApiClient) {
    h.register_static(
        "criticality_verify",
        Arc::new(CriticalityVerifyTool::new(ev.clone())),
    );
}

/// 临界机验证工具 spec（LLM 可见面；语义纪律写进描述兜底）
pub fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "criticality_verify".to_string(),
        description: "Verify a measurement claim proposition against the server's assembled \
                      reference frame (laws + constants) via the deterministic criticality \
                      pipeline. Returns verdict `stable` (claim consistent — may proceed), \
                      `alert` (constant out of bounds — do NOT persist; surface the proposal \
                      package to the user for human review), `contradiction` (law violated \
                      — regenerate or revise the claim), or `unverifiable` (no tolerance or \
                      interval judgment could be executed — report honestly that the claim \
                      is unverified, never treat it as verified; gather the missing inputs \
                      such as uncertainty declarations and retry). The verdict is advisory \
                      evidence for a human gate: never write it into durable state as a \
                      substitute for human approval. If the response carries error_code \
                      `law_host_not_configured`, honestly report that no reference frame is \
                      assembled in this environment; on 422 rejections, fix the claim shape \
                      per `detail` and retry. Read-only."
            .to_string(),
        parameters: vec![
            ParameterSpec {
                name: "subject".to_string(),
                r#type: "string".to_string(),
                description: "What the proposition is about (e.g. 'earth surface gravity \
                              recheck')."
                    .to_string(),
                required: true,
            },
            ParameterSpec {
                name: "claims".to_string(),
                r#type: "array".to_string(),
                description: "Measured claims: array of {symbol, value, si_unit, uncertainty?} \
                              (uncertainty in the same SI unit, optional but required for \
                              tolerance judgments)."
                    .to_string(),
                required: true,
            },
            ParameterSpec {
                name: "source".to_string(),
                r#type: "string".to_string(),
                description: "Provenance of the measurement (required: no-source claims are \
                              rejected)."
                    .to_string(),
                required: true,
            },
            ParameterSpec {
                name: "measured_at".to_string(),
                r#type: "string".to_string(),
                description: "When the measurement happened (e.g. ISO-8601 timestamp).".to_string(),
                required: true,
            },
            ParameterSpec {
                name: "method".to_string(),
                r#type: "string".to_string(),
                description: "Optional measurement method note (provenance enrichment)."
                    .to_string(),
                required: false,
            },
            ParameterSpec {
                name: "k_sigma".to_string(),
                r#type: "number".to_string(),
                description: "Optional interval width factor (0 < k <= 100, default 2)."
                    .to_string(),
                required: false,
            },
            ParameterSpec {
                name: "propagation".to_string(),
                r#type: "string".to_string(),
                description: "Optional uncertainty propagation mode for law-substitution \
                              tolerance: `none` (default, use declared uncertainty) or \
                              `gum_monomial` (exact GUM closed-form propagation for monomial \
                              law forms; falls back to the declared uncertainty with a \
                              recorded basis when not propagable)."
                    .to_string(),
                required: false,
            },
        ],
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_shape_validation() {
        let ok = json!([
            { "symbol": "F", "value": 10.0, "si_unit": "newton", "uncertainty": 0.5 }
        ]);
        assert!(validate_claim_shape(ok.as_array().unwrap()).is_ok());

        let missing_value = json!([
            { "symbol": "F", "si_unit": "newton" }
        ]);
        let err = validate_claim_shape(missing_value.as_array().unwrap()).unwrap_err();
        assert!(
            err.contains("missing required field `value`"),
            "err = {err}"
        );

        let not_object = json!(["F"]);
        let err = validate_claim_shape(not_object.as_array().unwrap()).unwrap_err();
        assert!(err.contains("must be an object"), "err = {err}");

        let bad_symbol = json!([
            { "symbol": 3, "value": 1.0, "si_unit": "m" }
        ]);
        let err = validate_claim_shape(bad_symbol.as_array().unwrap()).unwrap_err();
        assert!(err.contains("symbol must be a string"), "err = {err}");
    }

    #[test]
    fn error_report_shape_and_hints() {
        let e503 = json!({
            "error": {
                "code": "law_host_not_configured",
                "message": "law_set 装配未配置",
                "detail": Value::Null,
            }
        });
        let out = error_report(&e503);
        assert_eq!(out["verified"], false);
        assert_eq!(out["error_code"], "law_host_not_configured");
        assert!(
            out["hint"].as_str().unwrap().contains("law_host 未配置"),
            "hint = {}",
            out["hint"]
        );

        let e422 = json!({
            "error": {
                "code": "verify_rejected",
                "message": "命题结构拒收（1 项）",
                "detail": { "errors": ["x"] },
            }
        });
        let out = error_report(&e422);
        assert_eq!(out["error_code"], "verify_rejected");
        assert_eq!(out["detail"]["errors"][0], "x");
        assert!(
            out["hint"].as_str().unwrap().contains("422"),
            "hint = {}",
            out["hint"]
        );
    }

    #[test]
    fn spec_shape() {
        let specs = specs();
        assert_eq!(specs.len(), 1);
        let s = &specs[0];
        assert_eq!(s.name, "criticality_verify");
        let required: Vec<&str> = s
            .parameters
            .iter()
            .filter(|p| p.required)
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(
            required,
            vec!["subject", "claims", "source", "measured_at"],
            "必填面 = 溯源铁律 2 + 命题主体"
        );
        assert!(
            s.description.contains("never write it into durable state"),
            "判定只读语义应进描述"
        );
        assert!(
            s.description.contains("`unverifiable`"),
            "第四态判定语义应进描述"
        );
    }
}
