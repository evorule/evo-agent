// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `file_write` —— 写文件(工作目录沙箱 + 写入路径白名单 + overwrite 保护)
//!
//! ## 安全模型
//!
//! 这是最危险的工具(写磁盘)。多层防护:
//!
//! 1. **工作目录沙箱**(同 file_read):绝对路径/`..`/symlink escape 一律拒
//! 2. **写入路径白名单**:**只能**写 `writable_dir` 子目录(默认 `./workspace/`)
//!    防止 agent 误写源码 / 配置 / `.git/` 等
//! 3. **Overwrite 保护**:写已存在文件**必须**带 `overwrite=true`,否则拒
//! 4. **Size limit**:单次内容最大 1 MB(防止 OOM + 防 agent 写巨大文件)
//! 5. **自动创建父目录**:`create_parents=true` 才创建(默认 false)
//!
//! ## 使用模式
//!
//! - 安全默认:只写 `./workspace/`,已存在文件需 opt-in 覆盖
//! - 高级用户:可改 `writable_dir` 到 workdir 根(不推荐,慎用)
//! - 自动化:CI 环境可设 `overwrite=true`(环境可控,人为审查由 CI 流程保证)

use std::path::{Component, Path, PathBuf};

use evorule_tcb::JsonValue;

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 默认写入根目录(相对 workdir)
pub const DEFAULT_WRITABLE_DIR: &str = "workspace";

/// 默认单次内容最大字节数
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024; // 1 MB

/// `file_write` 工具
#[derive(Clone)]
pub struct FileWriteTool {
    workdir: PathBuf,
    writable_dir: PathBuf, // workdir 下的子目录
    max_bytes: u64,
}

impl FileWriteTool {
    /// TODO: doc
    pub fn new(workdir: PathBuf) -> Self {
        Self {
            workdir,
            writable_dir: PathBuf::from(DEFAULT_WRITABLE_DIR),
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// 设置可写子目录(相对 workdir)
    ///
    /// 默认 `./workspace/`。改成 `"."` 表示整个 workdir 都能写(慎用)。
    pub fn with_writable_dir(mut self, dir: &str) -> Self {
        self.writable_dir = PathBuf::from(dir);
        self
    }

    /// TODO: doc
    pub fn with_max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = n;
        self
    }

    /// 解析 + 校验:必须**在 writable_dir 内**
    ///
    /// 返回 `(target_path, canonical_or_logical_path)`:
    /// - 存在路径:返回 canonical(用于 symlink 校验)
    /// - 不存在路径:返回 logical target + writable_canonical(用于 containment 校验)
    fn resolve_safe_path(&self, raw: &str) -> Result<(PathBuf, PathBuf), String> {
        let path = Path::new(raw);
        if path.is_absolute() {
            return Err(format!("absolute path not allowed: '{}'", raw));
        }
        for component in path.components() {
            if matches!(component, Component::ParentDir) {
                return Err(format!(
                    "parent dir (..) not allowed: '{}' (must stay within writable_dir)",
                    raw
                ));
            }
        }

        // 1. workdir 必须是已存在的(整个工具的前提)
        let _workdir_canonical = self
            .workdir
            .canonicalize()
            .map_err(|e| format!("workdir invalid: {}", e))?;

        // 2. writable_dir 也必须存在(否则无法写入)
        let writable_abs = self.workdir.join(&self.writable_dir);
        let writable_canonical = writable_abs.canonicalize().map_err(|e| {
            format!(
                "writable_dir does not exist: {} ({})",
                writable_abs.display(),
                e
            )
        })?;

        // 3. 目标路径 = workdir + path(逻辑路径,不一定存在)
        let target = self.workdir.join(path);

        // 4. containment check(逻辑路径)
        if !target.starts_with(&writable_abs) {
            return Err(format!(
                "path '{}' is outside writable_dir '{}' (path traversal)",
                raw,
                self.writable_dir.display()
            ));
        }
        if !target.starts_with(&self.workdir) {
            return Err(format!("path escapes workdir: '{}'", raw));
        }

        // 5. 存在路径做 symlink 检查(canonical 必须仍在 writable_dir 内)
        let target_canonical = if target.exists() {
            let c = target
                .canonicalize()
                .map_err(|e| format!("path cannot be resolved: {}", e))?;
            // 再 check 一次(symlink 可能跳出)
            if !c.starts_with(&writable_canonical) {
                return Err(format!(
                    "path '{}' resolves outside writable_dir (symlink escape)",
                    raw
                ));
            }
            c
        } else {
            // 不存在:不 canonicalize(用 logical target 就够)
            target.clone()
        };

        Ok((target, target_canonical))
    }
}

#[async_trait::async_trait]
impl ToolFunction for FileWriteTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 操作
    async fn call(&self, args: &JsonValue) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("file_write tool panicked: {}", e))?
    }
}

