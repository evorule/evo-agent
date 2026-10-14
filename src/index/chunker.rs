// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! chunker —— 工作区遍历与文件清单（B14 结构面 v1）
//!
//! 职责（15 号 D7 / §四）：双层 ignore 遍历——`.gitignore` + `.evoignore`
//! （13 号语义，serve 索引层直接尊重，比 prompt 组装层更早拦截）+
//! `.evoindexingignore`（仅索引排除的用户追加口）+ 硬编码默认排除。敏感面
//! （.env 类/密钥/锁文件/二进制）拦在索引入口：源码内容根本不入库。
//! tree-sitter 符号抽取随 PR-2 加入；本文件当前交付遍历 + 内容哈希。
//!
//! 排除口径：
//! - hidden 文件/目录整体跳过（`.` 前缀——`.git`/`.env` 等点文件天然排除，
//!   与 grep_files `hidden(true)` 口径一致）；
//! - 默认排除集 [`DEFAULT_EXCLUDES`]（依赖/构建产物目录）；
//! - 扩展名黑名单 [`SKIP_EXTS`]（二进制/密钥/锁文件）；
//! - 单文件 [`MAX_FILE_BYTES`] 上限（索引内存护栏，超限不入索引）。

use std::path::{Path, PathBuf};

use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

use crate::index::store::IndexError;

/// 单文件大小上限（超过不入索引；索引护栏非校验语义）
pub const MAX_FILE_BYTES: u64 = 1_000_000;

/// 出厂默认排除集（依赖/构建产物目录；`.git`/`.env` 类点文件由 hidden 口径拦截；
/// `data/` 不入默认集——名字级剪枝会误杀源码子目录，需要时用户走
/// `.evoindexingignore`，与 grep_files DEFAULT_EXCLUDE_GLOBS 同一取舍）
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "target/**",
    "node_modules/**",
    ".venv/**",
    "venv/**",
    "dist/**",
    "build/**",
    ".evo-trash/**",
];

/// 扩展名黑名单：二进制 / 密钥凭据 / 锁文件（D7 硬编码默认排除的文件级部分）
pub const SKIP_EXTS: &[&str] = &[
    // 二进制资产
    "png", "jpg", "jpeg", "gif", "webp", "ico", "icns", "bmp", "pdf", "zip", "gz", "tgz", "bz2",
    "xz", "7z", "rar", "exe", "dll", "so", "dylib", "a", "lib", "obj", "o", "pdb", "wasm", "class",
    "jar", "pyc", "bin", "woff", "woff2", "ttf", "otf", "eot", "mp3", "mp4", "wav", "avi", "mov",
    "mkv", "flac", "ogg",
    // 密钥/凭据（.env 类已被 hidden 口径拦；此层拦非隐藏命名的证书/私钥）
    "pem", "key", "p12", "pfx", "crt", "der", "jks", "keystore",
    // 锁文件（Cargo.lock / package-lock.json / *.lock 全族）
    "lock",
];

/// 遍历收集的文件条目（内容哈希已就地计算，入库即用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// 相对工作区根路径（`/` 分隔——库内统一路径口径）
    pub rel_path: String,
    /// 绝对路径（回读源文件用）
    pub abs_path: PathBuf,
    /// 文件大小（字节）
    pub size: u64,
    /// 修改时间（UNIX 秒；取不到为 0）
    pub mtime: i64,
    /// 内容哈希（blake3 hex）
    pub content_hash: String,
}

/// 遍历工作区并收集可索引文件清单（尽力而为：单文件读失败/元数据缺失跳过不致命）
///
/// 根不存在或不可规范化 → Err；符号链接不跟随（防越出工作区根）。
pub fn collect_workspace_files(workdir: &Path) -> Result<Vec<FileEntry>, IndexError> {
    let root = workdir
        .canonicalize()
        .map_err(|e| IndexError(format!("canonicalize workdir: {e}")))?;
    let mut ovb = OverrideBuilder::new(&root);
    for d in DEFAULT_EXCLUDES {
        ovb.add(&format!("!{d}"))
            .map_err(|e| IndexError(format!("invalid exclude glob '{d}': {e}")))?;
    }
    let overrides = ovb
        .build()
        .map_err(|e| IndexError(format!("build overrides: {e}")))?;

    let mut wb = WalkBuilder::new(&root);
    wb.hidden(true) // .git / .env 类点文件整体排除（同 grep_files 口径）
        .overrides(overrides)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .follow_links(false); // 防符号链接越出根
                              // 双层 ignore（D7）：.evoignore=工作区通用排除（与补全/InlineChat 同口径）；
                              // .evoindexingignore=仅索引排除（用户追加口）
    wb.add_custom_ignore_filename(".evoignore");
    wb.add_custom_ignore_filename(".evoindexingignore");

    let mut out: Vec<FileEntry> = Vec::new();
    for entry in wb.build() {
        let Ok(entry) = entry else { continue }; // 单点遍历错误（权限等）不致命
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > MAX_FILE_BYTES {
            continue; // 超限护栏
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if SKIP_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(rel) = entry.path().strip_prefix(&root) else {
            continue;
        };
        out.push(FileEntry {
            rel_path: rel.to_string_lossy().replace('\\', "/"),
            abs_path: entry.path().to_path_buf(),
            size: meta.len(),
            mtime: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            content_hash: blake3::hash(&bytes).to_hex().to_string(),
        });
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path)); // 确定性次序（对账/测试稳定）
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_respects_layered_excludes() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        // hidden 口径：.env 点文件排除
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        // 默认排除集：构建产物/依赖目录
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/x.rs"), "fn t() {}").unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules/y.js"), "var y;").unwrap();
        // 扩展黑名单：锁文件 + 密钥
        std::fs::write(root.join("Cargo.lock"), "lock").unwrap();
        std::fs::write(root.join("server.pem"), "key").unwrap();
        // .evoignore（工作区通用排除）
        std::fs::write(root.join(".evoignore"), "gen.rs\n").unwrap();
        std::fs::write(root.join("gen.rs"), "generated").unwrap();
        // .evoindexingignore（仅索引排除，子目录级）
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/.evoindexingignore"), "secret.rs\n").unwrap();
        std::fs::write(root.join("sub/secret.rs"), "skip").unwrap();
        std::fs::write(root.join("sub/keep.rs"), "fn keep() {}").unwrap();

        let files = collect_workspace_files(root).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(paths, vec!["main.rs", "sub/keep.rs"]);
        let main = files.iter().find(|f| f.rel_path == "main.rs").unwrap();
        assert_eq!(
            main.content_hash,
            blake3::hash(b"fn main() {}").to_hex().to_string()
        );
    }

    #[test]
    fn oversized_files_are_skipped() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::write(root.join("big.rs"), "x".repeat(MAX_FILE_BYTES as usize + 1)).unwrap();
        std::fs::write(root.join("ok.rs"), "fn ok() {}").unwrap();
        let files = collect_workspace_files(root).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(paths, vec!["ok.rs"]);
    }

    #[test]
    fn missing_root_is_an_error() {
        let td = tempfile::tempdir().unwrap();
        let missing = td.path().join("nope");
        assert!(collect_workspace_files(&missing).is_err());
    }
}
