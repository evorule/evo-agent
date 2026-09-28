// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 文件工具族共享的路径安全 helpers —— **沙箱判据唯一权威(P0 收口)**
//!
//! 判据与 [`FileWriteTool`](crate::builtin_tools::file_write::FileWriteTool) 同源
//! (以 file_write 判据为权威,见 [`resolve_writable_target`]):
//! - 绝对路径 / `..` 父目录段一律拒绝
//! - 逻辑路径必须落在 `writable_dir` 内(containment)
//! - 已存在路径 canonicalize 后必须仍在沙箱内(symlink / junction 逃逸拒绝)
//! - 不存在的创建目标沿父目录链找最深已存在祖先复查 containment
//!
//! 写侧([`resolve_writable_target`])与读侧([`resolve_existing`])两枚权威 API;
//! file_write / file_read / file_list / search_files / grep_files 五处 resolver
//! 一律改调此处,不再各自持判据(六处对齐回归矩阵见文末 alignment_tests)。

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

/// **权威写路径判据**(原 file_write `resolve_safe_path` 逐字移植,P0 六处对齐的唯一权威):
/// 绝对/`..` 拒 → workdir/writable_dir canonicalize → 逻辑 containment →
/// symlink / reparse point 三情形判据:
///   a) 目标存在(普通文件或链接到已存在目标):canonicalize 解析后复查 containment
///   b) 悬空 symlink(P02/R03):`exists()`=false 但写会跟随链接在外创建 →
///      symlink_metadata 可见,canonicalize 失败即拒
///   c) 目标不存在且非链接:沿父目录链找最深已存在祖先 canonicalize
///      复查 containment(P02 实证 junction 父目录逃逸,CWE-59 变体)
///
/// 返回 `(target_path, canonical_or_logical_path)`:
/// - 存在路径:返回 canonical(用于 symlink 校验)
/// - 不存在路径:返回 logical target + writable_canonical(用于 containment 校验)
pub fn resolve_writable_target(
    workdir: &Path,
    writable_dir: &Path,
    raw: &str,
) -> Result<(PathBuf, PathBuf), String> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(format!(
            "absolute path not allowed: '{raw}' (all paths must stay within the sandbox \
             boundary '{}')",
            workdir.display()
        ));
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(format!(
                "parent dir (..) not allowed: '{raw}' (must stay within the sandbox \
                 boundary '{}')",
                workdir.display()
            ));
        }
    }

    // workdir 必须是已存在的(整个写面工具的前提)
    let _workdir_canonical = workdir
        .canonicalize()
        .map_err(|e| format!("workdir invalid: {e}"))?;

    // writable_dir 也必须存在(否则无法写入)
    let writable_abs = workdir.join(writable_dir);
    let writable_canonical = writable_abs.canonicalize().map_err(|e| {
        format!(
            "writable_dir does not exist: {} ({})",
            writable_abs.display(),
            e
        )
    })?;

    // 目标路径 = workdir + path(逻辑路径,不一定存在)
    let target = workdir.join(path);

    // containment check(逻辑路径)
    if !target.starts_with(&writable_abs) {
        return Err(format!(
            "path '{raw}' is outside writable_dir '{}' (path traversal)",
            writable_dir.display()
        ));
    }
    if !target.starts_with(workdir) {
        // M5-a:越界错误回报「不可访问 + 边界路径」
        return Err(format!(
            "path escapes workdir: '{raw}' is not accessible (outside the sandbox boundary '{}')",
            workdir.display()
        ));
    }

    // symlink / reparse point 检查(canonical 必须仍在 writable_dir 内)
    // 判据用 `symlink_metadata`(不跟随链接)而非 `exists()`(跟随链接),覆盖 a/b/c 三情形
    let target_canonical = match std::fs::symlink_metadata(&target) {
        Ok(_meta) => {
            let c = target
                .canonicalize()
                .map_err(|e| format!("path cannot be resolved: {e}"))?;
            // 再 check 一次(symlink 可能跳出)
            if !c.starts_with(&writable_canonical) {
                return Err(format!(
                    "path '{raw}' resolves outside writable_dir (symlink escape)"
                ));
            }
            c
        }
        Err(_) => {
            // 目标不存在(且不是悬空链接):父目录链可能含 junction/symlink
            let mut ancestor = target.parent();
            while let Some(p) = ancestor {
                if p.exists() {
                    let a = p
                        .canonicalize()
                        .map_err(|e| format!("path cannot be resolved: {e}"))?;
                    if !a.starts_with(&writable_canonical) {
                        return Err(format!(
                            "path '{raw}' resolves outside writable_dir (parent symlink/junction escape)",
                        ));
                    }
                    break;
                }
                ancestor = p.parent();
            }
            target.clone()
        }
    };

    Ok((target, target_canonical))
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

