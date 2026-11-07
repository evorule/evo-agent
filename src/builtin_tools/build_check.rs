// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! `build_check` —— 按需结构化构建诊断(工作区 build 反馈回路)
//!
//! 模型可主动调用的「跑一次构建检查、拿结构化诊断清单」工具,与写触发
//! 的 verify 门禁([`crate::agent::verify`])互补:verify 只在写面工具成功后
//! 的轮末触发,本工具覆盖「改之前先看现状」「回喂增量里要看全量清单」
//! 两类主动查询场景。
//!
//! ## 契约要点
//!
//! - **命令固定**:`cargo check --message-format=short`,模型不可传参——
//!   无模型可控命令面即无审批旁路面(工具审批档与 shell_exec 同族
//!   candidate:构建会执行工作区 build script,模型可通过写 build.rs
//!   引入执行面,candidate 档让人工默认否决点在场)。
//! - **输出结构化**:stdout/stderr 经 problem matcher(`$rustc`)逐行解析,
//!   返回 `{path,line,col,severity,code,message}` 条目集(errors 优先,
//!   帽 50 条+truncated 标记)与 verdict,替代裸文本构建日志回灌。
//! - **verdict 真值表与 verify 门禁同源**([`crate::agent::verify::classify_verdict`]):
//!   Error 诊断 > 0 = fail;零 Error 且 exit 0 = pass;其余 = unknown
//!   (非 Rust 工作区/manifest 级失败如实回 unknown,不假装通过)。
//! - 执行语义与 verify 门禁同缝([`crate::agent::verify::CommandRunner`]):
//!   同 shell 装配、同 256KB 输出帽、同超时杀进程语义。
//! - 无状态:每次调用返回当前全量现状(增量去重只属于 verify 门禁回喂)。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::agent::verify::{classify_verdict, CommandRunner, ShellCommandRunner};
use crate::io_handler::IoResult;
use crate::tasks::matcher::parse_line;

/// 工具名(LLM 契约面/manifest/注册三处同键)
pub const BUILD_CHECK_TOOL: &str = "build_check";
/// 固定验证命令(模型不可传参)
pub const BUILD_CHECK_COMMAND: &str = "cargo check --message-format=short";
/// 单次检查超时(秒):冷 target 的全量 check 留量
pub const BUILD_CHECK_TIMEOUT_SECS: u64 = 300;
/// 诊断条目帽(errors 优先;超出截断并标记)
pub const DIAGNOSTICS_CAP: usize = 50;

/// `build_check` 工具(执行缝注入:生产=工作目录 shell;测试=打桩)
pub struct BuildCheckTool {
    runner: Arc<dyn CommandRunner>,
}

impl BuildCheckTool {
    /// 生产构造(workdir = 构建检查的工作区根,与 shell_exec 同基线)
    pub fn wired(workdir: &Path) -> Self {
        Self {
            runner: Arc::new(ShellCommandRunner::new(workdir)),
        }
    }

    /// 测试缝(注入打桩 runner;仅测试构建编译,生产路径走 [`wired`])
    #[cfg(test)]
    pub(crate) fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    /// 解析单次命令输出为结构化诊断集(错误/告警计数+帽内条目)
    fn collect_diagnostics(output: &str) -> (usize, usize, Vec<Value>, bool) {
        let mut errors = 0usize;
        let mut warnings = 0usize;
        let mut diags: Vec<Value> = Vec::new();
        let mut truncated = false;
        for line in output.lines() {
            let Some(d) = parse_line("$rustc", line) else {
                continue;
            };
            let is_error = d.severity == 8;
            if is_error {
                errors += 1;
            } else {
                warnings += 1;
            }
            if diags.len() < DIAGNOSTICS_CAP {
                diags.push(serde_json::json!({
                    "path": d.path,
                    "line": d.line,
                    "col": d.col,
                    "severity": d.severity,
                    "code": d.code,
                    "message": d.msg,
                }));
            } else {
                truncated = true;
            }
        }
        (errors, warnings, diags, truncated)
    }
}

