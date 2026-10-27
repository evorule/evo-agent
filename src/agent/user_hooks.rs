// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! user_hooks —— 可编程 hooks（交互批 第二批）——用户定制挂点，治理之后、执行前后。
//!
//! 配置 `{workdir}/.evo-agent/hooks.json`：`pre_tool` / `post_tool` / `turn_end`
//! 三类挂点，各为有序规则列表（工具名匹配 `name` / `prefix*` + 命令行 + 超时）。
//!
//! ## 顺序契约（39 号 §三）
//!
//! 治理 enforce（硬闸）→ hooks（用户定制）→ 工具执行。hooks **不可绕过治理**：
//! pre_tool 挂点在管道内位于治理裁决与分级审批之后、执行（⑦）之前；治理拒绝
//! 的调用永远不会到达 hooks。
//!
//! ## 执行语义
//!
//! - 命中规则以平台 shell 子进程执行用户命令，stdin 传 JSON 上下文
//!   `{hook, tool, args, result?, session_id?}`；
//! - `pre_tool`：stdout 可返回 `{"decision":"deny","reason":…}`（拒绝执行，
//!   拒绝文案回喂 LLM）或 `{"decision":"rewrite","args":{…}}`（参数改写）；
//!   两者皆无（或 stdout 非法）=放行；规则依序执行，deny 即停；
//! - `post_tool` / `turn_end`：只读不可阻断——结局/退出码只留痕；
//! - 超时缺省 10s 可配（钳 1-120s）；超时与失败按 **pre fail-open**（放行+
//!   留痕）/ **post·turn_end 忽略**处理；
//! - 工作目录 = 会话 workdir（hooks 在边界内执行）。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

/// 单挂点规则数上限（防配置膨胀拖慢每次调用）
pub const HOOK_MAX_RULES: usize = 20;
/// 命令行字符上限
pub const HOOK_MAX_COMMAND_CHARS: usize = 2000;
/// 工具名匹配式字符上限
pub const HOOK_MAX_TOOL_PATTERN_CHARS: usize = 100;
/// 子进程 stdout 读取上限（防 hook 输出爆炸）
pub const HOOK_MAX_STDOUT_BYTES: usize = 64 * 1024;
/// 超时缺省（秒）
pub const HOOK_DEFAULT_TIMEOUT_SECS: u64 = 10;
/// 超时钳位下限（秒）
pub const HOOK_MIN_TIMEOUT_SECS: u64 = 1;
/// 超时钳位上限（秒）
pub const HOOK_MAX_TIMEOUT_SECS: u64 = 120;

/// 三类挂点
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// 工具执行前（可 deny/改写）
    PreTool,
    /// 工具执行后（只读）
    PostTool,
    /// 回合结束（只读）
    TurnEnd,
}

impl HookKind {
    /// 配置节名（hooks.json 顶层键）
    pub fn config_key(self) -> &'static str {
        match self {
            HookKind::PreTool => "pre_tool",
            HookKind::PostTool => "post_tool",
            HookKind::TurnEnd => "turn_end",
        }
    }
}

