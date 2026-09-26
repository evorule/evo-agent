// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `grep_files` —— 全项目内容搜索(工作目录沙箱,rg 官方 crate 家族)
//!
//! ## 安全模型(与 search_files 同款沙箱)
//! - 路径**相对** workdir;绝对路径/`..` 段/canonicalize 越界一律拒绝
//! - 默认排除集始终生效(`.git/**`/`target/**`/`node_modules/**`/
//!   `.evo-trash/**`/`data/**`——与 watcher 排除目录对齐,不向结果泄露
//!   工作台内部数据面)
//! - gitignore 尊重开关(默认开;无 git 仓时由 ignore crate 语义自然降级)
//! - 二进制文件跳过(NUL 探测 quit 模式)
//! - max_results 截断 + 30s 硬超时(返回已收集的 partial 结果)
//!
//! ## 分层
//! - [`grep_core`]:核心层,供 REST 端点薄委托(不进 agent 审计链的人工面
//!   与 agent 面共用同一实现);
//! - [`GrepFilesTool`]:agent 工具包装(spawn_blocking,与 file 工具族同构,
//!   经 serve 面白名单+agentTools.grep 开关过滤后暴露);
//! - [`replace_core`]:替换核心(REST-only 人工面写操作,不注册进 agent toolkit)
//!
//! ## 替换(replace_core)
//! - `apply=false`:同参数**重新匹配** → per-hit before/after 预览,零写盘
//! - `apply=true`:**重新匹配**(不信任预览时快照)→ 逐文件内存替换 →
//!   tmp+rename 原子写;全程持全局树写锁(与 file 增删改人工面共用单例;
//!   blocking_lock,必须在 spawn_blocking 中调用)
//! - 捕获组展开:`$1`/`${name}`/`$$` 经 grep-matcher `Captures::interpolate`
//!   (ripgrep 官方替换同源语法,与搜索 matcher 同一编译产物,零方言漂移)
//! - 非 UTF-8 文件拒绝写回(进 failed 列表),保护二进制内容
//!
//! ## 坐标约定
//! - `line`:1-based 行号(grep-searcher 原生)
//! - `col`/`endCol`:**行内 UTF-8 char 偏移,0-based**(byte 索引给前端会在
//!   多字节行错位/越界);Monaco 等前端消费者按需 +1 转 1-based 列
//! - 非 UTF-8 行按 lossy 解码降级搜索,列偏移与 preview 同源自洽

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grep_matcher::{Captures, Matcher};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 默认最大结果数(与 search_files max_results 量级一致)
pub const DEFAULT_MAX_RESULTS: usize = 1000;

/// maxResults 硬上限(设置键 search.maxResults 上限同域;防误传巨值撑爆内存,
/// 30s 硬超时之外的第二道防线)
pub const MAX_MAX_RESULTS: usize = 20_000;

/// 硬超时:超时即停止遍历,返回已收集的 partial 结果(truncated+timedOut)
pub const TIMEOUT_SECS: u64 = 30;

/// 出厂默认排除集(始终叠加;与 watcher 排除目录对齐)
pub const DEFAULT_EXCLUDE_GLOBS: &[&str] = &[
    ".git/**",
    "target/**",
    "node_modules/**",
    ".evo-trash/**",
    "data/**",
];

/// 搜索参数(REST body 与 agent 工具 args 共用一形;camelCase/snake_case 双认)
#[derive(Debug, Clone)]
pub struct GrepParams {
    pub query: String,
    pub is_regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub smart_case: bool,
    pub dir: String,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub use_ignore_files: bool,
    pub max_results: usize,
}

impl GrepParams {
    /// 从 JSON args/body 解析(缺省值见字段默认;query 必填)
    pub fn from_args(args: &Value) -> Result<Self, String> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: query (string)".to_string())?
            .to_string();
        if query.is_empty() {
            return Err("query must not be empty".to_string());
        }
        let get_bool = |camel: &str, snake: &str, default: bool| {
            args.get(camel)
                .or_else(|| args.get(snake))
                .and_then(|v| v.as_bool())
                .unwrap_or(default)
        };
        let get_strings = |camel: &str, snake: &str| -> Vec<String> {
            args.get(camel)
                .or_else(|| args.get(snake))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let max_results = args
            .get("maxResults")
            .or_else(|| args.get("max_results"))
            .and_then(|v| v.as_u64())
            .map(|n| (n.max(1) as usize).min(MAX_MAX_RESULTS))
            .unwrap_or(DEFAULT_MAX_RESULTS);
        let dir = args
            .get("dir")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        Ok(Self {
            query,
            is_regex: get_bool("isRegex", "is_regex", false),
            case_sensitive: get_bool("caseSensitive", "case_sensitive", false),
            whole_word: get_bool("wholeWord", "whole_word", false),
            smart_case: get_bool("smartCase", "smart_case", true),
            dir,
            include_globs: get_strings("includeGlobs", "include_globs"),
            exclude_globs: get_strings("excludeGlobs", "exclude_globs"),
            use_ignore_files: get_bool("useIgnoreFiles", "use_ignore_files", true),
            max_results,
        })
    }
}

