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
//! - **命令按剖面固定**:`profile` 参数从枚举表选固定命令(缺省
//!   `cargo_check`=现行为逐字节兼容),模型不可自由组命令——无模型可控
//!   命令面即无审批旁路面(工具审批档与 shell_exec 同族 candidate:
//!   构建会执行工作区 build script,模型可通过写 build.rs 引入执行面,
//!   candidate 档让人工默认否决点在场);未知剖面显式报错(fail-fast 不猜)。
//! - **输出结构化**:各剖面输出经专用解析器逐行解析,返回
//!   `{path,line,col,severity,code,message}` 条目集(errors 优先,帽 50
//!   条+truncated 标记)与 verdict,替代裸文本构建/测试日志回灌。
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
/// 缺省剖面验证命令(模型不可传参;profile 化前即此命令,逐字节兼容)
pub const BUILD_CHECK_COMMAND: &str = "cargo check --message-format=short";
/// 单次检查超时(秒):冷 target 的全量 check/大测试集留量
pub const BUILD_CHECK_TIMEOUT_SECS: u64 = 300;
/// 诊断条目帽(errors 优先;超出截断并标记)
pub const DIAGNOSTICS_CAP: usize = 50;

/// 检查剖面类别(决定输出解析器)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileKind {
    /// rustc short 形输出($rustc problem matcher 逐行解析)
    RustcShort,
    /// cargo test:$rustc 编译诊断 + failures 块测试名条目并存
    CargoTest,
    /// pytest(-q --tb=line 单行摘要形态专用解析)
    Pytest,
}

/// 检查剖面(枚举键 → 固定命令/matcher/解析器)
///
/// 命令面由本表封闭:模型只能按键选剖面,不能组命令——审批旁路面为零。
struct CheckProfile {
    key: &'static str,
    command: &'static str,
    matcher: &'static str,
    kind: ProfileKind,
}

