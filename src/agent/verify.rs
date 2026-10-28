// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! ReAct 自验证门禁(自验证门禁批)
//!
//! 写面工具(file_write/edit_file/file_create)成功后由 runner 置位
//! pending 标志;轮末回喂下一轮 LLM 前统一执行验证命令组,诊断经确定性
//! matcher(`crate::tasks::matcher`)解析后作为结构化附加观察块回喂——
//! 「写→验→修→验」内闭环,验证判定**零 LLM 参与**(severity==8 存在性
//! 布尔判定),闭环编排由宿主 runner 承担。
//!
//! ## 契约要点(设计批 §4)
//!
//! - 命令**仅来自配置文件**(agent definition `verify` 块,人类预置);
//!   模型只读消费、不可生成/修改——安全边界=配置文件写权限。
//! - `matcher` 仅接受 `$rustc` / `$tsc`(matcher.rs v1 词汇);未知值启动期
//!   显式报错拒绝(fail-fast,不猜)。
//! - 缺省无 `verify` 块 = 门禁不存在(零行为变化,缺省不变式)。
//! - 短路:多条命令顺序执行,首个含 Error 的命令后停(同族命令后跑无增量)。
//! - 超时/spawn 失败 = verdict `unknown` 显式回喂(不假装通过、不静默)。
//! - 输出帽:stdout/err 合计 256KB;错误行集 50 条 + truncated 标记。
//! - 连续 fail 计数达 `max_verdicts_per_turn`(缺省 3)= 显式上抛人审
//!   (对齐 PolicyReview max_corrections 精神,fail-visible 不再静默循环)。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::agent::journal::JournalWriter;
use crate::tasks::matcher::parse_line;

/// 单命令验证超时缺省值(秒)
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// max_verdicts_per_turn 缺省值(连续 fail 上抛人审的上限)
pub const DEFAULT_MAX_VERDICTS_PER_TURN: u32 = 3;
/// stdout/err 合计帽(字节;防巨型构建日志 OOM)
const OUTPUT_CAP_BYTES: usize = 256 * 1024;
/// Error 行集帽(条数;超出截断并标记)
const ERROR_LINES_CAP: usize = 50;

/// matcher 词汇(v1:matcher.rs 两组);未知值装载期拒绝
const MATCHER_RUSTC: &str = "$rustc";
const MATCHER_TSC: &str = "$tsc";

/// verify 配置块(agent definition `verify`;人类预置,模型只读)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifyConfig {
    /// 验证命令组(顺序执行,首个 Error 短路);空数组=显式配置但空转
    #[serde(default)]
    pub commands: Vec<VerifyCommandConfig>,
    /// 连续 fail 上抛人审的上限(缺省 3)
    #[serde(default = "default_max_verdicts")]
    pub max_verdicts_per_turn: u32,
}

fn default_max_verdicts() -> u32 {
    DEFAULT_MAX_VERDICTS_PER_TURN
}

impl VerifyConfig {
    /// 启动期契约校验(fail-fast):matcher 词汇白名单;返回人读错误
    pub fn validate(&self) -> Result<(), String> {
        for cmd in &self.commands {
            if cmd.matcher != MATCHER_RUSTC && cmd.matcher != MATCHER_TSC {
                return Err(format!(
                    "verify.commands[{}] unknown matcher '{}' (accepted: $rustc | $tsc)",
                    cmd.name, cmd.matcher
                ));
            }
            if cmd.command.trim().is_empty() {
                return Err(format!("verify.commands[{}] command is empty", cmd.name));
            }
        }
        Ok(())
    }
}

/// 验证命令单条配置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifyCommandConfig {
    /// 命令名(journal/回喂块标识)
    pub name: String,
    /// 命令行原文(经 shell 执行,与 shell_exec 同级安全口径)
    pub command: String,
    /// 输出行解析器(仅 $rustc | $tsc)
    pub matcher: String,
    /// 单命令超时(秒;缺省 120)
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// 单命令执行产物(CommandRunner 注入缝的产出形态)
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// 退出码(None=超时/启动失败)
    pub exit_code: Option<i32>,
    /// stdout(已按合计帽截断)
    pub stdout: String,
    /// stderr(已按合计帽截断)
    pub stderr: String,
    /// 是否超时(超时=verdict unknown)
    pub timed_out: bool,
}

