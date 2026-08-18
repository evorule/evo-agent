// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `search_files` —— 按 glob pattern 找文件(工作目录沙箱)
//!
//! ## 安全模型
//! - 路径**相对** workdir(绝对路径直接拒)
//! - 拒绝 `..` 段
//! - canonicalize 后必须仍在 workdir 内
//! - 跳过隐藏文件(`.` 开头)如 `.git/`、`.DS_Store`
//! - max_results 限制(防 OOM)
//!
//! ## pattern 语法(简化 glob)
//! - `*` —— 匹配任意数量字符(不含 `/`)
//! - `?` —— 匹配任意单个字符
//! - 字面字符 —— 精确匹配
//!
//! 不支持:字符集 `[abc]`、`{a,b}` 等。

use std::path::{Component, Path, PathBuf};

use evorule_tcb::JsonValue;

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 默认最大结果数
pub const DEFAULT_MAX_RESULTS: usize = 1000;

/// `search_files` 工具
#[derive(Clone)]
pub struct SearchFilesTool {
    workdir: PathBuf,
    max_results: usize,
}

impl SearchFilesTool {
    /// TODO: doc
    pub fn new(workdir: PathBuf) -> Self {
        Self {
            workdir,
            max_results: DEFAULT_MAX_RESULTS,
        }
    }

    /// TODO: doc
    pub fn with_max_results(mut self, n: usize) -> Self {
        self.max_results = n;
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

    /// 简单的 glob 匹配(支持 `*` 和 `?`)
    ///
    /// 算法:递归,每次处理第一个 `*` 或 `?`,把问题缩小。
    pub fn glob_match(pattern: &str, name: &str) -> bool {
        if pattern.is_empty() {
            return name.is_empty();
        }

        // 字面字符(无 metacharacter) → 精确匹配
        if !pattern.contains('*') && !pattern.contains('?') {
            return pattern == name;
        }

        // 找第一个 metacharacter
        let first_star = pattern.find('*');
        let first_q = pattern.find('?');

        let (pos, is_star) = match (first_star, first_q) {
            (Some(s), Some(q)) if s < q => (s, true),
            (_, Some(q)) => (q, false),
            (Some(s), None) => (s, true),
            (None, None) => unreachable!(),
        };

        let prefix = &pattern[..pos];
        let rest = &pattern[pos + 1..];

        if !name.starts_with(prefix) {
            return false;
        }
        let after_prefix = &name[prefix.len()..];

        if is_star {
            // `*` 匹配 0+ 个字符,尝试每个位置
            for i in 0..=after_prefix.len() {
                // 注意:要按 char 边界切,不能按 byte(中文/UTF-8 会断)
                if Self::glob_match_safe(rest, &after_prefix[i..]) {
                    return true;
                }
            }
            false
        } else {
            // `?` 匹配恰好 1 个字符(一个 char,不是 1 个 byte)
            let mut chars = after_prefix.chars();
            if chars.next().is_none() {
                return false;
            }
            let after_q: String = chars.collect();
            Self::glob_match_safe(rest, &after_q)
        }
    }

    /// 内部:在 char 边界上切 after_prefix,然后递归
    fn glob_match_safe(pattern: &str, name: &str) -> bool {
        Self::glob_match(pattern, name)
    }

    fn walk(root: &Path, dir: &Path, pattern: &str, max: usize, results: &mut Vec<PathBuf>) {
        if results.len() >= max {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            if results.len() >= max {
                return;
            }
            let path = entry.path();
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n,
                None => continue,
            };
            // 跳过隐藏文件 / 目录(.git, .DS_Store, .vscode 等)
            if name.starts_with('.') {
                continue;
            }
            if Self::glob_match(pattern, name) {
                if let Ok(rel) = path.strip_prefix(root) {
                    results.push(rel.to_path_buf());
                }
            }
            if path.is_dir() {
                Self::walk(root, &path, pattern, max, results);
            }
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for SearchFilesTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 操作
    async fn call(&self, args: &JsonValue) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("search_files tool panicked: {}", e))?
    }
}

impl SearchFilesTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &JsonValue) -> IoResult {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: pattern (string)".to_string())?;

        let dir = args.get("dir").and_then(|v| v.as_str()).unwrap_or(".");

        let max = args
            .get("max_results")
            .and_then(|v| v.as_i64())
            .map(|n| n.max(0) as usize)
            .unwrap_or(self.max_results);

        if max == 0 {
            return Err("max_results must be > 0".to_string());
        }

        let safe_dir = self.resolve_safe_dir(dir)?;

        let mut results = Vec::new();
        Self::walk(&safe_dir, &safe_dir, pattern, max, &mut results);

        let json_results: Vec<JsonValue> = results
            .iter()
            .map(|p| JsonValue::string(p.display().to_string()))
            .collect();