const PROFILES: &[CheckProfile] = &[
    CheckProfile {
        key: "cargo_check",
        command: BUILD_CHECK_COMMAND,
        matcher: "$rustc",
        kind: ProfileKind::RustcShort,
    },
    CheckProfile {
        key: "cargo_clippy",
        command: "cargo clippy --message-format=short",
        matcher: "$rustc",
        kind: ProfileKind::RustcShort,
    },
    CheckProfile {
        key: "cargo_test",
        command: "cargo test --message-format=short",
        matcher: "$rustc",
        kind: ProfileKind::CargoTest,
    },
    CheckProfile {
        key: "pytest",
        command: "pytest -q --tb=line",
        matcher: "",
        kind: ProfileKind::Pytest,
    },
];

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
    fn collect_diagnostics(output: &str, matcher: &str) -> (usize, usize, Vec<Value>, bool) {
        let mut errors = 0usize;
        let mut warnings = 0usize;
        let mut diags: Vec<Value> = Vec::new();
        let mut truncated = false;
        for line in output.lines() {
            let Some(d) = parse_line(matcher, line) else {
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

    /// cargo_test failures 块解析:`failures:` 行与 `test result:` 行之间的
    /// 非空行=失败测试名,逐条落 `test_failure` 条目(severity=8;编译诊断
    /// 由 $rustc 通道独立计数,两通道零重叠)。帽感知:超帽只记 truncated。
    fn collect_cargo_test(output: &str) -> (usize, usize, Vec<Value>, bool) {
        let (mut errors, warnings, mut diags, mut truncated) =
            Self::collect_diagnostics(output, "$rustc");
        let mut inside_failures = false;
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed == "failures:" {
                inside_failures = true;
                continue;
            }
            if trimmed.starts_with("test result:") {
                inside_failures = false;
                continue;
            }
            if !inside_failures || trimmed.is_empty() {
                continue;
            }
            errors += 1;
            if diags.len() < DIAGNOSTICS_CAP {
                diags.push(serde_json::json!({
                    "path": "",
                    "line": 0,
                    "col": 0,
                    "severity": 8,
                    "code": "test_failure",
                    "message": trimmed,
                }));
            } else {
                truncated = true;
            }
        }
        (errors, warnings, diags, truncated)
    }

    /// pytest `--tb=line` 单行失败摘要解析:
    /// `<path>.py:<line>: <ExcType>: <message>` → 同构诊断条目
    /// (以 `.py` 为锚切分——Windows 盘符冒号不误伤;severity 恒 8;
    /// 非失败行零损失跳过;pytest warnings summary 不入 v1 信封,如实不计)
    fn parse_pytest_line(line: &str) -> Option<Value> {
        let anchor = line.find(".py:")?;
        let path = &line[..anchor + 3];
        let rest = line[anchor + 3..].trim_start_matches(':');
        let (lineno_s, tail) = rest.split_once(':')?;
        let lineno: usize = lineno_s.trim().parse().ok()?;
        let tail = tail.trim();
        let (code, message) = match tail.split_once(": ") {
            Some((c, m)) => (c.to_string(), m.to_string()),
            None => (tail.to_string(), String::new()),
        };
        Some(serde_json::json!({
            "path": path,
            "line": lineno,
            "col": 0,
            "severity": 8,
            "code": code,
            "message": message,
        }))
    }

    /// pytest 剖面解析(失败行逐条计数;errors=命中失败行数)
    fn collect_pytest(output: &str) -> (usize, usize, Vec<Value>, bool) {
        let mut errors = 0usize;
        let mut diags: Vec<Value> = Vec::new();
        let mut truncated = false;
        for line in output.lines() {
            let Some(d) = Self::parse_pytest_line(line) else {
                continue;
            };
            errors += 1;
            if diags.len() < DIAGNOSTICS_CAP {
                diags.push(d);
            } else {
                truncated = true;
            }
        }
        (errors, 0, diags, truncated)
    }
}

#[async_trait::async_trait]
impl crate::io_handlers::tool_handler::ToolFunction for BuildCheckTool {
    async fn call(&self, args: &Value) -> IoResult {
        // profile 解析:缺省/Null=cargo_check(现行为逐字节兼容);非字符串=
        // 显式报错;未知键=fail-visible 列出合法剖面(不猜,不静默回落)
        let profile =
            match args.get("profile") {
                None | Some(Value::Null) => PROFILES
                    .iter()
                    .find(|p| p.key == "cargo_check")
                    .expect("cargo_check profile present in const table"),
                Some(Value::String(s)) => PROFILES
                    .iter()
                    .find(|p| p.key == s.as_str())
                    .ok_or_else(|| {
                        format!(
                            "build_check: unknown profile \"{s}\" (valid profiles: {})",
                            PROFILES
                                .iter()
                                .map(|p| p.key)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?,
                Some(_) => return Err("build_check: profile must be a string".to_string()),
            };
        let started = Instant::now();
        let out = self
            .runner
            .run(
                profile.command,
                Duration::from_secs(BUILD_CHECK_TIMEOUT_SECS),
            )
            .await
            .map_err(|e| format!("build_check could not run the check command: {e}"))?;

        let combined = {
            let mut s = out.stdout.clone();
            s.push_str(&out.stderr);
            s
        };
        let (errors, warnings, diags, truncated) = match profile.kind {
            ProfileKind::RustcShort => Self::collect_diagnostics(&combined, profile.matcher),
            ProfileKind::CargoTest => Self::collect_cargo_test(&combined),
            ProfileKind::Pytest => Self::collect_pytest(&combined),
        };
        let verdict = classify_verdict(out.exit_code, errors);

        let note = if verdict == "unknown" {
            if profile.kind == ProfileKind::Pytest {
                Some(
                    "non-zero exit with no parseable failure lines: pytest output did not \
                     contain --tb=line failure entries (collection error? check the \
                     pytest environment/interpreter)"
                        .to_string(),
                )
            } else {
                Some(
                    "non-zero exit with no parseable diagnostics: the workspace may not be \
                     a Rust project, or the failure is at the manifest/toolchain level"
                        .to_string(),
                )
            }
        } else {
            None
        };

        let mut map = serde_json::json!({
            "profile": profile.key,
            "command": profile.command,
            "exit_code": out.exit_code,
            "timed_out": out.timed_out,
            "verdict": verdict,
            "counts": { "errors": errors, "warnings": warnings },
            "diagnostics": diags,
            "truncated": truncated,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        });
        if let Some(n) = note {
            if let Some(o) = map.as_object_mut() {
                o.insert("note".to_string(), Value::String(n));
            }
        }
        Ok(map)
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

    /// 命令捕获桩(剖面命令契约断言专用)
    struct CommandCaptureRunner {
        command: Mutex<Option<String>>,
        output: Mutex<Option<Result<CommandOutput, String>>>,
    }

    #[async_trait::async_trait]
    impl CommandRunner for CommandCaptureRunner {
        async fn run(&self, command: &str, _timeout: Duration) -> Result<CommandOutput, String> {
            *self.command.lock().unwrap() = Some(command.to_string());
            self.output
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| "stub exhausted".to_string())?
        }
    }

    fn capture_tool(
        stdout: &str,
        exit_code: Option<i32>,
    ) -> (BuildCheckTool, Arc<CommandCaptureRunner>) {
        let runner = Arc::new(CommandCaptureRunner {
            command: Mutex::new(None),
            output: Mutex::new(Some(Ok(CommandOutput {
                exit_code,
                stdout: stdout.to_string(),
                stderr: String::new(),
                timed_out: false,
            }))),
        });
        (BuildCheckTool::with_runner(runner.clone()), runner)
    }

    #[tokio::test]
    async fn explicit_cargo_check_profile_matches_legacy_command() {
        // profile 化兼容契约:显式缺省剖面=原固定命令逐字节一致+信封增 profile 字段
        let (tool, runner) = capture_tool("Finished dev target(s)", Some(0));
        let r = tool
            .call(&serde_json::json!({"profile": "cargo_check"}))
            .await
            .unwrap();
        assert_eq!(
            runner.command.lock().unwrap().as_deref(),
            Some(BUILD_CHECK_COMMAND),
            "缺省剖面命令必须与 profile 化前逐字节一致"
        );
        assert_eq!(
            r.get("profile").and_then(|v| v.as_str()),
            Some("cargo_check")
        );
    }

    #[tokio::test]
    async fn pytest_profile_parses_tb_line_failures() {
        let (tool, runner) = capture_tool(
            "============================= test session starts =============================\n\
             collecting ... collected 3 items\n\
             tests\\video_core.py:12: ValueError: boom\n\
             tests/sampler.py:30: AssertionError: assert 1 == 2\n\
             2 failed, 1 passed in 0.42s",
            Some(1),
        );
        let r = tool
            .call(&serde_json::json!({"profile": "pytest"}))
            .await
            .unwrap();
        assert_eq!(
            runner.command.lock().unwrap().as_deref(),
            Some("pytest -q --tb=line")
        );
        assert_eq!(r.get("verdict").and_then(|v| v.as_str()), Some("fail"));
        assert_eq!(
            r.get("counts")
                .and_then(|c| c.get("errors"))
                .and_then(|v| v.as_i64()),
            Some(2)
        );
        let diags = r.get("diagnostics").and_then(|v| v.as_array()).unwrap();
        assert_eq!(diags.len(), 2);
        assert_eq!(
            diags[0].get("path").and_then(|v| v.as_str()),
            Some("tests\\video_core.py")
        );
        assert_eq!(diags[0].get("line").and_then(|v| v.as_i64()), Some(12));
        assert_eq!(
            diags[0].get("code").and_then(|v| v.as_str()),
            Some("ValueError")
        );
        assert_eq!(
            diags[0].get("message").and_then(|v| v.as_str()),
            Some("boom")
        );
        assert_eq!(
            diags[1].get("code").and_then(|v| v.as_str()),
            Some("AssertionError")
        );
    }

    #[tokio::test]
    async fn pytest_pass_reports_pass_without_note() {
        let (tool, _runner) = capture_tool("3 passed in 0.10s", Some(0));
        let r = tool
            .call(&serde_json::json!({"profile": "pytest"}))
            .await
            .unwrap();
        assert_eq!(r.get("verdict").and_then(|v| v.as_str()), Some("pass"));
        assert!(r.get("note").is_none(), "pass 时 note 必须缺席");
    }

    #[tokio::test]
    async fn pytest_no_parseable_failures_nonzero_exit_is_unknown() {
        // 收集期失败(--tb=line 无失败行、exit≠0)=如实 unknown+剖面化提示
        let (tool, _runner) = capture_tool("no tests ran in 0.01s", Some(5));
        let r = tool
            .call(&serde_json::json!({"profile": "pytest"}))
            .await
            .unwrap();
        assert_eq!(r.get("verdict").and_then(|v| v.as_str()), Some("unknown"));
        let note = r.get("note").and_then(|v| v.as_str()).unwrap_or("");
        assert!(note.contains("pytest"), "note 须指向 pytest 环境: {note}");
    }

    #[tokio::test]
    async fn cargo_clippy_profile_parses_warnings_via_rustc() {
        let (tool, runner) = capture_tool(
            "src/x.rs:12:5: warning: unused variable: `y`\n\
             src/y.rs:30:1: warning: unused import: `std::io`",
            Some(0),
        );
        let r = tool
            .call(&serde_json::json!({"profile": "cargo_clippy"}))
            .await
            .unwrap();
        assert_eq!(
            runner.command.lock().unwrap().as_deref(),
            Some("cargo clippy --message-format=short")
        );
        assert_eq!(r.get("verdict").and_then(|v| v.as_str()), Some("pass"));
        assert_eq!(
            r.get("counts")
                .and_then(|c| c.get("warnings"))
                .and_then(|v| v.as_i64()),
            Some(2)
        );
    }

    #[tokio::test]
    async fn cargo_test_profile_collects_compile_errors_and_failure_names() {
        let (tool, _runner) = capture_tool(
            "src/lib.rs:9:5: error[E0308]: mismatched types\n\
             test tests::a ... FAILED\n\
             failures:\n\
             \x20   tests::a\n\
             \x20   tests::b\n\
             test result: FAILED. 0 passed; 2 failed; 0 ignored",
            Some(101),
        );
        let r = tool
            .call(&serde_json::json!({"profile": "cargo_test"}))
            .await
            .unwrap();
        assert_eq!(r.get("verdict").and_then(|v| v.as_str()), Some("fail"));
        // errors = 1 编译诊断 + 2 失败测试名(两通道零重叠)
        assert_eq!(
            r.get("counts")
                .and_then(|c| c.get("errors"))
                .and_then(|v| v.as_i64()),
            Some(3)
        );
        let diags = r.get("diagnostics").and_then(|v| v.as_array()).unwrap();
        let codes: Vec<&str> = diags
            .iter()
            .filter_map(|d| d.get("code").and_then(|v| v.as_str()))
            .collect();
        assert!(codes.contains(&"E0308"), "编译诊断通道在场: {codes:?}");
        assert_eq!(
            codes.iter().filter(|c| **c == "test_failure").count(),
            2,
            "failures 块两条测试名条目: {codes:?}"
        );
    }

    #[tokio::test]
    async fn unknown_profile_is_fail_visible() {
        let (tool, _runner) = capture_tool("", Some(0));
        let err = tool
            .call(&serde_json::json!({"profile": "cargo run"}))
            .await
            .unwrap_err();
        assert!(err.contains("unknown profile"), "got: {err}");
        assert!(
            err.contains("cargo_check, cargo_clippy, cargo_test, pytest"),
            "错误须列出合法剖面全集: {err}"
        );
    }

    #[tokio::test]
    async fn non_string_profile_is_rejected() {
        let (tool, _runner) = capture_tool("", Some(0));
        let err = tool
            .call(&serde_json::json!({"profile": 42}))
            .await
            .unwrap_err();
        assert!(err.contains("profile must be a string"), "got: {err}");
    }

    #[tokio::test]
    async fn extraneous_args_do_not_alter_command() {
        // 注入守卫契约:多余参数键不得改变固定命令面(模型不可组命令)
        let (tool, runner) = capture_tool("Finished dev target(s)", Some(0));
        tool.call(&serde_json::json!({
            "profile": "cargo_check",
            "command": "evil --beyond-repair",
            "args": ["--anything"]
        }))
        .await
        .unwrap();
        assert_eq!(
            runner.command.lock().unwrap().as_deref(),
            Some(BUILD_CHECK_COMMAND),
            "命令面必须恒等于剖面表固定值"
        );
    }
}
