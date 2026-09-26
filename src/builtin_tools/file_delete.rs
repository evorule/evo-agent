// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `file_delete` —— 软删除(移入 `<workdir>/.evo-trash/` 回收目录)
//!
//! 删除语义:确认 + 软删除。条目移入 `<workdir>/.evo-trash/<unix_ts>_<名>`
//! (同秒重名追加 `_N` 序号),树与 watcher 均忽略该目录——误操作可从回收
//! 目录手动找回,留痕清晰。跨盘 rename 失败时递归 copy+remove 兜底。
//!
//! 安全模型与 file_create 同源(判据见 fs_safety):沙箱 containment、
//! symlink/junction 逃逸拒绝。额外守护:workdir 根、可写根与回收目录自身
//! 不可删除。
//!
//! 审批:candidate(与 file_create 同款两段式,`approved=true` 执行)。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::builtin_tools::fs_safety;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 回收目录名(workdir 下的固定落点;watcher/树均忽略)
pub const TRASH_DIR_NAME: &str = ".evo-trash";

/// `file_delete` 工具
#[derive(Clone)]
pub struct FileDeleteTool {
    workdir: PathBuf,
    writable_dir: PathBuf,
}

impl FileDeleteTool {
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
impl ToolFunction for FileDeleteTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 操作
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("file_delete tool panicked: {e}"))?
    }
}

impl FileDeleteTool {
    fn call_sync(&self, args: &Value) -> IoResult {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: path (string)".to_string())?;
        let approved = args
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let writable_canonical = fs_safety::writable_root(&self.workdir, &self.writable_dir)?;
        let workdir_canonical = self
            .workdir
            .canonicalize()
            .map_err(|e| format!("workdir invalid: {e}"))?;

        // 源:必须已存在且在 writable_dir 内
        let source = fs_safety::resolve_existing(&self.workdir, path)?;
        if !source.starts_with(&writable_canonical) {
            return Err(format!(
                "path '{path}' is outside writable_dir '{}' (path traversal)",
                self.writable_dir.display()
            ));
        }

        // 守护:根 / 可写根 / 回收目录自身不可删
        if source == workdir_canonical {
            return Err("cannot delete the workdir root".to_string());
        }
        if source == writable_canonical {
            return Err("cannot delete the writable root".to_string());
        }
        let trash_dir = self.workdir.join(TRASH_DIR_NAME);
        let trash_canonical = fs_safety::writable_root(&self.workdir, Path::new(TRASH_DIR_NAME))
            .unwrap_or(trash_dir.clone());
        if source == trash_canonical {
            return Err("cannot delete the trash directory itself".to_string());
        }

        let kind = match std::fs::symlink_metadata(&source) {
            Ok(m) if m.is_dir() => "dir",
            Ok(m) if m.file_type().is_symlink() => "symlink",
            _ => "file",
        };

        if !approved {
            return Ok(serde_json::json!({
                "status": "needs_approval",
                "category": "candidate",
                "description": format!("delete {kind} '{path}' (soft-delete to {TRASH_DIR_NAME}/)"),
                "risk": "removes the entry from the project tree (recoverable from the trash folder)",
                "alternative": "delete the entry manually in the workbench file tree",
            }));
        }

        std::fs::create_dir_all(&trash_dir)
            .map_err(|e| format!("failed to create trash dir: {e}"))?;
        let name = source
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .ok_or_else(|| "cannot delete a path without a file name".to_string())?;
        let dest = fs_safety::trash_destination(&trash_dir, &name);

        if let Err(rename_err) = std::fs::rename(&source, &dest) {
            // 跨盘 fallback:递归 copy 进回收目录 + 删源
            fs_safety::copy_recursive(&source, &dest)
                .map_err(|e| format!("soft-delete failed (rename: {rename_err}; fallback: {e})"))?;
            fs_safety::remove_recursive(&source)
                .map_err(|e| format!("soft-delete fallback cleanup failed: {e}"))?;
        }

        let mut map = serde_json::Map::new();
        map.insert(
            "path".to_string(),
            Value::from(source.display().to_string()),
        );
        map.insert(
            "trash_path".to_string(),
            Value::from(dest.display().to_string()),
        );
        map.insert("kind".to_string(), Value::from(kind));
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

    fn call(tool: &FileDeleteTool, body: Value) -> IoResult {
        tool.call_sync(&body)
    }

    #[test]
    fn test_soft_delete_file_lands_in_trash() {
        let (_d, workdir) = temp_workdir();
        std::fs::write(workdir.join("workspace/gone.txt"), b"bye").unwrap();
        let tool = FileDeleteTool::new(workdir.clone());
        let v = call(
            &tool,
            json!({"path": "workspace/gone.txt", "approved": true}),
        )
        .unwrap();
        assert_eq!(v["kind"], json!("file"));
        assert!(!workdir.join("workspace/gone.txt").exists());
        let trash_path = v["trash_path"].as_str().unwrap();
        let trash_path = PathBuf::from(trash_path);
        assert!(trash_path.is_file());
        assert!(trash_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("_gone.txt"));
    }

    #[test]
    fn test_soft_delete_dir_recursive() {
        let (_d, workdir) = temp_workdir();
        std::fs::create_dir_all(workdir.join("workspace/tree/sub")).unwrap();
        std::fs::write(workdir.join("workspace/tree/sub/f.txt"), b"x").unwrap();
        let tool = FileDeleteTool::new(workdir.clone());
        let v = call(&tool, json!({"path": "workspace/tree", "approved": true})).unwrap();
        assert_eq!(v["kind"], json!("dir"));
        assert!(!workdir.join("workspace/tree").exists());
        assert!(PathBuf::from(v["trash_path"].as_str().unwrap()).is_dir());
    }

    #[test]
    fn test_trash_filename_timestamp_structure() {
        let (_d, workdir) = temp_workdir();
        std::fs::write(workdir.join("workspace/a.txt"), b"x").unwrap();
        let tool = FileDeleteTool::new(workdir.clone());
        let v = call(&tool, json!({"path": "workspace/a.txt", "approved": true})).unwrap();
        let fname = PathBuf::from(v["trash_path"].as_str().unwrap())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        // <unix_secs>_<name> 结构
        let (ts, rest) = fname
            .split_once('_')
            .expect("trash name must be <ts>_<name>");
        assert!(ts.parse::<u64>().is_ok(), "timestamp prefix: {fname}");
        assert_eq!(rest, "a.txt");
    }

    #[test]
    fn test_trash_collision_appends_suffix() {
        let (_d, workdir) = temp_workdir();
        let trash = workdir.join(TRASH_DIR_NAME);
        std::fs::create_dir_all(&trash).unwrap();
        // 预占 <ts>_<name> 形态的落点,使下一次删除撞名
        std::fs::write(workdir.join("workspace/a.txt"), b"x").unwrap();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(trash.join(format!("{ts}_a.txt")), b"occupied").unwrap();
        let tool = FileDeleteTool::new(workdir.clone());
        let v = call(&tool, json!({"path": "workspace/a.txt", "approved": true})).unwrap();
        let fname = PathBuf::from(v["trash_path"].as_str().unwrap())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            fname != format!("{ts}_a.txt"),
            "collision must be resolved with a suffix, got {fname}"
        );
        assert!(fname.contains("_a.txt"), "unexpected trash name {fname}");
        assert!(PathBuf::from(v["trash_path"].as_str().unwrap()).is_file());
    }

