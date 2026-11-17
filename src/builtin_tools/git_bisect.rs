// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! `git_bisect` —— 回归定位二分自动化(stash/bisect 工具族批二,立项与设计
//! 档见 knowledge 设计面)。
//!
//! 把「二分走步」从多轮 shell_exec 审批往返收敛为单工具结构化调用:agent
//! 以 start(bad+good 双锚)开局、按当前步提交的判定逐调用 bad/good/skip,
//! 首个坏提交定位后(git 宣告 culprit,会话保持开放)reset 恢复原检出并
//! 闭合——回归定位的机制化承载(v1 交互步进;`bisect run` 自动化候二期,
//! 后台作业通道承载)。
//!
//! ## 安全栏
//!
//! - **argv 直调无 shell**:全部 git 调用为参数数组,无解释器面;
//! - **ref 白名单**:bad/good/start 的 ref 仅接受安全子集字符集
//!   (A-Za-z0-9._/- 与 `~`;拒前导 `-`、`..`、空格、`.lock` 尾);
//! - **start 前置 clean-tree 必检**:脏树即拒(二分走步要 checkout 中间
//!   提交,脏树会被 git 拒或卷入未提交改动,fail-visible 给指引);
//! - **status 状态可见性必达**:会话态(在不在二分/当前 HEAD/bad-good 锚/
//!   已走步数)单调用可查——并行写操作风险的唯一缓解面;
//! - **reset 必达语义**:任何在二分态可一键恢复原检出(测试锁覆盖中途
//!   reset 回主分支);不在二分态=如实报 not_in_bisect(no-op)。
//!
//! 风险注记:二分期间仓处于 detached HEAD+BISECT_* 状态文件在位,与写面
//! 工具(file_write/git_commit/git_stash)并发互斥无机制强制——缓解=status
//! 可见性+红线文档(二分会话态禁止并行写操作)+试点建议沙箱仓先行。

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::io_handler::IoResult;

/// 工具名(LLM 契约面/manifest/注册三处同键)
pub const GIT_BISECT_TOOL: &str = "git_bisect";

/// 工作区根(wired 注入;全部 git 调用的 cwd)
#[derive(Clone)]
pub struct GitBisectTool {
    workdir: PathBuf,
}

impl GitBisectTool {
    /// 生产构造(workdir=工作区根,与 shell_exec/git_tools 同基线)
    pub fn wired(workdir: &Path) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
        }
    }

    /// 测试构造
    #[cfg(test)]
    pub(crate) fn with_workdir(workdir: &Path) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
        }
    }

    /// 同步 git 调用(argv 直调;cwd=工作区根)
    fn git(&self, args: &[&str]) -> Result<std::process::Output, String> {
        Command::new("git")
            .args(args)
            .current_dir(&self.workdir)
            .output()
            .map_err(|e| format!("git spawn failed: {e} (is git installed?)"))
    }

    /// git 调用+成功断言(失败=stderr/stdout 尾段 fail-visible)
    fn git_ok(&self, args: &[&str], label: &str) -> Result<String, String> {
        let out = self.git(args)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let mut msg = String::from_utf8_lossy(&out.stderr).into_owned();
            if msg.trim().is_empty() {
                msg = String::from_utf8_lossy(&out.stdout).into_owned();
            }
            Err(format!("git {label} failed: {}", msg.trim()))
        }
    }

    /// ref 白名单(bad/good/start 的锚;hash/branch/tag 安全子集,含 `~` 供
    /// HEAD~N 形态;拒前导 `-`、`..`、空格、`.lock` 尾)
    fn valid_ref(name: &str) -> bool {
        !name.is_empty()
            && !name.starts_with('-')
            && !name.ends_with('/')
            && !name.ends_with(".lock")
            && !name.contains("..")
            && !name.contains(' ')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-' | '~'))
    }

    /// 是否在二分会话(`BISECT_LOG` 存活性,经 git-dir 真实路径——实证:
    /// `git bisect log` 在首个坏提交定位后仍成功打印上轮账,退出码判据
    /// 无效;文件判据与会话态严格同步)
    fn in_bisect(&self) -> bool {
        self.git(&["rev-parse", "--git-dir"])
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                let gitdir = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let p = PathBuf::from(&gitdir);
                let p = if p.is_absolute() {
                    p
                } else {
                    self.workdir.join(p)
                };
                p.join("BISECT_LOG").exists()
            })
            .unwrap_or(false)
    }

    /// 当前 HEAD 短 hash(不可得=None)
    fn head(&self) -> Option<String> {
        self.git(&["rev-parse", "HEAD"])
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    /// 会话步数(log 内 good/bad/skip 行计数)
    fn step_count(&self) -> usize {
        match self.git(&["bisect", "log"]) {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                text.lines()
                    .filter(|l| {
                        let t = l.trim_start();
                        t.starts_with("# good:")
                            || t.starts_with("# bad:")
                            || t.starts_with("# skip:")
                    })
                    .count()
            }
            _ => 0,
        }
    }

    /// 当前所在分支名(detached=空串)
    fn current_branch(&self) -> String {
        self.git(&["branch", "--show-current"])
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl crate::io_handlers::tool_handler::ToolFunction for GitBisectTool {
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("git_bisect tool panicked: {e}"))?
    }
}

