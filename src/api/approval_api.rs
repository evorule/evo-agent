// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 批次一 PR-4:审批策略工作台 REST 面 —— 条件文件状态读取与试判定
//!
//! 两端点挂 `agent_api::router` 的 route 链(G16 鉴权中间件自动覆盖),
//! 服务的是**人**在工作台设置页审批策略分区的直接管理操作(与
//! [`crate::api::mcp_api`] 同款定位:管理面,不进 agent 会话审计链)。
//!
//! ## 端点一览
//!
//! - `GET  /api/approval-policy`         → 条件文件状态(存在性/解析态/条件组/限额)
//! - `POST /api/approval-policy/trial`   → 试判定(机械预检,零 LLM 调用)
//!
//! ## 试判定语义(与决策端同口径的机械前半段)
//!
//! 运行时 policy 决策端([`crate::agent::approval_policy::PolicyReview`])的
//! 前半段是纯机械判定:红线豁免检查 → 条件清单拉取 → 修正限额。本端点把
//! 这段机械逻辑暴露给人类,用于在配置条件文件后预览「某操作会走哪条路」;
//! LLM 评审判定(后半段)只在真实运行时发生,试判定不代判、不落账。

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::approval_policy::{hits_red_line, ApprovalPolicyFile, PolicyFileError};
use crate::api::agent_api::AgentApiState;

/// 条件文件路径:{workdir}/.evo/approval-policy.toml(与 serve/CLI 接线层同源)
fn policy_path(state: &AgentApiState) -> std::path::PathBuf {
    state.workdir().join(".evo").join("approval-policy.toml")
}

/// 试判定请求体
#[derive(Debug, Deserialize)]
pub struct TrialBody {
    /// 工具名(如 shell_exec)
    pub tool: String,
    /// 操作参数(与运行时 ApprovalRequest.args 同形态,如 {"command": "..."})
    #[serde(default)]
    pub args: Value,
}

/// 机械预检结果(工作台渲染三色:forced_manual=红/reviewed=黄/fallback_manual=灰)
#[derive(Debug)]
struct TrialOutcome {
    /// forced_manual | reviewed | fallback_manual | file_missing | file_invalid
    status: &'static str,
    /// 红线标记(red_line.delete 等,无则 None)
    red_line: Option<String>,
    /// 该工具组的条件清单
    conditions: Vec<String>,
    /// 说明(给人看的一句话)
    note: String,
}

/// 机械预检:红线豁免 → 条件拉取 → 限额。与 PolicyReview::review 前半段同口径,
/// 纯函数零 IO(除读条件文件),不调 LLM、不落账。
fn trial(
    policy: Option<&ApprovalPolicyFile>,
    override_flag: bool,
    tool: &str,
    args: &Value,
) -> TrialOutcome {
    let limits = policy.map(|p| {
        (
            p.policy_limits.max_corrections,
            p.policy_limits.max_actions_per_run,
        )
    });

    let red_line = hits_red_line(tool, args);
    let conditions: Vec<String> = policy
        .map(|p| p.conditions_for(tool).to_vec())
        .unwrap_or_default();

    let (status, note) = match (policy, red_line) {
        (None, _) => (
            "file_missing",
            "条件文件不存在(.evo/approval-policy.toml 未预置)——policy 档运行时降级 manual".to_string(),
        ),
        (Some(_), Some(rl)) if !override_flag => (
            "forced_manual",
            format!(
                "红线命中({})且豁免覆盖未开启——任何档位下强制人工审批,评审不发生",
                rl.as_str()
            ),
        ),
        (Some(_), Some(rl)) => (
            "reviewed",
            format!(
                "红线命中({})但豁免覆盖已开启——交由评审按条件判定,判定留痕携带 exemption_overridden 标记",
                rl.as_str()
            ),
        ),
        (Some(_), None) if conditions.is_empty() => (
            "fallback_manual",
            "该工具未预置条件——policy 档运行时降级 manual(空条件清单无法评审)".to_string(),
        ),
        (Some(_), None) => (
            "reviewed",
            "交由评审按条件逐条判定(全部 pass 放行;rejected 带修正指令回喂,重查超限回落人工)".to_string(),
        ),
    };

    let _ = limits; // 限额随响应整体透出,不影响状态机
    TrialOutcome {
        status,
        red_line: red_line.map(|r| r.as_str().to_string()),
        conditions,
        note,
    }
}

