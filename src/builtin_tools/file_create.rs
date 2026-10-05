// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `file_create` —— 创建文件 / 目录(工作目录沙箱 + 写入路径白名单 + 重名拒)
//!
//! ## 安全模型(与 file_write 同源,判据见 fs_safety)
//!
//! 1. 工作目录沙箱:绝对路径 / `..` / symlink / junction 逃逸一律拒
//! 2. 写入路径白名单:只能落在 `writable_dir` 内(默认 `./workspace/`;
//!    工作台人工编辑面用 `"."` = workdir 全域,UX 边界非安全边界)
//! 3. 重名拒绝:目标已存在(文件/目录/链接)一律拒(调用方 409 语义)
//! 4. Windows 兼容名校验:保留名(CON/NUL/COM1-9/LPT1-9)/非法字符/
//!    尾部点空格(尚不存在的新组件;后端一等校验面,前端仅预校验)
//! 5. 父目录:`create_parents=true` 才逐级创建(默认 false)
//!
//! ## 审批(三层模型:candidate,管道⑤收编)
//!
//! agent 面调用属 candidate:管道⑤经 [`ToolFunction::evaluate_proposal`]
//! 评估钩子先行拦截(合法路径=写面 candidate),决策端批准后才进执行体
//! (工具侧两调协议已消灭,call 无 approved 分支)。工作台 REST 面以服务端
//! 构造的调用直落执行体(人工操作不进 agent 审批链)。

use std::path::PathBuf;

use serde_json::Value;

use crate::builtin_tools::fs_safety;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// `file_create` 工具
#[derive(Clone)]
pub struct FileCreateTool {
    workdir: PathBuf,
    writable_dir: PathBuf,
}

impl FileCreateTool {
    /// TODO: doc
    pub fn new(workdir: PathBuf) -> Self {
        Self {
            workdir,
            writable_dir: PathBuf::from(crate::builtin_tools::file_write::DEFAULT_WRITABLE_DIR),
        }
    }

    /// 设置可写子目录(相对 workdir);`"."` = workdir 全域(工作台人工面用)
    pub fn with_writable_dir(mut self, dir: &str) -> Self {
        self.writable_dir = PathBuf::from(dir);
        self
    }
}

#[async_trait::async_trait]
impl ToolFunction for FileCreateTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 操作
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("file_create tool panicked: {e}"))?
    }

    /// 管道⑤评估单源(PR-4 收编):candidate 分类钩子——路径可解析(沙箱+
    /// 白名单校验通过)即属写面 candidate,返回 proposal;参数形状非法或
    /// 路径非法返回 None(call 期显式报错,与原判定次序一致:非法路径先 Err)。
    /// 纯函数:仅做路径解析校验,无 IO 副作用。
    fn evaluate_proposal(&self, args: &Value) -> Option<Value> {
        let path = args.get("path").and_then(|v| v.as_str())?;
        let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("file");
        if kind != "file" && kind != "dir" {
            return None;
        }
        fs_safety::resolve_create_target(&self.workdir, &self.writable_dir, path)
            .ok()
            .map(|_| {
                serde_json::json!({
                    "status": "needs_approval",
                    "category": "candidate",
                    "description": format!("create {kind} '{path}'"),
                    "risk": "writes to the project workspace",
                    "alternative": "create the file or folder manually in the workbench file tree",
                })
            })
    }
}