impl GitBisectTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &Value) -> IoResult {
        let action = args.get("action").and_then(|v| v.as_str()).ok_or_else(|| {
            "missing required arg: action (start | bad | good | skip | status | log | reset)"
                .to_string()
        })?;
        match action {
            "start" => self.action_start(args),
            "bad" => self.action_judge(args, "bad"),
            "good" => self.action_judge(args, "good"),
            "skip" => self.action_skip(),
            "status" => self.action_status(),
            "log" => self.action_log(),
            "reset" => self.action_reset(),
            other => Err(format!(
                "unknown action \"{other}\" (valid actions: start, bad, good, skip, \
                 status, log, reset)"
            )),
        }
    }

    fn require_ref<'a>(&self, args: &'a Value, key: &str) -> Result<&'a str, String> {
        let r = args
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("missing required arg: {key} (a commit-ish ref)"))?;
        if !Self::valid_ref(r) {
            return Err(format!(
                "invalid ref \"{r}\" (allowed: A-Za-z0-9._/~ and '-'; no leading '-', \
                 no '..', no spaces, no .lock suffix)"
            ));
        }
        Ok(r)
    }

    fn action_start(&self, args: &Value) -> IoResult {
        // 前置 clean-tree 必检(二分走步要 checkout 中间提交)
        match self.git(&["status", "--porcelain"]) {
            Ok(o) if !o.stdout.is_empty() => {
                return Err(
                    "working tree is dirty: commit or stash changes first (bisect \
                     walks checkout intermediate commits and requires a clean tree)"
                        .to_string(),
                );
            }
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        if self.in_bisect() {
            return Err(
                "already in a bisect session: finish it first (reset to abandon)".to_string(),
            );
        }
        let bad = self.require_ref(args, "bad")?;
        let good = self.require_ref(args, "good")?;
        self.git_ok(&["bisect", "start", bad, good], "bisect start")?;
        self.action_status_payload("start")
    }

    /// bad/good 共用体(ref 可选=判定当前 HEAD)。首个坏提交定位语义(实测):
    /// git 宣告 culprit(stdout 含 "is the first")后**会话保持开放**
    /// (BISECT_LOG 在位,原检出未恢复)——回传 first_bad_found+指引 reset,
    /// 闭合归 reset 必达链;不谎报会话已闭合。
    fn action_judge(&self, args: &Value, term: &str) -> IoResult {
        if !self.in_bisect() {
            return Err(
                "not in a bisect session: start one first (start requires bad+good refs)"
                    .to_string(),
            );
        }
        let git_args: Vec<&str> = match args.get("ref").and_then(|v| v.as_str()) {
            Some(r) if !r.trim().is_empty() => {
                let rt = r.trim();
                if !Self::valid_ref(rt) {
                    return Err(format!(
                        "invalid ref \"{rt}\" (allowed: A-Za-z0-9._/~ and '-'; no \
                         leading '-', no '..', no spaces, no .lock suffix)"
                    ));
                }
                vec!["bisect", term, rt]
            }
            _ => vec!["bisect", term],
        };
        let out = self.git(&git_args)?;
        if !out.status.success() {
            let mut msg = String::from_utf8_lossy(&out.stderr).into_owned();
            if msg.trim().is_empty() {
                msg = String::from_utf8_lossy(&out.stdout).into_owned();
            }
            return Err(format!("git bisect {term} failed: {}", msg.trim()));
        }
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if stdout.contains("is the first") {
            return Ok(serde_json::json!({
                "action": term,
                "status": "first_bad_found",
                "culprit": stdout.trim(),
                "branch": self.current_branch(),
                "head": self.head(),
                "note": "first bad commit identified; the bisect session is still \
                         active - call reset to restore the original checkout and \
                         close it",
            }));
        }
        self.action_status_payload(term)
    }

    fn action_skip(&self) -> IoResult {
        if !self.in_bisect() {
            return Err("not in a bisect session: start one first".to_string());
        }
        let out = self.git(&["bisect", "skip"])?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() && stdout.contains("is the first") {
            return Ok(serde_json::json!({
                "action": "skip",
                "status": "first_bad_found",
                "culprit": stdout.trim(),
                "note": "first bad commit identified; call reset to restore and close",
            }));
        }
        if !out.status.success() {
            let mut msg = String::from_utf8_lossy(&out.stderr).into_owned();
            if msg.trim().is_empty() {
                msg = stdout;
            }
            return Err(format!("git bisect skip failed: {}", msg.trim()));
        }
        self.action_status_payload("skip")
    }

    fn action_status(&self) -> IoResult {
        self.action_status_payload("status")
    }

    /// 状态载荷(单调用可见全部会话态=并行风险缓解面)
    fn action_status_payload(&self, action: &str) -> IoResult {
        let in_bisect = self.in_bisect();
        let mut payload = serde_json::json!({
            "action": action,
            "in_bisect": in_bisect,
            "branch": self.current_branch(),
            "head": self.head(),
            "steps_taken": self.step_count(),
        });
        if in_bisect {
            // bad/good 锚从会话 log 首行呈现(start 行携带双锚)
            let anchors = self
                .git(&["bisect", "log"])
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .map(|t| {
                    t.lines()
                        .find(|l| l.trim_start().starts_with("git bisect start"))
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                })
                .unwrap_or_default();
            payload["start_line"] = Value::from(anchors);
            payload["note"] = Value::from(
                "bisect session active: judge the current HEAD with bad/good, skip \
                 if untestable, reset to abandon; avoid parallel write operations",
            );
        }
        Ok(payload)
    }

    fn action_log(&self) -> IoResult {
        if !self.in_bisect() {
            return Err("not in a bisect session: log unavailable".to_string());
        }
        let out = self.git_ok(&["bisect", "log"], "bisect log")?;
        Ok(serde_json::json!({
            "action": "log",
            "log": out.trim(),
        }))
    }

    fn action_reset(&self) -> IoResult {
        // reset 必达语义:在二分态=恢复原检出;不在=如实 no-op(不报错)
        if !self.in_bisect() {
            return Ok(serde_json::json!({
                "action": "reset",
                "status": "not_in_bisect",
                "branch": self.current_branch(),
                "head": self.head(),
            }));
        }
        self.git_ok(&["bisect", "reset"], "bisect reset")?;
        Ok(serde_json::json!({
            "action": "reset",
            "status": "restored",
            "branch": self.current_branch(),
            "head": self.head(),
            "in_bisect": self.in_bisect(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handlers::tool_handler::ToolFunction;

    /// 真 git 临时仓:main 上 5 个线性提交,c3 引入 marker("bug-here");
    /// 返回 (dir, tool, c1_sha)
    fn repo_linear_history() -> (tempfile::TempDir, GitBisectTool, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git spawn");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        std::fs::write(root.join("f.txt"), b"v1\n").unwrap();
        run(&["add", "f.txt"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-m",
            "c1",
        ]);
        let c1 = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&root)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "--allow-empty",
            "-m",
            "c2",
        ]);
        std::fs::write(root.join("marker.txt"), b"bug-here\n").unwrap();
        run(&["add", "marker.txt"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-m",
            "c3 introduce marker",
        ]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "--allow-empty",
            "-m",
            "c4",
        ]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "--allow-empty",
            "-m",
            "c5",
        ]);
        let tool = GitBisectTool::with_workdir(&root);
        (dir, tool, c1)
    }

    fn bs_args(action: &str, extra: Value) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("action".to_string(), Value::from(action));
        if let Value::Object(o) = extra {
            for (k, v) in o {
                m.insert(k, v);
            }
        }
        Value::Object(m)
    }

    #[tokio::test]
    async fn full_walk_auto_finish_and_restore() {
        let (d, tool, c1) = repo_linear_history();
        let root = d.path().to_path_buf();
        let r = tool
            .call(&bs_args(
                "start",
                serde_json::json!({"bad": "main", "good": c1}),
            ))
            .await
            .unwrap();
        assert_eq!(r.get("in_bisect").and_then(|v| v.as_bool()), Some(true));
        // 走步:当前步含 marker=bad,否则 good;首个坏提交定位后=first_bad_found
        // (git 宣告 culprit,会话保持开放——reset 闭合归调用方,不谎报已闭合)
        let mut steps = 0;
        let mut identified = false;
        while steps < 10 {
            let marker = root.join("marker.txt");
            let is_bad = marker.exists()
                && std::fs::read_to_string(&marker)
                    .unwrap()
                    .contains("bug-here");
            let term = if is_bad { "bad" } else { "good" };
            let r = tool.call(&bs_args(term, Value::Null)).await.unwrap();
            if r.get("status").and_then(|v| v.as_str()) == Some("first_bad_found") {
                identified = true;
                assert!(
                    r.get("culprit")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| s.contains("is the first")),
                    "culprit 宣告在回传: {r:?}"
                );
                break;
            }
            steps += 1;
        }
        assert!(identified, "走步应定位首个坏提交(≤10 步)");
        // 会话仍开放(实测语义)——status 如实在报
        let status = tool.call(&bs_args("status", Value::Null)).await.unwrap();
        assert_eq!(
            status.get("in_bisect").and_then(|v| v.as_bool()),
            Some(true)
        );
        // reset 闭合:恢复原检出回 main 分支
        let r = tool.call(&bs_args("reset", Value::Null)).await.unwrap();
        assert_eq!(r.get("status").and_then(|v| v.as_str()), Some("restored"));
        let status = tool.call(&bs_args("status", Value::Null)).await.unwrap();
        assert_eq!(
            status.get("in_bisect").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(status.get("branch").and_then(|v| v.as_str()), Some("main"));
    }

    #[tokio::test]
    async fn reset_restores_main_mid_walk() {
        // reset 必达锁:二分中途 reset 回主分支(工具族设计档验收锚)
        let (d, tool, c1) = repo_linear_history();
        let root = d.path().to_path_buf();
        tool.call(&bs_args(
            "start",
            serde_json::json!({"bad": "main", "good": c1}),
        ))
        .await
        .unwrap();
        assert_eq!(tool.current_branch(), "", "二分态=detached HEAD");
        let r = tool.call(&bs_args("reset", Value::Null)).await.unwrap();
        assert_eq!(r.get("status").and_then(|v| v.as_str()), Some("restored"));
        assert_eq!(
            r.get("branch").and_then(|v| v.as_str()),
            Some("main"),
            "reset 必达=恢复主分支"
        );
        let status = tool.call(&bs_args("status", Value::Null)).await.unwrap();
        assert_eq!(
            status.get("in_bisect").and_then(|v| v.as_bool()),
            Some(false)
        );
        // 复核工作树:marker 已随 checkout 消失(回 c5 前基线……c5 含 marker)
        // ——c5 在 main 上,marker 在位即恢复正确
        assert!(root.join("marker.txt").exists(), "回 main 后 marker 在位");
    }

    #[tokio::test]
    async fn start_refuses_dirty_tree_and_double_start() {
        let (d, tool, c1) = repo_linear_history();
        let root = d.path().to_path_buf();
        std::fs::write(root.join("dirty.txt"), b"x").unwrap();
        let err = tool
            .call(&bs_args(
                "start",
                serde_json::json!({"bad": "main", "good": c1}),
            ))
            .await
            .unwrap_err();
        assert!(err.contains("working tree is dirty"), "清树前置必检: {err}");
        std::fs::remove_file(root.join("dirty.txt")).unwrap();
        tool.call(&bs_args(
            "start",
            serde_json::json!({"bad": "main", "good": c1}),
        ))
        .await
        .unwrap();
        let err = tool
            .call(&bs_args(
                "start",
                serde_json::json!({"bad": "main", "good": c1}),
            ))
            .await
            .unwrap_err();
        assert!(
            err.contains("already in a bisect session"),
            "双 start 拒: {err}"
        );
        tool.call(&bs_args("reset", Value::Null)).await.unwrap();
    }

    #[tokio::test]
    async fn judge_outside_session_and_bad_refs_rejected() {
        let (_d, tool, _c1) = repo_linear_history();
        let err = tool.call(&bs_args("bad", Value::Null)).await.unwrap_err();
        assert!(
            err.contains("not in a bisect session"),
            "会话外判定拒: {err}"
        );
        let err = tool
            .call(&bs_args(
                "start",
                serde_json::json!({"bad": "-evil", "good": "main"}),
            ))
            .await
            .unwrap_err();
        assert!(err.contains("invalid ref"), "ref 白名单拒: {err}");
        let err = tool
            .call(&bs_args("start", serde_json::json!({"bad": "main"})))
            .await
            .unwrap_err();
        assert!(
            err.contains("missing required arg: good"),
            "双锚必填: {err}"
        );
    }

    #[tokio::test]
    async fn reset_outside_session_is_noop_visible() {
        let (_d, tool, _c1) = repo_linear_history();
        let r = tool.call(&bs_args("reset", Value::Null)).await.unwrap();
        assert_eq!(
            r.get("status").and_then(|v| v.as_str()),
            Some("not_in_bisect"),
            "会话外 reset=如实 no-op 不报错"
        );
    }

    #[tokio::test]
    async fn log_outside_session_errors_and_unknown_action_rejected() {
        let (_d, tool, _c1) = repo_linear_history();
        let err = tool.call(&bs_args("log", Value::Null)).await.unwrap_err();
        assert!(err.contains("not in a bisect session"), "got: {err}");
        let err = tool
            .call(&bs_args("rebase", Value::Null))
            .await
            .unwrap_err();
        assert!(
            err.contains("valid actions: start, bad, good, skip, status, log, reset"),
            "got: {err}"
        );
    }
}