/// 单条 hook 规则（hooks.json 解析形态）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRule {
    /// 工具名匹配：精确名或 `prefix*` 通配（turn_end 挂点忽略此字段）
    #[serde(default)]
    pub tool: String,
    /// 用户命令行（平台 shell 执行）
    pub command: String,
    /// 超时（秒；缺省 10，钳 1-120）
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl HookRule {
    /// 配置校验（构造期一次，fail-visible 列明全部问题）
    fn validate(&self, kind: HookKind) -> Result<(), String> {
        let mut problems = Vec::new();
        if self.command.is_empty() {
            problems.push("command is required".to_string());
        }
        if self.command.chars().count() > HOOK_MAX_COMMAND_CHARS {
            problems.push(format!("command exceeds {HOOK_MAX_COMMAND_CHARS} chars"));
        }
        if kind != HookKind::TurnEnd {
            if self.tool.is_empty() {
                problems.push("tool match pattern is required".to_string());
            }
            if self.tool.chars().count() > HOOK_MAX_TOOL_PATTERN_CHARS {
                problems.push(format!(
                    "tool pattern exceeds {HOOK_MAX_TOOL_PATTERN_CHARS} chars"
                ));
            }
        }
        if let Some(t) = self.timeout_secs {
            if !(HOOK_MIN_TIMEOUT_SECS..=HOOK_MAX_TIMEOUT_SECS).contains(&t) {
                problems.push(format!(
                    "timeout_secs must be {HOOK_MIN_TIMEOUT_SECS}-{HOOK_MAX_TIMEOUT_SECS}"
                ));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// 工具名匹配（精确或 `prefix*` 通配；与 serve_tools 允许面同法）
    fn matches_tool(&self, tool_name: &str) -> bool {
        match self.tool.strip_suffix('*') {
            Some(prefix) => tool_name.starts_with(prefix),
            None => self.tool == tool_name,
        }
    }

    /// 生效超时
    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.timeout_secs
                .unwrap_or(HOOK_DEFAULT_TIMEOUT_SECS)
                .clamp(HOOK_MIN_TIMEOUT_SECS, HOOK_MAX_TIMEOUT_SECS),
        )
    }
}

/// hooks 配置（三类挂点规则表）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserHooks {
    /// 工具执行前（可 deny/改写）
    #[serde(default)]
    pub pre_tool: Vec<HookRule>,
    /// 工具执行后（只读）
    #[serde(default)]
    pub post_tool: Vec<HookRule>,
    /// 回合结束（只读）
    #[serde(default)]
    pub turn_end: Vec<HookRule>,
}

impl UserHooks {
    /// 解析 + 校验（文件缺失=None；schema 非法=Err 由调用方 fail-visible）
    pub fn load(path: &std::path::Path) -> Result<Option<UserHooks>, String> {
        let s = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("hooks.json unreadable: {e}")),
        };
        if s.trim().is_empty() {
            return Ok(None);
        }
        let hooks: UserHooks =
            serde_json::from_str(&s).map_err(|e| format!("hooks.json invalid: {e}"))?;
        for (kind, rules) in [
            (HookKind::PreTool, &hooks.pre_tool),
            (HookKind::PostTool, &hooks.post_tool),
            (HookKind::TurnEnd, &hooks.turn_end),
        ] {
            if rules.len() > HOOK_MAX_RULES {
                return Err(format!(
                    "{} exceeds {HOOK_MAX_RULES} rules",
                    kind.config_key()
                ));
            }
            for (i, r) in rules.iter().enumerate() {
                r.validate(kind)
                    .map_err(|e| format!("{}[{i}]: {e}", kind.config_key()))?;
            }
        }
        Ok(Some(hooks))
    }

    /// 规则数（三类合计；装配期日志用）
    pub fn total_rules(&self) -> usize {
        self.pre_tool.len() + self.post_tool.len() + self.turn_end.len()
    }
}

/// 接线形态：hooks 规则表 + 执行工作目录（会话 workdir；hooks 在边界内执行）。
/// 管道依赖经 Arc 共享（每回合构造 fresh runner，构造期一次性装载）。
#[derive(Debug, Clone)]
pub struct WiredHooks {
    /// 解析后的规则表
    pub hooks: UserHooks,
    /// 执行工作目录（会话 workdir）
    pub workdir: std::path::PathBuf,
}

impl WiredHooks {
    /// 从 workdir 装载 `.evo-agent/hooks.json`（缺失=None；解析失败=Err
    /// 由调用方 fail-visible 决定降级或拒绝）
    pub fn load_from_workdir(workdir: &std::path::Path) -> Result<Option<WiredHooks>, String> {
        let path = workdir.join(".evo-agent").join("hooks.json");
        match UserHooks::load(&path)? {
            Some(hooks) => Ok(Some(WiredHooks {
                hooks,
                workdir: workdir.to_path_buf(),
            })),
            None => Ok(None),
        }
    }
}