// =============================================================================
// P0 六处对齐回归矩阵(PR0-3;03 号 §9.1)
// =============================================================================
//
// 每工具 × 判据维度:工具实测判定(公开 call() 结果 Ok/Err)⇄ 权威 API 判定
// (resolve_writable_target / resolve_existing)**逐格一致**=沙箱行为零放宽证明。
// 五工具 resolver 已全部改调本模块,矩阵把「单一权威」固化为回归防线。
// 链接类维度沿用 P02 教训:不信 Ok(()),必须核验链接真实创建,否则该格跳过。

#[cfg(test)]
mod alignment_tests {
    use super::*;
    use crate::builtin_tools::file_list::FileListTool;
    use crate::builtin_tools::file_read::FileReadTool;
    use crate::builtin_tools::file_write::FileWriteTool;
    use crate::builtin_tools::grep_files::GrepFilesTool;
    use crate::builtin_tools::search_files::SearchFilesTool;
    use crate::io_handlers::tool_handler::ToolFunction;
    use serde_json::json;

    /// 平台真实绝对路径形态("C:\..." 在 Unix 上不是绝对路径,走不到拒绝分支)
    const ABS: &str = if cfg!(windows) {
        "C:\\Windows\\System32"
    } else {
        "/etc/passwd"
    };

