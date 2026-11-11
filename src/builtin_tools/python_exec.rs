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
//!
//! ## P0 沙箱收权（design §四）
//!
//! 装配 workdir（toolkit 主装配面注入）后：spawn 锚定 `current_dir(workdir)`
//! （workspace 沙箱执行，相对路径读写不出界），结果带 `sandbox=workspace-cwd`
//! 审计标记；提案 risk 文案按逃逸意图静态检测（共用判定面
//! `crate::agent::approval_tier::hits_escape_intent`，与决策端同源）实际
//! 分级——沙箱内纯计算=T0 eligible（决策端 allowlist 命中即静默批准），
//! 逃逸意图=恒必问。未装配 workdir（G1 profile 重建先例）保持宿主 cwd
//! 历史行为（缺省不变式）。

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

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
///
/// P0 沙箱收权(design §四):装配 workdir 后 spawn 锚定 `current_dir(workdir)`
/// (workspace 沙箱执行,相对路径读写不出界);提案 risk 文案按逃逸意图静态
/// 检测结论实际分级——沙箱内纯计算=T0 eligible(allowlist 可自动批准),
/// 检测到逃逸意图(写面/网络/进程外)=恒必问。
pub struct PythonExecTool {
    /// 执行环境 profile(G1;缺省 Workbench = 历史行为)
    profile: crate::builtin_tools::shell_exec::ExecutionProfile,
    /// workspace 沙箱根(P0 收权;None = 未装配,继承宿主 cwd 历史行为)
    workdir: Option<PathBuf>,
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
            workdir: None,
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

    /// 装配 workspace 沙箱根(P0 收权:spawn cwd 锚定,相对路径读写不出界)
    pub fn with_workdir(mut self, dir: &Path) -> Self {
        self.workdir = Some(dir.to_path_buf());
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
        let out = {
            let mut cmd = tokio::process::Command::new(python_exe());
            cmd.arg("-c").arg(code);
            // P0 沙箱收权:装配 workdir 即锚定 workspace 沙箱执行(相对路径
            // 读写不出界);未装配时继承宿主 cwd(缺省不变式)
            if let Some(dir) = &self.workdir {
                cmd.current_dir(dir);
            }
            // 输出尾随接入(观察态):spawn+分段读共享 tap 通道,累积结果与
            // output() 全量形态字节一致(分段 read 只改读取节拍不改内容)
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            // 取消路径:future 被弃(回合取消)→杀子进程(缺省非 kill_on_drop
            // 会把 python 留成脱管孤儿);绑定摘除由 guard 兜底(与显式 unbind 幂等)
            cmd.kill_on_drop(true);
            let mut child = cmd.spawn().map_err(|e| format!("spawn python 失败: {e}"))?;
            let pid = child.id().unwrap_or(0);
            crate::builtin_tools::shell_exec::bind_pid(pid);
            let _pid_guard = crate::builtin_tools::shell_exec::PidUnbindGuard(pid);
            let mut so = child.stdout.take();
            let mut se = child.stderr.take();
            let (stdout_buf, stderr_buf, status) = tokio::join!(
                async {
                    match so.as_mut() {
                        Some(s) => drain_tapped(s, pid, "out").await.unwrap_or_default(),
                        None => Vec::new(),
                    }
                },
                async {
                    match se.as_mut() {
                        Some(s) => drain_tapped(s, pid, "err").await.unwrap_or_default(),
                        None => Vec::new(),
                    }
                },
                child.wait(),
            );
            // 显式 unbind 已由 _pid_guard 兜底承担(幂等)
            let status = status.map_err(|e| format!("wait python 失败: {e}"))?;
            std::process::Output {
                status,
                stdout: stdout_buf,
                stderr: stderr_buf,
            }
        };
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
        // P0 沙箱收权:装配 workdir 的执行带 sandbox 标记(审计可解释性)
        if self.workdir.is_some() {
            result["sandbox"] = json!("workspace-cwd");
        }
        Ok(result)
    }

