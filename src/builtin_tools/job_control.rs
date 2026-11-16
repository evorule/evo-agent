// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! `job_control` —— 后台作业轮询面(dev-feedback P0a 余量配套)。
//!
//! 与 shell_exec `background=true` 启动面配对:status 查询/read 读尾/
//! kill 族级终止。注册表=进程级单例([`super::background_jobs::registry`]),
//! 与 shell_exec 共享同一注册表面(同进程即同册)。零审批判断——本工具
//! 只操作本 serve 自己收容的作业句柄,不构成新的命令执行面。

use serde_json::Value;

use super::background_jobs::{registry, DEFAULT_TAIL_BYTES};
use crate::io_handler::IoResult;

/// 工具名(LLM 契约面/manifest/注册三处同键)
pub const JOB_CONTROL_TOOL: &str = "job_control";

/// `job_control` 轮询工具
pub struct JobControlTool;

impl JobControlTool {
    /// 生产构造(注册表=进程级单例,无独立状态)
    pub fn new() -> Self {
        Self
    }
}

impl Default for JobControlTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl crate::io_handlers::tool_handler::ToolFunction for JobControlTool {
    async fn call(&self, args: &Value) -> IoResult {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: action (status | read | kill)".to_string())?;
        let job_id = args.get("job_id").and_then(|v| v.as_str()).ok_or_else(|| {
            "missing required arg: job_id (string, from shell_exec background envelope)".to_string()
        })?;
        let reg = registry();
        match action {
            "status" => {
                let v = reg.status(job_id)?;
                Ok(serde_json::json!({
                    "action": "status",
                    "job_id": v.job_id,
                    "status": v.status,
                    "exit_code": v.exit_code,
                    "elapsed_ms": v.elapsed_ms,
                    "log_path": v.log_path.to_string_lossy(),
                    "log_bytes": v.log_bytes,
                }))
            }
            "read" => {
                let max_bytes = match args.get("bytes") {
                    None | Some(Value::Null) => DEFAULT_TAIL_BYTES,
                    Some(v) => v
                        .as_u64()
                        .ok_or_else(|| "bytes must be a positive integer".to_string())?,
                };
                let (v, tail) = reg.read_tail(job_id, max_bytes)?;
                Ok(serde_json::json!({
                    "action": "read",
                    "job_id": v.job_id,
                    "status": v.status,
                    "exit_code": v.exit_code,
                    "elapsed_ms": v.elapsed_ms,
                    "log_path": v.log_path.to_string_lossy(),
                    "log_bytes": v.log_bytes,
                    "tail": tail,
                    "tail_capped_bytes": max_bytes,
                }))
            }
            "kill" => {
                let v = reg.kill(job_id)?;
                Ok(serde_json::json!({
                    "action": "kill",
                    "job_id": v.job_id,
                    "status": v.status,
                    "exit_code": v.exit_code,
                    "elapsed_ms": v.elapsed_ms,
                    "log_path": v.log_path.to_string_lossy(),
                }))
            }
            other => Err(format!(
                "unknown action \"{other}\" (valid actions: status, read, kill)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handlers::tool_handler::ToolFunction;

    fn tool() -> JobControlTool {
        JobControlTool::new()
    }

    #[tokio::test]
    async fn unknown_job_id_is_fail_visible() {
        let err = tool()
            .call(&serde_json::json!({"action": "status", "job_id": "job-does-not-exist"}))
            .await
            .unwrap_err();
        assert!(err.contains("unknown job id"), "got: {err}");
    }

    #[tokio::test]
    async fn unknown_action_is_fail_visible() {
        let err = tool()
            .call(&serde_json::json!({"action": "restart", "job_id": "job-x"}))
            .await
            .unwrap_err();
        assert!(
            err.contains("valid actions: status, read, kill"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn missing_args_are_fail_visible() {
        let err = tool()
            .call(&serde_json::json!({"job_id": "job-x"}))
            .await
            .unwrap_err();
        assert!(err.contains("missing required arg: action"), "got: {err}");
        let err = tool()
            .call(&serde_json::json!({"action": "status"}))
            .await
            .unwrap_err();
        assert!(err.contains("missing required arg: job_id"), "got: {err}");
    }
}
