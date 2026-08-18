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
}

/// 审批回调 trait — 不同运行模式不同实现
///
/// runner 在 `maybe_handle_approval` 中调用 `request_approval()`,
/// 该方法阻塞直到用户做出决定(或超时)。
#[async_trait]
pub trait ApprovalCallback: Send + Sync {
    /// 问用户是否批准。返回 `true` = 批准,`false` = 拒绝
    async fn request_approval(&self, req: &ApprovalRequest) -> bool;
}

/// CLI 模式:从 stdin 读取 y/n
pub struct CliApproval {
    /// `--auto-approve-candidates` 时为 true,跳过交互直接批准
    pub auto_approve: bool,
}

#[async_trait]
impl ApprovalCallback for CliApproval {
    async fn request_approval(&self, req: &ApprovalRequest) -> bool {
        if self.auto_approve {
            info!(tool = %req.tool_name, command = %req.command, "auto-approved candidate tool");
            return true;
        }

        // 用 spawn_blocking 避免 stdin 读取阻塞 tokio runtime
        let command = req.command.clone();
        let risk = req.risk.clone();
        let alternative = req.alternative.clone();
        let tool_name = req.tool_name.clone();

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
    }
}

/// G8:HTTP 审批等待超时(秒)
pub const HTTP_APPROVAL_TIMEOUT_SECS: u64 = 60;

/// HTTP/SSE 模式:通过 oneshot channel 等 POST `/approve`
///
/// - `request_approval()` 创建 oneshot channel,把 sender 存入 `pending` map
/// - HTTP 端点 `POST /agents/{t}/approve` 从 `pending` 取出 sender 并发送审批结果
/// - 超时(`HTTP_APPROVAL_TIMEOUT_SECS` 秒)后自动拒绝
pub struct HttpApproval {
    /// session_id → approval response channel
    pub pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    /// 超时秒数(默认 60)
    pub timeout_secs: u64,
}

impl HttpApproval {
    /// 创建 HttpApproval,共享 `pending` map
    pub fn new(pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>) -> Self {
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
    async fn request_approval(&self, req: &ApprovalRequest) -> bool {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .expect("pending_approvals mutex poisoned");
            pending.insert(req.session_id.clone(), tx);
        }

        info!(
            session = %req.session_id,
            tool = %req.tool_name,
            command = %req.command,
            "G8: awaiting HTTP approval (timeout {}s)",
            self.timeout_secs
        );

        match tokio::time::timeout(Duration::from_secs(self.timeout_secs), rx).await {
            Ok(Ok(approved)) => approved,
            Ok(Err(_)) => {
                // sender 被 drop(不应该发生,但防御性处理)
                warn!(session = %req.session_id, "approval channel closed unexpectedly");
                false
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
                false
            }
        }
    }
}

/// Mock:总是批准(测试用)
pub struct AutoApprove;

#[async_trait]
impl ApprovalCallback for AutoApprove {
    async fn request_approval(&self, req: &ApprovalRequest) -> bool {
        info!(tool = %req.tool_name, "AutoApprove: auto-approved");
        true
    }
}

/// Mock:总是拒绝(测试用)
pub struct DenyAll;

#[async_trait]
impl ApprovalCallback for DenyAll {
    async fn request_approval(&self, req: &ApprovalRequest) -> bool {
        info!(tool = %req.tool_name, "DenyAll: auto-denied");
        false
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

    // ===== AutoApprove / DenyAll 测试 =====

    #[tokio::test]
    async fn test_auto_approve_always_true() {
        let cb = AutoApprove;
        let req = ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "rm -rf /tmp".to_string(),
            risk: "high".to_string(),
            alternative: "".to_string(),
        };
        assert!(cb.request_approval(&req).await);
    }

    #[tokio::test]
    async fn test_deny_all_always_false() {
        let cb = DenyAll;
        let req = ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: "shell_exec".to_string(),
            args: json!({}),
            command: "rm -rf /tmp".to_string(),
            risk: "high".to_string(),
            alternative: "".to_string(),
        };
        assert!(!cb.request_approval(&req).await);
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
        };

        // 模拟 runner 等待审批
        let approval_clone = HttpApproval {
            pending: pending.clone(),
            timeout_secs: 5,
        };
        let wait_fut = tokio::spawn(async move { approval_clone.request_approval(&req).await });

        // 模拟 HTTP 端点发送审批结果
        tokio::time::sleep(Duration::from_millis(50)).await;
        let sender = {
            let mut map = pending.lock().unwrap();
            map.remove("s1").unwrap()
        };
        sender.send(true).unwrap();

        let result = wait_fut.await.unwrap();
        assert!(result);
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
        };

        let approval_clone = HttpApproval {
            pending: pending.clone(),
            timeout_secs: 5,
        };
        let wait_fut = tokio::spawn(async move { approval_clone.request_approval(&req).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let sender = {
            let mut map = pending.lock().unwrap();
            map.remove("s2").unwrap()
        };
        sender.send(false).unwrap();

        let result = wait_fut.await.unwrap();
        assert!(!result);
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
        };

        let result = approval.request_approval(&req).await;

        // 超时后应自动拒绝
        assert!(!result);
        // pending map 应被清理
        assert!(pending.lock().unwrap().is_empty());
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
        };

        // 启动等待(不发送结果,只检查 pending 是否被填充)
        let approval_clone = HttpApproval {
            pending: pending.clone(),
            timeout_secs: 5,
        };
        let wait_fut = tokio::spawn(async move { approval_clone.request_approval(&req).await });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // 检查 pending map 中有 "s4" 的 entry
        {
            let map = pending.lock().unwrap();
            assert!(map.contains_key("s4"));
        }

        // 清理:发送拒绝结果
        let sender = {
            let mut map = pending.lock().unwrap();
            map.remove("s4").unwrap()
        };
        sender.send(false).unwrap();
        let _ = wait_fut.await;
    }
}