    /// 固定测具:workspace/(写面)+ subdir/(读侧目录格)+ 根域文件
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("workspace")).unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
        std::fs::write(dir.path().join("config.toml"), b"").unwrap();
        std::fs::write(dir.path().join("workspace/exists.txt"), b"old").unwrap();
        dir
    }

    /// 逐格断言:工具判定必须与权威 API 判定一致
    fn check(cell: &str, tool_ok: bool, expected_ok: bool) {
        assert_eq!(
            tool_ok, expected_ok,
            "alignment matrix cell `{cell}`: tool verdict diverges from authoritative API"
        );
    }

    /// 创建文件符号链接;false=环境不可创建(无特权/中介层假 Ok),对应格跳过。
    /// (P02 教训:Ok(()) 后必须核验链接真实存在)
    fn try_symlink_file(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
                && matches!(
                    std::fs::symlink_metadata(link),
                    Ok(m) if m.file_type().is_symlink()
                )
        }
        #[cfg(windows)]
        {
            match std::os::windows::fs::symlink_file(target, link) {
                Ok(()) => matches!(
                    std::fs::symlink_metadata(link),
                    Ok(m) if m.file_type().is_symlink()
                ),
                Err(_) => false,
            }
        }
    }

    /// 创建目录联接(junction,Windows 无需特权;unix=目录 symlink 等价物)。
    /// false=环境不可创建,对应格跳过。
    fn try_dir_link(target_dir: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            try_symlink_file(target_dir, link)
        }
        #[cfg(windows)]
        {
            let out = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target_dir)
                .output()
                .expect("failed to spawn mklink");
            if !out.status.success() {
                return false;
            }
            matches!(
                std::fs::symlink_metadata(link),
                Ok(m) if m.file_type().is_symlink()
            )
        }
    }

    /// 外部目录(链接逃逸目标)+ 目标文件
    fn outside_fixture() -> tempfile::TempDir {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        outside
    }

    #[tokio::test]
    async fn matrix_file_write() {
        let dir = fixture();
        let wd = dir.path().to_path_buf();
        let tool = FileWriteTool::new(wd.clone());
        let auth = |raw: &str| resolve_writable_target(&wd, Path::new("workspace"), raw).is_ok();
        let tool_ref = &tool;
        let call = |raw: &str| {
            let args = json!({
                "path": raw,
                "content": "x",
                "overwrite": true,
                "create_parents": true
            });
            async move { tool_ref.call(&args).await.is_ok() }
        };

        check("D1 绝对路径", call(ABS).await, auth(ABS));
        check(
            "D2 `..` 段",
            call("../escape.txt").await,
            auth("../escape.txt"),
        );
        check(
            "D3 workdir 内 writable 外",
            call("config.toml").await,
            auth("config.toml"),
        );
        check(
            "D4 writable 内已存在",
            call("workspace/exists.txt").await,
            auth("workspace/exists.txt"),
        );
        check(
            "D5 writable 内新建",
            call("workspace/new.txt").await,
            auth("workspace/new.txt"),
        );

        // D6/D7:符号链接族(无特权环境跳过;各维独立落点)
        let outside = outside_fixture();
        let link = wd.join("workspace").join("link.txt");
        if try_symlink_file(&outside.path().join("secret.txt"), &link) {
            check(
                "D6 symlink 逃逸(已存在目标)",
                call("workspace/link.txt").await,
                auth("workspace/link.txt"),
            );
            let _ = std::fs::remove_file(&link);
        }
        let dangling = wd.join("workspace").join("dangling.txt");
        if try_symlink_file(&outside.path().join("dangling_target.txt"), &dangling) {
            check(
                "D7 悬空 symlink",
                call("workspace/dangling.txt").await,
                auth("workspace/dangling.txt"),
            );
            let _ = std::fs::remove_file(&dangling);
        }

        // D8:junction 父目录逃逸(新文件;P02/R03 主判据,CI 可稳定复现)
        let jlink = wd.join("workspace").join("linkdir");
        if try_dir_link(outside.path(), &jlink) {
            check(
                "D8 junction 父目录逃逸",
                call("workspace/linkdir/escaped.txt").await,
                auth("workspace/linkdir/escaped.txt"),
            );
            let _ = std::fs::remove_dir(&jlink); // 先摘链接,防 TempDir 穿透删除
        }

        // 决定性核验:outside 不得出现任何写入物
        assert!(
            !outside.path().join("escaped.txt").exists()
                && !outside.path().join("new.txt").exists(),
            "sandbox escaped: file appeared outside writable_dir"
        );
    }

    #[tokio::test]
    async fn matrix_file_read() {
        let dir = fixture();
        let wd = dir.path().to_path_buf();
        let tool = FileReadTool::new(wd.clone());
        let auth = |raw: &str| resolve_existing(&wd, raw).is_ok();
        let tool_ref = &tool;
        let call = |raw: &str| {
            let args = json!({ "path": raw });
            async move { tool_ref.call(&args).await.is_ok() }
        };

        check("D1 绝对路径", call(ABS).await, auth(ABS));
        check(
            "D2 `..` 段",
            call("../escape.txt").await,
            auth("../escape.txt"),
        );
        check(
            "D3 workdir 内已存在文件",
            call("hello.txt").await,
            auth("hello.txt"),
        );
        check(
            "D4 workspace 内已存在文件",
            call("workspace/exists.txt").await,
            auth("workspace/exists.txt"),
        );
        check(
            "D5 不存在路径",
            call("missing.txt").await,
            auth("missing.txt"),
        );

        let outside = outside_fixture();
        let link = wd.join("link.txt");
        if try_symlink_file(&outside.path().join("secret.txt"), &link) {
            check("D6 symlink 逃逸", call("link.txt").await, auth("link.txt"));
            let _ = std::fs::remove_file(&link);
        }
        let dangling = wd.join("dangling.txt");
        if try_symlink_file(&outside.path().join("dangling_target.txt"), &dangling) {
            check(
                "D7 悬空 symlink",
                call("dangling.txt").await,
                auth("dangling.txt"),
            );
            let _ = std::fs::remove_file(&dangling);
        }

        let jlink = wd.join("linkdir");
        if try_dir_link(outside.path(), &jlink) {
            check(
                "D8 junction 目录读取",
                call("linkdir/secret.txt").await,
                auth("linkdir/secret.txt"),
            );
            let _ = std::fs::remove_dir(&jlink);
        }
    }

    /// 读侧目录族(file_list / search_files / grep_files)共用 resolve_existing,
    /// 同一矩阵三工具逐格一致(allow 格用**目录**输入,隔离 read_dir 等非沙箱后置语义)
    #[tokio::test]
    async fn matrix_dir_family() {
        let dir = fixture();
        let wd = dir.path().to_path_buf();
        let list = FileListTool::new(wd.clone());
        let search = SearchFilesTool::new(wd.clone());
        let grep = GrepFilesTool::new(wd.clone());
        let auth = |raw: &str| resolve_existing(&wd, raw).is_ok();

        let (list_ref, search_ref, grep_ref) = (&list, &search, &grep);
        let list_call = |raw: &str| {
            let args = json!({ "dir": raw });
            async move { list_ref.call(&args).await.is_ok() }
        };
        let search_call = |raw: &str| {
            let args = json!({ "pattern": "*", "dir": raw });
            async move { search_ref.call(&args).await.is_ok() }
        };
        let grep_call = |raw: &str| {
            let args = json!({ "query": "x", "dir": raw });
            async move { grep_ref.call(&args).await.is_ok() }
        };

        for (dim, input) in [
            ("D1 绝对路径", ABS),
            ("D2 `..` 段", "../escape_dir"),
            ("D3 workdir 内已存在目录", "subdir"),
            ("D4 workspace 目录", "workspace"),
            ("D5 不存在目录", "missing_dir"),
        ] {
            check(
                &format!("file_list {dim}"),
                list_call(input).await,
                auth(input),
            );
            check(
                &format!("search_files {dim}"),
                search_call(input).await,
                auth(input),
            );
            check(
                &format!("grep_files {dim}"),
                grep_call(input).await,
                auth(input),
            );
        }

        let outside = outside_fixture();
        let jlink = wd.join("linkdir");
        if try_dir_link(outside.path(), &jlink) {
            let expected = auth("linkdir");
            check(
                "file_list D8 junction 目录",
                list_call("linkdir").await,
                expected,
            );
            check(
                "search_files D8 junction 目录",
                search_call("linkdir").await,
                expected,
            );
            check(
                "grep_files D8 junction 目录",
                grep_call("linkdir").await,
                expected,
            );
            let _ = std::fs::remove_dir(&jlink);
        }
    }

    /// 权威 API 自证:写侧权威对 P02/R03 三情形的判定与既有 file_write 回归语义一致
    #[test]
    fn authority_self_check() {
        let dir = fixture();
        let wd = dir.path().to_path_buf();

        // 普通写目标:逻辑路径返回 Ok(创建判定交 overwrite/create_parents 后置层)
        assert!(resolve_writable_target(&wd, Path::new("workspace"), "workspace/new.txt").is_ok());
        // writable 外:拒
        assert!(resolve_writable_target(&wd, Path::new("workspace"), "config.toml").is_err());
        // 绝对 / `..`:拒
        assert!(resolve_writable_target(&wd, Path::new("workspace"), ABS).is_err());
        assert!(resolve_writable_target(&wd, Path::new("workspace"), "../x.txt").is_err());

        // D8:junction 父目录下的新文件必须拒(决定性:outside 无写入物)
        let outside = outside_fixture();
        let jlink = wd.join("workspace").join("linkdir");
        if try_dir_link(outside.path(), &jlink) {
            assert!(resolve_writable_target(
                &wd,
                Path::new("workspace"),
                "workspace/linkdir/e.txt"
            )
            .is_err());
            assert!(!outside.path().join("e.txt").exists());
            let _ = std::fs::remove_dir(&jlink);
        }

        // 读侧权威:存在→canonical containment;不存在→拒;越界→拒
        assert!(resolve_existing(&wd, "hello.txt").is_ok());
        assert!(resolve_existing(&wd, "missing.txt").is_err());
        assert!(resolve_existing(&wd, "../x").is_err());
    }
}