/// pre_tool 裁决（管道消费）
#[derive(Debug, Clone, PartialEq)]
pub enum PreToolVerdict {
    /// 放行（无 deny/rewrite；args 保持原样）
    Allow,
    /// 拒绝执行（reason 回喂 LLM）
    Deny(String),
    /// 参数改写（替换本次调用的 args）
    Rewrite(Value),
}

/// hook 执行结局（留痕用）
#[derive(Debug, Clone)]
pub struct HookOutcome {
    /// 规则序号（0 起，配置顺序）
    pub rule_index: usize,
    /// exited_ok = 退出码 0 且未超时
    pub exited_ok: bool,
    /// 超时/生成失败/读失败等运行性故障（与退出码非 0 区分）
    pub error: Option<String>,
    /// stdout（截到 HOOK_MAX_STDOUT_BYTES；pre_tool 裁决解析源）
    pub stdout: String,
}

impl UserHooks {
    /// 执行一类挂点的全部命中规则（顺序执行；上下文 JSON 随 stdin 下发）
    ///
    /// 返回各规则结局（含失败与超时——留痕由调用方做，本函数不吞不炸）。
    pub async fn run_kind(
        &self,
        kind: HookKind,
        tool_name: &str,
        ctx: &Value,
        workdir: &std::path::Path,
    ) -> Vec<HookOutcome> {
        let rules: &[HookRule] = match kind {
            HookKind::PreTool => &self.pre_tool,
            HookKind::PostTool => &self.post_tool,
            HookKind::TurnEnd => &self.turn_end,
        };
        let mut outcomes = Vec::new();
        for (i, rule) in rules.iter().enumerate() {
            if kind != HookKind::TurnEnd && !rule.matches_tool(tool_name) {
                continue;
            }
            outcomes.push(run_hook_rule(i, rule, kind, ctx, workdir).await);
        }
        outcomes
    }

    /// pre_tool 裁决：依序执行命中规则，首个显式 Deny/Rewrite 生效即返回；
    /// 全部放行 = [`PreToolVerdict::Allow`]。运行性故障（超时/生成失败）按
    /// fail-open 放行并随 [`HookOutcome`] 留痕。
    pub async fn pre_tool_verdict(
        &self,
        tool_name: &str,
        args: &Value,
        session_id: &str,
        workdir: &std::path::Path,
    ) -> (PreToolVerdict, Vec<HookOutcome>) {
        let ctx = json!({
            "hook": "pre_tool",
            "tool": tool_name,
            "args": args,
            "session_id": session_id,
        });
        // 逐条执行逐条解析:deny 即停(后续规则命令不执行)——ubuntu CI
        // 实证缺陷纠正:此前 run_kind 全量先跑、解析循环事后找 deny,
        // 「deny 即停」文档语义从未真正落地
        let mut outcomes: Vec<HookOutcome> = Vec::new();
        let mut rewritten: Option<Value> = None;
        for (i, rule) in self.pre_tool.iter().enumerate() {
            if !rule.matches_tool(tool_name) {
                continue;
            }
            let out = run_hook_rule(i, rule, HookKind::PreTool, &ctx, workdir).await;
            match parse_pre_decision(&out.stdout) {
                Some(PreToolVerdict::Deny(reason)) => {
                    outcomes.push(out);
                    return (PreToolVerdict::Deny(reason), outcomes);
                }
                Some(PreToolVerdict::Rewrite(args)) => {
                    rewritten = Some(args);
                }
                Some(PreToolVerdict::Allow) | None => {}
            }
            outcomes.push(out);
        }
        match rewritten {
            Some(args) => (PreToolVerdict::Rewrite(args), outcomes),
            None => (PreToolVerdict::Allow, outcomes),
        }
    }

