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

use crate::agent::approval_policy::{
    hits_red_line, host_code_execution_red_line, ApprovalPolicyFile, PolicyCondition,
    PolicyFileError,
};
use crate::api::agent_api::AgentApiState;

/// 试判定单条条件结论（响应 shape 只增不改：conditions 字符串清单保留，
/// 逐条红/绿结论走新增 condition_results 数组）
#[derive(Debug)]
struct TrialConditionResult {
    /// 条件在清单内的序号（1 起）
    index: usize,
    /// natural | match | exclude | verify
    kind: &'static str,
    /// 人类可读条件文本
    text: String,
    /// pass | fail | review | unexecuted
    result: &'static str,
    /// 结论说明（机械条目携命中证据；NL/verify 说明边界）
    evidence: String,
}

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
    /// 宿主任意代码执行上下文(python_exec 未装配 workspace 沙箱为 true,
    /// 与运行时提案 JSON 同名同义;缺省 false 与运行时缺省一致)
    #[serde(default)]
    pub host_execution: bool,
}

/// 机械预检结果(工作台渲染三色:forced_manual=红/reviewed=黄/fallback_manual=灰)
#[derive(Debug)]
struct TrialOutcome {
    /// forced_manual | reviewed | fallback_manual | file_missing | file_invalid
    status: &'static str,
    /// 红线标记(red_line.delete 等,无则 None)
    red_line: Option<String>,
    /// 该工具组的条件清单(纯文本,向后兼容)
    conditions: Vec<String>,
    /// 逐条条件结论(match/exclude 实判红绿;verify 标注不执行;NL 标注交评审)
    condition_results: Vec<TrialConditionResult>,
    /// 说明(给人看的一句话)
    note: String,
}

