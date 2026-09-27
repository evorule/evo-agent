// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! git 工具族(B3 agent 面)——两级注册治理(17 号工具面配置指南 §3.2)
//!
//! | 工具 | 开关组 | 审批层 | 默认 |
//! |---|---|---|---|
//! | `git_status` / `git_diff` / `git_log` | `agentTools.gitRead` | active(直跑) | 开 |
//! | `git_stage` / `git_commit` | `agentTools.gitWrite` | candidate(审批) | 关 |
//!
//! 三面同源:全部薄委托 [`crate::git::GitOps`],agent 与人(REST 面 / SCM 视图)
//! 看到同一份 git 语义;git 操作属于人工治理面,不进 evorule 审计链。
//!
//! candidate 两段式与 file_create 同款:首次调用返回 `needs_approval`
//! proposal,批后带 `approved=true` 重调即执行。

use std::path::PathBuf;

use serde_json::Value;

use crate::git::GitOps;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// `git_status` 工具(只读,active)
#[derive(Clone)]
pub struct GitStatusTool {
    workdir: PathBuf,
}

impl GitStatusTool {
    /// TODO: doc
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir }
    }
}

#[async_trait::async_trait]
impl ToolFunction for GitStatusTool {
    async fn call(&self, _args: &Value) -> IoResult {
        let ops = GitOps::new(self.workdir.clone());
        tokio::task::spawn_blocking(move || {
            let status = ops.status().map_err(|e| e.message())?;
            serde_json::to_value(status).map_err(|e| format!("serialize status failed: {e}"))
        })
        .await
        .map_err(|e| format!("git_status tool panicked: {e}"))?
    }
}

/// `git_diff` 工具(只读,active)
#[derive(Clone)]
pub struct GitDiffTool {
    workdir: PathBuf,
}

impl GitDiffTool {
    /// TODO: doc
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir }
    }
}

#[async_trait::async_trait]
impl ToolFunction for GitDiffTool {
    async fn call(&self, args: &Value) -> IoResult {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: path (string)".to_string())?
            .to_string();
        let ops = GitOps::new(self.workdir.clone());
        tokio::task::spawn_blocking(move || {
            let diff = ops.diff(&path).map_err(|e| e.message())?;
            serde_json::to_value(diff).map_err(|e| format!("serialize diff failed: {e}"))
        })
        .await
        .map_err(|e| format!("git_diff tool panicked: {e}"))?
    }
}

/// `git_log` 工具(只读,active)
#[derive(Clone)]
pub struct GitLogTool {
    workdir: PathBuf,
}

impl GitLogTool {
    /// TODO: doc
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir }
    }
}

#[async_trait::async_trait]
impl ToolFunction for GitLogTool {
    async fn call(&self, args: &Value) -> IoResult {
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let ops = GitOps::new(self.workdir.clone());
        tokio::task::spawn_blocking(move || {
            let commits = ops.log(limit).map_err(|e| e.message())?;
            serde_json::to_value(commits).map_err(|e| format!("serialize log failed: {e}"))
        })
        .await
        .map_err(|e| format!("git_log tool panicked: {e}"))?
    }
}

/// `git_stage` 工具(写面,candidate:暂存指定路径进 index)
#[derive(Clone)]
pub struct GitStageTool {
    workdir: PathBuf,
}

impl GitStageTool {
    /// TODO: doc
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir }
    }

    fn call_sync(&self, args: &Value) -> IoResult {
        let paths: Vec<String> = args
            .get("paths")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .ok_or_else(|| "missing required arg: paths (array of strings)".to_string())?;
        if paths.is_empty() {
            return Err("paths must not be empty".to_string());
        }
        let approved = args
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !approved {
            return Ok(serde_json::json!({
                "status": "needs_approval",
                "category": "candidate",
                "description": format!("git stage {} path(s): {}", paths.len(), paths.join(", ")),
                "risk": "modifies the git index (staging area)",
                "alternative": "stage changes manually in the workbench SCM view",
            }));
        }

        let ops = GitOps::new(self.workdir.clone());
        let staged = ops.stage(&paths).map_err(|e| e.message())?;
        Ok(serde_json::json!({ "staged": staged }))
    }
}

#[async_trait::async_trait]
impl ToolFunction for GitStageTool {
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("git_stage tool panicked: {e}"))?
    }
}

/// `git_commit` 工具(写面,candidate:全量暂存后提交,与 SCM 面提交语义一致)
#[derive(Clone)]
pub struct GitCommitTool {
    workdir: PathBuf,
}

impl GitCommitTool {
    /// TODO: doc
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir }
    }

    fn call_sync(&self, args: &Value) -> IoResult {
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: message (string)".to_string())?;
        let approved = args
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !approved {
            let summary: String = message
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect();
            return Ok(serde_json::json!({
                "status": "needs_approval",
                "category": "candidate",
                "description": format!("git commit: {summary}"),
                "risk": "creates a commit on the current branch (all pending changes are staged first)",
                "alternative": "commit manually in the workbench SCM view",
            }));
        }

        let ops = GitOps::new(self.workdir.clone());
        let id = ops.commit(message).map_err(|e| e.message())?;
        Ok(serde_json::json!({ "committed": true, "commit": id }))
    }
}