#[async_trait::async_trait]
impl crate::io_handlers::tool_handler::ToolFunction for BuildCheckTool {
    async fn call(&self, _args: &Value) -> IoResult {
        let started = Instant::now();
        let out = self
            .runner
            .run(
                BUILD_CHECK_COMMAND,
                Duration::from_secs(BUILD_CHECK_TIMEOUT_SECS),
            )
            .await
            .map_err(|e| format!("build_check could not run the check command: {e}"))?;

        let combined = {
            let mut s = out.stdout.clone();
            s.push_str(&out.stderr);
            s
        };
        let (errors, warnings, diags, truncated) = Self::collect_diagnostics(&combined);
        let verdict = classify_verdict(out.exit_code, errors);

        let map = serde_json::json!({
            "command": BUILD_CHECK_COMMAND,
            "exit_code": out.exit_code,
            "timed_out": out.timed_out,
            "verdict": verdict,
            "counts": { "errors": errors, "warnings": warnings },
            "diagnostics": diags,
            "truncated": truncated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "note": if verdict == "unknown" {
                "non-zero exit with no parseable diagnostics: the workspace may not be \
                 a Rust project, or the failure is at the manifest/toolchain level"
            } else {
                ""
            },
        });
        let mut obj = map;
        if let Some(o) = obj.as_object_mut() {
            if o.get("note").and_then(|v| v.as_str()) == Some("") {
                o.remove("note");
            }
        }
        Ok(obj)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::verify::CommandOutput;
    use crate::io_handlers::tool_handler::ToolFunction;
    use std::sync::Mutex;

    struct StubRunner {
        outputs: Mutex<Vec<Result<CommandOutput, String>>>,
    }

    #[async_trait::async_trait]
    impl CommandRunner for StubRunner {
        async fn run(&self, _command: &str, _timeout: Duration) -> Result<CommandOutput, String> {
            let mut q = self.outputs.lock().unwrap();
            if q.is_empty() {
                return Err("stub exhausted".to_string());
            }
            q.remove(0)
        }
    }

    fn tool_with(stdout: &str, exit_code: Option<i32>) -> BuildCheckTool {
        BuildCheckTool::with_runner(Arc::new(StubRunner {
            outputs: Mutex::new(vec![Ok(CommandOutput {
                exit_code,
                stdout: stdout.to_string(),
                stderr: String::new(),
                timed_out: false,
            })]),
        }))
    }

    #[tokio::test]
    async fn clean_check_reports_pass_without_note() {
        let tool = tool_with(
            "Compiling evorule-server v0.11.0\nFinished dev target(s)",
            Some(0),
        );
        let r = tool.call(&Value::Null).await.unwrap();
        assert_eq!(r.get("verdict").and_then(|v| v.as_str()), Some("pass"));
        assert_eq!(
            r.get("counts")
                .and_then(|c| c.get("errors"))
                .and_then(|v| v.as_i64()),
            Some(0)
        );
        assert!(r.get("note").is_none(), "note must be absent on pass");
    }

    #[tokio::test]
    async fn errors_are_structured_and_verdict_fails() {
        let tool = tool_with(
            "src/lsp/bridge.rs:86:24: error[E0308]: mismatched types\n\
             src/x.rs:12:5: warning: unused variable: `y`",
            Some(101),
        );
        let r = tool.call(&Value::Null).await.unwrap();
        assert_eq!(r.get("verdict").and_then(|v| v.as_str()), Some("fail"));
        let counts = r.get("counts").unwrap();
        assert_eq!(counts.get("errors").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(counts.get("warnings").and_then(|v| v.as_i64()), Some(1));
        let diags = r.get("diagnostics").and_then(|v| v.as_array()).unwrap();
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].get("code").and_then(|v| v.as_str()), Some("E0308"));
        assert_eq!(diags[0].get("severity").and_then(|v| v.as_i64()), Some(8));
        assert_eq!(diags[1].get("severity").and_then(|v| v.as_i64()), Some(4));
        assert!(r.get("truncated").and_then(|v| v.as_bool()) == Some(false));
    }

    #[tokio::test]
    async fn non_rust_workspace_reports_unknown_with_note() {
        let tool = tool_with("error: could not find `Cargo.toml` in `D:/docs`", Some(101));
        let r = tool.call(&Value::Null).await.unwrap();
        assert_eq!(r.get("verdict").and_then(|v| v.as_str()), Some("unknown"));
        let note = r.get("note").and_then(|v| v.as_str()).unwrap_or("");
        assert!(note.contains("may not be a Rust project"), "got: {note}");
    }

    #[tokio::test]
    async fn diagnostics_capped_at_50_with_truncation() {
        let mut stdout = String::new();
        for i in 1..=60 {
            stdout.push_str(&format!("src/f{i}.rs:{i}:1: error[E0001]: boom {i}\n"));
        }
        let tool = tool_with(&stdout, Some(101));
        let r = tool.call(&Value::Null).await.unwrap();
        let diags = r.get("diagnostics").and_then(|v| v.as_array()).unwrap();
        assert_eq!(diags.len(), 50);
        assert_eq!(
            r.get("counts")
                .and_then(|c| c.get("errors"))
                .and_then(|v| v.as_i64()),
            Some(60),
            "counts must stay exact beyond the cap"
        );
        assert_eq!(r.get("truncated").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn spawn_failure_is_fail_visible_error() {
        let tool = BuildCheckTool::with_runner(Arc::new(StubRunner {
            outputs: Mutex::new(vec![Err("timeout".to_string())]),
        }));
        let err = tool.call(&Value::Null).await.unwrap_err();
        assert!(
            err.contains("could not run the check command"),
            "got: {err}"
        );
    }
}