    /// post_tool 只读通知（结局随 stdin 下发；失败只留痕不阻断）
    pub async fn post_tool_notify(
        &self,
        tool_name: &str,
        args: &Value,
        result: &str,
        session_id: &str,
        workdir: &std::path::Path,
    ) -> Vec<HookOutcome> {
        let ctx = json!({
            "hook": "post_tool",
            "tool": tool_name,
            "args": args,
            "result": result,
            "session_id": session_id,
        });
        self.run_kind(HookKind::PostTool, tool_name, &ctx, workdir)
            .await
    }

    /// turn_end 只读通知（工具无关；失败只留痕不阻断）
    pub async fn turn_end_notify(
        &self,
        session_id: &str,
        workdir: &std::path::Path,
    ) -> Vec<HookOutcome> {
        let ctx = json!({
            "hook": "turn_end",
            "session_id": session_id,
        });
        self.run_kind(HookKind::TurnEnd, "", &ctx, workdir).await
    }
}

/// 解析 pre_tool stdout 裁决（非法/空 = None；宽容 JSON 解析）
fn parse_pre_decision(stdout: &str) -> Option<PreToolVerdict> {
    let s = stdout.trim();
    if s.is_empty() {
        return None;
    }
    let v: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        // Windows 回退:cmd echo 会把 `"` 转成 `\"`(参数传递层转义,真机
        // 实证)——严格解析失败时反转义重试,免得 hook 作者必须绕引号
        Err(_) => serde_json::from_str(&s.replace("\\\"", "\"")).ok()?,
    };
    match v.get("decision").and_then(|d| d.as_str())? {
        "deny" => Some(PreToolVerdict::Deny(
            v.get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("blocked by user hook")
                .to_string(),
        )),
        "rewrite" => {
            let args = v.get("args").filter(|a| a.is_object()).cloned()?;
            Some(PreToolVerdict::Rewrite(args))
        }
        "allow" => Some(PreToolVerdict::Allow),
        _ => None,
    }
}

