// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `file_read` —— 读取文件内容(工作目录沙箱 + size limit)
//!
//! ## 安全模型
//! - 路径必须**相对** workdir(绝对路径直接拒)
//! - 拒绝 `..` 路径段(防止逃出沙箱)
//! - canonicalize 后必须仍在 workdir 内(防 symlink 攻击)
//! - 文件大小限制(默认 10 MB,可通过 `max_bytes` 调整)
//! - 只读常规文件(拒绝目录、socket、设备等)

use std::path::{Component, Path, PathBuf};

use evorule_tcb::JsonValue;

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 默认最大文件大小(10 MB)
pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// `file_read` 工具
///
/// 通过 `new()` 构造时绑定工作目录,实例化后**只能读取 workdir 内的文件**。
#[derive(Clone)]
pub struct FileReadTool {
    workdir: PathBuf,
    max_bytes: u64,
}

impl FileReadTool {
    /// 创建绑定到 `workdir` 的 file_read 工具
    pub fn new(workdir: PathBuf) -> Self {
        Self {
            workdir,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// 设置最大文件大小(字节)
    pub fn with_max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = n;
        self
    }

    /// 解析并校验路径,确保在工作目录内
    fn resolve_safe_path(&self, raw: &str) -> Result<PathBuf, String> {
        let path = Path::new(raw);

        // 拒绝绝对路径
        if path.is_absolute() {
            return Err(format!("absolute path not allowed: '{}'", raw));
        }

        // 拒绝 `..` 段
        for component in path.components() {
            if matches!(component, Component::ParentDir) {
                return Err(format!(
                    "parent dir (..) not allowed: '{}' (must stay within workdir)",
                    raw
                ));
            }
        }

        let joined = self.workdir.join(path);

        // canonicalize 解析 symlink 和 . / ..
        let canonical = joined
            .canonicalize()
            .map_err(|e| format!("path does not exist or cannot resolve: {}", e))?;

        // 必须在 workdir 内
        let workdir_canonical = self
            .workdir
            .canonicalize()
            .map_err(|e| format!("workdir invalid: {}", e))?;

        if !canonical.starts_with(&workdir_canonical) {
            return Err(format!(
                "path escapes workdir: '{}' resolves outside sandbox",
                raw
            ));
        }

        Ok(canonical)
    }
}

#[async_trait::async_trait]
impl ToolFunction for FileReadTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 操作
    async fn call(&self, args: &JsonValue) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("file_read tool panicked: {}", e))?
    }
}

impl FileReadTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &JsonValue) -> IoResult {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: path (string)".to_string())?;

        let safe_path = self.resolve_safe_path(path)?;

        let metadata = std::fs::metadata(&safe_path).map_err(|e| format!("stat failed: {}", e))?;

        if !metadata.is_file() {
            return Err(format!("not a regular file: '{}'", path));
        }

        if metadata.len() > self.max_bytes {
            return Err(format!(
                "file too large: {} bytes (max {} bytes); use head/tail/grep on the agent side",
                metadata.len(),
                self.max_bytes
            ));
        }

        let content = std::fs::read_to_string(&safe_path)
            .map_err(|e| format!("read failed (binary or permission?): {}", e))?;

        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "path".to_string(),
            JsonValue::string(safe_path.display().to_string()),
        );
        map.insert(
            "size".to_string(),
            JsonValue::Integer(metadata.len() as i64),
        );
        map.insert("content".to_string(), JsonValue::string(content));
        Ok(JsonValue::object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // canonicalize 后路径才稳定(Windows 短/长文件名差异)
        let _ = dir.path().canonicalize();
        dir
    }

    fn make_tool(dir: &Path) -> FileReadTool {
        FileReadTool::new(dir.to_path_buf())
    }

    #[test]
    fn test_read_existing_file() {
        let dir = temp_workdir();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, b"hello, world").unwrap();

        let tool = make_tool(dir.path());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("hello.txt"));
            m
        }));
        assert!(result.is_ok());
        let v = result.unwrap();
        assert_eq!(v.get("content").unwrap().as_str().unwrap(), "hello, world");
        assert_eq!(v.get("size").unwrap().as_i64().unwrap(), 12);
    }

    #[test]
    fn test_reject_absolute_path() {
        let dir = temp_workdir();
        let tool = make_tool(dir.path());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert(
                "path".to_string(),
                JsonValue::string("C:\\Windows\\System32"),
            );
            m
        }));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("absolute path not allowed"), "got: {}", err);
    }

    #[test]
    fn test_reject_parent_dir_traversal() {
        let dir = temp_workdir();
        let tool = make_tool(dir.path());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("../etc/passwd"));
            m
        }));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("parent dir (..) not allowed"), "got: {}", err);
    }

    #[test]
    fn test_reject_symlink_escape() {
        let dir = temp_workdir();
        // 在 workdir 外创建一个文件,然后用 symlink 指向它
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("secret.txt");
        std::fs::write(&outside_file, b"secret content").unwrap();
        let link_path = dir.path().join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_file, &link_path).unwrap();
        #[cfg(windows)]
        {
            // Windows 创建符号链接需要 SeCreateSymbolicLinkPrivilege 特权
            // （管理员或开发者模式），无特权环境错误码 1314，直接跳过
            match std::os::windows::fs::symlink_file(&outside_file, &link_path) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(1314) => {
                    eprintln!("skip: no symlink privilege on this Windows env");
                    return;
                }
                Err(e) => panic!("unexpected symlink error: {e}"),
            }
        }

        let tool = make_tool(dir.path());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("link.txt"));
            m
        }));
        assert!(result.is_err(), "symlink escape should be rejected");
    }

    #[test]
    fn test_missing_path_arg() {
        let dir = temp_workdir();
        let tool = make_tool(dir.path());
        let result = tool.call_sync(&JsonValue::object(std::collections::BTreeMap::new()));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing required arg"));
    }

    #[test]
    fn test_reject_nonexistent() {
        let dir = temp_workdir();
        let tool = make_tool(dir.path());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("does_not_exist.txt"));
            m
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_reject_file_too_large() {
        let dir = temp_workdir();
        let big = dir.path().join("big.txt");
        // 写一个超大的文件(只是 metadata,稀疏文件)
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(20 * 1024 * 1024).unwrap(); // 20 MB
        drop(f);

        let tool = make_tool(dir.path()).with_max_bytes(1024 * 1024); // 1 MB limit
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("big.txt"));
            m
        }));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("too large"), "got: {}", err);
    }

    #[test]
    fn test_reject_directory() {
        let dir = temp_workdir();
        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        let tool = make_tool(dir.path());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("subdir"));
            m
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not a regular file"));
    }
}