/// 命令执行注入 trait(生产=tokio 进程 spawn;测试=打桩)
// async_trait 展开体自带 #[must_use](Future)与返回 Result 的 must_use 语义
// 重叠,新 clippy 判 double_must_use;属性出自宏展开,调用侧无法定向压制。
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    /// 执行一条 shell 命令(超时/输出截断由实现保证与 shell_exec 同级口径)
    async fn run(&self, command: &str, timeout: Duration) -> Result<CommandOutput, String>;
}

/// 单命令判定结果
#[derive(Debug, Clone)]
struct CommandVerdict {
    name: String,
    command: String,
    exit_code: Option<i32>,
    verdict: String, // pass | fail | unknown
    error_lines: Vec<String>,
    errors_truncated: bool,
    error_count: usize,
    diags_digest: String,
    output_digest: String,
}

/// 轮末 verify 执行报告(回喂块 + 入账要素 + 上抛信号)
#[derive(Debug, Clone)]
pub struct VerdictReport {
    /// 总判定(pass | fail | unknown)
    pub verdict: String,
    /// 结构化回喂块(拼入工具观察面;空=全过的一句话)
    pub observation: String,
    /// 是否触发上抛人审(连续 fail 达 max_verdicts_per_turn)
    pub escalate: bool,
}

/// 自验证编排器(pending 标志 + 延迟执行 + 短路 + 帽 + 上限)
pub struct VerifyOrchestrator {
    config: VerifyConfig,
    runner: Arc<dyn CommandRunner>,
    pending: AtomicBool,
    consecutive_fails: AtomicU32,
}

impl VerifyOrchestrator {
    /// 构造(config 已过 validate;调用方负责 fail-fast)
    pub fn new(config: VerifyConfig, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            config,
            runner,
            pending: AtomicBool::new(false),
            consecutive_fails: AtomicU32::new(0),
        }
    }

    /// 写面工具成功后置位(延迟触发:轮末统一执行,一轮 N 写只跑 1 次)
    pub fn note_write_success(&self) {
        self.pending.store(true, Ordering::SeqCst);
    }

    /// 是否有待执行的验证(诊断/测试面)
    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst)
    }

    /// 轮末执行 pending verify(回喂下一轮前调用);无 pending = None(零开销路径)。
    /// 执行后 pending 复位;verdict 全量入账(journal 可选面)。
    pub async fn run_pending(&self, journal: Option<&JournalWriter>) -> Option<VerdictReport> {
        if !self.pending.swap(false, Ordering::SeqCst) {
            return None;
        }
        if self.config.commands.is_empty() {
            return Some(VerdictReport {
                verdict: "pass".to_string(),
                observation: String::new(),
                escalate: false,
            });
        }

        let mut last: Option<CommandVerdict> = None;
        let mut passed: usize = 0;
        for cmd in &self.config.commands {
            let timeout = Duration::from_secs(cmd.timeout_secs);
            let out = match self.runner.run(&cmd.command, timeout).await {
                Ok(o) => o,
                Err(e) => {
                    let v = CommandVerdict {
                        name: cmd.name.clone(),
                        command: cmd.command.clone(),
                        exit_code: None,
                        verdict: "unknown".to_string(),
                        error_lines: Vec::new(),
                        errors_truncated: false,
                        error_count: 0,
                        diags_digest: crate::agent::journal::evorule_digest(&e),
                        output_digest: crate::agent::journal::evorule_digest(&e),
                    };
                    last = Some(v);
                    break;
                }
            };
            let verdict = build_command_verdict(cmd, &out);
            let is_fail = verdict.verdict == "fail";
            let is_unknown = verdict.verdict == "unknown";
            last = Some(verdict);
            if is_fail || is_unknown {
                break; // 短路:首个含 Error(或不可判)的命令后停
            }
            passed += 1;
        }

        let v = last.expect("at least one command executed");
        // journal 全量入账(best-effort;失败不阻塞回喂)
        if let Some(j) = journal {
            let _ = j.verify_ran(
                &v.name,
                &v.command,
                v.exit_code,
                v.error_count,
                &v.diags_digest,
                &v.verdict,
                &v.output_digest,
            );
        }

        // 连续 fail 计数与上抛判定
        let mut escalate = false;
        if v.verdict == "fail" {
            let n = self.consecutive_fails.fetch_add(1, Ordering::SeqCst) + 1;
            escalate = n >= self.config.max_verdicts_per_turn;
        } else if v.verdict == "pass" {
            self.consecutive_fails.store(0, Ordering::SeqCst);
        }

        Some(VerdictReport {
            observation: render_observation(&v, passed, escalate),
            verdict: v.verdict.clone(),
            escalate,
        })
    }
}

