// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! `git_stash` —— 工作树暂存自动化(stash/bisect 工具族批一,立项与设计
//! 档见 knowledge 设计面)。
//!
//! 把「暂存/查看/恢复现场」从多轮 shell_exec 审批往返收敛为单工具结构化
//! 调用:长程会话中途需要切换任务面时,agent 可一键收起当前工作树改动、
//! 一键恢复——上下文切换的机制化承载。
//!
//! ## 安全栏
//!
//! - **argv 直调无 shell**:全部 git 调用为参数数组,无解释器面;
//! - **ref 白名单**:stash ref 仅接受 `stash@{n}` 形态(n=十进制数字),
//!   拒自由字符串(防 argv 注入与越 ref 引用);
//! - **push 前置 dirty-tree 必检**:clean tree 即拒(无现场可言,fail-visible
//!   给指引,不等 git 报 "No local changes to save");
//! - **默认 tracked-only**:untracked 文件收起须显式 `include_untracked=true`
//!   (面最小化;误收大产物目录的反悔代价高);
//! - **pop/apply 冲突 fail-visible**:非零退出回传冲突 stderr 尾段与冲突
//!   文件清单,禁止静默半态;pop 失败时 stash 条目保留(git 语义)可审计。

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::io_handler::IoResult;

/// 工具名(LLM 契约面/manifest/注册三处同键)
pub const GIT_STASH_TOOL: &str = "git_stash";

/// 工作区根(wired 注入;全部 git 调用的 cwd)
#[derive(Clone)]
pub struct GitStashTool {
    workdir: PathBuf,
}

impl GitStashTool {
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

    /// stash ref 白名单:仅 `stash@{n}` 形态(n=十进制数字)
    fn valid_stash_ref(name: &str) -> bool {
        let inner = match name
            .strip_prefix("stash@{")
            .and_then(|s| s.strip_suffix('}'))
        {
            Some(i) => i,
            None => return false,
        };
        !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit())
    }

    /// 工作树是否有可收起改动（push 前置检查）——口径随 include_untracked：
    /// tracked-only 模式滤除 `??` 行（纯 untracked 场景对默认 push=无现场），
    /// untracked 模式含 `??` 行。不可读按干净处理（push 自会报）。
    fn stashable_changes_present(&self, include_untracked: bool) -> bool {
        match self.git(&["status", "--porcelain"]) {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                if include_untracked {
                    !text.trim().is_empty()
                } else {
                    text.lines()
                        .any(|l| !l.trim().is_empty() && !l.starts_with("??"))
                }
            }
            Err(_) => false,
        }
    }

    /// stash 条目数(list 行数)
    fn stash_count(&self) -> usize {
        match self.git(&["stash", "list"]) {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count(),
            Err(_) => 0,
        }
    }
}

#[async_trait::async_trait]
impl crate::io_handlers::tool_handler::ToolFunction for GitStashTool {
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("git_stash tool panicked: {e}"))?
    }
}

