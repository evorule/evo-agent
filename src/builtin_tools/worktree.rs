// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! `worktree` —— 多 worktree 自动化(P2 并行面)。
//!
//! 把「建/查/删/清 git worktree」从多轮 shell_exec 审批往返收敛为单工具
//! 结构化调用:agent 可为并行任务(长测试/实验分支/隔离改动)一键开
//! 隔离检出,完工一键回收——并行隔离的机制化承载。
//!
//! ## 安全栏
//!
//! - **argv 直调无 shell**:全部 git 调用为参数数组,无解释器面;
//! - **路径沙箱**:`path` 参数拒绝绝对路径/`..` 组件,解析后必须落在
//!   工作区内;remove 的目标与主检出/workdir 本身重合即拒;
//! - **分支名校验**:白名单字符集(`A-Za-z0-9._/-`),拒前导 `-`、`..`、
//!   空串、`.lock` 尾;remove 的 `delete_branch` 拒 `main`/`master`;
//! - **脏树 fail-visible**:remove 遇未提交改动默认拒(`force=true` 显式
//!   越过,旗标进返回体可审计);
//! - **主检出保护**:list 首条目(建仓检出)永不作为 remove 目标。

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::io_handler::IoResult;

/// 工具名(LLM 契约面/manifest/注册三处同键)
pub const WORKTREE_TOOL: &str = "worktree";

/// 工作区根(wired 注入;全部 git 调用的 cwd 与路径沙箱锚)
#[derive(Clone)]
pub struct WorktreeTool {
    workdir: PathBuf,
}

impl WorktreeTool {
    /// 生产构造(workdir=工作区根,与 shell_exec 同基线)
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

    /// 同步 git 调用(argv 直调;cwd=工作区根或指定目录)
    fn git(&self, cwd: &Path, args: &[&str]) -> Result<std::process::Output, String> {
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|e| format!("git spawn failed: {e} (is git installed?)"))
    }

    /// git 调用+成功断言(失败=stderr/stdout 尾段 fail-visible)
    fn git_ok(&self, cwd: &Path, args: &[&str], label: &str) -> Result<String, String> {
        let out = self.git(cwd, args)?;
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

    /// 分支/ref 组件校验(白名单字符集;git ref 安全子集)
    fn valid_ref(name: &str) -> bool {
        !name.is_empty()
            && !name.starts_with('-')
            && !name.ends_with('/')
            && !name.ends_with(".lock")
            && !name.contains("..")
            && !name.contains(' ')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
    }

    /// 路径沙箱:拒绝绝对路径/`..` 组件,解析后须落在工作区内
    fn resolve_under_workdir(&self, raw: &str) -> Result<PathBuf, String> {
        let p = Path::new(raw);
        if p.is_absolute() {
            return Err(format!(
                "path must be relative to the workspace root (got absolute: '{raw}')"
            ));
        }
        for comp in p.components() {
            if matches!(comp, std::path::Component::ParentDir) {
                return Err(format!("path must not contain '..' (got: '{raw}')"));
            }
        }
        let joined: PathBuf = p.components().collect();
        let joined = self.workdir.join(joined);
        if !joined.starts_with(&self.workdir) {
            return Err(format!("path escapes the workspace: '{raw}'"));
        }
        Ok(joined)
    }

    /// porcelain 解析:`worktree list --porcelain` 块 → 结构化条目
    /// (首条=建仓主检出)
    fn parse_porcelain(out: &str) -> Vec<WorktreeEntry> {
        let mut out_v = Vec::new();
        let mut path = None;
        let mut head = None;
        let mut branch = None;
        let mut bare = false;
        let mut locked = false;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                if path.is_some() {
                    out_v.push(WorktreeEntry {
                        path: path.take().unwrap_or_default(),
                        head: head.take(),
                        branch: branch.take(),
                        bare,
                        locked,
                    });
                    bare = false;
                    locked = false;
                }
                path = Some(p.to_string());
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                head = Some(h.to_string());
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = b.strip_prefix("refs/heads/").map(|s| s.to_string());
            } else if line == "bare" {
                bare = true;
            } else if line.starts_with("locked") {
                locked = true;
            }
        }
        if let Some(p) = path.take() {
            out_v.push(WorktreeEntry {
                path: p,
                head: head.take(),
                branch: branch.take(),
                bare,
                locked,
            });
        }
        out_v
    }

    /// 主检出路径( porcelain 首条;非 bare 时=建仓检出)
    fn main_worktree_path(&self) -> Result<PathBuf, String> {
        let out = self.git_ok(
            &self.workdir,
            &["worktree", "list", "--porcelain"],
            "worktree list",
        )?;
        let entries = Self::parse_porcelain(&out);
        entries
            .first()
            .map(|e| PathBuf::from(&e.path))
            .ok_or_else(|| "no worktrees found (not a git repo?)".to_string())
    }

    /// 同一路径判定(Windows 大小写不敏感;朴素后缀/前缀比对)
    fn same_path(a: &Path, b: &Path) -> bool {
        let norm = |p: &Path| -> String {
            p.to_string_lossy()
                .trim_end_matches(['/', '\\'])
                .replace('/', "\\")
                .to_lowercase()
        };
        norm(a) == norm(b)
    }
}