/// 单命令判定:输出行经 matcher 解析,Error 级(severity==8)存在性布尔判定
fn build_command_verdict(cmd: &VerifyCommandConfig, out: &CommandOutput) -> CommandVerdict {
    if out.timed_out || out.exit_code.is_none() {
        return CommandVerdict {
            name: cmd.name.clone(),
            command: cmd.command.clone(),
            exit_code: out.exit_code,
            verdict: "unknown".to_string(),
            error_lines: Vec::new(),
            errors_truncated: false,
            error_count: 0,
            diags_digest: crate::agent::journal::evorule_digest(""),
            output_digest: crate::agent::journal::evorule_digest(&combined_output(out)),
        };
    }

    let mut error_lines: Vec<String> = Vec::new();
    let mut error_count = 0usize;
    let mut truncated = false;
    for line in combined_output(out).lines() {
        let Some(d) = parse_line(&cmd.matcher, line) else {
            continue;
        };
        if d.severity != 8 {
            continue;
        }
        error_count += 1;
        if error_lines.len() < ERROR_LINES_CAP {
            let code = if d.code.is_empty() {
                String::new()
            } else {
                format!("[{}]", d.code)
            };
            error_lines.push(format!(
                "{}:{}:{} error{} {}",
                d.path, d.line, d.col, code, d.msg
            ));
        } else {
            truncated = true;
        }
    }

    let verdict = if error_count > 0 { "fail" } else { "pass" };
    let diags_text = error_lines.join("\n");
    CommandVerdict {
        name: cmd.name.clone(),
        command: cmd.command.clone(),
        exit_code: out.exit_code,
        verdict: verdict.to_string(),
        error_lines,
        errors_truncated: truncated,
        error_count,
        diags_digest: crate::agent::journal::evorule_digest(&diags_text),
        output_digest: crate::agent::journal::evorule_digest(&combined_output(out)),
    }
}

/// stdout/err 合计帽(256KB):合计超限时 stderr 让位截断(诊断多在 stderr,
/// 先保 stdout 全量——matcher 两形态均可能走 stdout,如 cargo short)
fn combined_output(out: &CommandOutput) -> String {
    let mut s = out.stdout.clone();
    if s.len() + out.stderr.len() > OUTPUT_CAP_BYTES {
        let remain = OUTPUT_CAP_BYTES.saturating_sub(s.len());
        let cut: String = out.stderr.chars().take(remain).collect();
        s.push_str(&cut);
    } else {
        s.push_str(&out.stderr);
    }
    s
}

/// 回喂块渲染(结构化诊断行集 + 指引;全过=一句话)
fn render_observation(v: &CommandVerdict, passed: usize, escalate: bool) -> String {
    let mut text = String::new();
    match v.verdict.as_str() {
        "pass" => {
            text.push_str(&format!("verify: {passed} command(s) passed"));
        }
        "fail" => {
            text.push_str(&format!(
                "verify: FAILED (command '{}', exit_code: {})\n",
                v.command,
                v.exit_code.unwrap_or(-1)
            ));
            text.push_str(&format!(
                "errors: {}{}\n",
                v.error_count,
                if v.errors_truncated {
                    " (truncated: showing first 50)"
                } else {
                    ""
                }
            ));
            for l in &v.error_lines {
                text.push_str(l);
                text.push('\n');
            }
            text.push_str(
                "Fix the errors above; saving the fix will trigger automatic re-verification.",
            );
        }
        _ => {
            text.push_str(&format!(
                "verify: UNKNOWN (command '{}' did not complete; timed out or failed to launch). \
                 Verification could not run — treat this as unverified, not passed.",
                v.command
            ));
        }
    }
    if escalate {
        text.push_str(
            "\nverify: fix loop limit reached (max consecutive failed verdicts) — \
             surface this to the user for manual review instead of retrying blindly.",
        );
    }
    text
}

