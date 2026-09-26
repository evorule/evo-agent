// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `file_move` —— 移动 / 重命名(工作目录沙箱 + 写入路径白名单 + 目标重名拒)
//!
//! rename = 同目录移动特例;跨目录移动一次完成。安全模型与 file_create 同源
//! (判据见 fs_safety):沙箱 containment、symlink/junction 逃逸拒绝、目标重名
//! 拒(409 语义)、目录移入自身拒绝。跨盘 rename 失败时递归 copy+remove 兜底。
//!
//! 审批:candidate(与 file_create 同款两段式,`approved=true` 执行)。

use std::path::PathBuf;

use serde_json::Value;

use crate::builtin_tools::fs_safety;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// `file_move` 工具
#[derive(Clone)]
pub struct FileMoveTool {
    workdir: PathBuf,
    writable_dir: PathBuf,
}

impl FileMoveTool {
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
impl ToolFunction for FileMoveTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 操作
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("file_move tool panicked: {e}"))?
    }
}

impl FileMoveTool {
    fn call_sync(&self, args: &Value) -> IoResult {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: path (string)".to_string())?;
        let target_dir = args
            .get("target_dir")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: target_dir (string)".to_string())?;
        let new_name = args.get("new_name").and_then(|v| v.as_str());
        let approved = args
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let writable_canonical = fs_safety::writable_root(&self.workdir, &self.writable_dir)?;

        // 源:必须已存在且在 writable_dir 内
        let source = fs_safety::resolve_existing(&self.workdir, path)?;
        if !source.starts_with(&writable_canonical) {
            return Err(format!(
                "path '{path}' is outside writable_dir '{}' (path traversal)",
                self.writable_dir.display()
            ));
        }

        // 目标目录:必须已存在、是目录、且在 writable_dir 内
        let tdir = fs_safety::resolve_existing(&self.workdir, target_dir)?;
        let is_dir = std::fs::symlink_metadata(&tdir)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if !is_dir {
            return Err(format!("target_dir is not a directory: '{target_dir}'"));
        }
        if !tdir.starts_with(&writable_canonical) {
            return Err(format!(
                "target_dir '{target_dir}' is outside writable_dir '{}' (path traversal)",
                self.writable_dir.display()
            ));
        }

        // 目录移入自身(或自身子目录)拒
        if tdir.starts_with(&source) {
            return Err(format!(
                "cannot move '{path}' into itself or its own subtree ('{target_dir}')"
            ));
        }

        // 新名:缺省沿用源名;必须过 Windows 兼容名校验
        let name = match new_name {
            Some(n) => {
                fs_safety::validate_node_name(n)?;
                n.to_string()
            }
            None => source
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .ok_or_else(|| "cannot move the workdir root".to_string())?,
        };

        let target = tdir.join(&name);
        if std::fs::symlink_metadata(&target).is_ok() {
            return Err(format!(
                "already exists: '{}' (move target collision)",
                target.display()
            ));
        }

        if !approved {
            let description = if new_name.is_some() {
                format!("move '{path}' to '{target_dir}' as '{name}'")
            } else {
                format!("move '{path}' to '{target_dir}'")
            };
            return Ok(serde_json::json!({
                "status": "needs_approval",
                "category": "candidate",
                "description": description,
                "risk": "relocates project files",
                "alternative": "move or rename the file manually in the workbench file tree",
            }));
        }

        if let Err(rename_err) = std::fs::rename(&source, &target) {
            // 跨盘 fallback:递归 copy + 删源(rename 语义尽力保持:目标重名已预检)
            fs_safety::copy_recursive(&source, &target)
                .map_err(|e| format!("move failed (rename: {rename_err}; fallback: {e})"))?;
            fs_safety::remove_recursive(&source)
                .map_err(|e| format!("move fallback cleanup failed: {e}"))?;
        }

        let mut map = serde_json::Map::new();
        map.insert(
            "path".to_string(),
            Value::from(target.display().to_string()),
        );
        map.insert(
            "from".to_string(),
            Value::from(source.display().to_string()),
        );
        map.insert("name".to_string(), Value::from(name));
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

    fn call(tool: &FileMoveTool, body: Value) -> IoResult {
        tool.call_sync(&body)
    }

    #[test]
    fn test_rename_same_dir() {
        let (_d, workdir) = temp_workdir();
        std::fs::write(workdir.join("workspace/old.txt"), b"data").unwrap();
        let tool = FileMoveTool::new(workdir.clone());
        let v = call(
            &tool,
            json!({"path": "workspace/old.txt", "target_dir": "workspace", "new_name": "new.txt", "approved": true}),
        )
        .unwrap();
        assert_eq!(v["name"], json!("new.txt"));
        assert!(!workdir.join("workspace/old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(workdir.join("workspace/new.txt")).unwrap(),
            "data"
        );
    }

    #[test]
    fn test_move_across_dirs() {
        let (_d, workdir) = temp_workdir();
        std::fs::create_dir(workdir.join("workspace/dst")).unwrap();
        std::fs::write(workdir.join("workspace/src.txt"), b"x").unwrap();
        let tool = FileMoveTool::new(workdir.clone());
        call(
            &tool,
            json!({"path": "workspace/src.txt", "target_dir": "workspace/dst", "approved": true}),
        )
        .unwrap();
        assert!(workdir.join("workspace/dst/src.txt").is_file());
        assert!(!workdir.join("workspace/src.txt").exists());
    }

    #[test]
    fn test_move_dir_with_children() {
        let (_d, workdir) = temp_workdir();
        std::fs::create_dir_all(workdir.join("workspace/a/b")).unwrap();
        std::fs::write(workdir.join("workspace/a/b/f.txt"), b"x").unwrap();
        std::fs::create_dir(workdir.join("workspace/dst")).unwrap();
        let tool = FileMoveTool::new(workdir.clone());
        call(
            &tool,
            json!({"path": "workspace/a", "target_dir": "workspace/dst", "approved": true}),
        )
        .unwrap();
        assert!(workdir.join("workspace/dst/a/b/f.txt").is_file());
    }

    #[test]
    fn test_source_missing_404_semantics() {
        let (_d, workdir) = temp_workdir();
        let tool = FileMoveTool::new(workdir.clone());
        let err = call(
            &tool,
            json!({"path": "workspace/ghost.txt", "target_dir": "workspace", "approved": true}),
        )
        .unwrap_err();
        assert!(err.contains("does not exist"), "got: {err}");
    }

    #[test]
    fn test_target_dir_missing_or_not_dir() {
        let (_d, workdir) = temp_workdir();
        std::fs::write(workdir.join("workspace/f.txt"), b"x").unwrap();
        let tool = FileMoveTool::new(workdir.clone());
        let e1 = call(
            &tool,
            json!({"path": "workspace/f.txt", "target_dir": "workspace/ghost", "approved": true}),
        )
        .unwrap_err();
        assert!(e1.contains("does not exist"), "got: {e1}");
        let e2 = call(
            &tool,
            json!({"path": "workspace/f.txt", "target_dir": "workspace/f.txt", "approved": true}),
        )
        .unwrap_err();
        assert!(e2.contains("not a directory"), "got: {e2}");
    }

    #[test]
    fn test_target_collision_409_semantics() {
        let (_d, workdir) = temp_workdir();
        std::fs::write(workdir.join("workspace/a.txt"), b"a").unwrap();
        std::fs::write(workdir.join("workspace/b.txt"), b"b").unwrap();
        let tool = FileMoveTool::new(workdir.clone());
        let err = call(
            &tool,
            json!({"path": "workspace/a.txt", "target_dir": "workspace", "new_name": "b.txt", "approved": true}),
        )
        .unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
        // 内容未被破坏
        assert_eq!(
            std::fs::read_to_string(workdir.join("workspace/b.txt")).unwrap(),
            "b"
        );
    }

    #[test]
    fn test_move_into_own_subtree_rejected() {
        let (_d, workdir) = temp_workdir();
        std::fs::create_dir_all(workdir.join("workspace/outer/inner")).unwrap();
        let tool = FileMoveTool::new(workdir.clone());
        let err = call(
            &tool,
            json!({"path": "workspace/outer", "target_dir": "workspace/outer/inner", "approved": true}),
        )
        .unwrap_err();
        assert!(err.contains("into itself"), "got: {err}");
    }

    #[test]
    fn test_outside_writable_dir_rejected() {
        let (_d, workdir) = temp_workdir();
        std::fs::write(workdir.join("root.txt"), b"x").unwrap();
        std::fs::create_dir(workdir.join("workspace/dst")).unwrap();
        let tool = FileMoveTool::new(workdir.clone());
        let e1 = call(
            &tool,
            json!({"path": "root.txt", "target_dir": "workspace/dst", "approved": true}),
        )
        .unwrap_err();
        assert!(e1.contains("outside writable_dir"), "got: {e1}");
        let e2 = call(
            &tool,
            json!({"path": "workspace/dst", "target_dir": ".", "approved": true}),
        )
        .unwrap_err();
        assert!(
            e2.contains("outside writable_dir") || e2.contains("not a directory"),
            "target_dir '.'（workdir 根,不在 workspace 内）必须拒: {e2}"
        );
    }

    #[test]
    fn test_invalid_new_name() {
        let (_d, workdir) = temp_workdir();
        std::fs::write(workdir.join("workspace/a.txt"), b"x").unwrap();
        let tool = FileMoveTool::new(workdir.clone());
        let err = call(
            &tool,
            json!({"path": "workspace/a.txt", "target_dir": "workspace", "new_name": "con", "approved": true}),
        )
        .unwrap_err();
        assert!(err.contains("reserved Windows device name"), "got: {err}");
    }

    #[test]
    fn test_no_approval_returns_proposal() {
        let (_d, workdir) = temp_workdir();
        std::fs::write(workdir.join("workspace/a.txt"), b"x").unwrap();
        let tool = FileMoveTool::new(workdir.clone());
        let v = call(
            &tool,
            json!({"path": "workspace/a.txt", "target_dir": "workspace", "new_name": "b.txt"}),
        )
        .unwrap();
        assert_eq!(v["status"], json!("needs_approval"));
        assert!(workdir.join("workspace/a.txt").is_file());
    }

    #[test]
    fn test_missing_args() {
        let (_d, workdir) = temp_workdir();
        let tool = FileMoveTool::new(workdir.clone());
        assert!(call(&tool, json!({"target_dir": "workspace", "approved": true})).is_err());
        assert!(call(&tool, json!({"path": "workspace", "approved": true})).is_err());
    }
}
