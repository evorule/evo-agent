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
//! ## 三种实现
//!
//! - [`CliApproval`]:CLI 模式,从 stdin 读 y/n
//! - [`HttpApproval`]:HTTP/SSE 模式,通过 oneshot channel 等 POST `/approve`
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
}

/// 审批回调 trait — 不同运行模式不同实现
///
/// runner 在 `maybe_handle_approval` 中调用 `request_approval()`,
/// 该方法阻塞直到用户做出决定(或超时)。
#[async_trait]
pub trait ApprovalCallback: Send + Sync {
    /// 问用户是否批准。返回 [`ApprovalDecision`](带决策者身份),决定执行与否
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision;
}

/// CLI 模式:从 stdin 读取 y/n
pub struct CliApproval {
    /// `--auto-approve-candidates` 时为 true,跳过交互直接批准
    pub auto_approve: bool,
}

#[async_trait]
impl ApprovalCallback for CliApproval {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        if self.auto_approve {
            info!(tool = %req.tool_name, command = %req.command, "auto-approved candidate tool");
            return ApprovalDecision {
                approved: true,
                approver: "cli-user".to_string(),
                verified: false,
                reason: String::new(),
                auto_rejected: false,
            };
        }

        // 用 spawn_blocking 避免 stdin 读取阻塞 tokio runtime
        let command = req.command.clone();
        let risk = req.risk.clone();
        let alternative = req.alternative.clone();
        let tool_name = req.tool_name.clone();

        let approved = tokio::task::spawn_blocking(move || {
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
        .unwrap_or(false);

        ApprovalDecision {
            approved,
            approver: "cli-user".to_string(),
            verified: false,
            reason: String::new(),
            auto_rejected: false,
        }
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
        }
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
        // 提案 ID 在解析出 proposal 时即生成,后续审批留痕与 /approve 校验共用
        proposal_id: new_proposal_id(),
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
        let cb = CliApproval { auto_approve: true };
        let req = ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "ls /tmp".to_string(),
            risk: "medium".to_string(),
            alternative: "".to_string(),
            proposal_id: "ap-1".to_string(),
        };
        let decision = cb.request_approval(&req).await;
        assert!(decision.approved);
        assert_eq!(decision.approver, "cli-user");
        assert!(!decision.verified);
        assert!(!decision.auto_rejected);
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
            })
            .unwrap();
        let _ = wait_fut.await;
    }
}