impl FileCreateTool {
    fn call_sync(&self, args: &Value) -> IoResult {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: path (string)".to_string())?;
        let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("file");
        if kind != "file" && kind != "dir" {
            return Err(format!(
                "invalid kind '{kind}' (must be \"file\" or \"dir\")"
            ));
        }
        let create_parents = args
            .get("create_parents")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let target = fs_safety::resolve_create_target(&self.workdir, &self.writable_dir, path)?;

        // 审批门在管道⑤(PR-4 收编):评估臂先行拦截 candidate,只有决策端
        // 批准后才进本执行体;工作台 REST 人工面为服务端构造调用(人工操作
        // 不进 agent 审批链),直接落此处执行。

        if let Some(parent) = target.parent() {
            if !parent.exists() {
                if !create_parents {
                    return Err(format!(
                        "parent dir does not exist: '{}'; pass create_parents=true to create",
                        parent.display()
                    ));
                }
                fs_safety::create_missing_parents(&target)?;
            }
        }
        if kind == "dir" {
            std::fs::create_dir(&target).map_err(|e| format!("create_dir failed: {e}"))?;
        } else {
            std::fs::File::create(&target).map_err(|e| format!("create_file failed: {e}"))?;
        }

        let mut map = serde_json::Map::new();
        map.insert(
            "path".to_string(),
            Value::from(target.display().to_string()),
        );
        map.insert("kind".to_string(), Value::from(kind));
        map.insert("created".to_string(), Value::Bool(true));
        Ok(Value::Object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_workdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        std::fs::create_dir(workdir.join("workspace")).unwrap();
        (dir, workdir)
    }

    fn call(tool: &FileCreateTool, body: Value) -> IoResult {
        tool.call_sync(&body)
    }

    #[test]
    fn test_create_file_in_workspace() {
        let (_d, workdir) = temp_workdir();
        let tool = FileCreateTool::new(workdir.clone());
        let r = call(
            &tool,
            json!({"path": "workspace/new.txt", "approved": true}),
        )
        .unwrap();
        assert_eq!(r["created"], json!(true));
        assert!(workdir.join("workspace/new.txt").is_file());
    }

    #[test]
    fn test_create_dir_kind() {
        let (_d, workdir) = temp_workdir();
        let tool = FileCreateTool::new(workdir.clone());
        call(
            &tool,
            json!({"path": "workspace/sub", "kind": "dir", "approved": true}),
        )
        .unwrap();
        assert!(workdir.join("workspace/sub").is_dir());
    }

    #[test]
    fn test_reject_duplicate_409_semantics() {
        let (_d, workdir) = temp_workdir();
        std::fs::write(workdir.join("workspace/dup.txt"), b"x").unwrap();
        let tool = FileCreateTool::new(workdir.clone());
        let err = call(
            &tool,
            json!({"path": "workspace/dup.txt", "approved": true}),
        )
        .unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
    }

    #[test]
    fn test_reject_outside_writable_dir() {
        let (_d, workdir) = temp_workdir();
        let tool = FileCreateTool::new(workdir.clone());
        let err = call(&tool, json!({"path": "root.txt", "approved": true})).unwrap_err();
        assert!(err.contains("outside writable_dir"), "got: {err}");
    }

    #[test]
    fn test_reject_absolute_and_parent_traversal() {
        let (_d, workdir) = temp_workdir();
        let tool = FileCreateTool::new(workdir.clone());
        let abs = if cfg!(windows) {
            "C:\\evil.txt"
        } else {
            "/tmp/evil.txt"
        };
        let e1 = call(&tool, json!({"path": abs, "approved": true})).unwrap_err();
        assert!(e1.contains("absolute path not allowed"), "got: {e1}");
        let e2 = call(&tool, json!({"path": "../evil.txt", "approved": true})).unwrap_err();
        assert!(e2.contains("parent dir"), "got: {e2}");
    }

    #[test]
    fn test_reject_windows_reserved_name() {
        let (_d, workdir) = temp_workdir();
        let tool = FileCreateTool::new(workdir.clone());
        for name in ["CON", "nul.txt", "COM1", "aux.md"] {
            let err = call(
                &tool,
                json!({"path": format!("workspace/{name}"), "approved": true}),
            )
            .unwrap_err();
            assert!(
                err.contains("reserved Windows device name"),
                "name {name}: {err}"
            );
        }
    }

    #[test]
    fn test_reject_illegal_chars_and_trailing_dot_space() {
        let (_d, workdir) = temp_workdir();
        let tool = FileCreateTool::new(workdir.clone());
        for name in ["bad<name", "pipe|name", "trail.", "trail "] {
            let err = call(
                &tool,
                json!({"path": format!("workspace/{name}"), "approved": true}),
            )
            .unwrap_err();
            assert!(
                err.contains("illegal character") || err.contains("ends with a dot or space"),
                "name '{name}': {err}"
            );
        }
    }

    #[test]
    fn test_create_parents_false_by_default() {
        let (_d, workdir) = temp_workdir();
        let tool = FileCreateTool::new(workdir.clone());
        let err = call(
            &tool,
            json!({"path": "workspace/a/b.txt", "approved": true}),
        )
        .unwrap_err();
        assert!(err.contains("parent dir does not exist"), "got: {err}");
    }

    #[test]
    fn test_create_parents_true_builds_hierarchy() {
        let (_d, workdir) = temp_workdir();
        let tool = FileCreateTool::new(workdir.clone());
        call(
            &tool,
            json!({"path": "workspace/a/b/c.md", "create_parents": true, "approved": true}),
        )
        .unwrap();
        assert!(workdir.join("workspace/a/b/c.md").is_file());
    }

    #[test]
    fn test_candidate_evaluated_by_proposal_hook() {
        let (_d, workdir) = temp_workdir();
        let tool = FileCreateTool::new(workdir.clone());
        // 管道⑤评估单源(PR-4 收编):candidate 分类由 evaluate_proposal 钩子
        // 判定,call 不再自管 approved 分支——合法路径评估为 Some(proposal)
        let v = tool
            .evaluate_proposal(&json!({"path": "workspace/x.txt"}))
            .expect("valid workspace path is a candidate");
        assert_eq!(v["status"], json!("needs_approval"));
        assert_eq!(v["category"], json!("candidate"));
        assert!(!workdir.join("workspace/x.txt").exists());
        // 非法路径评估为 None(call 期显式报错,与原判定次序一致)
        assert!(tool
            .evaluate_proposal(&json!({"path": "root.txt"}))
            .is_none());
        // 直接调用(人工面/决策端批准后形态)真实执行——不再返回 proposal
        let r = call(&tool, json!({"path": "workspace/x.txt"})).unwrap();
        assert_eq!(r["created"], json!(true));
    }

    #[test]
    fn test_missing_path_arg() {
        let (_d, workdir) = temp_workdir();
        let tool = FileCreateTool::new(workdir.clone());
        assert!(call(&tool, json!({"approved": true})).is_err());
    }

    #[test]
    fn test_invalid_kind() {
        let (_d, workdir) = temp_workdir();
        let tool = FileCreateTool::new(workdir.clone());
        let err = call(
            &tool,
            json!({"path": "workspace/x", "kind": "symlink", "approved": true}),
        )
        .unwrap_err();
        assert!(err.contains("invalid kind"), "got: {err}");
    }
}