/// 核心层:执行一次内容搜索,返回分组 JSON
///
/// 取消令牌可选(REST 断连取消;agent 面传 None)。30s 硬超时始终生效。
pub fn grep_core(
    workdir: &Path,
    params: &GrepParams,
    cancel: Option<&CancellationToken>,
) -> Result<Value, String> {
    let started = Instant::now();
    let root = resolve_safe_dir(workdir, &params.dir)?;
    let env = build_search_env(&root, params)?;

    let shared = Arc::new(Capture {
        env,
        groups: Mutex::new(CaptureState {
            total: 0,
            groups: BTreeMap::new(),
        }),
        searched: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
        truncated: AtomicBool::new(false),
        timed_out: AtomicBool::new(false),
        deadline: started + Duration::from_secs(TIMEOUT_SECS),
        max_results: params.max_results,
        cancel: cancel.cloned(),
    });

    build_walker(&shared.env).build_parallel().run(|| {
        let capture = Arc::clone(&shared);
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .build();
        Box::new(move |entry: Result<ignore::DirEntry, ignore::Error>| {
            visit_entry(&capture, &mut searcher, entry)
        })
    });

    let guard = shared
        .groups
        .lock()
        .map_err(|_| "capture state poisoned".to_string())?;
    let total = guard.total;
    let groups: Vec<Value> = guard
        .groups
        .iter()
        .map(|(path, hits)| json!({ "path": path, "hits": hits }))
        .collect();
    drop(guard);
    Ok(json!({
        "groups": groups,
        "totalMatches": total,
        "fileCount": groups.len(),
        "truncated": shared.truncated.load(Ordering::SeqCst),
        "timedOut": shared.timed_out.load(Ordering::SeqCst),
        "searchedFiles": shared.searched.load(Ordering::SeqCst),
        "elapsedMs": started.elapsed().as_millis() as u64,
    }))
}

/// 编译后的搜索环境(根目录+matcher+glob 集):grep_core 与 replace_core 共用
struct SearchEnv {
    root: PathBuf,
    matcher: grep_regex::RegexMatcher,
    use_ignore_files: bool,
    overrides: ignore::overrides::Override,
}

fn build_search_env(root: &Path, params: &GrepParams) -> Result<SearchEnv, String> {
    // query 编译:字面模式转义;smart-case 全小写查询自动不敏感
    let pattern = if params.is_regex {
        params.query.clone()
    } else {
        regex::escape(&params.query)
    };
    // 大小写三态:显式敏感 > smart-case(全小写查询自动不敏感) > 不敏感
    // (case_insensitive(true) 会覆盖 case_smart,三者必须互斥设置)
    let mut mb = RegexMatcherBuilder::new();
    if params.case_sensitive {
        mb.case_insensitive(false).case_smart(false);
    } else if params.smart_case {
        mb.case_insensitive(false).case_smart(true);
    } else {
        mb.case_insensitive(true).case_smart(false);
    }
    let matcher = mb
        .word(params.whole_word)
        .build(&pattern)
        .map_err(|e| format!("invalid regex: {e}"))?;

    // include/exclude glob:白名单命中才搜,!前缀排除;默认排除集恒叠加
    let mut ovb = OverrideBuilder::new(root);
    for g in &params.include_globs {
        ovb.add(g)
            .map_err(|e| format!("invalid include glob '{g}': {e}"))?;
    }
    let mut all_excludes: Vec<String> = params.exclude_globs.clone();
    for d in DEFAULT_EXCLUDE_GLOBS {
        all_excludes.push((*d).to_string());
    }
    for g in &all_excludes {
        let neg = format!("!{g}");
        ovb.add(&neg)
            .map_err(|e| format!("invalid exclude glob '{g}': {e}"))?;
    }
    let overrides = ovb.build().map_err(|e| format!("invalid glob set: {e}"))?;
    Ok(SearchEnv {
        root: root.to_path_buf(),
        matcher,
        use_ignore_files: params.use_ignore_files,
        overrides,
    })
}

fn build_walker(env: &SearchEnv) -> WalkBuilder {
    let mut wb = WalkBuilder::new(&env.root);
    wb.hidden(true).overrides(env.overrides.clone());
    if env.use_ignore_files {
        wb.git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true)
            .parents(true);
    } else {
        wb.git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false);
    }
    wb
}

struct CaptureState {
    total: usize,
    groups: BTreeMap<String, Vec<Value>>,
}

struct Capture {
    env: SearchEnv,
    groups: Mutex<CaptureState>,
    searched: AtomicUsize,
    stop: AtomicBool,
    truncated: AtomicBool,
    timed_out: AtomicBool,
    deadline: Instant,
    max_results: usize,
    cancel: Option<CancellationToken>,
}

impl Capture {
    fn should_stop(&self) -> bool {
        if self.stop.load(Ordering::SeqCst) {
            return true;
        }
        if let Some(tok) = &self.cancel {
            if tok.is_cancelled() {
                self.stop.store(true, Ordering::SeqCst);
                return true;
            }
        }
        if Instant::now() >= self.deadline {
            self.stop.store(true, Ordering::SeqCst);
            self.timed_out.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }
}

fn visit_entry(
    capture: &Arc<Capture>,
    searcher: &mut Searcher,
    entry: Result<ignore::DirEntry, ignore::Error>,
) -> WalkState {
    if capture.should_stop() {
        return WalkState::Quit;
    }
    let entry = match entry {
        Ok(e) => e,
        // 遍历层错误(权限/竞态删除)静默跳过,与 search_files walk 同语义
        Err(_) => return WalkState::Continue,
    };
    if !entry.file_type().is_some_and(|ft| ft.is_file()) {
        return WalkState::Continue;
    }
    capture.searched.fetch_add(1, Ordering::SeqCst);
    let rel = entry
        .path()
        .strip_prefix(&capture.env.root)
        .unwrap_or(entry.path());
    let rel_display = rel.to_string_lossy().replace('\\', "/");
    let mut sink = HitSink {
        capture,
        rel: rel_display,
    };
    // 单文件搜索失败(权限/占用)跳过该文件,不中断整体
    let _ = searcher.search_path(&capture.env.matcher, entry.path(), &mut sink);
    if capture.stop.load(Ordering::SeqCst) {
        WalkState::Quit
    } else {
        WalkState::Continue
    }
}

struct HitSink<'a> {
    capture: &'a Arc<Capture>,
    rel: String,
}

