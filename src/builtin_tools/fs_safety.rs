// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 文件增删改工具族(file_create / file_move / file_delete)共享的路径安全 helpers
//!
//! 判据与 [`FileWriteTool`](crate::builtin_tools::file_write::FileWriteTool) 同源:
//! - 绝对路径 / `..` 父目录段一律拒绝
//! - 逻辑路径必须落在 `writable_dir` 内(containment)
//! - 已存在路径 canonicalize 后必须仍在沙箱内(symlink / junction 逃逸拒绝)
//! - 不存在的创建目标沿父目录链找最深已存在祖先复查 containment
//!
//! 三工具与 file_write 的沙箱语义差异:增删改的可写面按实例配置
//! (agent 面 `workspace/`,工作台人工编辑面 `"."` = workdir 全域),
//! 与 file_write 的 `with_writable_dir` 同一约定。

use std::path::{Component, Path, PathBuf};

/// Windows 保留设备名(大小写不敏感;含扩展名形态如 `con.txt` 按主名判定)
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Windows 文件名非法字符(跨平台统一从严) + 控制字符在 [`validate_node_name`] 内判定
fn is_illegal_char(ch: char) -> bool {
    matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || ch.is_control()
}

/// 校验单个路径组件名(新建节点用):
/// 非空 / 非 `.` `..` / 无非法与控制字符 / 不以点或空格结尾 / 非 Windows 保留名
pub fn validate_node_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty name not allowed".to_string());
    }
    if name == "." || name == ".." {
        return Err(format!("name '{name}' is a reserved relative component"));
    }
    if let Some(ch) = name.chars().find(|c| is_illegal_char(*c)) {
        return Err(format!(
            "illegal character {ch:?} in name '{name}' (Windows-invalid characters rejected)"
        ));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(format!(
            "name '{name}' ends with a dot or space (Windows-invalid)"
        ));
    }
    let stem = name.split('.').next().unwrap_or(name);
    if WINDOWS_RESERVED.contains(&stem.to_ascii_uppercase().as_str()) {
        return Err(format!(
            "name '{name}' uses a reserved Windows device name (stem '{stem}')"
        ));
    }
    Ok(())
}

/// 校验相对路径形态并逐段构造 workdir 下的逻辑路径:
/// - 绝对路径 / `..` 段拒绝
/// - **尚不存在**的组件必须过 [`validate_node_name`](保留名/非法字符);
///   已存在组件跳过名称校验(容纳历史命名,仅形态校验)
///
/// 返回逻辑路径(未 canonicalize;不一定存在)。
pub fn logical_target(workdir: &Path, raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(format!(
            "absolute path not allowed: '{raw}' (all paths must stay within the sandbox boundary '{}')",
            workdir.display()
        ));
    }
    let mut cumulative = workdir.to_path_buf();
    for component in path.components() {
        match component {
            Component::Normal(seg) => {
                let name = seg.to_string_lossy().to_string();
                let candidate = cumulative.join(&name);
                if std::fs::symlink_metadata(&candidate).is_err() {
                    validate_node_name(&name)?;
                }
                cumulative = candidate;
            }
            Component::ParentDir => {
                return Err(format!(
                    "parent dir (..) not allowed: '{raw}' (must stay within the sandbox boundary '{}')",
                    workdir.display()
                ));
            }
            Component::CurDir => { /* "./" 形态:components() 语义保留仅前导,跳过 */ }
            other => {
                return Err(format!("unsupported path component in '{raw}': {other:?}"));
            }
        }
    }
    Ok(cumulative)
}

/// 解析**已存在**路径:canonicalize 后必须仍在 workdir 内(symlink/junction 逃逸拒绝)。
/// 错误文案含 "does not exist"(404 映射判据,同 file_api err_status 约定)。
pub fn resolve_existing(workdir: &Path, raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(format!(
            "absolute path not allowed: '{raw}' (all paths must stay within the sandbox boundary '{}')",
            workdir.display()
        ));
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(format!(
                "parent dir (..) not allowed: '{raw}' (must stay within the sandbox boundary '{}')",
                workdir.display()
            ));
        }
    }
    let joined = workdir.join(path);
    let canonical = joined
        .canonicalize()
        .map_err(|e| format!("path does not exist or cannot resolve: '{raw}' ({e})"))?;
    let workdir_canonical = workdir
        .canonicalize()
        .map_err(|e| format!("workdir invalid: {e}"))?;
    if !canonical.starts_with(&workdir_canonical) {
        return Err(format!(
            "path not accessible: '{raw}' resolves outside the sandbox boundary '{}'",
            workdir_canonical.display()
        ));
    }
    Ok(canonical)
}

