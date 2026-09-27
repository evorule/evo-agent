// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Git 基础面核心层（B3）—— SCM 侧栏 / REST git 面 / agent git 工具的单一实现
//!
//! 三个消费面（REST `git_api`、agent `git_tools`、未来 B15）都薄委托本模块，
//! 保证「agent 与人看到同一份 git 语义」。职责与边界（板块设计档 §3.2）：
//!
//! - **双态 status**：`暂存的更改`（index vs HEAD）+ `更改`（workdir vs index，
//!   untracked 并入更改组——VS Code 同款两组模型）；全量 status 恒带 rename
//!   检测（libgit2 约束：启用 rename 不可带 pathspec，路径过滤留给前端）；
//! - **diff 两版全文**：HEAD blob vs 工作区文件内容（不自产 patch），untracked
//!   → original 为空串；
//! - **stage / unstage / discard**：index.add_path·add_all / reset_default /
//!   checkout_index 恢复 index 态（untracked = 删除文件）；
//! - **提交流程**：身份预检（user.name/email 缺失→结构化错误）→ hooks 检测
//!   （存在 pre-commit/commit-msg → CLI 子进程回退，libgit2 不跑 hooks 的已知
//!   差异被此弥合）→ 无 hooks 走 git2 原生提交；
//! - **`.evo-*` 工具私有目录隔离**（文件 API 批次遗留待办闭环）：确保 workdir
//!   `.git/info/exclude` 含 `.evo-trash/`——info/exclude 是 git 官方本地忽略位，
//!   不污染用户 .gitignore / 不进版本库；
//! - **边缘形态拒绝**：`.git` 为文件（worktree/submodule）→ 明确报错不半残工作。
//!
//! 治理边界（板块设计档）：本模块不读不改 git config（身份缺失只报错引导用户
//! 自行配置）；CLI 回退仅白名单子命令（`add -A` / `commit --file=-`）、argv
//! 数组构造无 shell、消息走 stdin、30s 超时、工作目录锁死 workdir。

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use git2::{IndexAddOption, Repository, Status, StatusOptions};
use serde::Serialize;

/// evo-agent 工具私有目录（B7 软删除回收站）——git status 中必须不可见
pub const TRASH_DIR: &str = ".evo-trash/";

/// CLI 回退单条子命令超时（板块设计档：30s 硬性 DoD）
const CLI_TIMEOUT: Duration = Duration::from_secs(30);

/// stderr 透传尾部最大字节数（防长输出刷屏，保留离错误最近的部分）
const STDERR_TAIL_BYTES: usize = 800;

/// Git 操作错误（REST 层映射 HTTP 状态码；见 [`GitError::http_status`]）
#[derive(Debug)]
pub enum GitError {
    /// workdir 不是 git 仓库（无 `.git` 目录）→ 400
    NotARepository,
    /// `.git` 为文件（worktree/submodule 等边缘形态）→ 400（板块设计档裁定）
    NotSupported(&'static str),
    /// 提交身份缺失（user.name/user.email 任一为空）→ 400 + 引导 hint
    IdentityMissing,
    /// CLI 回退子命令失败（含 hooks 拒绝）→ 400，携带 stderr 尾部
    CliFailed(String),
    /// 参数/状态非法（空消息、无内容可 diff 等）→ 400
    Invalid(String),
    /// git2 库错误（仓库损坏/索引冲突等）→ 500
    Lib(String),
    /// 文件系统 IO 错误 → 500
    Io(String),
}

impl GitError {
    /// 映射 HTTP 状态码（`git_api` 薄委托层的错误面约定）
    pub fn http_status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            GitError::NotARepository
            | GitError::NotSupported(_)
            | GitError::IdentityMissing
            | GitError::CliFailed(_)
            | GitError::Invalid(_) => StatusCode::BAD_REQUEST,
            GitError::Lib(_) | GitError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// 错误消息（前端展示面；`identity_missing` 为结构化错误码，前端引导）
    pub fn message(&self) -> String {
        match self {
            GitError::NotARepository => "not a git repository".to_string(),
            GitError::NotSupported(what) => format!("git operation not supported: {what}"),
            GitError::IdentityMissing => "identity_missing".to_string(),
            GitError::CliFailed(stderr) => {
                format!("git command failed: {}", stderr)
            }
            GitError::Invalid(msg) => msg.clone(),
            GitError::Lib(msg) => format!("git2 error: {msg}"),
            GitError::Io(msg) => format!("git io error: {msg}"),
        }
    }

