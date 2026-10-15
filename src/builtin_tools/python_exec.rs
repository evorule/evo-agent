// SPDX-License-Identifier: AGPL-3.0-or-later
//! `python_exec` — 执行一段 Python 代码（一次性）
//!
//! 每次调用 spawn `python -c <code>`，stdout/stderr 回传。
//!
//! ## 治理定位
//!
//! 任意代码执行（out-of-process、无命令白名单可比 shell_exec）——安全模型上
//! 属 candidate 全审批：任何调用都经 [`PythonExecTool::evaluate_proposal`]
//! 出 needs_approval 提案交用户批准后执行，无 active 直跑名单。

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;
use async_trait::async_trait;
use serde_json::{json, Value};

/// 审批卡代码摘要上限(bytes,防巨型 code 撑爆审批卡)
const PROPOSAL_SUMMARY_BYTES: usize = 200;

/// 一次性执行一段 Python 代码(candidate 全审批)
pub struct PythonExecTool;

#[async_trait]
impl ToolFunction for PythonExecTool {
    async fn call(&self, args: &Value) -> IoResult {
        let code = args
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "python_exec 需要 code 字符串".to_string())?;
        if code.len() > 64 * 1024 {
            return Err("code 超过 64KB 限制".to_string());
        }
        let out = tokio::process::Command::new(r"D:\Python312\python.exe")
            .arg("-c")
            .arg(code)
            .output()
            .await
            .map_err(|e| format!("spawn python 失败: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let ok = out.status.success();
        Ok(json!({
            "ok": ok,
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": out.status.code().unwrap_or(-1),
        }))
    }

    /// 管道⑤评估钩子:任意代码执行无安全子集可言(shell_exec 尚有白名单),
    /// 任何调用都出提案——code 缺参时返回 None 由执行体如实报错
    fn evaluate_proposal(&self, args: &Value) -> Option<Value> {
        let code = args.get("code").and_then(|v| v.as_str())?;
        let mut cut = PROPOSAL_SUMMARY_BYTES.min(code.len());
        while cut > 0 && !code.is_char_boundary(cut) {
            cut -= 1;
        }
        let summary = if code.len() > PROPOSAL_SUMMARY_BYTES {
            format!("{}…(truncated)", &code[..cut])
        } else {
            code.to_string()
        };
        Some(json!({
            "status": "needs_approval",
            "category": "candidate",
            "description": format!("execute Python code: {summary}"),
            "risk": "arbitrary code execution on the host (runs outside the \
                     workspace sandbox, no command allowlist)",
            "alternative": "run the code yourself, or use shell_exec with an \
                            allowlisted command",
        }))
    }
}