    #[test]
    fn test_source_missing_404_semantics() {
        let (_d, workdir) = temp_workdir();
        let tool = FileDeleteTool::new(workdir.clone());
        let err = call(
            &tool,
            json!({"path": "workspace/ghost.txt", "approved": true}),
        )
        .unwrap_err();
        assert!(err.contains("does not exist"), "got: {err}");
    }

    #[test]
    fn test_outside_writable_dir_rejected() {
        let (_d, workdir) = temp_workdir();
        std::fs::write(workdir.join("root.txt"), b"x").unwrap();
        let tool = FileDeleteTool::new(workdir.clone());
        let err = call(&tool, json!({"path": "root.txt", "approved": true})).unwrap_err();
        assert!(err.contains("outside writable_dir"), "got: {err}");
    }

    #[test]
    fn test_root_and_trash_guarded() {
        let (_d, workdir) = temp_workdir();
        std::fs::create_dir_all(workdir.join(TRASH_DIR_NAME)).unwrap();
        let tool = FileDeleteTool::new(workdir.clone()).with_writable_dir(".");
        let e1 = call(&tool, json!({"path": ".", "approved": true})).unwrap_err();
        assert!(e1.contains("cannot delete the workdir root"), "got: {e1}");
        let e2 = call(&tool, json!({"path": ".evo-trash", "approved": true})).unwrap_err();
        assert!(
            e2.contains("cannot delete the trash directory"),
            "got: {e2}"
        );
    }

    #[test]
    fn test_no_approval_returns_proposal() {
        let (_d, workdir) = temp_workdir();
        std::fs::write(workdir.join("workspace/a.txt"), b"x").unwrap();
        let tool = FileDeleteTool::new(workdir.clone());
        let v = call(&tool, json!({"path": "workspace/a.txt"})).unwrap();
        assert_eq!(v["status"], json!("needs_approval"));
        assert!(workdir.join("workspace/a.txt").is_file());
        assert!(
            !workdir.join(TRASH_DIR_NAME).exists(),
            "proposal 不得创建回收目录"
        );
    }

    #[test]
    fn test_missing_path_arg() {
        let (_d, workdir) = temp_workdir();
        let tool = FileDeleteTool::new(workdir.clone());
        assert!(call(&tool, json!({"approved": true})).is_err());
    }
}