    /// 身份缺失时的配置引导（VS Code 同款拦截语义；serve 不代写 git config）
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            GitError::IdentityMissing => {
                Some("set your git identity first: git config --global user.name <name> && git config --global user.email <email>")
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

/// 单条变更条目（path 相对 workdir，`/` 分隔；untracked 目录折叠条目带尾 `/`）
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StatusEntry {
    /// 相对 workdir 路径（`/` 分隔；折叠目录条目以 `/` 结尾）
    pub path: String,
    /// 状态字母：M 修改 / A 新增（暂存组）/ D 删除 / R 重命名 / U 未跟踪（更改组）
    pub status: char,
}

/// 双态 status 结果（单状态源：SCM 视图与 Explorer 装饰共同订阅）
#[derive(Debug, Clone, Serialize)]
pub struct GitStatus {
    /// 当前分支名（shorthand；detached HEAD 时为短 hash；未出生 HEAD 为空串）
    pub branch: String,
    /// 是否有任意变更（staged 或 changes 任一非空）
    pub dirty: bool,
    /// 暂存的更改组（index vs HEAD）：A/M/D/R
    pub staged: Vec<StatusEntry>,
    /// 更改组（workdir vs index，含 untracked）：M/D/R/U
    pub changes: Vec<StatusEntry>,
}

/// diff 两版全文（Monaco DiffEditor 直接消费；不自产 patch）
#[derive(Debug, Clone, Serialize)]
pub struct GitDiff {
    /// HEAD 版内容（untracked / 未出生 HEAD → 空串 = 全新增）
    pub original: String,
    /// 工作区版内容（文件已删除 → 空串 = 全删除）
    pub modified: String,
    /// 语言 id（Monaco language id，按扩展名映射）
    pub language: String,
}

/// 提交记录（agent `git_log` 工具消费面；新→旧排序）
#[derive(Debug, Clone, Serialize)]
pub struct CommitInfo {
    /// 完整 commit hash（40 hex）
    pub id: String,
    /// 短 hash（前 7 位）
    pub short_id: String,
    /// 作者名
    pub author: String,
    /// 作者邮箱
    pub email: String,
    /// 提交时间（epoch 秒）
    pub time: i64,
    /// 提交消息首行
    pub summary: String,
}

/// GitOps：workdir 绑定的 git 操作门面（每方法独立 open Repository，无共享态）
#[derive(Debug, Clone)]
pub struct GitOps {
    workdir: PathBuf,
}

impl GitOps {
    /// 构造 GitOps（构造即执行 info/exclude 隔离；不可写降级仅告警，见
    /// [`Self::ensure_exclude`]——板块设计档「serve 启动时确保」语义，
    /// 每次构造自愈等价且更强）
    pub fn new(workdir: PathBuf) -> Self {
        let ops = Self { workdir };
        if let Err(e) = ops.ensure_exclude() {
            tracing::warn!("git info/exclude isolation skipped: {e}");
        }
        ops
    }

    /// 打开仓库（含边缘形态门卫：`.git` 文件 = worktree/submodule → 拒绝）
    fn open(&self) -> Result<Repository, GitError> {
        let dotgit = self.workdir.join(".git");
        if dotgit.is_file() {
            return Err(GitError::NotSupported(
                "worktree/submodule repositories are not supported in v1",
            ));
        }
        if !dotgit.is_dir() {
            return Err(GitError::NotARepository);
        }
        Repository::open(&self.workdir).map_err(|e| GitError::Lib(e.message().to_string()))
    }

    /// 规范化相对路径：`\` → `/`（Windows 输入兼容），拒绝空串/绝对路径/越界
    fn normalize(&self, raw: &str) -> Result<String, GitError> {
        let p = raw.replace('\\', "/");
        if p.is_empty() || p.starts_with('/') || p.contains("..") {
            return Err(GitError::Invalid(format!("invalid path: {raw:?}")));
        }
        if Path::new(&p).is_absolute() {
            return Err(GitError::Invalid(format!(
                "absolute path rejected: {raw:?}"
            )));
        }
        Ok(p)
    }

    /// 确保 `.git/info/exclude` 含 `.evo-trash/`（幂等；原子写；不动用户 .gitignore）
    ///
    /// info/exclude 是 git 官方本地忽略位（不进版本库），对齐「共享配置提交、
    /// 运行时状态忽略」行业先例。不可写 → 返回 Err 由调用方告警降级。
    pub fn ensure_exclude(&self) -> Result<(), GitError> {
        let git_dir = self.workdir.join(".git");
        if !git_dir.is_dir() {
            return Ok(()); // 非 git 仓库：无事可做（不报错，保持构造零失败）
        }
        let info_dir = git_dir.join("info");
        let exclude_path = info_dir.join("exclude");
        let existing = match std::fs::read_to_string(&exclude_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(GitError::Io(e.to_string())),
        };
        let already = existing.lines().any(|l| l.trim() == TRASH_DIR);
        if already {
            return Ok(());
        }
        std::fs::create_dir_all(&info_dir).map_err(|e| GitError::Io(e.to_string()))?;
        let mut next = existing.clone();
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&format!(
            "\n# evo-agent workbench private dirs (added by evo-agent serve)\n{TRASH_DIR}\n"
        ));
        // 原子写：tmp + rename（Windows 侧 std rename 走 MOVEFILE_REPLACE_EXISTING）
        let tmp = info_dir.join("exclude.evo-agent.tmp");
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| GitError::Io(e.to_string()))?;
            f.write_all(next.as_bytes())
                .map_err(|e| GitError::Io(e.to_string()))?;
            f.sync_all().ok();
        }
        std::fs::rename(&tmp, &exclude_path).map_err(|e| GitError::Io(e.to_string()))?;
        Ok(())
    }

    /// 双态 status（全量 + rename 检测；untracked 目录折叠为一条带尾 `/`）
    pub fn status(&self) -> Result<GitStatus, GitError> {
        let repo = self.open()?;
        let branch = repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(|s| s.to_string()))
            .unwrap_or_default();

        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .include_ignored(false)
            .recurse_untracked_dirs(false)
            .exclude_submodules(true)
            .renames_head_to_index(true)
            .renames_index_to_workdir(true);
        let statuses = repo
            .statuses(Some(&mut opts))
            .map_err(|e| GitError::Lib(e.message().to_string()))?;

