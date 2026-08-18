// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `file_list` —— 列出目录内容(工作目录沙箱)
//!
//! ## 安全模型
//! - 路径**相对** workdir,绝对路径拒
//! - 拒绝 `..` 段
//! - canonicalize 后必须仍在 workdir 内
//! - 默认跳过隐藏文件(`.` 开头) — 可选 `include_hidden=true` 显示
//! - 结果按名字排序
//! - max_entries 限制(防 OOM)

use std::path::{Component, Path, PathBuf};

use evorule_tcb::JsonValue;

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 默认最大结果数
pub const DEFAULT_MAX_ENTRIES: usize = 1000;

/// `file_list` 工具
#[derive(Clone)]
pub struct FileListTool {
    workdir: PathBuf,
    max_entries: usize,
}

impl FileListTool {
    /// TODO: doc
    pub fn new(workdir: PathBuf) -> Self {
        Self {
            workdir,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    /// TODO: doc
    pub fn with_max_entries(mut self, n: usize) -> Self {
        self.max_entries = n;
        self
    }

    fn resolve_safe_dir(&self, raw: &str) -> Result<PathBuf, String> {
        let path = Path::new(raw);
        if path.is_absolute() {
            return Err(format!("absolute path not allowed: '{}'", raw));
        }
        for component in path.components() {
            if matches!(component, Component::ParentDir) {
                return Err(format!("parent dir (..) not allowed: '{}'", raw));
            }
        }
        let joined = self.workdir.join(path);
        let canonical = joined
            .canonicalize()
            .map_err(|e| format!("dir does not exist or cannot resolve: {}", e))?;
        let workdir_canonical = self
            .workdir
            .canonicalize()
            .map_err(|e| format!("workdir invalid: {}", e))?;
        if !canonical.starts_with(&workdir_canonical) {
            return Err(format!("path escapes workdir: '{}'", raw));
        }
        Ok(canonical)
    }
}

#[async_trait::async_trait]
impl ToolFunction for FileListTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 操作
    async fn call(&self, args: &JsonValue) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("file_list tool panicked: {}", e))?
    }
}

impl FileListTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &JsonValue) -> IoResult {
        let dir = args.get("dir").and_then(|v| v.as_str()).unwrap_or(".");

        let include_hidden = args
            .get("include_hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let max = args
            .get("max_entries")
            .and_then(|v| v.as_i64())
            .map(|n| n.max(0) as usize)
            .unwrap_or(self.max_entries);

        if max == 0 {
            return Err("max_entries must be > 0".to_string());
        }

        let safe_dir = self.resolve_safe_dir(dir)?;

        let entries =
            std::fs::read_dir(&safe_dir).map_err(|e| format!("read_dir failed: {}", e))?;

        let mut items: Vec<JsonValue> = Vec::new();
        let mut total_seen = 0;
        for entry in entries.flatten() {
            total_seen += 1;
            if items.len() >= max {
                break;
            }
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            // 跳过隐藏文件(除非显式 include_hidden)
            if !include_hidden && name.starts_with('.') {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let kind = if file_type.is_dir() {
                "dir"
            } else if file_type.is_file() {
                "file"
            } else if file_type.is_symlink() {
                "symlink"
            } else {
                "other"
            };
            let size = entry.metadata().ok().map(|m| m.len() as i64);

            let mut item = std::collections::BTreeMap::new();
            item.insert("name".to_string(), JsonValue::string(name));
            item.insert("kind".to_string(), JsonValue::string(kind.to_string()));
            if let Some(s) = size {
                item.insert("size".to_string(), JsonValue::Integer(s));
            }
            items.push(JsonValue::object(item));
        }

        // 排序(按名字)
        items.sort_by(|a, b| {
            let na = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let nb = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            na.cmp(nb)
        });

        let truncated = total_seen > items.len();
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "dir".to_string(),
            JsonValue::string(safe_dir.display().to_string()),
        );
        map.insert("count".to_string(), JsonValue::Integer(items.len() as i64));
        map.insert("truncated".to_string(), JsonValue::Bool(truncated));
        map.insert("entries".to_string(), JsonValue::array(items));

        Ok(JsonValue::object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reject_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileListTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("dir".to_string(), JsonValue::string("C:\\Windows"));
            m
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_reject_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileListTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("dir".to_string(), JsonValue::string("../etc"));
            m
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_list_skips_hidden_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.txt"), b"x").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        let tool = FileListTool::new(dir.path().to_path_buf());
        let result = tool
            .call_sync(&JsonValue::object(Default::default()))
            .unwrap();
        let entries = result.get("entries").unwrap();
        let count = entries.as_array().map(|a| a.len()).unwrap_or(0);
        assert_eq!(count, 1, "should only show visible.txt");
    }

    #[test]
    fn test_list_includes_hidden_when_requested() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.txt"), b"x").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        let tool = FileListTool::new(dir.path().to_path_buf());
        let result = tool
            .call_sync(&JsonValue::object({
                let mut m = std::collections::BTreeMap::new();
                m.insert("include_hidden".to_string(), JsonValue::Bool(true));
                m
            }))
            .unwrap();
        let count = result.get("count").unwrap().as_i64().unwrap();
        assert_eq!(count, 2, "should show both visible.txt and .git");
    }

    #[test]
    fn test_list_distinguishes_file_and_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let tool = FileListTool::new(dir.path().to_path_buf());
        let result = tool
            .call_sync(&JsonValue::object(Default::default()))
            .unwrap();
        let entries = result.get("entries").unwrap().as_array().unwrap();
        let mut by_name: std::collections::HashMap<String, String> = Default::default();
        for e in entries {
            let name = e.get("name").unwrap().as_str().unwrap().to_string();
            let kind = e.get("kind").unwrap().as_str().unwrap().to_string();
            by_name.insert(name, kind);
        }
        assert_eq!(by_name.get("a.txt").map(|s| s.as_str()), Some("file"));
        assert_eq!(by_name.get("subdir").map(|s| s.as_str()), Some("dir"));
    }

    #[test]
    fn test_list_nonexistent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileListTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("dir".to_string(), JsonValue::string("does_not_exist"));
            m
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_max_entries_limit() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{}.txt", i)), b"x").unwrap();
        }
        let tool = FileListTool::new(dir.path().to_path_buf()).with_max_entries(3);
        let result = tool
            .call_sync(&JsonValue::object(Default::default()))
            .unwrap();
        let count = result.get("count").unwrap().as_i64().unwrap();
        let truncated = result.get("truncated").unwrap().as_bool().unwrap();
        assert_eq!(count, 3);
        assert!(truncated);
    }
}