/// 机械预检:红线豁免 → 条件拉取 → 限额 → match/exclude 实判。与
/// PolicyReview::review 前半段同口径,纯函数零 IO(除读条件文件),不调 LLM、
/// 不执行 verify 命令、不落账。
fn trial(
    policy: Option<&ApprovalPolicyFile>,
    override_flag: bool,
    tool: &str,
    args: &Value,
    host_execution: bool,
) -> TrialOutcome {
    let limits = policy.map(|p| {
        (
            p.policy_limits.max_corrections,
            p.policy_limits.max_actions_per_run,
        )
    });

    let red_line =
        hits_red_line(tool, args).or_else(|| host_code_execution_red_line(tool, host_execution));
    let conds: Vec<&PolicyCondition> = policy
        .map(|p| p.conditions_for(tool).iter().collect())
        .unwrap_or_default();

    // 逐条结论:match/exclude 对试判定请求 args 实判(真实机械红/绿);
    // verify 标注「试判定不执行命令」;NL 标注「运行时交评审模型」
    let condition_results: Vec<TrialConditionResult> = conds
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (result, evidence) = match c {
                PolicyCondition::Natural(_) => (
                    "review",
                    "运行时交评审模型判定(试判定不代判)".to_string(),
                ),
                PolicyCondition::Verify { .. } => (
                    "unexecuted",
                    "含外部验证器——试判定不执行命令,运行时 serve 侧执行(exit 0=符合,超时/失败降级人工)"
                        .to_string(),
                ),
                _ => {
                    let (pass, ev) = c
                        .evaluate_mechanical(args)
                        .expect("match/exclude 条目必有机械实判");
                    if pass { ("pass", ev) } else { ("fail", ev) }
                }
            };
            TrialConditionResult {
                index: i + 1,
                kind: c.kind(),
                text: c.text().to_string(),
                result,
                evidence,
            }
        })
        .collect();

    let conditions: Vec<String> = conds.iter().map(|c| c.text().to_string()).collect();
    let mech_fail_count = condition_results
        .iter()
        .filter(|r| r.result == "fail")
        .count();
    let has_verify = condition_results.iter().any(|r| r.kind == "verify");

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
    // 机械实判结论注记(红线/空清单场景无实判意义,只在 reviewed 态补充)
    let mut note = note;
    if status == "reviewed" && mech_fail_count > 0 {
        note.push_str(&format!(
            " 存在 {mech_fail_count} 条机械不符——运行时将拒绝并回喂修正指令(不进评审)。",
        ));
    }
    if status == "reviewed" && has_verify {
        note.push_str(" 含外部验证器条目——运行时 serve 侧执行验证命令。");
    }

    let _ = limits; // 限额随响应整体透出,不影响状态机
    TrialOutcome {
        status,
        red_line: red_line.map(|r| r.as_str().to_string()),
        conditions,
        condition_results,
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
                .map(|(k, g)| {
                    (
                        k.clone(),
                        json!(g.conditions.iter().map(|c| c.text()).collect::<Vec<_>>()),
                    )
                })
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
/// 请求 {tool, args};响应 {status, red_line, conditions, condition_results,
/// note, limits, override}。conditions 为纯文本清单(shape 向后兼容不变);
/// condition_results(新增)逐条给红/绿结论——match/exclude 对 args 实判、
/// verify 标注试判定不执行命令、NL 标注运行时交评审。
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

    let mut outcome = trial(
        policy.as_ref(),
        override_flag,
        &body.tool,
        &body.args,
        body.host_execution,
    );
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
            "condition_results": outcome
                .condition_results
                .iter()
                .map(|r| {
                    json!({
                        "index": r.index,
                        "kind": r.kind,
                        "text": r.text,
                        "result": r.result,
                        "evidence": r.evidence,
                    })
                })
                .collect::<Vec<_>>(),
            "note": outcome.note,
            "limits": limits,
            "override": override_flag,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::approval_policy::ApprovalPolicyFile;

    /// 试判定机械实判样例条件文件（match/exclude/verify/NL 四形态并存）
    const TRIAL_POLICY: &str = r#"[shell_exec]
conditions = [
  { text = "白名单", match = ["cargo*"] },
  { text = "禁外发", exclude = ["git push"] },
  { text = "构建过", verify = { cmd = ["cargo", "check"] } },
  "仅工作区内路径",
]
[policy_limits]
max_corrections = 3
max_actions_per_run = 200"#;

    #[test]
    fn trial_evaluates_mechanical_and_annotates_verify_and_nl() {
        let p = ApprovalPolicyFile::from_toml(TRIAL_POLICY).unwrap();
        let out = trial(
            Some(&p),
            false,
            "shell_exec",
            &json!({"command": "cargo check"}),
            false,
        );
        assert_eq!(out.status, "reviewed");
        // conditions 纯文本清单保持向后兼容
        assert_eq!(out.conditions.len(), 4);
        let kinds: Vec<(&str, &str)> = out
            .condition_results
            .iter()
            .map(|r| (r.kind, r.result))
            .collect();
        assert_eq!(kinds[0], ("match", "pass"));
        assert_eq!(kinds[1], ("exclude", "pass"));
        assert_eq!(kinds[2], ("verify", "unexecuted"));
        assert_eq!(kinds[3], ("natural", "review"));
        // verify 条目注记（试判定不执行命令）
        assert!(out.note.contains("外部验证器"));
        assert!(!out.note.contains("机械不符"));
    }

    #[test]
    fn trial_reports_mechanical_fail_with_correction_note() {
        let p = ApprovalPolicyFile::from_toml(TRIAL_POLICY).unwrap();
        let out = trial(
            Some(&p),
            false,
            "shell_exec",
            &json!({"command": "rm -rf /"}),
            false,
        );
        assert_eq!(out.status, "reviewed");
        // match 实判红（零命中 fail-closed）+exclude 实判绿
        assert_eq!(out.condition_results[0].result, "fail");
        assert!(out.condition_results[0].evidence.contains("白名单模式"));
        assert_eq!(out.condition_results[1].result, "pass");
        assert!(out.note.contains("机械不符"), "note: {}", out.note);
    }

    #[test]
    fn trial_red_line_dominates_regardless_of_mechanical() {
        let p = ApprovalPolicyFile::from_toml(TRIAL_POLICY).unwrap();
        let out = trial(
            Some(&p),
            false,
            "shell_exec",
            &json!({"command": "git push origin main"}),
            false,
        );
        // 红线闸先于一切机械判定（与决策端同口径）
        assert_eq!(out.status, "forced_manual");
        assert!(out.red_line.is_some());
        assert!(out.note.contains("红线"));
    }

    #[test]
    fn trial_without_policy_reports_file_missing() {
        let out = trial(None, false, "shell_exec", &json!({"command": "ls"}), false);
        assert_eq!(out.status, "file_missing");
        assert!(out.condition_results.is_empty());
    }
}