impl FileWriteTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &JsonValue) -> IoResult {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: path (string)".to_string())?;

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: content (string)".to_string())?;

        let overwrite = args
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let create_parents = args
            .get("create_parents")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Size limit
        if content.len() as u64 > self.max_bytes {
            return Err(format!(
                "content too large: {} bytes (max {} bytes)",
                content.len(),
                self.max_bytes
            ));
        }

        // 路径安全
        let (target, target_canonical) = self.resolve_safe_path(path)?;

        // Overwrite 保护
        if target_canonical.exists() {
            if !overwrite {
                return Err(format!(
                    "file already exists: '{}'; pass overwrite=true to replace (destructive!)",
                    path
                ));
            }
        } else {
            // 父目录存在?
            if let Some(parent) = target.parent() {
                if !parent.exists() {
                    if !create_parents {
                        return Err(format!(
                            "parent dir does not exist: '{}'; pass create_parents=true to create",
                            parent.display()
                        ));
                    }
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("failed to create parent dir: {}", e))?;
                }
            }
        }

        // 写入
        std::fs::write(&target, content.as_bytes()).map_err(|e| format!("write failed: {}", e))?;
        let bytes_written = content.len();

        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "path".to_string(),
            JsonValue::string(target_canonical.display().to_string()),
        );
        map.insert(
            "bytes_written".to_string(),
            JsonValue::Integer(bytes_written as i64),
        );
        map.insert("created".to_string(), JsonValue::Bool(!overwrite));
        map.insert(
            "writable_dir".to_string(),
            JsonValue::string(self.writable_dir.display().to_string()),
        );
        Ok(JsonValue::object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workdir_with_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        // canonicalize 一下让路径稳定
        let _ = workspace.canonicalize();
        dir
    }

    #[test]
    fn test_reject_absolute_path() {
        let dir = temp_workdir_with_workspace();
        let tool = FileWriteTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("C:\\evil.txt"));
            m.insert("content".to_string(), JsonValue::string("x"));
            m
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_reject_parent_dir() {
        let dir = temp_workdir_with_workspace();
        let tool = FileWriteTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("../evil.txt"));
            m.insert("content".to_string(), JsonValue::string("x"));
            m
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_reject_writing_outside_writable_dir() {
        let dir = temp_workdir_with_workspace();
        // 想写 workdir 根(不在 workspace/ 里)
        std::fs::write(dir.path().join("config.toml"), b"").unwrap();
        let tool = FileWriteTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("config.toml"));
            m.insert("content".to_string(), JsonValue::string("x"));
            m
        }));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("outside writable_dir") || err.contains("path escapes"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_write_new_file_in_workspace() {
        let dir = temp_workdir_with_workspace();
        let tool = FileWriteTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("workspace/new.txt"));
            m.insert("content".to_string(), JsonValue::string("hello"));
            m
        }));
        assert!(result.is_ok(), "got: {:?}", result);
        // 验证文件真创建了
        let written = std::fs::read_to_string(dir.path().join("workspace/new.txt")).unwrap();
        assert_eq!(written, "hello");
    }

    #[test]
    fn test_reject_overwrite_without_flag() {
        let dir = temp_workdir_with_workspace();
        // 先创建一个文件
        std::fs::write(dir.path().join("workspace/exists.txt"), b"old").unwrap();
        let tool = FileWriteTool::new(dir.path().to_path_buf());
        // 试图覆盖(没带 overwrite=true)
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert(
                "path".to_string(),
                JsonValue::string("workspace/exists.txt"),
            );
            m.insert("content".to_string(), JsonValue::string("new"));
            m
        }));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("already exists") || err.contains("overwrite=true"),
            "got: {}",
            err
        );
        // 内容应未变
        let content = std::fs::read_to_string(dir.path().join("workspace/exists.txt")).unwrap();
        assert_eq!(content, "old");
    }

    #[test]
    fn test_overwrite_with_flag() {
        let dir = temp_workdir_with_workspace();
        std::fs::write(dir.path().join("workspace/x.txt"), b"old").unwrap();
        let tool = FileWriteTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("workspace/x.txt"));
            m.insert("content".to_string(), JsonValue::string("new content"));
            m.insert("overwrite".to_string(), JsonValue::Bool(true));
            m
        }));
        assert!(result.is_ok());
        let content = std::fs::read_to_string(dir.path().join("workspace/x.txt")).unwrap();
        assert_eq!(content, "new content");
    }

    #[test]
    fn test_create_parents() {
        let dir = temp_workdir_with_workspace();
        let tool = FileWriteTool::new(dir.path().to_path_buf());
        // 写 workspace/sub/deep/file.txt,父目录不存在
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert(
                "path".to_string(),
                JsonValue::string("workspace/sub/deep/file.txt"),
            );
            m.insert("content".to_string(), JsonValue::string("x"));
            m.insert("create_parents".to_string(), JsonValue::Bool(true));
            m
        }));
        assert!(result.is_ok(), "got: {:?}", result);
    }

    #[test]
    fn test_no_create_parents_fails() {
        let dir = temp_workdir_with_workspace();
        let tool = FileWriteTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert(
                "path".to_string(),
                JsonValue::string("workspace/sub/deep/file.txt"),
            );
            m.insert("content".to_string(), JsonValue::string("x"));
            // 没有 create_parents
            m
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_content_too_large() {
        let dir = temp_workdir_with_workspace();
        let tool = FileWriteTool::new(dir.path().to_path_buf()).with_max_bytes(100);
        let big = "x".repeat(200);
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("workspace/big.txt"));
            m.insert("content".to_string(), JsonValue::string(big));
            m
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too large"));
    }

    #[test]
    fn test_missing_args() {
        let dir = temp_workdir_with_workspace();
        let tool = FileWriteTool::new(dir.path().to_path_buf());
        // no path
        let r1 = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("content".to_string(), JsonValue::string("x"));
            m
        }));
        assert!(r1.is_err());
        // no content
        let r2 = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("workspace/x"));
            m
        }));
        assert!(r2.is_err());
    }

    #[test]
    fn test_reject_symlink_escape_via_writable() {
        let dir = temp_workdir_with_workspace();
        // 在 workspace 内放一个 symlink 指向外面
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, b"secret").unwrap();
        let link_path = dir.path().join("workspace").join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_file, &link_path).unwrap();
        #[cfg(windows)]
        {
            // Windows 创建符号链接需要特权（错误码 1314），无特权环境跳过
            match std::os::windows::fs::symlink_file(&outside_file, &link_path) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(1314) => {
                    eprintln!("skip: no symlink privilege on this Windows env");
                    return;
                }
                Err(e) => panic!("unexpected symlink error: {e}"),
            }
        }

        let tool = FileWriteTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("workspace/link.txt"));
            m.insert("content".to_string(), JsonValue::string("overwrite"));
            m
        }));
        // symlink 通过 exists() 解析后指向 workdir 外
        // canonicalize 会解析 symlink → 目标在 outside
        // starts_with(writable_dir) 应为 false → reject
        assert!(result.is_err());
    }
}