/// `GET /api/approval-policy` —— 条件文件状态
///
/// 返回 {exists, parse_error, groups, limits, override}:
/// - exists=false:文件未预置(policy 档运行时降级 manual,这是合法态)
/// - parse_error 非空:文件存在但 TOML/schema 失败(fail-visible 显式留痕)
/// - groups:工具组 → 条件清单;limits:修正/操作限额
/// - override:approval.exemptionOverride 当前合并值(决策端同源)
pub async fn get_policy(State(state): State<AgentApiState>) -> (StatusCode, Json<Value>) {
    let override_flag = state
        .workbench_settings()
        .merged()
        .0
        .get("approval.exemptionOverride")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match ApprovalPolicyFile::load(&policy_path(&state)) {
        Ok(p) => {
            let groups: serde_json::Map<String, Value> = p
                .groups
                .iter()
                .map(|(k, g)| (k.clone(), json!(g.conditions)))
                .collect();
            (
                StatusCode::OK,
                Json(json!({
                    "exists": true,
                    "parse_error": Value::Null,
                    "groups": groups,
                    "limits": {
                        "max_corrections": p.policy_limits.max_corrections,
                        "max_actions_per_run": p.policy_limits.max_actions_per_run,
                    },
                    "override": override_flag,
                })),
            )
        }
        Err(PolicyFileError::NotFound(_)) => (
            StatusCode::OK,
            Json(json!({
                "exists": false,
                "parse_error": Value::Null,
                "groups": {},
                "limits": { "max_corrections": 3, "max_actions_per_run": 200 },
                "override": override_flag,
            })),
        ),
        Err(e @ (PolicyFileError::Io(_) | PolicyFileError::Schema(_))) => (
            StatusCode::OK,
            Json(json!({
                "exists": true,
                "parse_error": e.to_string(),
                "groups": {},
                "limits": { "max_corrections": 3, "max_actions_per_run": 200 },
                "override": override_flag,
            })),
        ),
    }
}

/// `POST /api/approval-policy/trial` —— 试判定(机械预检,零 LLM)
///
/// 请求 {tool, args};响应 {status, red_line, conditions, note, limits, override}。
/// 条件文件缺失/无效时仍可试判定(status=file_missing/file_invalid,note 说明
/// 运行时行为)——预检的职责是把「这段配置会发生什么」讲清楚,不隐藏失败。
pub async fn trial_judge(
    State(state): State<AgentApiState>,
    Json(body): Json<TrialBody>,
) -> (StatusCode, Json<Value>) {
    if body.tool.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "tool must not be empty" })),
        );
    }

    let override_flag = state
        .workbench_settings()
        .merged()
        .0
        .get("approval.exemptionOverride")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let (policy, parse_error): (Option<ApprovalPolicyFile>, Option<String>) =
        match ApprovalPolicyFile::load(&policy_path(&state)) {
            Ok(p) => (Some(p), None),
            Err(PolicyFileError::NotFound(_)) => (None, None),
            Err(e) => (None, Some(e.to_string())),
        };

    let mut outcome = trial(policy.as_ref(), override_flag, &body.tool, &body.args);
    if let Some(err) = parse_error {
        outcome.status = "file_invalid";
        outcome.note = format!("条件文件解析失败({err})——policy 档运行时 fail-visible 降级 manual");
    }

    let limits = policy
        .map(|p| {
            json!({
                "max_corrections": p.policy_limits.max_corrections,
                "max_actions_per_run": p.policy_limits.max_actions_per_run,
            })
        })
        .unwrap_or_else(|| json!({ "max_corrections": 3, "max_actions_per_run": 200 }));

    (
        StatusCode::OK,
        Json(json!({
            "status": outcome.status,
            "red_line": outcome.red_line,
            "conditions": outcome.conditions,
            "note": outcome.note,
            "limits": limits,
            "override": override_flag,
        })),
    )
}