#[async_trait::async_trait]
impl ToolFunction for GitCommitTool {
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("git_commit tool panicked: {e}"))?
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    /// 干净夹具:临时目录 + git init + local 身份 + 关闭 autocrlf(隔离宿主全局配置)
    fn fresh_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        let repo = git2::Repository::init(&workdir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Tester").unwrap();
        cfg.set_str("user.email", "tester@example.com").unwrap();
        cfg.set_bool("core.autocrlf", false).unwrap();
        drop(repo);
        (dir, workdir)
    }

    /// 带 pending 修改的仓库(写一个 untracked 文件)
    fn repo_with_change(workdir: &std::path::Path) {
        std::fs::write(workdir.join("a.txt"), "hello\n").unwrap();
    }

    #[tokio::test]
    async fn test_status_reads_pending_change() {
        let (_d, workdir) = fresh_repo();
        repo_with_change(&workdir);
        let tool = GitStatusTool::new(workdir.clone());
        let v = tool.call(&json!({})).await.unwrap();
        assert_eq!(v["dirty"], json!(true));
        assert_eq!(v["changes"][0]["path"], json!("a.txt"));
        assert_eq!(v["changes"][0]["status"], json!("U"));
    }

    #[tokio::test]
    async fn test_diff_returns_both_sides() {
        let (_d, workdir) = fresh_repo();
        repo_with_change(&workdir);
        let tool = GitDiffTool::new(workdir.clone());
        let v = tool.call(&json!({"path": "a.txt"})).await.unwrap();
        assert_eq!(v["original"], json!(""));
        assert_eq!(v["modified"], json!("hello\n"));
    }

    #[tokio::test]
    async fn test_log_lists_commits_after_commit() {
        let (_d, workdir) = fresh_repo();
        repo_with_change(&workdir);
        let ops = GitOps::new(workdir.clone());
        ops.commit("first commit").unwrap();

        let tool = GitLogTool::new(workdir.clone());
        let v = tool.call(&json!({"limit": 10})).await.unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["summary"], json!("first commit"));
    }

    #[tokio::test]
    async fn test_stage_without_approval_returns_proposal() {
        let (_d, workdir) = fresh_repo();
        repo_with_change(&workdir);
        let tool = GitStageTool::new(workdir.clone());
        let v = tool.call(&json!({"paths": ["a.txt"]})).await.unwrap();
        assert_eq!(v["status"], json!("needs_approval"));
        assert_eq!(v["category"], json!("candidate"));
        // 未批准不落任何变更
        let status = GitOps::new(workdir.clone()).status().unwrap();
        assert!(status.staged.is_empty());
    }

    #[tokio::test]
    async fn test_stage_with_approval_moves_into_index() {
        let (_d, workdir) = fresh_repo();
        repo_with_change(&workdir);
        let tool = GitStageTool::new(workdir.clone());
        let v = tool
            .call(&json!({"paths": ["a.txt"], "approved": true}))
            .await
            .unwrap();
        assert_eq!(v["staged"], json!(1));
        let status = GitOps::new(workdir.clone()).status().unwrap();
        assert_eq!(status.staged[0].status, 'A');
    }

    #[tokio::test]
    async fn test_stage_rejects_empty_paths() {
        let (_d, workdir) = fresh_repo();
        let tool = GitStageTool::new(workdir.clone());
        let err = tool
            .call(&json!({"paths": [], "approved": true}))
            .await
            .unwrap_err();
        assert!(err.contains("must not be empty"), "got: {err}");
    }

    #[tokio::test]
    async fn test_commit_without_approval_returns_proposal() {
        let (_d, workdir) = fresh_repo();
        repo_with_change(&workdir);
        let tool = GitCommitTool::new(workdir.clone());
        let v = tool
            .call(&json!({"message": "agent commit"}))
            .await
            .unwrap();
        assert_eq!(v["status"], json!("needs_approval"));
        // 未批准不产生提交
        assert!(GitOps::new(workdir.clone()).log(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_commit_with_approval_creates_commit() {
        let (_d, workdir) = fresh_repo();
        repo_with_change(&workdir);
        let tool = GitCommitTool::new(workdir.clone());
        let v = tool
            .call(&json!({"message": "agent commit", "approved": true}))
            .await
            .unwrap();
        assert_eq!(v["committed"], json!(true));
        let log = GitOps::new(workdir.clone()).log(10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].summary, "agent commit");
    }

    #[tokio::test]
    async fn test_commit_reports_identity_missing() {
        // local config 显式置空 = 模拟身份缺失（隔离宿主全局配置，git2 local 优先）
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        let repo = git2::Repository::init(&workdir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "").unwrap();
        cfg.set_str("user.email", "").unwrap();
        drop(repo);
        std::fs::write(workdir.join("a.txt"), "x\n").unwrap();

        let tool = GitCommitTool::new(workdir.clone());
        let err = tool
            .call(&json!({"message": "m", "approved": true}))
            .await
            .unwrap_err();
        assert!(err.contains("identity_missing"), "got: {err}");
    }

    #[tokio::test]
    async fn test_commit_rejects_empty_message() {
        let (_d, workdir) = fresh_repo();
        repo_with_change(&workdir);
        let tool = GitCommitTool::new(workdir.clone());
        let err = tool
            .call(&json!({"message": "  ", "approved": true}))
            .await
            .unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[tokio::test]
    async fn test_status_errors_on_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        let tool = GitStatusTool::new(dir.path().to_path_buf());
        let err = tool.call(&json!({})).await.unwrap_err();
        assert!(err.contains("not a git repository"), "got: {err}");
    }
}
