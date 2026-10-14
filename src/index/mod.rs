// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 代码库语义索引（B14，v1 结构面）—— 对工作区源码建符号图谱并提供统一
//! 检索入口 `/api/index/*`（search/status/rebuild，随 PR-3 挂载）。
//!
//! 模块五件（15 号 §3.2，与 [`crate::mcp`] 独立子系统先例同款组织）：
//! - [`store`]：SQLite 持久层（files/symbols/refs，单写者纪律）；
//! - [`chunker`]：工作区遍历（双层 ignore）+ 内容哈希（PR-2 加 tree-sitter
//!   符号抽取）；
//! - `graph`：PageRank 图谱检索（PR-3）；
//! - `search_api`：HTTP 处理器（PR-3）。
//!
//! 治理面（15 号 §四）：serve 本地后台行为，无 agent 工具执行、无 shell/写
//! 工作区动作——无审批接点；落盘只存哈希+符号+元数据（D8，源码明文不落索引
//! 库）；v1 零外发；`index.enabled` 设置键（B6 schema 单源注册）关闭时
//! `/api/index/*` 返回 503（随 PR-4 接入）。
//!
//! 索引库为可丢弃派生缓存（`rebuild` 随时重建），不是第二真相源。

pub mod chunker;
pub mod symbols;
pub mod store;

use std::path::{Path, PathBuf};

use crate::index::store::IndexError;

pub use chunker::{collect_workspace_files, FileEntry};
pub use store::{FileHash, IndexStore, RefRow, SymbolRow};

/// 索引落盘目录：`<用户主目录>/.evo-agent/index/<workspace-hash16>/`
///
/// workspace 哈希 = workdir 规范化路径 blake3 前 16 hex——同一工作区跨启动
/// 稳定，不同工作区互不串库。
pub fn index_dir(workdir: &Path) -> Result<PathBuf, IndexError> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| IndexError("user home dir not found (USERPROFILE/HOME)".into()))?;
    let canonical = workdir
        .canonicalize()
        .map_err(|e| IndexError(format!("canonicalize workdir: {e}")))?;
    let hex = blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex();
    Ok(PathBuf::from(home)
        .join(".evo-agent")
        .join("index")
        .join(&hex[..16]))
}

/// 索引库文件路径：[`index_dir`] 下的 `index.db`
pub fn index_db_path(workdir: &Path) -> Result<PathBuf, IndexError> {
    Ok(index_dir(workdir)?.join("index.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_db_path_is_scoped_by_workspace_hash() {
        let td = tempfile::tempdir().unwrap();
        let a = index_db_path(td.path()).unwrap();
        assert!(a.ends_with("index.db"));
        let dir_name = a.parent().unwrap().file_name().unwrap().to_str().unwrap();
        assert_eq!(dir_name.len(), 16);
        assert!(dir_name.chars().all(|c| c.is_ascii_hexdigit()));
        // 路径含 .evo-agent/index 两级
        assert!(a
            .components()
            .any(|c| c.as_os_str().to_str() == Some(".evo-agent")));
        // 同一工作区稳定；不同工作区不同库
        let a2 = index_db_path(td.path()).unwrap();
        assert_eq!(a, a2);
        let td2 = tempfile::tempdir().unwrap();
        let b = index_db_path(td2.path()).unwrap();
        assert_ne!(a, b);
    }
}