impl Sink for HitSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        if self.capture.should_stop() {
            return Ok(false);
        }
        let line_no = mat.line_number().unwrap_or(0);
        let text = String::from_utf8_lossy(strip_terminator(mat.bytes())).into_owned();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let _ = self.capture.env.matcher.find_iter(text.as_bytes(), |m| {
            if m.start() != m.end() {
                // 空匹配(如 `a*`)会在行内每个位置命中,跳过防爆炸
                spans.push((m.start(), m.end()));
            }
            true
        });
        if spans.is_empty() {
            return Ok(true);
        }

        let mut state = self
            .capture
            .groups
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let remaining = self.capture.max_results.saturating_sub(state.total);
        let take = spans.len().min(remaining);
        if take == 0 {
            self.capture.stop.store(true, Ordering::SeqCst);
            self.capture.truncated.store(true, Ordering::SeqCst);
            return Ok(false);
        }
        let hits: Vec<Value> = spans[..take]
            .iter()
            .map(|&(s, e)| {
                let mut hit = Map::new();
                hit.insert("line".to_string(), json!(line_no));
                // char 偏移(0-based):regex 匹配边界天然对齐 char boundary
                hit.insert("col".to_string(), json!(text[..s].chars().count()));
                hit.insert("endCol".to_string(), json!(text[..e].chars().count()));
                hit.insert("preview".to_string(), json!(text));
                Value::Object(hit)
            })
            .collect();
        state
            .groups
            .entry(self.rel.clone())
            .or_default()
            .extend(hits);
        state.total += take;
        if state.total >= self.capture.max_results {
            self.capture.stop.store(true, Ordering::SeqCst);
            self.capture.truncated.store(true, Ordering::SeqCst);
        }
        Ok(true)
    }
}

/// 去掉行尾 \r\n(grep-searcher 行缓冲含终止符;列偏移按展示内容计)
fn strip_terminator(raw: &[u8]) -> &[u8] {
    let mut end = raw.len();
    while end > 0 && (raw[end - 1] == b'\n' || raw[end - 1] == b'\r') {
        end -= 1;
    }
    &raw[..end]
}

/// 沙箱解析(与 search_files 同款语义与文案):相对/拒 ../canonicalize containment
fn resolve_safe_dir(workdir: &Path, raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(format!(
            "absolute path not allowed: '{}' (all paths must stay within the sandbox boundary '{}')",
            raw,
            workdir.display()
        ));
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(format!(
                "parent dir (..) not allowed: '{}' (must stay within the sandbox boundary '{}')",
                raw,
                workdir.display()
            ));
        }
    }
    let joined = workdir.join(path);
    let canonical = joined
        .canonicalize()
        .map_err(|e| format!("dir does not exist or cannot resolve: {}", e))?;
    let workdir_canonical = workdir
        .canonicalize()
        .map_err(|e| format!("workdir invalid: {}", e))?;
    if !canonical.starts_with(&workdir_canonical) {
        return Err(format!(
            "path not accessible: '{}' resolves outside the sandbox boundary '{}'",
            raw,
            workdir_canonical.display()
        ));
    }
    Ok(canonical)
}

// =============================================================================
// 替换核心(REST-only 人工面写操作;不注册进 agent toolkit,不进 agent 审计链)
// =============================================================================

/// 替换参数:搜索参数 + replacement/apply/paths(REST body 形;camelCase/snake_case 双认)
#[derive(Debug, Clone)]
pub struct ReplaceParams {
    /// 搜索部分(与 grep_core 完全同参)
    pub search: GrepParams,
    /// 替换文本;正则模式下可含 `$1`/`${name}` 捕获组引用
    pub replacement: String,
    /// false=仅预览(零写盘);true=**重新匹配**后逐文件原子写
    pub apply: bool,
    /// 限定替换文件集(相对 workdir 的路径,前端「按所选文件替换」传勾选集);
    /// 空集=全部命中文件。路径分隔符归一为 `/` 比较。
    pub paths: Vec<String>,
}

impl ReplaceParams {
    /// 从 JSON body 解析;replacement 必填,apply 缺省 false
    pub fn from_args(args: &Value) -> Result<Self, String> {
        let search = GrepParams::from_args(args)?;
        let replacement = args
            .get("replacement")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: replacement (string)".to_string())?
            .to_string();
        let apply = args.get("apply").and_then(|v| v.as_bool()).unwrap_or(false);
        let paths = args
            .get("paths")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.replace('\\', "/")))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            search,
            replacement,
            apply,
            paths,
        })
    }
}

/// 单处替换编辑(byte span 相对「去行终止符」行文本;replacement=展开后文本;
/// before=匹配到的原文,apply 路径忽略仅预览消费)
struct HitEdit {
    start: usize,
    end: usize,
    replacement: String,
    before: String,
}

/// 单行编辑集合(1-based line_no)
struct LineEdits {
    line_no: usize,
    edits: Vec<HitEdit>,
}

/// 单文件匹配结果(收集阶段产物;apply 阶段按此重放写回)
struct FileEdits {
    rel: String,
    abs: PathBuf,
    lines: Vec<LineEdits>,
}

struct ReplaceState {
    files: BTreeMap<String, FileEdits>,
    match_count: usize,
}

struct ReplaceCapture {
    env: SearchEnv,
    replacement: String,
    is_regex: bool,
    paths: Vec<String>,
    state: Mutex<ReplaceState>,
    stop: AtomicBool,
    timed_out: AtomicBool,
    deadline: Instant,
    cancel: Option<CancellationToken>,
}

