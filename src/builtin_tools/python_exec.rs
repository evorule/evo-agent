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

/// 既有硬编码解释器路径(存在性探测保留:本机缺省行为零变化)
const LEGACY_PYTHON_PATH: &str = r"D:\Python312\python.exe";

/// python 解释器路径解析:
/// `EVO_AGENT_PYTHON` env > 既有硬编码路径(存在性探测) > PATH `python`
///
/// 本机缺省行为零变化(既有安装路径存在时仍优先使用);其他机器从
/// spawn 必炸变为可用(回落 PATH 查找)。env 覆盖为显式声明通道。
fn python_exe() -> String {
    python_exe_with(std::env::var("EVO_AGENT_PYTHON").ok().as_deref())
}

/// `python_exe` 的可测变体:env 值由参数注入(避免测试改进程 env)
fn python_exe_with(env_override: Option<&str>) -> String {
    if let Some(p) = env_override {
        let p = p.trim();
        if !p.is_empty() {
            return p.to_string();
        }
    }
    if std::path::Path::new(LEGACY_PYTHON_PATH).exists() {
        return LEGACY_PYTHON_PATH.to_string();
    }
    "python".to_string()
}

/// 一次性执行一段 Python 代码(candidate 全审批)
///
/// G1 执行环境 profile:[`ExecutionProfile::Workbench`](缺省)维持 candidate
/// 全审批历史行为;[`ExecutionProfile::SandboxTrusted`](一次性隔离环境)下
/// python 是任务本体——不再出提案直接执行,执行仍全量落 tool_trace/journal
/// (审计同权),结果带 execution_profile 标记。
pub struct PythonExecTool {
    /// 执行环境 profile(G1;缺省 Workbench = 历史行为)
    profile: crate::builtin_tools::shell_exec::ExecutionProfile,
}

impl Default for PythonExecTool {
    fn default() -> Self {
        Self::new()
    }
}

impl PythonExecTool {
    /// Workbench 缺省构造(人在环基线)
    pub fn new() -> Self {
        Self {
            profile: crate::builtin_tools::shell_exec::ExecutionProfile::Workbench,
        }
    }

    /// 设置执行环境 profile(G1:trusted 档直跑不出提案)
    pub fn with_execution_profile(
        mut self,
        profile: crate::builtin_tools::shell_exec::ExecutionProfile,
    ) -> Self {
        self.profile = profile;
        self
    }
}

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
        let out = tokio::process::Command::new(python_exe())
            .arg("-c")
            .arg(code)
            .output()
            .await
            .map_err(|e| format!("spawn python 失败: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let ok = out.status.success();
        let mut result = json!({
            "ok": ok,
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": out.status.code().unwrap_or(-1),
        });
        // G1 审计同权:trusted 档结果带 profile 标记(workbench 结果不变)
        if self.profile == crate::builtin_tools::shell_exec::ExecutionProfile::SandboxTrusted {
            result["execution_profile"] = json!(self.profile.as_str());
        }
        Ok(result)
    }

    /// 管道⑤评估钩子:任意代码执行无安全子集可言(shell_exec 尚有白名单),
    /// Workbench 档任何调用都出提案——code 缺参时返回 None 由执行体如实报错;
    /// G1:sandbox-trusted 档不出提案(隔离环境内 python 是任务本体,直跑落账)
    fn evaluate_proposal(&self, args: &Value) -> Option<Value> {
        let code = args.get("code").and_then(|v| v.as_str())?;
        if self.profile == crate::builtin_tools::shell_exec::ExecutionProfile::SandboxTrusted {
            return None;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// G1:Workbench 缺省出提案(candidate 全审批历史行为)
    #[test]
    fn test_workbench_proposes() {
        let tool = PythonExecTool::new();
        let p = tool
            .evaluate_proposal(&json!({"code": "print(1)"}))
            .expect("workbench must propose");
        assert_eq!(
            p.get("category").and_then(|v| v.as_str()),
            Some("candidate")
        );
    }

    /// G1:trusted 档不出提案(隔离环境内 python 是任务本体,直跑落账)
    #[test]
    fn test_trusted_no_proposal() {
        let tool = PythonExecTool::new().with_execution_profile(
            crate::builtin_tools::shell_exec::ExecutionProfile::SandboxTrusted,
        );
        assert!(tool
            .evaluate_proposal(&json!({"code": "print(1)"}))
            .is_none());
    }

    /// G1:缺省 = Workbench(人在环基线零变化)
    #[test]
    fn test_default_is_workbench() {
        assert!(PythonExecTool::default()
            .evaluate_proposal(&json!({"code": "1"}))
            .is_some());
    }

    /// 解释器路径解析次序:env 显式覆盖 > 硬编码存在性探测 > PATH python
    #[test]
    fn test_python_exe_resolution_order() {
        // env 显式覆盖最优先(不去探测文件系统)
        assert_eq!(
            python_exe_with(Some("C:\\custom\\py.exe")),
            "C:\\custom\\py.exe"
        );
        // 空白 env 视为未设,回落既有解析链
        assert_eq!(python_exe_with(Some("   ")), python_exe_with(None));
        // 无 env:硬编码存在→硬编码;不存在→PATH python(两分支都合法)
        let fallback = python_exe_with(None);
        assert!(fallback == LEGACY_PYTHON_PATH || fallback == "python");
    }
}