        let truncated = results.len() >= max;
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "pattern".to_string(),
            JsonValue::string(pattern.to_string()),
        );
        map.insert(
            "dir".to_string(),
            JsonValue::string(safe_dir.display().to_string()),
        );
        map.insert(
            "count".to_string(),
            JsonValue::Integer(results.len() as i64),
        );
        map.insert("truncated".to_string(), JsonValue::Bool(truncated));
        map.insert("results".to_string(), JsonValue::array(json_results));

        Ok(JsonValue::object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // === glob_match 单元测试(纯函数) ===

    #[test]
    fn test_glob_match_literal() {
        assert!(SearchFilesTool::glob_match("hello.txt", "hello.txt"));
        assert!(!SearchFilesTool::glob_match("hello.txt", "world.txt"));
        assert!(!SearchFilesTool::glob_match("hello.txt", "hello.tx"));
    }

    #[test]
    fn test_glob_match_star() {
        assert!(SearchFilesTool::glob_match("*", "anything"));
        assert!(SearchFilesTool::glob_match("*.txt", "hello.txt"));
        assert!(SearchFilesTool::glob_match("*.txt", "a.txt"));
        assert!(!SearchFilesTool::glob_match("*.txt", "hello.md"));
        assert!(SearchFilesTool::glob_match("hello*", "hello.txt"));
        assert!(SearchFilesTool::glob_match("hello*", "hello"));
        assert!(SearchFilesTool::glob_match("a*b", "ab"));
        assert!(SearchFilesTool::glob_match("a*b", "axxxb"));
        assert!(!SearchFilesTool::glob_match("a*b", "axxx")); // 没有 trailing b
        assert!(!SearchFilesTool::glob_match("a*b", "yxxx"));
        assert!(SearchFilesTool::glob_match("a*c", "abc"));
        assert!(SearchFilesTool::glob_match("*.test.*", "main.test.rs"));
    }

    #[test]
    fn test_glob_match_question() {
        assert!(SearchFilesTool::glob_match("?", "a"));
        assert!(SearchFilesTool::glob_match("?", "Z"));
        assert!(!SearchFilesTool::glob_match("?", ""));
        assert!(!SearchFilesTool::glob_match("?", "ab"));
        assert!(SearchFilesTool::glob_match("a?c", "abc"));
        assert!(SearchFilesTool::glob_match("a?c", "axc"));
        assert!(!SearchFilesTool::glob_match("a?c", "ac"));
    }

    #[test]
    fn test_glob_match_combined() {
        assert!(SearchFilesTool::glob_match("*.t?t", "test.txt"));
        assert!(SearchFilesTool::glob_match("h*o", "hello"));
        assert!(!SearchFilesTool::glob_match("h*o", "hell"));
    }

    // === 安全检查测试 ===

    #[test]
    fn test_reject_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = SearchFilesTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("pattern".to_string(), JsonValue::string("*.txt"));
            m.insert("dir".to_string(), JsonValue::string("C:\\Windows"));
            m
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_reject_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tool = SearchFilesTool::new(dir.path().to_path_buf());
        let result = tool.call_sync(&JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("pattern".to_string(), JsonValue::string("*"));
            m.insert("dir".to_string(), JsonValue::string("../etc"));
            m
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_walk_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo.txt"), b"").unwrap();
        std::fs::write(dir.path().join("bar.md"), b"").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/baz.txt"), b"").unwrap();

        let tool = SearchFilesTool::new(dir.path().to_path_buf());
        let result = tool
            .call_sync(&JsonValue::object({
                let mut m = std::collections::BTreeMap::new();
                m.insert("pattern".to_string(), JsonValue::string("*.txt"));
                m
            }))
            .expect("should succeed");

        let count = result.get("count").unwrap().as_i64().unwrap();
        assert_eq!(
            count, 2,
            "should find foo.txt and sub/baz.txt, got: {:?}",
            result
        );
    }

    #[test]
    fn test_walk_skips_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.txt"), b"").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/secret.txt"), b"").unwrap();

        let tool = SearchFilesTool::new(dir.path().to_path_buf());
        let result = tool
            .call_sync(&JsonValue::object({
                let mut m = std::collections::BTreeMap::new();
                m.insert("pattern".to_string(), JsonValue::string("*.txt"));
                m
            }))
            .unwrap();

        let count = result.get("count").unwrap().as_i64().unwrap();
        assert_eq!(
            count, 1,
            "should only find visible.txt, not .git/secret.txt"
        );
    }

    #[test]
    fn test_respects_max_results() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{}.txt", i)), b"").unwrap();
        }
        let tool = SearchFilesTool::new(dir.path().to_path_buf()).with_max_results(3);
        let result = tool
            .call_sync(&JsonValue::object({
                let mut m = std::collections::BTreeMap::new();
                m.insert("pattern".to_string(), JsonValue::string("*.txt"));
                m
            }))
            .unwrap();
        let count = result.get("count").unwrap().as_i64().unwrap();
        let truncated = result.get("truncated").unwrap().as_bool().unwrap();
        assert_eq!(count, 3);
        assert!(truncated);
    }
}