impl ReplaceCapture {
    fn should_stop(&self) -> bool {
        if self.stop.load(Ordering::SeqCst) {
            return true;
        }
        if let Some(tok) = &self.cancel {
            if tok.is_cancelled() {
                self.stop.store(true, Ordering::SeqCst);
                return true;
            }
        }
        if Instant::now() >= self.deadline {
            self.stop.store(true, Ordering::SeqCst);
            self.timed_out.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }
}

/// 核心层:全局搜索替换(预览/应用)。
///
/// - `apply=false`:与搜索同参数**重新匹配**,per-hit 生成 `before/after`,
///   零写盘;返回 `{preview:[{path,edits:[{line,before,after}]}], fileCount, matchCount}`
/// - `apply=true`:**重新匹配**(不信任预览时快照)→ 逐文件内存替换 →
///   tmp+rename 原子写;返回 `{appliedFiles, appliedMatches, failed:[{path,reason}]}`
///
/// ## 写锁
/// `apply=true` 全程(匹配+写回)持全局树写互斥锁
/// [`fs_safety::tree_mutation_lock`](crate::builtin_tools::fs_safety)——与 file
/// 增删改人工面共用单例,防并发变更产生中间态。tokio Mutex 经 `blocking_lock`
/// 获取:**必须在 spawn_blocking 中调用,禁在 async 上下文直接调用**(G13 惯例)。
///
/// ## 捕获组展开
/// `$1`/`${name}`/`$$` 经 grep-matcher `Captures::interpolate` 展开——与搜索
/// matcher 同一编译产物(零方言漂移),ripgrep 官方替换同源语法;无效组名
/// 展开为空串(标准语义)。
///
/// ## 保护边界
/// - 非 UTF-8 文件拒绝写回(进 `failed`),保护二进制内容
/// - 逐文件「读→匹配→替换→写」在锁内闭环,行内 byte span 切换校验
///   char boundary(防 `(?-u)` 字节模式切片越界 panic)
/// - 30s 硬超时/取消:停止遍历,已收集部分照常返回(apply 即部分应用,
///   `timedOut` 标注,用户重跑即可,git 兜底)
pub fn replace_core(
    workdir: &Path,
    params: &ReplaceParams,
    cancel: Option<&CancellationToken>,
) -> Result<Value, String> {
    let started = Instant::now();
    let root = resolve_safe_dir(workdir, &params.search.dir)?;

    // apply=true 持全局树写锁(预览只读不持锁)
    let _guard = if params.apply {
        Some(crate::builtin_tools::fs_safety::tree_mutation_lock().blocking_lock())
    } else {
        None
    };

    let env = build_search_env(&root, &params.search)?;
    let capture = Arc::new(ReplaceCapture {
        env,
        replacement: params.replacement.clone(),
        is_regex: params.search.is_regex,
        paths: params.paths.clone(),
        state: Mutex::new(ReplaceState {
            files: BTreeMap::new(),
            match_count: 0,
        }),
        stop: AtomicBool::new(false),
        timed_out: AtomicBool::new(false),
        deadline: started + Duration::from_secs(TIMEOUT_SECS),
        cancel: cancel.cloned(),
    });

    build_walker(&capture.env).build_parallel().run(|| {
        let capture = Arc::clone(&capture);
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .build();
        Box::new(move |entry: Result<ignore::DirEntry, ignore::Error>| {
            visit_replace_entry(&capture, &mut searcher, entry)
        })
    });

    let mut state = capture
        .state
        .lock()
        .map_err(|_| "replace state poisoned".to_string())?;
    // mem::take 换出整 map:BTreeMap 无稳定 drain;into_iter 保 key 升序,
    // 输出顺序与搜索分组一致
    let files: Vec<FileEdits> = std::mem::take(&mut state.files).into_values().collect();
    let match_count = state.match_count;
    drop(state);

    let timed_out = capture.timed_out.load(Ordering::SeqCst);
    if params.apply {
        let (mut applied_files, mut applied_matches) = (0usize, 0usize);
        let mut failed: Vec<Value> = Vec::new();
        for fe in &files {
            match apply_file_edits(fe) {
                Ok(hits) => {
                    applied_files += 1;
                    applied_matches += hits;
                }
                Err(reason) => {
                    failed.push(json!({ "path": fe.rel, "reason": reason }));
                }
            }
        }
        Ok(json!({
            "appliedFiles": applied_files,
            "appliedMatches": applied_matches,
            "failed": failed,
            "timedOut": timed_out,
            "elapsedMs": started.elapsed().as_millis() as u64,
        }))
    } else {
        let preview: Vec<Value> = files
            .iter()
            .map(|fe| {
                let edits: Vec<Value> = fe
                    .lines
                    .iter()
                    .flat_map(|le| {
                        le.edits.iter().map(move |e| {
                            json!({
                                "line": le.line_no,
                                "before": e.before,
                                "after": e.replacement,
                            })
                        })
                    })
                    .collect();
                json!({ "path": fe.rel, "edits": edits })
            })
            .collect();
        Ok(json!({
            "preview": preview,
            "fileCount": files.len(),
            "matchCount": match_count,
            "timedOut": timed_out,
            "elapsedMs": started.elapsed().as_millis() as u64,
        }))
    }
}

fn visit_replace_entry(
    capture: &Arc<ReplaceCapture>,
    searcher: &mut Searcher,
    entry: Result<ignore::DirEntry, ignore::Error>,
) -> WalkState {
    if capture.should_stop() {
        return WalkState::Quit;
    }
    let entry = match entry {
        Ok(e) => e,
        Err(_) => return WalkState::Continue,
    };
    if !entry.file_type().is_some_and(|ft| ft.is_file()) {
        return WalkState::Continue;
    }
    let rel = entry
        .path()
        .strip_prefix(&capture.env.root)
        .unwrap_or(entry.path());
    let rel_display = rel.to_string_lossy().replace('\\', "/");
    // paths 白名单:限定替换文件集(空集=不限制)
    if !capture.paths.is_empty() && !capture.paths.contains(&rel_display) {
        return WalkState::Continue;
    }
    let mut sink = ReplaceSink {
        capture,
        rel: rel_display,
        abs: entry.path().to_path_buf(),
        caps: None,
    };
    let _ = searcher.search_path(&capture.env.matcher, entry.path(), &mut sink);
    if capture.stop.load(Ordering::SeqCst) {
        WalkState::Quit
    } else {
        WalkState::Continue
    }
}

struct ReplaceSink<'a> {
    capture: &'a Arc<ReplaceCapture>,
    rel: String,
    abs: PathBuf,
    /// 捕获缓冲 per-sink 复用(captures_iter 内部重置)
    caps: Option<grep_regex::RegexCaptures>,
}