/// 平台 shell 命令形态（用户自持配置，信任级与 shell_exec 同源）
fn shell_command(command: &str) -> (std::process::Command, String) {
    #[cfg(target_os = "windows")]
    {
        let mut c = std::process::Command::new("cmd");
        c.arg("/S").arg("/C").arg(command);
        (c, "cmd /S /C".to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(command);
        (c, "sh -c".to_string())
    }
}

/// 单规则执行（超时杀子进程；一切运行性故障记入 outcome，不上抛）
async fn run_hook_rule(
    rule_index: usize,
    rule: &HookRule,
    kind: HookKind,
    ctx: &Value,
    workdir: &std::path::Path,
) -> HookOutcome {
    let _ = kind; // 预留:turn_end 无工具名匹配,形态参数留位(当前调用方已滤)
    let (mut cmd, _label) = shell_command(&rule.command);
    cmd.current_dir(workdir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // 继承面收敛：最小环境（用户命令如需 PATH 等可显式引入；缺省继承系统
    // 基线——hooks 是用户自持配置，环境继承与 shell_exec 同级信任）
    let mut child = match tokio::process::Command::from(cmd).spawn() {
        Ok(c) => c,
        Err(e) => {
            return HookOutcome {
                rule_index,
                exited_ok: false,
                error: Some(format!("spawn failed: {e}")),
                stdout: String::new(),
            };
        }
    };
    {
        let mut stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                return HookOutcome {
                    rule_index,
                    exited_ok: false,
                    error: Some("stdin unavailable".to_string()),
                    stdout: String::new(),
                };
            }
        };
        let payload = serde_json::to_vec(ctx).unwrap_or_default();
        if let Err(e) = stdin.write_all(&payload).await {
            // 管道关闭（进程早退）不算致命——继续等退出码
            let _ = e;
        }
        let _ = stdin.shutdown().await;
    }
    let timed = tokio::time::timeout(rule.timeout(), child.wait_with_output()).await;
    match timed {
        Err(_) => HookOutcome {
            rule_index,
            exited_ok: false,
            error: Some(format!(
                "timeout after {}s (killed)",
                rule.timeout().as_secs()
            )),
            stdout: String::new(),
        },
        Ok(Err(e)) => HookOutcome {
            rule_index,
            exited_ok: false,
            error: Some(format!("wait failed: {e}")),
            stdout: String::new(),
        },
        Ok(Ok(output)) => {
            let mut stdout = String::from_utf8_lossy(&output.stdout)
                .chars()
                .take(HOOK_MAX_STDOUT_BYTES)
                .collect::<String>();
            if output.stdout.len() > HOOK_MAX_STDOUT_BYTES {
                stdout.push_str("\n…[hook stdout truncated]");
            }
            HookOutcome {
                rule_index,
                exited_ok: output.status.success(),
                error: None,
                stdout,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_hooks(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hooks.json");
        std::fs::write(&p, body).unwrap();
        (dir, p)
    }

    #[test]
    fn missing_and_empty_files_are_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(UserHooks::load(&dir.path().join("absent.json"))
            .unwrap()
            .is_none());
        let (d, p) = tmp_hooks("   ");
        let _ = d;
        assert!(UserHooks::load(&p).unwrap().is_none());
    }

    #[test]
    fn invalid_schema_fails_visible() {
        let (d, p) = tmp_hooks(r#"{"pre_tool": [{"tool": "shell_exec"}]}"#);
        let _ = d;
        // serde 先拦缺失字段;无论走 serde 还是 validate,都必须 fail-visible
        let err = UserHooks::load(&p).unwrap_err();
        assert!(
            err.contains("hooks.json invalid") || err.contains("command is required"),
            "{err}"
        );
    }

    #[test]
    fn tool_pattern_matching_exact_and_wildcard() {
        let r = HookRule {
            tool: "shell_exec".into(),
            command: "echo hi".into(),
            timeout_secs: None,
        };
        assert!(r.matches_tool("shell_exec"));
        assert!(!r.matches_tool("shell_execx"));
        let w = HookRule {
            tool: "file_*".into(),
            command: "echo hi".into(),
            timeout_secs: None,
        };
        assert!(w.matches_tool("file_write"));
        assert!(!w.matches_tool("git_status"));
    }

    #[test]
    fn parse_pre_decision_forms() {
        assert_eq!(
            parse_pre_decision(r#"{"decision":"deny","reason":"no rm"}"#),
            Some(PreToolVerdict::Deny("no rm".into()))
        );
        assert_eq!(
            parse_pre_decision(r#"{"decision":"deny"}"#),
            Some(PreToolVerdict::Deny("blocked by user hook".into()))
        );
        assert_eq!(
            parse_pre_decision(r#"{"decision":"rewrite","args":{"path":"b.txt"}}"#),
            Some(PreToolVerdict::Rewrite(json!({"path": "b.txt"})))
        );
        // rewrite 非 object args = 不采纳
        assert_eq!(
            parse_pre_decision(r#"{"decision":"rewrite","args":"x"}"#),
            None
        );
        // 放行/噪声/垃圾
        assert_eq!(
            parse_pre_decision(r#"{"decision":"allow"}"#),
            Some(PreToolVerdict::Allow)
        );
        assert_eq!(parse_pre_decision("plain text"), None);
        assert_eq!(parse_pre_decision("   "), None);
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pre_tool_deny_stops_execution() {
        // 决策 JSON 经文件零引用下发(cat 直读;shell 引号在各平台形态不同,
        // 不进测试依赖——Windows 侧由 cmd 转义回退路径覆盖)
        let dir = tempfile::tempdir().unwrap();
        let decision_file = dir.path().join("decision.json");
        std::fs::write(&decision_file, r#"{"decision":"deny","reason":"no rm"}"#).unwrap();
        let hooks = UserHooks {
            pre_tool: vec![
                HookRule {
                    tool: "shell_exec".into(),
                    command: format!("cat {}", decision_file.display()),
                    timeout_secs: Some(15),
                },
                HookRule {
                    tool: "shell_exec".into(),
                    command: "exit 3".into(),
                    timeout_secs: None,
                },
            ],
            ..Default::default()
        };
        let (v, outs) = hooks
            .pre_tool_verdict(
                "shell_exec",
                &json!({"command": "rm -rf /"}),
                "s1",
                dir.path(),
            )
            .await;
        assert_eq!(
            v,
            PreToolVerdict::Deny("no rm".into()),
            "outcomes: {outs:?}"
        );
        assert_eq!(
            outs.len(),
            1,
            "deny must stop later rules; outcomes: {outs:?}"
        );
    }

    #[tokio::test]
    async fn pre_tool_timeout_fails_open() {
        let hooks = UserHooks {
            pre_tool: vec![HookRule {
                tool: "*".into(),
                command: if cfg!(target_os = "windows") {
                    // timeout.exe 拒绝重定向 stdin;ping 是 Windows 惯用睡眠
                    "ping -n 30 127.0.0.1".into()
                } else {
                    "sleep 30".into()
                },
                timeout_secs: Some(1),
            }],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let (v, outs) = hooks
            .pre_tool_verdict("file_write", &json!({}), "s1", dir.path())
            .await;
        assert_eq!(v, PreToolVerdict::Allow, "timeout must fail open");
        assert!(outs[0].error.as_deref().unwrap_or("").contains("timeout"));
    }

    #[tokio::test]
    async fn post_tool_is_fire_and_forget() {
        let hooks = UserHooks {
            post_tool: vec![HookRule {
                tool: "file_read".into(),
                command: "exit 5".into(), // 非零退出也只留痕
                timeout_secs: None,
            }],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let outs = hooks
            .post_tool_notify("file_read", &json!({}), "ok", "s1", dir.path())
            .await;
        assert_eq!(outs.len(), 1);
        assert!(!outs[0].exited_ok);
    }

    #[tokio::test]
    async fn stdin_receives_context_json() {
        // hook 把 stdin 原样回显到 stdout;校验上下文形状
        let hooks = UserHooks {
            post_tool: vec![HookRule {
                tool: "*".into(),
                command: if cfg!(target_os = "windows") {
                    r#"findstr ".*"more"#.into()
                } else {
                    "cat".into()
                },
                timeout_secs: None,
            }],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let outs = hooks
            .post_tool_notify(
                "file_read",
                &json!({"path": "a.txt"}),
                "RES",
                "sid-9",
                dir.path(),
            )
            .await;
        if cfg!(not(target_os = "windows")) {
            let ctx: Value = serde_json::from_str(&outs[0].stdout).unwrap();
            assert_eq!(ctx["hook"], "post_tool");
            assert_eq!(ctx["tool"], "file_read");
            assert_eq!(ctx["args"]["path"], "a.txt");
            assert_eq!(ctx["result"], "RES");
            assert_eq!(ctx["session_id"], "sid-9");
        } else {
            // Windows findstr 回显形态不保证逐字节,只要求不炸
            assert!(!outs.is_empty());
        }
    }

    #[test]
    fn rule_count_cap_enforced() {
        let rules: Vec<HookRule> = (0..=HOOK_MAX_RULES)
            .map(|_| HookRule {
                tool: "*".into(),
                command: "echo x".into(),
                timeout_secs: None,
            })
            .collect();
        let body = serde_json::to_string(&json!({ "turn_end": rules })).unwrap();
        let (d, p) = tmp_hooks(&body);
        let _ = d;
        let err = UserHooks::load(&p).unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
    }
}