impl GitStashTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &Value) -> IoResult {
        let action = args.get("action").and_then(|v| v.as_str()).ok_or_else(|| {
            "missing required arg: action (push | list | show | pop | apply | drop)".to_string()
        })?;
        match action {
            "push" => self.action_push(args),
            "list" => self.action_list(),
            "show" => self.action_show(args),
            "pop" => self.action_restore(args, "pop"),
            "apply" => self.action_restore(args, "apply"),
            "drop" => self.action_drop(args),
            other => Err(format!(
                "unknown action \"{other}\" (valid actions: push, list, show, pop, apply, drop)"
            )),
        }
    }

    /// 可选 stash ref 参数(缺省=git 缺省最新条目;给了必须过白名单)
    fn optional_stash_ref<'a>(&self, args: &'a Value) -> Result<Option<&'a str>, String> {
        match args.get("ref").and_then(|v| v.as_str()) {
            None => Ok(None),
            Some(r) if r.trim().is_empty() => Ok(None),
            Some(r) => {
                let rt = r.trim();
                if !Self::valid_stash_ref(rt) {
                    return Err(format!(
                        "invalid stash ref \"{rt}\" (allowed form: stash@{{n}}, n = digits)"
                    ));
                }
                Ok(Some(rt))
            }
        }
    }

    fn action_push(&self, args: &Value) -> IoResult {
        let include_untracked = args
            .get("include_untracked")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // 前置必检(clean=无现场,fail-visible 给指引;口径随 include_untracked)
        if !self.stashable_changes_present(include_untracked) {
            return Err(
                "working tree is clean: nothing to stash (stash records uncommitted \
                 changes; make changes first or check the workspace root; untracked \
                 files need include_untracked=true)"
                    .to_string(),
            );
        }
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut git_args: Vec<&str> = vec!["stash", "push"];
        if include_untracked {
            git_args.push("-u");
        }
        if let Some(m) = message {
            git_args.push("-m");
            git_args.push(m);
        }
        let count_before = self.stash_count();
        self.git_ok(&git_args, "stash push")?;
        let count_after = self.stash_count();
        // 兜底守卫:git 零退出但未产生新条目(如仅 untracked 且未带 -u)——
        // 如实报失败,不得谎报 stashed
        if count_after <= count_before {
            return Err(format!(
                "git stash push saved nothing (no stashable changes for this mode; \
                 untracked files require include_untracked=true); stash_count={count_after}"
            ));
        }
        Ok(serde_json::json!({
            "action": "push",
            "status": "stashed",
            "message": message,
            "include_untracked": include_untracked,
            "stash_count": count_after,
        }))
    }

    fn action_list(&self) -> IoResult {
        let out = self.git_ok(&["stash", "list", "--format=%gd|%gs"], "stash list")?;
        let entries: Vec<Value> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| match l.split_once('|') {
                Some((ref_name, subject)) => serde_json::json!({
                    "ref": ref_name,
                    "subject": subject,
                }),
                None => serde_json::json!({ "raw": l }),
            })
            .collect();
        Ok(serde_json::json!({
            "action": "list",
            "count": entries.len(),
            "stashes": entries,
        }))
    }

    fn action_show(&self, args: &Value) -> IoResult {
        let stash_ref = self.optional_stash_ref(args)?;
        let mut git_args: Vec<&str> = vec!["stash", "show", "--stat"];
        if let Some(r) = stash_ref {
            git_args.push(r);
        }
        let out = self.git_ok(&git_args, "stash show")?;
        Ok(serde_json::json!({
            "action": "show",
            "ref": stash_ref.unwrap_or("stash@{0} (latest)"),
            "stat": out.trim(),
        }))
    }

    /// pop/apply 共用体(pop=恢复+删条目;apply=恢复+保留条目)
    fn action_restore(&self, args: &Value, kind: &str) -> IoResult {
        let stash_ref = self.optional_stash_ref(args)?;
        let count_before = self.stash_count();
        let mut git_args: Vec<&str> = vec!["stash", kind];
        if let Some(r) = stash_ref {
            git_args.push(r);
        }
        // 冲突 fail-visible:非零退出回传 stderr 尾段;pop 失败时条目保留
        // (git 语义)可审计,apply 失败时条目本就不动
        let out = match self.git(&git_args) {
            Ok(o) if o.status.success() => o,
            Ok(o) => {
                let mut msg = String::from_utf8_lossy(&o.stderr).into_owned();
                if msg.trim().is_empty() {
                    msg = String::from_utf8_lossy(&o.stdout).into_owned();
                }
                return Err(format!(
                    "git stash {kind} failed (working tree left in conflict state if \
                     files were listed below; resolve then commit/delete as appropriate): {}",
                    msg.trim()
                ));
            }
            Err(e) => return Err(e),
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let count_after = self.stash_count();
        Ok(serde_json::json!({
            "action": kind,
            "status": if kind == "pop" { "restored_and_dropped" } else { "restored_stash_kept" },
            "ref": stash_ref.unwrap_or("stash@{0} (latest)"),
            "stash_count_before": count_before,
            "stash_count_after": count_after,
            "detail": stdout.trim(),
        }))
    }

    fn action_drop(&self, args: &Value) -> IoResult {
        let stash_ref = self.optional_stash_ref(args)?;
        let mut git_args: Vec<&str> = vec!["stash", "drop"];
        if let Some(r) = stash_ref {
            git_args.push(r);
        }
        let out = self.git_ok(&git_args, "stash drop")?;
        Ok(serde_json::json!({
            "action": "drop",
            "status": "dropped",
            "ref": stash_ref.unwrap_or("stash@{0} (latest)"),
            "stash_count": self.stash_count(),
            "detail": out.trim(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handlers::tool_handler::ToolFunction;

    /// 真 git 临时仓(≥1 commit+一条 tracked 文件;内联身份防全局配置缺位)
    fn repo_with_commit() -> (tempfile::TempDir, GitStashTool) {
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
        std::fs::write(root.join("tracked.txt"), b"line1\nline2\n").unwrap();
        run(&["add", "tracked.txt"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-m",
            "init",
        ]);
        let tool = GitStashTool::with_workdir(&root);
        (dir, tool)
    }

    fn make_dirty(root: &Path) {
        std::fs::write(root.join("tracked.txt"), b"line1\nstashed-change\n").unwrap();
    }

    fn stash_args(action: &str, extra: Value) -> Value {
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
    async fn lifecycle_push_list_pop_restores_tree() {
        let (d, tool) = repo_with_commit();
        let root = d.path().to_path_buf();
        make_dirty(&root);
        let r = tool
            .call(&stash_args("push", serde_json::json!({"message": "wip"})))
            .await
            .unwrap();
        assert_eq!(r.get("status").and_then(|v| v.as_str()), Some("stashed"));
        assert_eq!(r.get("stash_count").and_then(|v| v.as_u64()), Some(1));
        // 工作树恢复干净(Windows 检出 CRLF 归一后比对)
        let restored = std::fs::read_to_string(root.join("tracked.txt"))
            .unwrap()
            .replace('\r', "");
        assert_eq!(restored, "line1\nline2\n", "push 后工作树回基线");

        let list = tool.call(&stash_args("list", Value::Null)).await.unwrap();
        let items = list.get("stashes").and_then(|v| v.as_array()).unwrap();
        assert_eq!(items.len(), 1);
        assert!(
            items[0]
                .get("ref")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.starts_with("stash@{")),
            "ref 形态在册: {list:?}"
        );

        let r = tool.call(&stash_args("pop", Value::Null)).await.unwrap();
        assert_eq!(
            r.get("status").and_then(|v| v.as_str()),
            Some("restored_and_dropped")
        );
        assert_eq!(r.get("stash_count_after").and_then(|v| v.as_u64()), Some(0));
        let now = std::fs::read_to_string(root.join("tracked.txt"))
            .unwrap()
            .replace('\r', "");
        assert_eq!(now, "line1\nstashed-change\n", "pop 恢复改动");
    }

    #[tokio::test]
    async fn push_on_clean_tree_fails() {
        let (_d, tool) = repo_with_commit();
        let err = tool
            .call(&stash_args("push", Value::Null))
            .await
            .unwrap_err();
        assert!(
            err.contains("working tree is clean"),
            "clean tree 前置必检: {err}"
        );
    }

    #[tokio::test]
    async fn apply_keeps_entry_pop_drops_entry() {
        let (d, tool) = repo_with_commit();
        let root = d.path().to_path_buf();
        make_dirty(&root);
        tool.call(&stash_args("push", serde_json::json!({"message": "a"})))
            .await
            .unwrap();
        make_dirty(&root);
        tool.call(&stash_args("push", serde_json::json!({"message": "b"})))
            .await
            .unwrap();
        assert_eq!(tool.stash_count(), 2);
        // apply:恢复但条目保留
        let r = tool
            .call(&stash_args(
                "apply",
                serde_json::json!({"ref": "stash@{0}"}),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.get("status").and_then(|v| v.as_str()),
            Some("restored_stash_kept")
        );
        assert_eq!(r.get("stash_count_after").and_then(|v| v.as_u64()), Some(2));
        // drop:条目移除
        let r = tool
            .call(&stash_args("drop", serde_json::json!({"ref": "stash@{0}"})))
            .await
            .unwrap();
        assert_eq!(r.get("stash_count").and_then(|v| v.as_u64()), Some(1));
    }

    #[tokio::test]
    async fn pop_conflict_fails_visible_and_keeps_entry() {
        let (d, tool) = repo_with_commit();
        let root = d.path().to_path_buf();
        // 改 tracked.txt → stash
        make_dirty(&root);
        tool.call(&stash_args("push", Value::Null)).await.unwrap();
        // 提交同区域不同改动(pop 时必然冲突)
        std::fs::write(root.join("tracked.txt"), b"line1\nupstream-change\n").unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git spawn");
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["add", "tracked.txt"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-m",
            "upstream",
        ]);
        let err = tool
            .call(&stash_args("pop", Value::Null))
            .await
            .unwrap_err();
        assert!(err.contains("conflict"), "冲突 fail-visible: {err}");
        assert_eq!(tool.stash_count(), 1, "pop 失败=条目保留(git 语义)");
    }

    #[tokio::test]
    async fn include_untracked_flag_gates_untracked_capture() {
        let (d, tool) = repo_with_commit();
        let root = d.path().to_path_buf();
        std::fs::write(root.join("new_untracked.txt"), b"spawn").unwrap();
        // 默认 tracked-only:纯 untracked 场景 push 被前置检查拒(clean)
        let err = tool
            .call(&stash_args("push", Value::Null))
            .await
            .unwrap_err();
        assert!(err.contains("working tree is clean"), "got: {err}");
        // 显式旗标才收 untracked(工作树含 untracked=porcelain 非空,过检)
        let r = tool
            .call(&stash_args(
                "push",
                serde_json::json!({"include_untracked": true, "message": "spawn"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.get("stash_count").and_then(|v| v.as_u64()), Some(1));
        assert!(
            !root.join("new_untracked.txt").exists(),
            "untracked 文件随 stash 收起"
        );
    }

    #[tokio::test]
    async fn invalid_stash_ref_rejected() {
        let (d, tool) = repo_with_commit();
        let root = d.path().to_path_buf();
        make_dirty(&root);
        tool.call(&stash_args("push", Value::Null)).await.unwrap();
        for bad in ["master", "stash", "stash@{x}", "stash@{}", "-evil"] {
            let err = tool
                .call(&stash_args("pop", serde_json::json!({"ref": bad})))
                .await
                .unwrap_err();
            assert!(
                err.contains("invalid stash ref"),
                "ref 白名单拒 \"{bad}\": {err}"
            );
        }
        let _ = root;
    }

    #[tokio::test]
    async fn show_returns_stat() {
        let (d, tool) = repo_with_commit();
        let root = d.path().to_path_buf();
        make_dirty(&root);
        tool.call(&stash_args("push", Value::Null)).await.unwrap();
        let r = tool
            .call(&stash_args("show", serde_json::json!({"ref": "stash@{0}"})))
            .await
            .unwrap();
        let stat = r.get("stat").and_then(|v| v.as_str()).unwrap_or("");
        assert!(stat.contains("tracked.txt"), "stat 含变更文件: {stat}");
    }

    #[test]
    fn stash_ref_whitelist_shape() {
        assert!(GitStashTool::valid_stash_ref("stash@{0}"));
        assert!(GitStashTool::valid_stash_ref("stash@{12}"));
        for bad in [
            "",
            "stash",
            "stash@",
            "stash@{}",
            "stash@{x}",
            "stash@{0x1}",
            "master",
            "stash@{0",
        ] {
            assert!(!GitStashTool::valid_stash_ref(bad), "应拒: {bad:?}");
        }
    }

    #[tokio::test]
    async fn unknown_action_rejected() {
        let (_d, tool) = repo_with_commit();
        let err = tool
            .call(&stash_args("rebase", Value::Null))
            .await
            .unwrap_err();
        assert!(
            err.contains("valid actions: push, list, show, pop, apply, drop"),
            "got: {err}"
        );
    }
}