    /// 管道⑤评估钩子:任意代码执行无安全子集可言(shell_exec 尚有白名单),
    /// Workbench 档任何调用都出提案——code 缺参时返回 None 由执行体如实报错;
    /// G1:sandbox-trusted 档不出提案(隔离环境内 python 是任务本体,直跑落账)
    ///
    /// P0 沙箱收权:risk 文案按逃逸意图静态检测结论实际分级(设计 §四
    /// 可解释性=信任面)——沙箱内纯计算=T0 eligible(allowlist 可自动批准),
    /// 逃逸意图(写面/网络/进程外)=恒必问。
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
        let sandboxed = self.workdir.is_some();
        // 原因类别判定(与布尔门同源):risk 携带类别+命中片段,让审批卡上
        // 「为何必问」可读可审
        let escape = crate::agent::approval_tier::escape_intent_reason(code);
        let (risk, alternative) = match (sandboxed, escape) {
            (true, None) => (
                "workspace sandboxed execution (cwd anchored to the workspace); pure compute, \
                 no network/process/file-write escape intent detected — workspace allowlist may \
                 auto-approve"
                    .to_string(),
                "write results to workspace files via file_write, or run shell commands via \
                 shell_exec with an allowlisted command"
                    .to_string(),
            ),
            (_, Some(reason)) => (
                format!(
                    "escape intent detected in code: {} (matched: \"{}\") — always requires \
                     manual approval",
                    reason.category, reason.matched
                ),
                "keep the code to pure computation within the workspace sandbox, or request \
                 explicit permission for the escape operation"
                    .to_string(),
            ),
            (false, None) => (
                "arbitrary code execution on the host (runs outside the workspace sandbox, no \
                 command allowlist)"
                    .to_string(),
                "run the code yourself, or use shell_exec with an allowlisted command".to_string(),
            ),
        };
        Some(json!({
            "status": "needs_approval",
            "category": "candidate",
            "description": format!("execute Python code: {summary}"),
            "risk": risk,
            "alternative": alternative,
            // 宿主任意代码执行上下文(机器可读,决策端红线按执行面判定):
            // 未装配 workspace 沙箱=落宿主解释器任意执行;装配=workspace
            // cwd 锚定;sandbox-trusted 档不出提案(上方提前返回),天然不触发
            "host_execution": self.workdir.is_none(),
        }))
    }
}