        let mut staged = Vec::new();
        let mut changes = Vec::new();
        for entry in statuses.iter() {
            let s = entry.status();
            // 暂存组（index vs HEAD）
            if s.intersects(
                Status::INDEX_NEW
                    | Status::INDEX_MODIFIED
                    | Status::INDEX_DELETED
                    | Status::INDEX_RENAMED
                    | Status::INDEX_TYPECHANGE,
            ) {
                let delta = entry.head_to_index();
                let path = rename_new_path(delta, entry.path());
                let letter = if s.contains(Status::INDEX_NEW) {
                    'A'
                } else if s.contains(Status::INDEX_DELETED) {
                    'D'
                } else if s.contains(Status::INDEX_RENAMED) {
                    'R'
                } else {
                    'M' // INDEX_MODIFIED / INDEX_TYPECHANGE
                };
                staged.push(StatusEntry {
                    path,
                    status: letter,
                });
            }
            // 更改组（workdir vs index；WT_NEW = untracked → U）
            if s.intersects(
                Status::WT_NEW
                    | Status::WT_MODIFIED
                    | Status::WT_DELETED
                    | Status::WT_RENAMED
                    | Status::WT_TYPECHANGE,
            ) {
                let delta = entry.index_to_workdir();
                let path = rename_new_path(delta, entry.path());
                let letter = if s.contains(Status::WT_NEW) {
                    'U'
                } else if s.contains(Status::WT_DELETED) {
                    'D'
                } else if s.contains(Status::WT_RENAMED) {
                    'R'
                } else {
                    'M' // WT_MODIFIED / WT_TYPECHANGE
                };
                changes.push(StatusEntry {
                    path,
                    status: letter,
                });
            }
        }
        let dirty = !staged.is_empty() || !changes.is_empty();
        Ok(GitStatus {
            branch,
            dirty,
            staged,
            changes,
        })
    }

    /// diff 两版全文：HEAD blob vs 工作区文件（untracked → original 为空串）
    pub fn diff(&self, raw_path: &str) -> Result<GitDiff, GitError> {
        let path = self.normalize(raw_path)?;
        let repo = self.open()?;
        let rel = Path::new(&path);

        let original = match repo.head() {
            Ok(head) => {
                let oid = head
                    .target()
                    .ok_or_else(|| GitError::Lib("HEAD has no target".into()))?;
                let commit = repo
                    .find_commit(oid)
                    .map_err(|e| GitError::Lib(e.message().to_string()))?;
                match commit.tree().and_then(|t| t.get_path(rel)) {
                    Ok(entry) => entry
                        .to_object(&repo)
                        .ok()
                        .and_then(|o| o.as_blob().map(|b| b.content().to_vec()))
                        .and_then(|bytes| String::from_utf8(bytes).ok())
                        .unwrap_or_default(),
                    Err(_) => String::new(), // HEAD 中不存在（新文件）
                }
            }
            Err(_) => String::new(), // 未出生 HEAD（空仓）：一切皆新增
        };

        let fs_path = self.workdir.join(&path);
        let mut modified = if fs_path.is_file() {
            std::fs::read(&fs_path)
                .map_err(|e| GitError::Io(e.to_string()))
                .and_then(|bytes| {
                    String::from_utf8(bytes)
                        .map_err(|_| GitError::Invalid(format!("binary or non-utf8 file: {path}")))
                })?
        } else {
            String::new() // 工作区已删除 → modified 为空 = 全删除
        };

        // autocrlf 语义对齐：库内 blob 为 LF、工作区为 CRLF 时，git 的
        // clean filter 在比较前把工作区 CRLF→LF（`git diff` 对 clean 文件输出空）。
        // 手工取两版全文绕过了 filter，会在 autocrlf=true/input 的 Windows 环境
        // 对 clean 文件产生「全行假差异」——此处按同语义规范化工作区内容：
        // 仅当 HEAD 侧 blob 不含 CR（入库为 LF）且 core.autocrlf 为 true/input。
        // 已知覆盖边界：`.gitattributes` 的 text/eol 自定义属性不展开（登记册留痕）。
        if !original.is_empty() && !original.contains('\r') && modified.contains("\r\n") {
            if let Ok(mode) = repo.config().and_then(|c| c.get_string("core.autocrlf")) {
                let m = mode.to_ascii_lowercase();
                if m == "true" || m == "input" || m == "1" || m == "yes" || m == "on" {
                    modified = modified.replace("\r\n", "\n");
                }
            }
        }

        if original.is_empty() && modified.is_empty() {
            return Err(GitError::Invalid(format!("no content to diff for: {path}")));
        }
        Ok(GitDiff {
            original,
            modified,
            language: language_id(&path).to_string(),
        })
    }

    /// 暂存（文件走 add_path；折叠 untracked 目录走 add_all pathspec）
    pub fn stage(&self, paths: &[String]) -> Result<usize, GitError> {
        let repo = self.open()?;
        let mut index = repo
            .index()
            .map_err(|e| GitError::Lib(e.message().to_string()))?;
        let mut n = 0;
        for raw in paths {
            let path = self.normalize(raw)?;
            if path.ends_with('/') {
                let dir = path.trim_end_matches('/');
                let spec_dir = dir.to_string();
                let spec_deep = format!("{dir}/**");
                let specs = vec![spec_dir.as_str(), spec_deep.as_str()];
                index
                    .add_all(specs, IndexAddOption::DEFAULT, None)
                    .map_err(|e| GitError::Lib(e.message().to_string()))?;
            } else {
                index
                    .add_path(Path::new(&path))
                    .map_err(|e| GitError::Lib(e.message().to_string()))?;
            }
            n += 1;
        }
        index
            .write()
            .map_err(|e| GitError::Lib(e.message().to_string()))?;
        Ok(n)
    }

    /// 取消暂存（index → HEAD 态；reset_default；空仓 unborn HEAD = 从 index 移除）
    pub fn unstage(&self, paths: &[String]) -> Result<usize, GitError> {
        let repo = self.open()?;
        let specs: Vec<String> = paths
            .iter()
            .map(|p| self.normalize(p))
            .collect::<Result<_, _>>()?;
        let mut index = repo
            .index()
            .map_err(|e| GitError::Lib(e.message().to_string()))?;
        match repo.head().ok().and_then(|h| h.target()) {
            Some(oid) => {
                let commit = repo
                    .find_commit(oid)
                    .map_err(|e| GitError::Lib(e.message().to_string()))?;
                let obj = commit.as_object();
                let refs: Vec<&str> = specs.iter().map(|s| s.as_str()).collect();
                repo.reset_default(Some(obj), refs.iter().copied())
                    .map_err(|e| GitError::Lib(e.message().to_string()))?;
            }
            None => {
                // 空仓无 HEAD：unstage = index 条目移除（等价 git rm --cached）
                for spec in &specs {
                    index
                        .remove_path(Path::new(spec))
                        .map_err(|e| GitError::Lib(e.message().to_string()))?;
                }
                index
                    .write()
                    .map_err(|e| GitError::Lib(e.message().to_string()))?;
            }
        }
        Ok(specs.len())
    }

    /// 丢弃工作区变更（恢复到 index 态；untracked = 删除文件）——危险操作，
    /// 前端强制确认；unstage 可找回 index 兜底，未 stage 的丢弃不可逆是 git 语义
    pub fn discard(&self, paths: &[String]) -> Result<usize, GitError> {
        let repo = self.open()?;
        let mut index = repo
            .index()
            .map_err(|e| GitError::Lib(e.message().to_string()))?;
        let mut n = 0;
        for raw in paths {
            let path = self.normalize(raw)?;
            let rel = Path::new(&path);
            if index.get_path(rel, 0).is_some() {
                // index 已跟踪：checkout_index 强制恢复该路径到 index 态
                let mut opts = git2::build::CheckoutBuilder::new();
                opts.force().update_index(false).path(path.as_str());
                repo.checkout_index(Some(&mut index), Some(&mut opts))
                    .map_err(|e| GitError::Lib(e.message().to_string()))?;
            } else {
                // untracked（不在 index）：删除文件 / 折叠目录整删
                let fs_path = self.workdir.join(&path);
                if path.ends_with('/') {
                    if fs_path.is_dir() {
                        std::fs::remove_dir_all(&fs_path)
                            .map_err(|e| GitError::Io(e.to_string()))?;
                    }
                } else if fs_path.is_file() {
                    std::fs::remove_file(&fs_path).map_err(|e| GitError::Io(e.to_string()))?;
                }
            }
            n += 1;
        }
        Ok(n)
    }

    /// 读提交身份（user.name + user.email；任一缺失/为空 → None）
    ///
    /// 只读不写——serve 代写 git config 越权，明确不做（板块设计档）。
    pub fn identity(&self) -> Result<Option<(String, String)>, GitError> {
        let repo = self.open()?;
        let config = repo
            .config()
            .map_err(|e| GitError::Lib(e.message().to_string()))?;
        let name = config_string(&config, "user.name");
        let email = config_string(&config, "user.email");
        match (name, email) {
            (Some(n), Some(e)) if !n.is_empty() && !e.is_empty() => Ok(Some((n, e))),
            _ => Ok(None),
        }
    }

    /// 检测提交 hooks（pre-commit / commit-msg 任一存在且可执行）
    ///
    /// Windows 无可执行位语义 → 仅存在性判定（Windows 场景 hooks 少见）。
    pub fn has_commit_hooks(&self) -> Result<bool, GitError> {
        let hooks = self.workdir.join(".git").join("hooks");
        for name in ["pre-commit", "commit-msg"] {
            let p = hooks.join(name);
            if p.is_file() {
                if cfg!(windows) {
                    return Ok(true);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    if let Ok(meta) = std::fs::metadata(&p) {
                        if meta.permissions().mode() & 0o111 != 0 {
                            return Ok(true);
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    /// 提交（统一入口）：身份预检 → hooks 检测分流（CLI 回退 / git2 原生）
    ///
    /// 提交语义 = **全量暂存后提交**（stage 一切含 untracked，尊重 .gitignore）；
    /// CLI 路径以 `git add -A` 保证与原生路径 `index.add_all` 语义一致。
    pub fn commit(&self, message: &str) -> Result<String, GitError> {
        if message.trim().is_empty() {
            return Err(GitError::Invalid("commit message is empty".into()));
        }
        if self.identity()?.is_none() {
            return Err(GitError::IdentityMissing);
        }
        if self.has_commit_hooks()? {
            self.commit_cli(message)
        } else {
            self.commit_native(message)
        }
    }

    /// git2 原生提交（无 hooks 路径）：add_all → write_tree → commit → HEAD 前移
    fn commit_native(&self, message: &str) -> Result<String, GitError> {
        let repo = self.open()?;
        let (name, email) = self.identity()?.expect("identity prechecked by commit()");
        let sig = git2::Signature::now(&name, &email)
            .map_err(|e| GitError::Lib(e.message().to_string()))?;

        let mut index = repo
            .index()
            .map_err(|e| GitError::Lib(e.message().to_string()))?;
        index
            .add_all(["*"], IndexAddOption::DEFAULT, None)
            .map_err(|e| GitError::Lib(e.message().to_string()))?;
        index
            .write()
            .map_err(|e| GitError::Lib(e.message().to_string()))?;
        let tree_id = index
            .write_tree()
            .map_err(|e| GitError::Lib(e.message().to_string()))?;
        let tree = repo
            .find_tree(tree_id)
            .map_err(|e| GitError::Lib(e.message().to_string()))?;

        let oid = match repo.head().ok().and_then(|h| h.target()) {
            Some(parent_oid) => {
                let parent = repo
                    .find_commit(parent_oid)
                    .map_err(|e| GitError::Lib(e.message().to_string()))?;
                repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
            }
            None => {
                repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[]) // 首个提交
            }
        }
        .map_err(|e| GitError::Lib(e.message().to_string()))?;
        Ok(oid.to_string())
    }

    /// CLI 回退提交（hooks 保真路径）：`git add -A` + `git commit --file=-`（stdin 传消息）
    ///
    /// 硬性约束（板块设计档 DoD）：argv 数组构造 / 无 shell / 仅白名单子命令
    /// （add·commit）/ 消息走 stdin（防参数长度与转义）/ 30s 超时 / 工作目录锁死 workdir。
    fn commit_cli(&self, message: &str) -> Result<String, GitError> {
        run_git_cli(&self.workdir, &["add", "-A"], None)?;
        run_git_cli(&self.workdir, &["commit", "--file=-"], Some(message))?;
        // 提交成功 → 回读新 HEAD id（git2 与 CLI 共享同一仓库文件）
        let repo = self.open()?;
        let oid = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .ok_or_else(|| GitError::Lib("HEAD missing after commit".into()))?;
        Ok(oid.to_string())
    }

    /// 提交历史（agent `git_log` 只读消费面；新→旧，limit 上限 500）
    ///
    /// 未出生 HEAD（空仓零提交）→ 空列表（"没有历史"不是错误）
    pub fn log(&self, limit: usize) -> Result<Vec<CommitInfo>, GitError> {
        let limit = limit.clamp(1, 500);
        let repo = self.open()?;
        if repo.head().is_err() {
            return Ok(Vec::new());
        }
        let mut walk = repo
            .revwalk()
            .map_err(|e| GitError::Lib(e.message().to_string()))?;
        walk.push_head()
            .map_err(|e| GitError::Lib(e.message().to_string()))?;
        let mut out = Vec::new();
        for oid in walk.take(limit) {
            let oid = oid.map_err(|e| GitError::Lib(e.message().to_string()))?;
            let commit = repo
                .find_commit(oid)
                .map_err(|e| GitError::Lib(e.message().to_string()))?;
            let author = commit.author();
            out.push(CommitInfo {
                id: oid.to_string(),
                short_id: oid.to_string()[..7].to_string(),
                author: author.name().unwrap_or("").to_string(),
                email: author.email().unwrap_or("").to_string(),
                time: commit.time().seconds(),
                summary: commit.summary().unwrap_or("").to_string(),
            });
        }
        Ok(out)
    }
}

/// rename 条目的展示路径：优先 delta 新路径（head_to_index / index_to_workdir），
/// 无 delta（含 delta 内无路径）回落 entry.path
fn rename_new_path(delta: Option<git2::DiffDelta>, fallback: Option<&str>) -> String {
    let from_delta = delta
        .as_ref()
        .and_then(|d| d.new_file().path())
        .map(|p| p.to_string_lossy().replace('\\', "/"));
    from_delta
        .or_else(|| fallback.map(|f| f.to_string()))
        .unwrap_or_default()
}

/// 读 config 字符串（缺失 → None；空串原样返回由调用方按缺失处理）
fn config_string(config: &git2::Config, key: &str) -> Option<String> {
    config.get_string(key).ok()
}

/// 扩展名 → Monaco 语言 id（与前端 langOf 同映射的 serve 侧最小投影）
fn language_id(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "md" | "markdown" => "markdown",
        "json" => "json",
        "toml" => "ini",
        "rs" => "rust",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "svelte" | "html" | "htm" | "vue" => "html",
        "css" | "scss" | "less" => "css",
        "py" => "python",
        "yml" | "yaml" => "yaml",
        "sh" | "bash" => "shell",
        "ps1" => "powershell",
        "sql" => "sql",
        "xml" => "xml",
        _ => "plaintext",
    }
}

/// 运行 git CLI 子命令（白名单调用方：`add -A` / `commit --file=-`）
///
/// 安全约束：argv 数组直接 exec（无 shell 解析）/ stdin 定向注入 / 30s 超时
/// kill / 工作目录锁死 workdir / stdout·stderr 由独立线程排空（防管道填满死锁）。
fn run_git_cli(workdir: &Path, args: &[&str], stdin_data: Option<&str>) -> Result<(), GitError> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                GitError::CliFailed(
                    "git executable not found in PATH (CLI fallback unavailable)".into(),
                )
            } else {
                GitError::Io(format!("spawn git failed: {e}"))
            }
        })?;

    // stdin 注入后立即关闭（`commit --file=-` 读到 EOF 收尾）
    if let Some(data) = stdin_data {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(data.as_bytes())
                .map_err(|e| GitError::Io(format!("write git stdin failed: {e}")))?;
        }
    }
    child.stdin.take(); // 显式关闭 stdin

    // 独立线程排空两根管道，等待期间不死锁
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_string(&mut buf);
        }
        buf
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if start.elapsed() > CLI_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GitError::CliFailed(format!(
                        "git {} timed out after {}s",
                        args.join(" "),
                        CLI_TIMEOUT.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(GitError::Io(format!("wait git failed: {e}"))),
        }
    };
    let _stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    let status = status.ok_or_else(|| GitError::Io("git child status lost".into()))?;

    if !status.success() {
        return Err(GitError::CliFailed(stderr_tail(&stderr)));
    }
    Ok(())
}

