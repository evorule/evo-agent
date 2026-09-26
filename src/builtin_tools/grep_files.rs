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
//!   经 serve 面白名单+agentTools.grep 开关过滤后暴露)。
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

use grep_matcher::Matcher;
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
            .map(|n| n.max(1) as usize)
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
    let mut ovb = OverrideBuilder::new(&root);
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

    let mut wb = WalkBuilder::new(&root);
    wb.hidden(true).overrides(overrides);
    if params.use_ignore_files {
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

    let shared = Arc::new(Capture {
        root: root.clone(),
        matcher,
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

    wb.build_parallel().run(|| {
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

struct CaptureState {
    total: usize,
    groups: BTreeMap<String, Vec<Value>>,
}

struct Capture {
    root: PathBuf,
    matcher: grep_regex::RegexMatcher,
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
        .strip_prefix(&capture.root)
        .unwrap_or(entry.path());
    let rel_display = rel.to_string_lossy().replace('\\', "/");
    let mut sink = HitSink {
        capture,
        rel: rel_display,
    };
    // 单文件搜索失败(权限/占用)跳过该文件,不中断整体
    let _ = searcher.search_path(&capture.matcher, entry.path(), &mut sink);
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
        let _ = self.capture.matcher.find_iter(text.as_bytes(), |m| {
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

    /// 测试辅助:带 dir 的 args 构造(camelCase 与核心层解析共用)
    fn with_dir(mut v: Value, dir: &str) -> Value {
        v["dir"] = json!(dir);
        v
    }
}