/// porcelain 条目
struct WorktreeEntry {
    path: String,
    head: Option<String>,
    branch: Option<String>,
    bare: bool,
    locked: bool,
}

#[async_trait::async_trait]
impl crate::io_handlers::tool_handler::ToolFunction for WorktreeTool {
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("worktree tool panicked: {e}"))?
    }
}

impl WorktreeTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &Value) -> IoResult {
        let action = args.get("action").and_then(|v| v.as_str()).ok_or_else(|| {
            "missing required arg: action (list | add | remove | prune)".to_string()
        })?;
        match action {
            "list" => self.action_list(),
            "add" => self.action_add(args),
            "remove" => self.action_remove(args),
            "prune" => self.action_prune(),
            other => Err(format!(
                "unknown action \"{other}\" (valid actions: list, add, remove, prune)"
            )),
        }
    }

    fn action_list(&self) -> IoResult {
        let out = self.git_ok(
            &self.workdir,
            &["worktree", "list", "--porcelain"],
            "worktree list",
        )?;
        let entries = Self::parse_porcelain(&out);
        let items: Vec<Value> = entries
            .iter()
            .map(|e| {
                let p = Path::new(&e.path);
                let dirty = !e.bare && {
                    let st = self.git(p, &["status", "--porcelain"]);
                    match st {
                        Ok(o) => !o.stdout.is_empty(),
                        Err(_) => false, // 不可读视为干净(列表面非执法面)
                    }
                };
                serde_json::json!({
                    "path": e.path,
                    "branch": e.branch,
                    "head": e.head,
                    "bare": e.bare,
                    "locked": e.locked,
                    "dirty": dirty,
                    "is_main": Self::same_path(p, &self.workdir),
                })
            })
            .collect();
        Ok(serde_json::json!({
            "action": "list",
            "count": items.len(),
            "worktrees": items,
        }))
    }

    fn action_add(&self, args: &Value) -> IoResult {
        let branch = args
            .get("branch")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: branch (string)".to_string())?;
        if !Self::valid_ref(branch) {
            return Err(format!(
                "invalid branch name \"{branch}\" (allowed: A-Za-z0-9._/-; no leading \
                 '-', no '..', no spaces, no .lock suffix)"
            ));
        }
        let raw_path = args.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
            "missing required arg: path (relative to workspace root, e.g. \
                 '.worktrees/feature-x')"
                .to_string()
        })?;
        let resolved = self.resolve_under_workdir(raw_path)?;
        if resolved.exists() {
            return Err(format!("path already exists: '{raw_path}'"));
        }
        if Self::same_path(&resolved, &self.workdir) {
            return Err("path must differ from the workspace root".to_string());
        }
        let start_point = args
            .get("start_point")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty() && Self::valid_ref(s.trim()));
        let mut git_args: Vec<&str> = vec!["worktree", "add", "-b", branch];
        let path_str = resolved.to_string_lossy().to_string();
        git_args.push(&path_str);
        if let Some(sp) = start_point {
            git_args.push(sp.trim());
        }
        self.git_ok(&self.workdir, &git_args, "worktree add")?;
        let head = self
            .git_ok(&resolved, &["rev-parse", "HEAD"], "rev-parse")
            .map(|s| s.trim().to_string())
            .ok();
        Ok(serde_json::json!({
            "action": "add",
            "path": path_str,
            "branch": branch,
            "start_point": start_point,
            "head": head,
            "status": "created",
            "note": "cd into the path (shell_exec) to work inside it; remove via this tool when done",
        }))
    }

    fn action_remove(&self, args: &Value) -> IoResult {
        let raw_path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: path".to_string())?;
        let resolved = self.resolve_under_workdir(raw_path)?;
        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
        let delete_branch = args
            .get("delete_branch")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty());

        // 主检出/workdir 本身拒删
        if Self::same_path(&resolved, &self.workdir) {
            return Err("refusing to remove the main worktree (workspace root)".to_string());
        }
        let main = self.main_worktree_path()?;
        let resolved_canon = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
        let main_canon = std::fs::canonicalize(&main).unwrap_or(main.clone());
        if Self::same_path(&resolved_canon, &main_canon) {
            return Err("refusing to remove the main worktree".to_string());
        }
        // 待删分支名(显式传入才删;拒 main/master)
        if let Some(b) = delete_branch {
            let bt = b.trim();
            if bt == "main" || bt == "master" || bt == "HEAD" {
                return Err(format!("refusing to delete protected branch '{bt}'"));
            }
            if !Self::valid_ref(bt) {
                return Err(format!("invalid branch name \"{bt}\""));
            }
        }

        // 脏树 fail-visible(force 显式越过,旗标进返回体)
        let dirty = match self.git(&resolved, &["status", "--porcelain"]) {
            Ok(o) => !o.stdout.is_empty(),
            Err(_) => false, // 不可读(已残)按干净处理,remove 自会报
        };
        if dirty && !force {
            return Err(
                "worktree has uncommitted changes: commit/stash first, or pass \
                 force=true explicitly to discard them"
                    .to_string(),
            );
        }
        let path_str = resolved.to_string_lossy().to_string();
        let mut git_args: Vec<&str> = vec!["worktree", "remove"];
        if force {
            git_args.push("--force");
        }
        git_args.push(&path_str);
        self.git_ok(&self.workdir, &git_args, "worktree remove")?;

        // 分支删除(显式请求才执行)
        let mut branch_deleted = None;
        if let Some(b) = delete_branch {
            let bt = b.trim();
            let d = self.git(&self.workdir, &["branch", "-D", bt]);
            branch_deleted = Some(match d.map(|o| o.status.success()) {
                Ok(true) => format!("deleted '{bt}'"),
                _ => format!("branch '{bt}' not deleted (git refused)"),
            });
        }
        Ok(serde_json::json!({
            "action": "remove",
            "path": path_str,
            "status": "removed",
            "dirty_discarded": dirty && force,
            "branch_deleted": branch_deleted,
        }))
    }

    fn action_prune(&self) -> IoResult {
        let out = self.git(&self.workdir, &["worktree", "prune", "-v"])?;
        let tail = String::from_utf8_lossy(&out.stderr).into_owned();
        Ok(serde_json::json!({
            "action": "prune",
            "status": "done",
            "detail": tail.trim(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handlers::tool_handler::ToolFunction;

    /// 真 git 临时仓(≥1 commit;内联身份防全局配置缺位)
    fn repo_with_commit() -> (tempfile::TempDir, WorktreeTool) {
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
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ]);
        let tool = WorktreeTool::with_workdir(&root);
        (dir, tool)
    }

    fn wt_args(action: &str, extra: Value) -> Value {
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
    async fn lifecycle_add_list_remove() {
        let (_d, tool) = repo_with_commit();
        let r = tool
            .call(&wt_args(
                "add",
                serde_json::json!({"branch": "feature/x", "path": ".wt/x"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.get("status").and_then(|v| v.as_str()), Some("created"));

        let list = tool.call(&wt_args("list", Value::Null)).await.unwrap();
        let items = list.get("worktrees").and_then(|v| v.as_array()).unwrap();
        assert_eq!(items.len(), 2, "主检出+新 worktree");
        assert!(
            items
                .iter()
                .any(|i| i.get("branch").and_then(|v| v.as_str()) == Some("feature/x")),
            "新分支在册: {list:?}"
        );

        let r = tool
            .call(&wt_args(
                "remove",
                serde_json::json!({"path": ".wt/x", "delete_branch": "feature/x"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.get("status").and_then(|v| v.as_str()), Some("removed"));
        let list = tool.call(&wt_args("list", Value::Null)).await.unwrap();
        assert_eq!(
            list.get("worktrees")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            1,
            "回收后仅剩主检出"
        );
    }

    #[tokio::test]
    async fn add_rejects_existing_path_and_bad_branch() {
        let (_d, tool) = repo_with_commit();
        std::fs::write(_d.path().join("exists.txt"), b"").unwrap();
        let err = tool
            .call(&wt_args(
                "add",
                serde_json::json!({"branch": "ok/x", "path": "exists.txt"}),
            ))
            .await
            .unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
        let err = tool
            .call(&wt_args(
                "add",
                serde_json::json!({"branch": "-evil", "path": ".wt/y"}),
            ))
            .await
            .unwrap_err();
        assert!(err.contains("invalid branch name"), "got: {err}");
    }

    #[tokio::test]
    async fn remove_dirty_requires_explicit_force() {
        let (_d, tool) = repo_with_commit();
        tool.call(&wt_args(
            "add",
            serde_json::json!({"branch": "feature/d", "path": ".wt/d"}),
        ))
        .await
        .unwrap();
        // 制造脏改动
        let wt_path = _d.path().join(".wt").join("d");
        std::fs::write(wt_path.join("dirty.txt"), b"pending").unwrap();
        let err = tool
            .call(&wt_args("remove", serde_json::json!({"path": ".wt/d"})))
            .await
            .unwrap_err();
        assert!(err.contains("force"), "脏树默认拒: {err}");
        let r = tool
            .call(&wt_args(
                "remove",
                serde_json::json!({"path": ".wt/d", "force": true}),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.get("dirty_discarded").and_then(|v| v.as_bool()),
            Some(true),
            "force 旗标入返回体可审计"
        );
    }

    #[tokio::test]
    async fn remove_refuses_main_worktree() {
        let (_d, tool) = repo_with_commit();
        let err = tool
            .call(&wt_args("remove", serde_json::json!({"path": "."})))
            .await
            .unwrap_err();
        assert!(
            err.contains("refusing to remove the main worktree"),
            "got: {err}"
        );
    }

    #[test]
    fn delete_branch_validation_refuses_protected_names() {
        // 纯校验面:protected 分支名/非法名在分支校验层即拒(与主检出判定
        // 先后无关——两者独立防线)
        assert!(WorktreeTool::valid_ref("main"), "main 是合法 ref 名");
        assert!(WorktreeTool::valid_ref("feature/x"));
        assert!(!WorktreeTool::valid_ref(""));
        assert!(!WorktreeTool::valid_ref("-lead"));
        assert!(!WorktreeTool::valid_ref("a..b"));
        assert!(!WorktreeTool::valid_ref("has space"));
        assert!(!WorktreeTool::valid_ref("end.lock"));
    }

    #[tokio::test]
    async fn delete_branch_refuses_protected_names() {
        let (_d, tool) = repo_with_commit();
        tool.call(&wt_args(
            "add",
            serde_json::json!({"branch": "feature/z", "path": ".wt/z"}),
        ))
        .await
        .unwrap();
        let err = tool
            .call(&wt_args(
                "remove",
                serde_json::json!({"path": ".wt/z", "delete_branch": "main"}),
            ))
            .await
            .unwrap_err();
        assert!(
            err.contains("refusing to delete protected branch"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn unknown_action_and_traversal_rejected() {
        let (_d, tool) = repo_with_commit();
        let err = tool
            .call(&wt_args("rebase", Value::Null))
            .await
            .unwrap_err();
        assert!(
            err.contains("valid actions: list, add, remove, prune"),
            "got: {err}"
        );
        let err = tool
            .call(&wt_args(
                "add",
                serde_json::json!({"branch": "ok", "path": "../escape"}),
            ))
            .await
            .unwrap_err();
        assert!(err.contains("'..'"), "路径逃逸拒: {err}");
    }

    #[tokio::test]
    async fn prune_smoke_on_clean_repo() {
        let (_d, tool) = repo_with_commit();
        let r = tool.call(&wt_args("prune", Value::Null)).await.unwrap();
        assert_eq!(r.get("status").and_then(|v| v.as_str()), Some("done"));
    }
}