// ---------------------------------------------------------------------------
// 生产命令执行器(tokio 进程 spawn)
// ---------------------------------------------------------------------------

/// 生产 [`CommandRunner`]:经 shell 执行验证命令(命令行仅来自人类预置配置)
///
/// - Windows: `cmd /C <command>`;Unix: `sh -c <command>`
/// - 工作目录 = agent workdir(验证命令的工作区语义与写面工具一致)
/// - 超时 = 杀进程,verdict `unknown`(不假装通过、不静默)
/// - 单侧输出帽 256KB(合计帽在 [`combined_output`] 统一强制)
pub struct ShellCommandRunner {
    workdir: std::path::PathBuf,
}

impl ShellCommandRunner {
    /// 构造(workdir = 验证命令的执行工作目录)
    pub fn new(workdir: &std::path::Path) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
        }
    }
}

#[async_trait::async_trait]
impl CommandRunner for ShellCommandRunner {
    async fn run(&self, command: &str, timeout: Duration) -> Result<CommandOutput, String> {
        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", command]);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", command]);
            c
        };
        cmd.current_dir(&self.workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = cmd
            .spawn()
            .map_err(|e| format!("verify command spawn failed: {}", e))?;
        let waited = child.wait_with_output();
        match tokio::time::timeout(timeout, waited).await {
            Ok(Ok(out)) => Ok(CommandOutput {
                exit_code: out.status.code(),
                stdout: cap_string(String::from_utf8_lossy(&out.stdout).into_owned()),
                stderr: cap_string(String::from_utf8_lossy(&out.stderr).into_owned()),
                timed_out: false,
            }),
            Ok(Err(e)) => Err(format!("verify command wait failed: {}", e)),
            Err(_) => {
                // 超时:wait_with_output 的 future 被 drop,child 随之 drop,
                // kill_on_drop(true) 负责终止进程;verdict 交由上层判 unknown
                Ok(CommandOutput {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    timed_out: true,
                })
            }
        }
    }
}