async fn drain_tapped<S: tokio::io::AsyncRead + Unpin>(
    s: &mut S,
    pid: u32,
    stream: &'static str,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut seg = [0u8; 8192];
    loop {
        match s.read(&mut seg).await {
            Ok(0) => break,
            Ok(n) => {
                crate::builtin_tools::shell_exec::tap_send_pid(pid, stream, &seg[..n]);
                buf.extend_from_slice(&seg[..n]);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {

    /// 取消路径:future 被弃（回合取消）→ python 子进程必须被杀
    /// （kill_on_drop）。started 标记证明 python 真跑起来了；done 标记在
    /// 弃置后不得出现（修复前=脱管存活会写出）。
    #[tokio::test]
    async fn test_cancel_drop_kills_python_child() {
        use serde_json::json;
        let tool = PythonExecTool::new();
        let dir = std::env::temp_dir();
        let tag = format!("b286-{}", std::process::id());
        let started = dir.join(format!("{}-started.marker", tag));
        let done = dir.join(format!("{}-done.marker", tag));
        let _ = std::fs::remove_file(&started);
        let _ = std::fs::remove_file(&done);
        let s = started.to_string_lossy().replace('\\', "/");
        let d = done.to_string_lossy().replace('\\', "/");
        let code = format!(
            "import time
open(r\"{}\", \"w\").write(\"on\")
time.sleep(6)
open(r\"{}\", \"w\").write(\"done\")
",
            s, d
        );
        let args = json!({"code": code});
        // owned future 与 started 轮询赛跑(与生产形态同构:工具 future 挂在
        // runner select 里,弃置=取消路径)
        let mut fut = Box::pin(tool.call(&args));
        let poll_started = async {
            loop {
                if started.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        };
        tokio::pin!(poll_started);
        tokio::select! {
            r = &mut fut => {
                panic!("tool returned before cancel-check: {r:?}");
            }
            _ = &mut poll_started => {
                // python 真启动(started 标记在位)
            }
        }
        // 弃置 future(=取消路径) → kill_on_drop 应杀子进程
        drop(fut);
        // python 原计划 6s 后写 done 标记；给足 10s 观察窗
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        assert!(
            !done.exists(),
            "python child must be killed when the tool future is dropped"
        );
        let _ = std::fs::remove_file(&started);
        let _ = std::fs::remove_file(&done);
    }

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

    /// 逃逸提案卡 risk 携带原因类别+命中片段(可读可审,沙箱审计批)
    #[test]
    fn test_escape_proposal_risk_carries_reason() {
        let tool = PythonExecTool::new();
        let p = tool
            .evaluate_proposal(&json!({"code": "import socket\ns = socket.socket()"}))
            .expect("escape code must propose");
        let risk = p.get("risk").and_then(|v| v.as_str()).expect("risk str");
        assert!(risk.contains("network"), "risk: {}", risk);
        assert!(risk.contains("import socket"), "risk: {}", risk);
        // 纯计算沙箱内:风险文案不含逃逸字样(基线零变化)
        let p2 = tool
            .evaluate_proposal(&json!({"code": "print(1)"}))
            .expect("pure compute must propose");
        let risk2 = p2.get("risk").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!risk2.contains("escape intent"), "risk: {}", risk2);
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

    // ===== P0 沙箱收权 =====

    /// 提案 risk 文案实际分级:沙箱内纯计算=T0 eligible 文案
    #[test]
    fn test_proposal_sandbox_clean_risk_grading() {
        let dir = std::env::temp_dir().join(format!("evo-py-sbx-a-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = PythonExecTool::new().with_workdir(&dir);
        let p = tool
            .evaluate_proposal(&json!({"code": "print(sum(range(10)))"}))
            .expect("workbench must propose");
        let risk = p.get("risk").and_then(|v| v.as_str()).unwrap();
        assert!(risk.contains("sandboxed execution"), "risk: {risk}");
        assert!(risk.contains("allowlist may auto-approve"), "risk: {risk}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 提案 risk 文案实际分级:逃逸意图(网络/写面/进程)=恒必问文案
    #[test]
    fn test_proposal_escape_risk_grading() {
        let dir = std::env::temp_dir().join(format!("evo-py-sbx-b-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = PythonExecTool::new().with_workdir(&dir);
        for code in [
            "import requests\nprint(requests.get(u))",
            "open('x.txt', 'w').write(d)",
            "import subprocess\nsubprocess.run(x)",
        ] {
            let p = tool
                .evaluate_proposal(&json!({ "code": code }))
                .expect("workbench must propose");
            let risk = p.get("risk").and_then(|v| v.as_str()).unwrap();
            assert!(risk.contains("escape intent detected"), "risk: {risk}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未装配 workdir:提案维持宿主执行历史文案(缺省不变式)
    #[test]
    fn test_proposal_unsandboxed_risk_unchanged() {
        let tool = PythonExecTool::new();
        let p = tool
            .evaluate_proposal(&json!({"code": "print(1)"}))
            .expect("workbench must propose");
        let risk = p.get("risk").and_then(|v| v.as_str()).unwrap();
        assert!(
            risk.contains("runs outside the workspace sandbox"),
            "risk: {risk}"
        );
    }

    /// 提案携带宿主执行上下文标记(机器可读,决策端红线按执行面判定):
    /// 未装配 workdir=true;装配 workdir=false
    #[test]
    fn test_proposal_carries_host_execution_context() {
        let host = PythonExecTool::new()
            .evaluate_proposal(&json!({"code": "print(1)"}))
            .expect("host proposal");
        assert_eq!(
            host.get("host_execution").and_then(|v| v.as_bool()),
            Some(true),
            "unsandboxed proposal must be flagged host_execution"
        );
        let dir = std::env::temp_dir().join(format!("evo-py-sbx-d-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sandboxed = PythonExecTool::new()
            .with_workdir(&dir)
            .evaluate_proposal(&json!({"code": "print(1)"}))
            .expect("sandboxed proposal");
        assert_eq!(
            sandboxed.get("host_execution").and_then(|v| v.as_bool()),
            Some(false),
            "workspace-anchored proposal must not be flagged host_execution"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// spawn cwd 锚定:相对路径写落 workdir(沙箱执行实证)
    #[tokio::test]
    async fn test_call_anchors_workdir() {
        let dir = std::env::temp_dir().join(format!("evo-py-sbx-c-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = PythonExecTool::new().with_workdir(&dir);
        let out = tool
            .call(&json!({"code": "import os; print(os.getcwd())"}))
            .await
            .expect("call ok");
        let stdout = out.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            stdout.contains(dir.file_name().unwrap().to_string_lossy().as_ref())
                || stdout
                    .to_lowercase()
                    .contains(&dir.to_string_lossy().to_lowercase()),
            "cwd must anchor to workdir, got: {stdout}"
        );
        // 相对路径写落 workdir(不出界)
        let out = tool
            .call(&json!({"code": "open('sbx_probe.txt', 'w').write('ok')"}))
            .await
            .expect("call ok");
        assert!(out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
        assert!(
            dir.join("sbx_probe.txt").is_file(),
            "write must land in workdir"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