/// stderr 尾部截取（保留离错误最近的部分；字符边界安全）
fn stderr_tail(s: &str) -> String {
    let s = s.trim_end();
    if s.len() <= STDERR_TAIL_BYTES {
        return s.to_string();
    }
    let mut start = s.len() - STDERR_TAIL_BYTES;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

// =====================================================================================
// 测试：临时仓夹具（git2::Repository::init 构仓，不经 CLI；CLI 回退用真实 hook 验证）
// =====================================================================================
#[cfg(test)]
mod tests {
    use super::*;

    /// 构造临时 git 仓（未设身份——需要身份的用例自行写入 local config）
    fn fresh_repo() -> (tempfile::TempDir, GitOps) {
        let dir = tempfile::tempdir().unwrap();
        let ops = GitOps::new(dir.path().to_path_buf());
        // GitOps::new 对非仓库静默跳过 exclude；init 后再补一次
        let repo = Repository::init(dir.path()).unwrap();
        // 稳定测试：关闭 gpg / 隔离全局配置干扰（local 优先级最高）
        repo.config().unwrap().set_bool("core.autocrlf", false).ok();
        (dir, ops)
    }

    /// 写身份到 local config（隔离宿主全局 user.name/email 干扰）
    fn set_identity(ops: &GitOps, name: &str, email: &str) {
        let repo = Repository::open(&ops.workdir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", name).unwrap();
        cfg.set_str("user.email", email).unwrap();
    }

    /// 用 git2 原生路径做一次初始提交（夹具基建）
    fn seed_commit(ops: &GitOps, file: &str, content: &str, msg: &str) {
        std::fs::write(ops.workdir.join(file), content).unwrap();
        ops.stage(&[file.to_string()]).unwrap();
        ops.commit(msg).unwrap();
    }

    fn find<'a>(list: &'a [StatusEntry], path: &str) -> Option<&'a StatusEntry> {
        list.iter().find(|e| e.path == path)
    }

    #[test]
    fn test_status_double_state_and_untracked() {
        let (_d, ops) = fresh_repo();
        set_identity(&ops, "T", "t@example.com");
        seed_commit(&ops, "a.txt", "one\n", "init");

        // 修改已跟踪文件 + 新增 untracked 文件
        std::fs::write(ops.workdir.join("a.txt"), "one-two\n").unwrap();
        std::fs::write(ops.workdir.join("b.txt"), "new\n").unwrap();

        let st = ops.status().unwrap();
        assert_eq!(st.branch, "master");
        assert!(st.dirty);
        let a = find(&st.changes, "a.txt").expect("a.txt in changes");
        assert_eq!(a.status, 'M');
        let b = find(&st.changes, "b.txt").expect("b.txt in changes");
        assert_eq!(b.status, 'U');
        assert!(st.staged.is_empty(), "nothing staged yet");

        // 暂存 a.txt → staged M 且 changes 中消失（无进一步 workdir 改动）
        ops.stage(&["a.txt".to_string()]).unwrap();
        let st2 = ops.status().unwrap();
        let sa = find(&st2.staged, "a.txt").expect("a.txt staged");
        assert_eq!(sa.status, 'M');
        assert!(find(&st2.changes, "a.txt").is_none());
        assert!(
            find(&st2.changes, "b.txt").is_some(),
            "b.txt still untracked"
        );
    }

    #[test]
    fn test_status_rename_detection() {
        let (_d, ops) = fresh_repo();
        set_identity(&ops, "T", "t@example.com");
        seed_commit(&ops, "old.txt", "same content here\n", "init");

        // index 侧 rename：remove old + add new（内容相同 → rename 100% 相似度）
        let repo = Repository::open(&ops.workdir).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("old.txt")).unwrap();
        std::fs::write(ops.workdir.join("new.txt"), "same content here\n").unwrap();
        index.add_path(Path::new("new.txt")).unwrap();
        index.write().unwrap();

        let st = ops.status().unwrap();
        let r = find(&st.staged, "new.txt").expect("rename shown at new path");
        assert_eq!(
            r.status, 'R',
            "staged rename detected: staged={:?}",
            st.staged
        );
    }

    #[test]
    fn test_untracked_dir_folded_single_entry() {
        let (_d, ops) = fresh_repo();
        set_identity(&ops, "T", "t@example.com");
        std::fs::create_dir(ops.workdir.join("bundle")).unwrap();
        std::fs::write(ops.workdir.join("bundle/x.txt"), "x").unwrap();
        std::fs::write(ops.workdir.join("bundle/y.txt"), "y").unwrap();

        let st = ops.status().unwrap();
        assert_eq!(st.changes.len(), 1, "untracked dir folds to one entry");
        assert_eq!(st.changes[0].path, "bundle/");
        assert_eq!(st.changes[0].status, 'U');
    }

    #[test]
    fn test_stage_dir_pathspec_and_unstage_roundtrip() {
        let (_d, ops) = fresh_repo();
        set_identity(&ops, "T", "t@example.com");
        std::fs::create_dir(ops.workdir.join("bundle")).unwrap();
        std::fs::write(ops.workdir.join("bundle/x.txt"), "x").unwrap();

        // 折叠目录条目按目录整体暂存
        ops.stage(&["bundle/".to_string()]).unwrap();
        let st = ops.status().unwrap();
        assert!(
            find(&st.staged, "bundle/x.txt").is_some(),
            "file staged via dir"
        );
        assert!(st.changes.is_empty());

        // unstage 回到 untracked
        ops.unstage(&["bundle/x.txt".to_string()]).unwrap();
        let st2 = ops.status().unwrap();
        assert!(
            find(&st2.changes, "bundle/").is_some(),
            "back to untracked fold"
        );
        assert!(st2.staged.is_empty());
    }

    #[test]
    fn test_discard_restores_index_state_and_removes_untracked() {
        let (_d, ops) = fresh_repo();
        set_identity(&ops, "T", "t@example.com");
        seed_commit(&ops, "a.txt", "base\n", "init");

        // 已跟踪文件修改 → discard 恢复
        std::fs::write(ops.workdir.join("a.txt"), "changed\n").unwrap();
        ops.discard(&["a.txt".to_string()]).unwrap();
        assert_eq!(
            std::fs::read_to_string(ops.workdir.join("a.txt")).unwrap(),
            "base\n"
        );
        let st = ops.status().unwrap();
        assert!(!st.dirty, "clean after discard");

        // 已跟踪文件删除 → discard 恢复文件
        std::fs::remove_file(ops.workdir.join("a.txt")).unwrap();
        ops.discard(&["a.txt".to_string()]).unwrap();
        assert!(ops.workdir.join("a.txt").is_file(), "deleted file restored");

        // untracked → discard 删除
        std::fs::write(ops.workdir.join("junk.txt"), "tmp").unwrap();
        ops.discard(&["junk.txt".to_string()]).unwrap();
        assert!(!ops.workdir.join("junk.txt").exists());
    }

    #[test]
    fn test_diff_tracked_and_untracked() {
        let (_d, ops) = fresh_repo();
        set_identity(&ops, "T", "t@example.com");
        seed_commit(&ops, "a.rs", "fn old() {}\n", "init");

        // 已跟踪修改：original = HEAD 版
        std::fs::write(ops.workdir.join("a.rs"), "fn new() {}\n").unwrap();
        let d = ops.diff("a.rs").unwrap();
        assert_eq!(d.original, "fn old() {}\n");
        assert_eq!(d.modified, "fn new() {}\n");
        assert_eq!(d.language, "rust");

        // untracked：original 空串 = 全新增
        std::fs::write(ops.workdir.join("b.md"), "# hi\n").unwrap();
        let d2 = ops.diff("b.md").unwrap();
        assert_eq!(d2.original, "");
        assert_eq!(d2.modified, "# hi\n");
        assert_eq!(d2.language, "markdown");
    }

    #[test]
    fn test_diff_autocrlf_clean_file_no_false_changes() {
        // autocrlf=true 时库内 LF blob vs 工作区 CRLF，clean 文件必须
        // 判空差异（对齐 `git diff` 的 clean filter 语义），真修改照常显示
        let (_d, ops) = fresh_repo();
        set_identity(&ops, "T", "t@example.com");
        {
            let repo = ops.open().unwrap();
            let mut cfg = repo.config().unwrap();
            cfg.set_str("core.autocrlf", "true").unwrap();
        }
        seed_commit(&ops, "a.rs", "fn old() {}\n", "init");

        // clean（内容一致，仅换行形态不同）→ 两版相等 = 无差异
        std::fs::write(ops.workdir.join("a.rs"), "fn old() {}\r\n").unwrap();
        let d = ops.diff("a.rs").unwrap();
        assert_eq!(d.original, "fn old() {}\n");
        assert_eq!(d.modified, "fn old() {}\n");

        // 真修改（CRLF 工作区 + 内容不同）→ 规范化后仍显示真实差异
        std::fs::write(ops.workdir.join("a.rs"), "fn new() {}\r\n").unwrap();
        let d2 = ops.diff("a.rs").unwrap();
        assert_eq!(d2.original, "fn old() {}\n");
        assert_eq!(d2.modified, "fn new() {}\n");

        // autocrlf 未设（缺省 false）→ 不规范化，保持字节级两版全文
        {
            let repo = ops.open().unwrap();
            let mut cfg = repo.config().unwrap();
            cfg.set_str("core.autocrlf", "false").unwrap();
        }
        std::fs::write(ops.workdir.join("a.rs"), "fn old() {}\r\n").unwrap();
        let d3 = ops.diff("a.rs").unwrap();
        assert_eq!(d3.modified, "fn old() {}\r\n");
    }

    #[test]
    fn test_exclude_isolation_idempotent_and_gitignore_untouched() {
        let (_d, ops) = fresh_repo();
        // GitOps::new 已执行一次；再显式执行两次 → 幂等且只追加一条
        ops.ensure_exclude().unwrap();
        let content = std::fs::read_to_string(ops.workdir.join(".git/info/exclude")).unwrap();
        let hits = content.lines().filter(|l| l.trim() == TRASH_DIR).count();
        assert_eq!(hits, 1, "exactly one .evo-trash/ entry, got: {content:?}");

        // 不污染用户 .gitignore（文件根本不该被创建）
        assert!(!ops.workdir.join(".gitignore").exists());

        // 已有内容时追加保留原文
        let exclude = ops.workdir.join(".git/info/exclude");
        std::fs::write(&exclude, "# custom\n*.tmp\n").unwrap();
        ops.ensure_exclude().unwrap();
        let content = std::fs::read_to_string(&exclude).unwrap();
        assert!(
            content.starts_with("# custom\n*.tmp\n"),
            "original preserved"
        );
        assert!(content.contains(TRASH_DIR));
    }

    #[test]
    fn test_status_rejects_non_repo_and_worktree_form() {
        // 非 git 目录 → NotARepository
        let dir = tempfile::tempdir().unwrap();
        let ops = GitOps::new(dir.path().to_path_buf());
        assert!(matches!(ops.status(), Err(GitError::NotARepository)));

        // .git 为文件（worktree/submodule 形态）→ NotSupported 明确拒绝
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(dir2.path().join(".git"), "gitdir: elsewhere\n").unwrap();
        let ops2 = GitOps::new(dir2.path().to_path_buf());
        assert!(matches!(ops2.status(), Err(GitError::NotSupported(_))));
    }

    #[test]
    fn test_identity_missing_structured_error() {
        let (_d, ops) = fresh_repo();
        set_identity(&ops, "", ""); // local 显式置空 = 模拟身份缺失（隔离全局配置）

        assert!(ops.identity().unwrap().is_none());
        let err = ops.commit("msg").unwrap_err();
        assert!(matches!(err, GitError::IdentityMissing));
        assert_eq!(err.message(), "identity_missing");
        assert!(err.hint().unwrap().contains("git config"));
    }

    #[test]
    fn test_commit_native_roundtrip_and_log() {
        let (_d, ops) = fresh_repo();
        set_identity(&ops, "Tester", "t@example.com");

        std::fs::write(ops.workdir.join("a.txt"), "v1\n").unwrap();
        ops.stage(&["a.txt".to_string()]).unwrap();
        let id = ops.commit("first commit").unwrap();
        assert_eq!(id.len(), 40);

        // 提交后 status 干净 + log 首条即新提交
        let st = ops.status().unwrap();
        assert!(!st.dirty, "clean after commit");
        let log = ops.log(10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].id, id);
        assert_eq!(log[0].short_id, &id[..7]);
        assert_eq!(log[0].author, "Tester");
        assert_eq!(log[0].summary, "first commit");

        // 空消息拦截
        let err = ops.commit("   ").unwrap_err();
        assert!(matches!(err, GitError::Invalid(_)));
    }

    #[cfg(unix)]
    #[test]
    fn test_commit_cli_fallback_on_hook_rejection_and_success() {
        let (_d, ops) = fresh_repo();
        set_identity(&ops, "Tester", "t@example.com");
        std::fs::write(ops.workdir.join("a.txt"), "v1\n").unwrap();

        // 伪 pre-commit（exit 1）→ 提交失败透传 hook 输出
        let hook = ops.workdir.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho blocked-by-test-hook >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        assert!(ops.has_commit_hooks().unwrap());
        let err = ops.commit("should fail").unwrap_err();
        assert!(matches!(err, GitError::CliFailed(_)), "got: {err:?}");
        assert!(err.message().contains("blocked-by-test-hook"));

        // hook 通过 → CLI 提交成功且消息经 stdin 完整落库
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
        let id = ops.commit("cli committed message").unwrap();
        let repo = Repository::open(&ops.workdir).unwrap();
        let head = repo
            .find_commit(repo.head().unwrap().target().unwrap())
            .unwrap();
        assert_eq!(head.id().to_string(), id);
        assert_eq!(head.summary().unwrap(), "cli committed message");
        let st = ops.status().unwrap();
        assert!(!st.dirty, "clean after cli commit");
    }

    #[test]
    fn test_stderr_tail_char_boundary() {
        assert_eq!(stderr_tail("short"), "short");
        let long = "啊".repeat(600); // 1800 bytes > 800
        let tail = stderr_tail(&long);
        assert!(tail.len() <= STDERR_TAIL_BYTES + 10);
        assert!(tail.starts_with('…'));
        assert!(tail.chars().all(|c| c == '…' || c == '啊'), "no mojibake");
    }

    #[test]
    fn test_normalize_rejects_bad_paths() {
        let (_d, ops) = fresh_repo();
        assert!(matches!(ops.normalize(""), Err(GitError::Invalid(_))));
        assert!(matches!(ops.normalize("..\\x"), Err(GitError::Invalid(_))));
        assert!(matches!(ops.normalize("/abs"), Err(GitError::Invalid(_))));
        assert_eq!(ops.normalize("a\\b.txt").unwrap(), "a/b.txt");
    }
}