impl Sink for ReplaceSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        if self.capture.should_stop() {
            return Ok(false);
        }
        let line_no = mat.line_number().unwrap_or(0) as usize;
        let text = String::from_utf8_lossy(strip_terminator(mat.bytes())).into_owned();
        let mut edits: Vec<HitEdit> = Vec::new();
        if self.capture.is_regex {
            // 捕获组模式:captures_iter 逐匹配展开 $1/${name}
            if self.caps.is_none() {
                self.caps = Some(
                    self.capture
                        .env
                        .matcher
                        .new_captures()
                        .expect("regex captures buffer"),
                );
            }
            let caps = self.caps.as_mut().unwrap();
            let _ = self
                .capture
                .env
                .matcher
                .captures_iter(text.as_bytes(), caps, |caps| {
                    let m = caps.as_match();
                    if m.start() == m.end() {
                        // 空匹配防爆炸,与搜索路径同语义
                        return true;
                    }
                    let mut dst = Vec::new();
                    caps.interpolate(
                        |name| self.capture.env.matcher.capture_index(name),
                        text.as_bytes(),
                        self.capture.replacement.as_bytes(),
                        &mut dst,
                    );
                    edits.push(HitEdit {
                        start: m.start(),
                        end: m.end(),
                        before: text[m.start()..m.end()].to_string(),
                        replacement: String::from_utf8_lossy(&dst).into_owned(),
                    });
                    true
                });
        } else {
            let _ = self.capture.env.matcher.find_iter(text.as_bytes(), |m| {
                if m.start() != m.end() {
                    edits.push(HitEdit {
                        start: m.start(),
                        end: m.end(),
                        before: text[m.start()..m.end()].to_string(),
                        replacement: self.capture.replacement.clone(),
                    });
                }
                true
            });
        }
        if edits.is_empty() {
            return Ok(true);
        }

        let mut state = self
            .capture
            .state
            .lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let file = state
            .files
            .entry(self.rel.clone())
            .or_insert_with(|| FileEdits {
                rel: self.rel.clone(),
                abs: self.abs.clone(),
                lines: Vec::new(),
            });
        file.lines.push(LineEdits { line_no, edits });
        state.match_count += 1;
        Ok(true)
    }
}

/// 逐文件应用:读→校验 UTF-8→按行内存替换→tmp+rename 原子写。
/// 返回该文件应用 hit 数;失败进 failed(文件保持原样,无部分写)。
fn apply_file_edits(fe: &FileEdits) -> Result<usize, String> {
    let bytes = std::fs::read(&fe.abs).map_err(|e| format!("failed to read file: {e}"))?;
    let content = String::from_utf8(bytes)
        .map_err(|_| "file is not valid UTF-8; skipped to protect binary content".to_string())?;
    // split('\n') 保尾空段:文件以 \n 结尾时最后一段为 "",join 后还原终止符
    let mut lines: Vec<String> = content.split('\n').map(|s| s.to_string()).collect();
    let mut hits = 0usize;
    for le in &fe.lines {
        let idx = le
            .line_no
            .checked_sub(1)
            .ok_or_else(|| format!("invalid line number {}", le.line_no))?;
        let line = lines
            .get_mut(idx)
            .ok_or_else(|| format!("line {} out of range (file changed)", le.line_no))?;
        // \r\n 行尾:匹配发生在去 \r 的内容上,替换后拼回
        let (content_part, cr_suffix) = match line.strip_suffix('\r') {
            Some(head) => (head.to_string(), "\r"),
            None => (line.clone(), ""),
        };
        let mut buf = content_part;
        for edit in le.edits.iter().rev() {
            // 从后往前替换:byte span 不漂移;char boundary 校验防
            // `(?-u)` 字节模式切片 panic
            if edit.start > edit.end
                || edit.end > buf.len()
                || !buf.is_char_boundary(edit.start)
                || !buf.is_char_boundary(edit.end)
            {
                return Err("file content changed during replace; aborted".to_string());
            }
            buf.replace_range(edit.start..edit.end, &edit.replacement);
        }
        *line = format!("{buf}{cr_suffix}");
        hits += le.edits.len();
    }
    atomic_write(&fe.abs, &lines.join("\n"))?;
    Ok(hits)
}