/// 可写面 canonical 根(workdir + writable_dir,canonicalize 失败即沙箱配置无效)
pub fn writable_root(workdir: &Path, writable_dir: &Path) -> Result<PathBuf, String> {
    let abs = workdir.join(writable_dir);
    abs.canonicalize()
        .map_err(|e| format!("writable_dir does not exist: {} ({})", abs.display(), e))
}

/// 解析**创建目标**(file_create 用):目标必须不存在(重名 → "already exists",
/// 409 映射判据);逻辑路径必须落在 writable_dir 内;父目录链上最深已存在祖先
/// canonicalize 后必须仍在 writable_dir 内(防 junction 父目录逃逸)。
pub fn resolve_create_target(
    workdir: &Path,
    writable_dir: &Path,
    raw: &str,
) -> Result<PathBuf, String> {
    let target = logical_target(workdir, raw)?;
    let writable_canonical = writable_root(workdir, writable_dir)?;
    let writable_abs = workdir.join(writable_dir);
    if !target.starts_with(&writable_abs) {
        return Err(format!(
            "path '{raw}' is outside writable_dir '{}' (path traversal)",
            writable_dir.display()
        ));
    }
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(format!("already exists: '{raw}'"));
    }
    let mut ancestor = target.parent();
    while let Some(p) = ancestor {
        if p.exists() {
            let a = p
                .canonicalize()
                .map_err(|e| format!("path cannot be resolved: {e}"))?;
            if !a.starts_with(&writable_canonical) {
                return Err(format!(
                    "path '{raw}' resolves outside writable_dir (parent symlink/junction escape)"
                ));
            }
            break;
        }
        ancestor = p.parent();
    }
    Ok(target)
}

/// 创建目标的所有缺失父目录(`create_parents=true` 语义,同 file_write)。
/// 返回实际新建的目录数。
pub fn create_missing_parents(target: &Path) -> Result<usize, String> {
    let mut created = 0usize;
    let mut to_create: Vec<PathBuf> = Vec::new();
    let mut cursor = target.parent();
    while let Some(p) = cursor {
        if p.exists() {
            break;
        }
        to_create.push(p.to_path_buf());
        cursor = p.parent();
    }
    // 自浅至深创建
    for p in to_create.into_iter().rev() {
        std::fs::create_dir(&p).map_err(|e| format!("failed to create parent dir: {e}"))?;
        created += 1;
    }
    Ok(created)
}

/// 递归复制(跨盘 move/delete fallback 用):文件逐字节拷贝,目录深度优先。
/// 目标必须不存在(调用方先做重名检查)。
pub fn copy_recursive(from: &Path, to: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(from)
        .map_err(|e| format!("cannot read source entry {}: {e}", from.display()))?;
    if meta.is_dir() {
        std::fs::create_dir_all(to)
            .map_err(|e| format!("failed to create dir {}: {e}", to.display()))?;
        let entries = std::fs::read_dir(from)
            .map_err(|e| format!("read_dir failed on {}: {e}", from.display()))?;
        for entry in entries.flatten() {
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to)
            .map(|_| ())
            .map_err(|e| format!("copy failed {} -> {}: {e}", from.display(), to.display()))
    }
}

/// 递归删除(recurse 目录;文件/链接单删)
pub fn remove_recursive(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot read entry {}: {e}", path.display()))?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
            .map_err(|e| format!("remove_dir_all failed on {}: {e}", path.display()))
    } else {
        std::fs::remove_file(path)
            .map_err(|e| format!("remove_file failed on {}: {e}", path.display()))
    }
}

// =============================================================================
// 文件树变更全局写互斥(设计 §3.1 v1)
// =============================================================================

/// REST 文件增删改端点共用的全局树写互斥锁。
///
/// 串行化工作台文件树的增删改(create/move/delete),防并发变更产生
/// 中间态(如移动目标目录的同时该目录被删除)。tokio Mutex 因持锁
/// 段跨 await(工具调用经 spawn_blocking 异步执行);agent 面工具
/// 不持此锁——其并发由会话自身串行保证,锁只覆盖人工面端点。
static TREE_MUTATION_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

/// 取全局树写互斥锁(handler 内 `.lock().await` 后持锁执行变更)
pub fn tree_mutation_lock() -> &'static tokio::sync::Mutex<()> {
    TREE_MUTATION_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 生成 `.evo-trash/` 内唯一落点名:`<unix_ts>_<name>`,同秒重名追加 `_1`/`_2`…
pub fn trash_destination(trash_dir: &Path, name: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut dest = trash_dir.join(format!("{ts}_{name}"));
    let mut n = 0u32;
    while dest.exists() {
        n += 1;
        dest = trash_dir.join(format!("{ts}_{name}_{n}"));
    }
    dest
}