/// 单侧输出帽(字节;防巨型构建日志整段进入内存)
fn cap_string(s: String) -> String {
    if s.len() <= OUTPUT_CAP_BYTES {
        s
    } else {
        s.chars().take(OUTPUT_CAP_BYTES).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::journal::JournalEvent;
    use std::sync::Mutex;

    /// 打桩 runner:按调用次序返回预设输出
    struct StubRunner {
        outputs: Mutex<Vec<Result<CommandOutput, String>>>,
    }

    impl StubRunner {
        fn new(outputs: Vec<Result<CommandOutput, String>>) -> Self {
            Self {
                outputs: Mutex::new(outputs),
            }
        }
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

    fn ok_output(stdout: &str) -> Result<CommandOutput, String> {
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
            timed_out: false,
        })
    }

    fn config(commands: Vec<(&str, &str, &str)>) -> VerifyConfig {
        VerifyConfig {
            commands: commands
                .into_iter()
                .map(|(name, command, matcher)| VerifyCommandConfig {
                    name: name.to_string(),
                    command: command.to_string(),
                    matcher: matcher.to_string(),
                    timeout_secs: DEFAULT_TIMEOUT_SECS,
                })
                .collect(),
            max_verdicts_per_turn: DEFAULT_MAX_VERDICTS_PER_TURN,
        }
    }

    #[test]
    fn config_parses_defaults_and_full_form() {
        let v: serde_json::Value = serde_json::json!({
            "commands": [
                { "name": "cargo-check", "command": "cargo check --message-format=short --all-targets", "matcher": "$rustc", "timeout_secs": 120 }
            ],
            "max_verdicts_per_turn": 3
        });
        let cfg: VerifyConfig = serde_json::from_value(v).unwrap();
        assert_eq!(cfg.commands.len(), 1);
        assert_eq!(cfg.max_verdicts_per_turn, 3);
        assert_eq!(cfg.commands[0].timeout_secs, 120);
        cfg.validate().unwrap();

        // 缺省形态:timeout 缺省 120、max 缺省 3、commands 缺省空
        let cfg2: VerifyConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(cfg2.commands.is_empty());
        assert_eq!(cfg2.max_verdicts_per_turn, 3);
    }

    #[test]
    fn config_rejects_unknown_matcher_fail_fast() {
        let cfg = config(vec![("x", "echo hi", "$go")]);
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("unknown matcher"), "got: {err}");
        // serde 反序列化不拦(装载点 validate 拦)——契约语义单点
        let raw: VerifyConfig = serde_json::from_value(
            serde_json::json!({"commands":[{"name":"x","command":"y","matcher":"$go"}]}),
        )
        .unwrap();
        assert!(raw.validate().is_err());
    }

    #[tokio::test]
    async fn no_pending_means_zero_execution() {
        let orch = VerifyOrchestrator::new(
            config(vec![("c", "echo", "$rustc")]),
            Arc::new(StubRunner::new(vec![])),
        );
        assert!(orch.run_pending(None).await.is_none());
    }

    #[tokio::test]
    async fn pending_fires_once_and_resets() {
        let orch = VerifyOrchestrator::new(
            config(vec![("c", "cargo check", "$rustc")]),
            Arc::new(StubRunner::new(vec![ok_output("Compiling x")])),
        );
        orch.note_write_success();
        assert!(orch.has_pending());
        let report = orch.run_pending(None).await.unwrap();
        assert_eq!(report.verdict, "pass");
        assert!(report.observation.contains("1 command(s) passed"));
        // 单次触发:第二轮末无 pending 不再执行
        assert!(!orch.has_pending());
        assert!(orch.run_pending(None).await.is_none());
    }

    #[tokio::test]
    async fn short_circuits_on_first_failing_command() {
        let runner = Arc::new(StubRunner::new(vec![
            ok_output("src/a.rs:1:1: error[E0308]: mismatched types"),
            ok_output("ignored"), // 不应被消费
        ]));
        let orch = VerifyOrchestrator::new(
            config(vec![("a", "cargo check", "$rustc"), ("b", "tsc", "$tsc")]),
            runner,
        );
        orch.note_write_success();
        let report = orch.run_pending(None).await.unwrap();
        assert_eq!(report.verdict, "fail");
        assert!(report
            .observation
            .contains("src/a.rs:1:1 error[E0308] mismatched types"));
        assert!(report.observation.contains("automatic re-verification"));
    }

    #[tokio::test]
    async fn timeout_yields_unknown_verdict() {
        let orch = VerifyOrchestrator::new(
            config(vec![("c", "slow", "$rustc")]),
            Arc::new(StubRunner::new(vec![Err("timeout".to_string())])),
        );
        orch.note_write_success();
        let report = orch.run_pending(None).await.unwrap();
        assert_eq!(report.verdict, "unknown");
        assert!(report.observation.contains("UNKNOWN"));
    }

    #[tokio::test]
    async fn error_lines_capped_at_50_with_truncation_note() {
        let mut stdout = String::new();
        for i in 1..=60 {
            stdout.push_str(&format!("src/f{i}.rs:{i}:1: error[E0001]: boom {i}\n"));
        }
        let orch = VerifyOrchestrator::new(
            config(vec![("c", "cargo check", "$rustc")]),
            Arc::new(StubRunner::new(vec![Ok(CommandOutput {
                exit_code: Some(101),
                stdout,
                stderr: String::new(),
                timed_out: false,
            })])),
        );
        orch.note_write_success();
        let report = orch.run_pending(None).await.unwrap();
        assert_eq!(report.verdict, "fail");
        assert!(report.observation.contains("errors: 60"));
        assert!(report.observation.contains("truncated"));
        assert_eq!(report.observation.matches("error[E0001]").count(), 50);
    }

    #[tokio::test]
    async fn consecutive_fails_escalate_at_limit() {
        let outputs = (0..3)
            .map(|_| ok_output("src/a.rs:1:1: error[E0308]: boom"))
            .collect();
        let orch = VerifyOrchestrator::new(
            config(vec![("c", "cargo check", "$rustc")]),
            Arc::new(StubRunner::new(outputs)),
        );
        // 前两次 fail:不上抛
        orch.note_write_success();
        assert!(!orch.run_pending(None).await.unwrap().escalate);
        orch.note_write_success();
        assert!(!orch.run_pending(None).await.unwrap().escalate);
        // 第三次连续 fail:达 max_verdicts_per_turn=3,显式上抛
        orch.note_write_success();
        let report = orch.run_pending(None).await.unwrap();
        assert!(report.escalate);
        assert!(report.observation.contains("manual review"));
        // pass 后计数归零
        // (StubRunner 已耗尽,另起一轮验证 pass 清零语义由单元覆盖:
        //  连续 fail 计数在 pass 时 reset——下方直接断言内部状态)
    }

    #[tokio::test]
    async fn pass_resets_consecutive_fail_counter() {
        let outputs = vec![
            ok_output("src/a.rs:1:1: error[E0308]: boom"),
            ok_output("all good"),
        ];
        let orch = VerifyOrchestrator::new(
            config(vec![("c", "cargo check", "$rustc")]),
            Arc::new(StubRunner::new(outputs)),
        );
        orch.note_write_success();
        assert_eq!(orch.run_pending(None).await.unwrap().verdict, "fail");
        orch.note_write_success();
        assert_eq!(orch.run_pending(None).await.unwrap().verdict, "pass");
        // 计数已清零:再 fail 一次不触上抛(需重新累计到 3)
        assert_eq!(
            orch.consecutive_fails.load(Ordering::SeqCst),
            0,
            "pass must reset the counter"
        );
    }

    #[tokio::test]
    async fn journal_records_verify_ran_with_digests() {
        let dir = tempfile::tempdir().unwrap();
        let journal_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&journal_dir).unwrap();
        let journal = JournalWriter::open(&journal_dir, "s-verify").unwrap();

        let orch = VerifyOrchestrator::new(
            config(vec![("c", "cargo check", "$rustc")]),
            Arc::new(StubRunner::new(vec![ok_output(
                "src/a.rs:3:7: error[E0308]: mismatched types",
            )])),
        );
        orch.note_write_success();
        let report = orch.run_pending(Some(&journal)).await.unwrap();
        assert_eq!(report.verdict, "fail");

        let path = JournalWriter::path_for(&journal_dir, "s-verify");
        let lines = crate::agent::journal::read_all(&path).unwrap();
        let hit = lines.iter().find_map(|l| match &l.event {
            JournalEvent::VerifyRan {
                name,
                verdict,
                error_count,
                exit_code,
                ..
            } => Some((name.clone(), verdict.clone(), *error_count, *exit_code)),
            _ => None,
        });
        let (name, verdict, error_count, exit_code) = hit.expect("VerifyRan recorded");
        assert_eq!(name, "c");
        assert_eq!(verdict, "fail");
        assert_eq!(error_count, 1);
        assert_eq!(exit_code, Some(0));
    }

    #[test]
    fn tsc_matcher_lines_are_parsed() {
        let out = CommandOutput {
            exit_code: Some(2),
            stdout: "web/src/main.ts(3,7): error TS2322: Type 'string' is not assignable to type 'number'.".to_string(),
            stderr: String::new(),
            timed_out: false,
        };
        let cmd = VerifyCommandConfig {
            name: "tsc".to_string(),
            command: "npx tsc --noEmit".to_string(),
            matcher: "$tsc".to_string(),
            timeout_secs: 60,
        };
        let v = build_command_verdict(&cmd, &out);
        assert_eq!(v.verdict, "fail");
        assert_eq!(v.error_count, 1);
        assert!(v.error_lines[0].starts_with("web/src/main.ts:3:7 error[TS2322]"));
    }
}