/// tmp+rename 原子写(FileWriteTool std::fs::write 直写不同——替换多行多文件
/// 中途失败不能留半截内容)。tmp 落同目录(保证同盘 rename 原子性),失败清理。
fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| format!("no parent dir: {}", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| format!("no file name: {}", path.display()))?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(".{name}.evo-replace-tmp-{nanos}-{seq}"));
    let result = (|| -> Result<(), String> {
        std::fs::write(&tmp, content.as_bytes())
            .map_err(|e| format!("failed to write temp file: {e}"))?;
        // Windows 侧 std::fs::rename 对应 MoveFileEx(REPLACE_EXISTING),可覆盖已存在文件
        std::fs::rename(&tmp, path)
            .map_err(|e| format!("failed to rename temp file into place: {e}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// `grep_files` agent 工具(核心层薄包装;G13 spawn_blocking 惯例)
#[derive(Clone)]
pub struct GrepFilesTool {
    workdir: PathBuf,
    max_results: usize,
}

impl GrepFilesTool {
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

    fn call_sync(&self, args: &Value) -> IoResult {
        let mut params = GrepParams::from_args(args)?;
        params.max_results = params.max_results.min(self.max_results);
        grep_core(&self.workdir, &params, None)
    }
}

#[async_trait::async_trait]
impl ToolFunction for GrepFilesTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 遍历
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("grep_files tool panicked: {}", e))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grep(workdir: &Path, args: Value) -> Result<Value, String> {
        let params = GrepParams::from_args(&args)?;
        grep_core(workdir, &params, None)
    }

    fn literal_args(query: &str) -> Value {
        json!({ "query": query, "smartCase": false })
    }

    #[test]
    fn test_rejects_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let err = grep(dir.path(), with_dir(literal_args("x"), "../outside")).unwrap_err();
        assert!(err.contains("sandbox boundary"), "got: {err}");
    }

    #[test]
    fn test_rejects_absolute_dir() {
        let abs = if cfg!(windows) { "C:\\Windows" } else { "/etc" };
        let dir = tempfile::tempdir().unwrap();
        let err = grep(dir.path(), with_dir(literal_args("x"), abs)).unwrap_err();
        assert!(err.contains("sandbox boundary"), "got: {err}");
    }

    #[test]
    fn test_literal_search_groups_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\nother\nhello rust\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "no match here\n").unwrap();

        let out = grep(dir.path(), literal_args("hello")).unwrap();
        assert_eq!(out["totalMatches"], 2);
        assert_eq!(out["fileCount"], 1);
        assert_eq!(out["truncated"], false);
        let groups = out["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["path"], "a.txt");
        let hits = groups[0]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["line"], 1);
        assert_eq!(hits[0]["col"], 0);
        assert_eq!(hits[0]["endCol"], 5);
        assert_eq!(hits[1]["line"], 3);
    }

    #[test]
    fn test_multibyte_char_offsets() {
        let dir = tempfile::tempdir().unwrap();
        // "中文 hello" — 'hello' 的 byte 偏移是 7,char 偏移必须是 3
        std::fs::write(dir.path().join("cn.txt"), "中文 hello\n").unwrap();
        let out = grep(dir.path(), literal_args("hello")).unwrap();
        let hits = out["groups"][0]["hits"].as_array().unwrap();
        assert_eq!(
            hits[0]["col"], 3,
            "col must be char offset, not byte offset"
        );
        assert_eq!(hits[0]["endCol"], 8);
    }

    #[test]
    fn test_binary_file_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bin.dat"), b"\x00\x01\x02 binary hello").unwrap();
        std::fs::write(dir.path().join("ok.txt"), "text hello\n").unwrap();
        let out = grep(dir.path(), literal_args("hello")).unwrap();
        assert_eq!(out["fileCount"], 1);
        assert_eq!(out["groups"][0]["path"], "ok.txt");
    }

    #[test]
    fn test_max_results_truncation() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "needle here\n").unwrap();
        }
        let mut args = literal_args("needle");
        args["maxResults"] = json!(3);
        let out = grep(dir.path(), args).unwrap();
        assert_eq!(out["totalMatches"], 3);
        assert_eq!(out["truncated"], true);
    }

    #[test]
    fn test_cancellation_returns_partial() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let params = GrepParams::from_args(&literal_args("hello")).unwrap();
        let out = grep_core(dir.path(), &params, Some(&token)).unwrap();
        assert_eq!(
            out["totalMatches"], 0,
            "cancelled search must return partial(empty)"
        );
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn test_regex_vs_literal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello h.llo\n").unwrap();

        let mut args = literal_args("h.llo");
        args["isRegex"] = json!(true);
        let out = grep(dir.path(), args).unwrap();
        assert_eq!(out["totalMatches"], 2, "regex mode: '.' is a wildcard");

        let out = grep(dir.path(), literal_args("h.llo")).unwrap();
        assert_eq!(
            out["totalMatches"], 1,
            "literal mode must not interpret metachars"
        );
    }

    #[test]
    fn test_invalid_regex_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut args = literal_args("(");
        args["isRegex"] = json!(true);
        let err = grep(dir.path(), args).unwrap_err();
        assert!(err.contains("invalid regex"), "got: {err}");
    }

    #[test]
    fn test_invalid_glob_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut args = literal_args("x");
        args["includeGlobs"] = json!(["[a-"]);
        let err = grep(dir.path(), args).unwrap_err();
        assert!(err.contains("invalid include glob"), "got: {err}");
    }

    #[test]
    fn test_whole_word() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "bar foo\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "barfoo baz\n").unwrap();
        let mut args = literal_args("bar");
        args["wholeWord"] = json!(true);
        let out = grep(dir.path(), args).unwrap();
        assert_eq!(out["fileCount"], 1);
        assert_eq!(out["groups"][0]["path"], "a.txt");
    }

    #[test]
    fn test_smart_case() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "HELLO world\nhello world\n").unwrap();

        // 全小写查询 + smartCase(默认开) → 大小写不敏感
        let out = grep(dir.path(), json!({ "query": "hello" })).unwrap();
        assert_eq!(out["totalMatches"], 2);

        // 带大写查询 → 大小写敏感
        let out = grep(dir.path(), json!({ "query": "Hello" })).unwrap();
        assert_eq!(out["totalMatches"], 0);

        // 关闭 smartCase → 不敏感
        let out = grep(dir.path(), json!({ "query": "Hello", "smartCase": false })).unwrap();
        assert_eq!(out["totalMatches"], 2);
    }

    #[test]
    fn test_case_sensitive_explicit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello HELLO\n").unwrap();
        let out = grep(
            dir.path(),
            json!({ "query": "hello", "caseSensitive": true }),
        )
        .unwrap();
        assert_eq!(out["totalMatches"], 1);
    }

    #[test]
    fn test_gitignore_respected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap(); // require_git 语义:需 git 仓标志
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "needle\n").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "needle\n").unwrap();

        let out = grep(dir.path(), literal_args("needle")).unwrap();
        assert_eq!(out["fileCount"], 1);
        assert_eq!(out["groups"][0]["path"], "visible.txt");

        // 关闭开关 → 强制全搜
        let mut args = literal_args("needle");
        args["useIgnoreFiles"] = json!(false);
        let out = grep(dir.path(), args).unwrap();
        assert_eq!(out["fileCount"], 2);
    }

    #[test]
    fn test_include_glob_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "needle\n").unwrap();
        let mut args = literal_args("needle");
        args["includeGlobs"] = json!(["*.rs"]);
        let out = grep(dir.path(), args).unwrap();
        assert_eq!(out["fileCount"], 1);
        assert_eq!(out["groups"][0]["path"], "a.rs");
    }

    #[test]
    fn test_default_excludes_always_applied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();
        std::fs::write(dir.path().join("data/secret.txt"), "needle\n").unwrap();
        std::fs::write(dir.path().join("code.txt"), "needle\n").unwrap();
        let out = grep(dir.path(), literal_args("needle")).unwrap();
        assert_eq!(out["fileCount"], 1, "data/** must stay excluded");
        assert_eq!(out["groups"][0]["path"], "code.txt");
    }

    #[test]
    fn test_request_exclude_glob() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/x.txt"), "needle\n").unwrap();
        std::fs::write(dir.path().join("y.txt"), "needle\n").unwrap();
        let mut args = literal_args("needle");
        args["excludeGlobs"] = json!(["docs/**"]);
        let out = grep(dir.path(), args).unwrap();
        assert_eq!(out["fileCount"], 1);
        assert_eq!(out["groups"][0]["path"], "y.txt");
    }

    #[test]
    fn test_dir_scope() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/in.txt"), "needle\n").unwrap();
        std::fs::write(dir.path().join("out.txt"), "needle\n").unwrap();
        let args = with_dir(literal_args("needle"), "sub");
        let out = grep(dir.path(), args).unwrap();
        assert_eq!(out["fileCount"], 1);
        assert_eq!(out["groups"][0]["path"], "in.txt");
    }

    #[test]
    fn test_empty_query_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = grep(dir.path(), literal_args("")).unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    // =========================================================================
    // 替换核心(PR2)
    // =========================================================================

    fn replace(workdir: &Path, args: Value) -> Result<Value, String> {
        let params = ReplaceParams::from_args(&args)?;
        replace_core(workdir, &params, None)
    }

    fn replace_args(query: &str, replacement: &str) -> Value {
        json!({ "query": query, "replacement": replacement, "smartCase": false })
    }

    #[test]
    fn test_replace_preview_edits_literal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo bar\nkeep\nfoo again\n").unwrap();
        let out = replace(dir.path(), replace_args("foo", "baz")).unwrap();
        assert_eq!(out["matchCount"], 2);
        assert_eq!(out["fileCount"], 1);
        assert_eq!(out["timedOut"], false);
        let preview = out["preview"].as_array().unwrap();
        assert_eq!(preview.len(), 1);
        assert_eq!(preview[0]["path"], "a.txt");
        let edits = preview[0]["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0]["line"], 1);
        assert_eq!(edits[0]["before"], "foo");
        assert_eq!(edits[0]["after"], "baz");
        assert_eq!(edits[1]["line"], 3);
        // 预览零写盘
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "foo bar\nkeep\nfoo again\n"
        );
    }

    #[test]
    fn test_replace_preview_case_insensitive_uses_actual_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "Hello world\n").unwrap();
        // 不敏感(显式关 smartCase)匹配 Hello 原文
        let mut args = replace_args("hello", "bye");
        args["smartCase"] = json!(false);
        let out = replace(dir.path(), args).unwrap();
        let edits = out["preview"][0]["edits"].as_array().unwrap();
        assert_eq!(
            edits[0]["before"], "Hello",
            "before must be actual matched text"
        );
        assert_eq!(edits[0]["after"], "bye");
    }

    #[test]
    fn test_replace_apply_rewrites_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo bar\nkeep\nfoo\n").unwrap();
        let mut args = replace_args("foo", "baz");
        args["apply"] = json!(true);
        let out = replace(dir.path(), args).unwrap();
        assert_eq!(out["appliedFiles"], 1);
        assert_eq!(out["appliedMatches"], 2);
        assert_eq!(out["failed"].as_array().unwrap().len(), 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "baz bar\nkeep\nbaz\n"
        );
    }

    #[test]
    fn test_replace_apply_rematch_protection() {
        // 预览后文件再变 → apply 按 apply 时刻重匹配,不信任预览快照
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        let out = replace(dir.path(), replace_args("needle", "x")).unwrap();
        assert_eq!(out["matchCount"], 1, "preview sees 1 hit");

        std::fs::write(dir.path().join("a.txt"), "needle\nneedle\nneedle\n").unwrap();
        let mut args = replace_args("needle", "x");
        args["apply"] = json!(true);
        let out = replace(dir.path(), args).unwrap();
        assert_eq!(
            out["appliedMatches"], 3,
            "apply must re-match at apply time, not reuse preview snapshot"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "x\nx\nx\n"
        );
    }

    #[test]
    fn test_replace_regex_capture_groups() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "user=alice\n").unwrap();
        // $2/$1 反转捕获组
        let mut args = replace_args(r"(\w+)=(\w+)", "$2=$1");
        args["isRegex"] = json!(true);
        let out = replace(dir.path(), args.clone()).unwrap();
        let edits = out["preview"][0]["edits"].as_array().unwrap();
        assert_eq!(edits[0]["after"], "alice=user");
        args["apply"] = json!(true);
        replace(dir.path(), args).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alice=user\n"
        );
    }

    #[test]
    fn test_replace_regex_named_group_and_dollar_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "key:value\n").unwrap();
        // ${name} 命名组 + $$ 字面 $:Rust 字面 3 个 $(前两个=interpolate 的 $$ 字面 $,
        // 第三个与 { 组成 ${k} 组引用)
        let mut args = replace_args(r"(?P<k>\w+):(\w+)", "$$${k}=${2}");
        args["isRegex"] = json!(true);
        let out = replace(dir.path(), args).unwrap();
        let edits = out["preview"][0]["edits"].as_array().unwrap();
        assert_eq!(edits[0]["after"], "$key=value");
    }

    #[test]
    fn test_replace_not_utf8_failed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bin.dat"), b"\x00\x01\x02 foo").unwrap();
        std::fs::write(dir.path().join("ok.txt"), "foo here\n").unwrap();
        let mut args = replace_args("foo", "bar");
        args["apply"] = json!(true);
        let out = replace(dir.path(), args).unwrap();
        // bin.dat 含 NUL 被 BinaryDetection quit 跳过(不进匹配);用纯非 UTF-8 无 NUL 文件再验
        assert_eq!(out["appliedFiles"], 1);
    }

    #[test]
    fn test_replace_non_utf8_file_goes_to_failed() {
        let dir = tempfile::tempdir().unwrap();
        // 非 UTF-8 字节且无 NUL:搜索路径 lossy 能"匹配",apply 路径必须拒绝写回
        std::fs::write(dir.path().join("latin.txt"), b"caf\xe9 foo\n").unwrap();
        let mut args = replace_args("foo", "bar");
        args["apply"] = json!(true);
        let out = replace(dir.path(), args).unwrap();
        assert_eq!(out["appliedFiles"], 0);
        let failed = out["failed"].as_array().unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["path"], "latin.txt");
        assert!(failed[0]["reason"].as_str().unwrap().contains("UTF-8"));
        // 原文件未被破坏
        assert_eq!(
            std::fs::read(dir.path().join("latin.txt")).unwrap(),
            b"caf\xe9 foo\n"
        );
    }

    #[test]
    fn test_replace_paths_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "foo\n").unwrap();
        let mut args = replace_args("foo", "bar");
        args["apply"] = json!(true);
        args["paths"] = json!(["a.txt"]);
        let out = replace(dir.path(), args).unwrap();
        assert_eq!(out["appliedFiles"], 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "bar\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "foo\n"
        );
    }

    #[test]
    fn test_replace_apply_holds_shared_write_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        let mut args = replace_args("needle", "x");
        args["apply"] = json!(true);
        let (tx, rx) = std::sync::mpsc::channel();
        let path = dir.path().to_path_buf();
        let handle = std::thread::spawn(move || {
            let out = replace(&path, args).unwrap();
            let _ = tx.send(out);
        });
        // 主线程占住全局树写锁 → apply 必须被阻塞(锁互斥)
        let guard = loop {
            if let Ok(g) = crate::builtin_tools::fs_safety::tree_mutation_lock().try_lock() {
                break g;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            rx.try_recv().is_err(),
            "apply=true must block on the shared tree mutation lock"
        );
        drop(guard);
        let out = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("apply should finish after lock release");
        assert_eq!(out["appliedFiles"], 1);
        handle.join().unwrap();
    }

    #[test]
    fn test_replace_preview_does_not_hold_write_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        // 主线程持锁时预览照常完成(只读不持锁)
        let guard = loop {
            if let Ok(g) = crate::builtin_tools::fs_safety::tree_mutation_lock().try_lock() {
                break g;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let out = replace(dir.path(), replace_args("needle", "x")).unwrap();
        assert_eq!(out["matchCount"], 1);
        drop(guard);
    }

    #[test]
    fn test_replace_atomic_write_no_tmp_residue() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo\n").unwrap();
        let mut args = replace_args("foo", "bar");
        args["apply"] = json!(true);
        replace(dir.path(), args).unwrap();
        let residue: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".evo-replace-tmp-")
            })
            .collect();
        assert!(
            residue.is_empty(),
            "tmp files must be renamed away: {residue:?}"
        );
    }

    #[test]
    fn test_replace_multibyte_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cn.txt"), "你好世界\n").unwrap();
        let out = replace(dir.path(), replace_args("世界", "Rust")).unwrap();
        let edits = out["preview"][0]["edits"].as_array().unwrap();
        assert_eq!(edits[0]["before"], "世界");
        assert_eq!(edits[0]["after"], "Rust");
        let mut args = replace_args("世界", "Rust");
        args["apply"] = json!(true);
        replace(dir.path(), args).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cn.txt")).unwrap(),
            "你好Rust\n",
            "byte spans must not corrupt multibyte chars"
        );
    }

    #[test]
    fn test_replace_crlf_and_trailing_newline_preserved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\r\nfoo\r\ntwo\r\n").unwrap();
        let mut args = replace_args("foo", "bar");
        args["apply"] = json!(true);
        replace(dir.path(), args).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("a.txt")).unwrap(),
            b"one\r\nbar\r\ntwo\r\n",
            "CRLF line endings must survive replacement"
        );
        // 无尾换行文件同样保留
        std::fs::write(dir.path().join("b.txt"), "x foo y").unwrap();
        let out = replace(dir.path(), {
            let mut a = replace_args("foo", "bar");
            a["apply"] = json!(true);
            a
        })
        .unwrap();
        assert_eq!(out["appliedMatches"], 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "x bar y"
        );
    }

    #[test]
    fn test_replace_empty_replacement_deletes_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "keep REMOVEME tail\n").unwrap();
        let mut args = replace_args("REMOVEME ", "");
        args["apply"] = json!(true);
        let out = replace(dir.path(), args).unwrap();
        assert_eq!(out["appliedMatches"], 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "keep tail\n"
        );
    }

    #[test]
    fn test_replace_missing_replacement_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = replace(dir.path(), json!({ "query": "x", "smartCase": false })).unwrap_err();
        assert!(err.contains("replacement"), "got: {err}");
    }

    #[test]
    fn test_replace_invalid_regex_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut args = replace_args("(", "x");
        args["isRegex"] = json!(true);
        let err = replace(dir.path(), args).unwrap_err();
        assert!(err.contains("invalid regex"), "got: {err}");
    }

    #[test]
    fn test_replace_whole_word() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "bar barfoo\n").unwrap();
        let mut args = replace_args("bar", "zoo");
        args["wholeWord"] = json!(true);
        args["apply"] = json!(true);
        let out = replace(dir.path(), args).unwrap();
        assert_eq!(out["appliedMatches"], 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "zoo barfoo\n"
        );
    }

    /// 测试辅助:带 dir 的 args 构造(camelCase 与核心层解析共用)
    fn with_dir(mut v: Value, dir: &str) -> Value {
        v["dir"] = json!(dir);
        v
    }
}
