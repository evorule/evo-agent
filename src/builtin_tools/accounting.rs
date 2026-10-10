// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 查账工具族（工具面统一架构 PR-11a）：会话账面只读查询
//!
//! 四个只读工具，全部走唯一管道（Standard：免裁决但落账；AutoPolicy：
//! 免审批）——查账自身入账，查账行为可审计（设计档 §11.1）：
//!
//! - `query_journal`：会话 journal 事件流查询（时间窗/回合/类型过滤）
//! - `query_trace`：工具调用轨迹查询（工具名/状态/调用序过滤）
//! - `read_back`：文件现势内容 + staleness 判定（会话内纯轨迹推算——
//!   只对「会话内已知的写」负责，外部进程改动不在射程，边界随 spec 落）
//! - `diff_runs`：两段文本的行级结构化比对（纯函数；「两次运行」的
//!   取数组合由 LLM 经 query_trace 完成后传入）
//!
//! ## 接线形态
//!
//! 工具实例在 `default_safe_toolkit` 以未接线 deps 注册（进程级 toolkit
//! 单例）；runner 构造后经 `AgentRunner::wire_accounting(workdir)` 以本
//! 会话态重绑（per-runner 实例 + 共享 collector/journal 槽，与 delegate
//! 定义级注册同型）。未接线的实例被调用时如实报错，不静默返回空账。
//!
//! ## 只读纪律
//!
//! 消费 [`ToolTraceCollector::snapshot`]（只读快照）与 journal 文件只读
//! 重放（[`journal::read_all`]，不经写者打开路径），零账面写入。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::{json, Value};

use crate::agent::journal::{self, JournalWriter};
use crate::agent::tool_trace::ToolTraceCollector;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// journal 会话上下文（runner set_session_id 时同步；journal_dir 未注入
/// = CLI 直跑无账面，query_journal 如实报错）
#[derive(Debug, Clone)]
pub struct JournalCtx {
    /// journal 目录（`<workdir>/data/sessions`）
    pub dir: PathBuf,
    /// 当前会话 id
    pub session_id: String,
}

/// 查账工具族共享依赖（per-runner 重绑的接线载体）
#[derive(Debug, Clone, Default)]
pub struct AccountingDeps {
    /// 工作目录沙箱根（read_back 读盘用）
    pub workdir: Option<PathBuf>,
    /// 会话轨迹采集器共享句柄（runner 接线后 Some）
    pub traces: Option<Arc<Mutex<ToolTraceCollector>>>,
    /// 会话 journal 上下文（runner set_session_id 后 Some）
    pub journal: Arc<RwLock<Option<JournalCtx>>>,
}

impl AccountingDeps {
    /// 未接线 deps（default_safe_toolkit 注册占位实例用）
    pub fn unwired_placeholder(workdir: &Path) -> Self {
        Self {
            workdir: Some(workdir.to_path_buf()),
            traces: None,
            journal: Arc::new(RwLock::new(None)),
        }
    }

    /// 已接线 deps（runner wire_accounting 重绑用）
    pub fn wired(
        workdir: &Path,
        traces: Arc<Mutex<ToolTraceCollector>>,
        journal: Arc<RwLock<Option<JournalCtx>>>,
    ) -> Self {
        Self {
            workdir: Some(workdir.to_path_buf()),
            traces: Some(traces),
            journal,
        }
    }

    fn trace_snapshot(&self) -> Result<Vec<Value>, String> {
        let collector = self.traces.as_ref().ok_or_else(|| {
            "accounting not wired to a session (placeholder instance)".to_string()
        })?;
        let guard = collector
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(guard.snapshot())
    }

    fn journal_ctx(&self) -> Result<JournalCtx, String> {
        let guard = self
            .journal
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clone().ok_or_else(|| {
            "journal not available: session not started or journal dir not enabled".to_string()
        })
    }
}

/// 路径归一（staleness 匹配口径）：分隔符统一为 `/` + 小写化
/// （Windows 大小写不敏感；轨迹 args 与本次查询参数写法可能不同）
fn normalize_path(p: &str) -> String {
    p.replace('\\', "/").to_lowercase()
}

/// 从轨迹条目提取该调用涉及的路径（读=主路径；写=主路径 + file_move 目标）
fn touched_paths(entry: &Value) -> Vec<String> {
    let tool = entry
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let args = entry.get("args");
    let mut out = Vec::new();
    if let Some(args) = args {
        // file_read/file_write/file_create/file_delete 用 `path`；
        // file_move 用 `path`（源）+ `target_dir`/`new_name`（目标，可能拆分存放）
        if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
            out.push(normalize_path(p));
        }
        if tool == "file_move" {
            if let Some(dir) = args.get("target_dir").and_then(|v| v.as_str()) {
                let name = args.get("new_name").and_then(|v| v.as_str());
                let base = Path::new(dir);
                let target = match name {
                    Some(n) => base.join(n),
                    None => base.to_path_buf(),
                };
                out.push(normalize_path(&target.to_string_lossy()));
            }
        }
    }
    out
}

const READ_TOOLS: &[&str] = &["file_read"];
const WRITE_TOOLS: &[&str] = &["file_write", "file_create", "file_move", "file_delete"];

// =============================================================================
// query_journal —— 会话 journal 事件流查询
// =============================================================================

/// `query_journal` 工具
pub struct QueryJournalTool {
    deps: AccountingDeps,
}

impl QueryJournalTool {
    /// 构造（deps 由 default_safe_toolkit 占位 / runner wire_accounting 重绑提供）
    pub fn new(deps: AccountingDeps) -> Self {
        Self { deps }
    }
}

#[async_trait::async_trait]
impl ToolFunction for QueryJournalTool {
    async fn call(&self, args: &Value) -> IoResult {
        let turn = args.get("turn").and_then(|v| v.as_u64());
        let since_seq = args.get("since_seq").and_then(|v| v.as_u64());
        let event_type = args
            .get("type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(100)
            .clamp(1, 500);

        let ctx = self.deps.journal_ctx()?;
        let path = JournalWriter::path_for(&ctx.dir, &ctx.session_id);
        if !path.exists() {
            return Ok(json!({
                "session": ctx.session_id,
                "total_lines": 0,
                "matched": 0,
                "truncated": false,
                "events": [],
                "note": "journal file not found (no session activity recorded yet)",
            }));
        }
        let lines = journal::read_all(&path).map_err(|e| format!("journal read failed: {e}"))?;
        let total_lines = lines.len();

        // 过滤：seq 起点 / 事件类型；回合过滤 = 该 TurnStarted 序至下一
        // TurnStarted 序之前（重放时跟踪当前 turn_seq）
        let mut current_turn = 0u64;
        let mut matched: Vec<Value> = Vec::new();
        for line in &lines {
            let ev = serde_json::to_value(&line.event)
                .map_err(|e| format!("journal event encode failed: {e}"))?;
            if let Some(t) = ev
                .get("payload")
                .and_then(|p| p.get("turn_seq"))
                .and_then(|v| v.as_u64())
            {
                current_turn = t;
            }
            if let Some(s) = since_seq {
                if line.seq < s {
                    continue;
                }
            }
            if let Some(t) = turn {
                if current_turn != t {
                    continue;
                }
            }
            let ty = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(ft) = &event_type {
                if ty != ft {
                    continue;
                }
            }
            // 输出行 = seq/ts/type + payload 平铺（与 journal 文件行形态一致）
            let mut row = json!({"seq": line.seq, "ts": line.ts, "type": ty});
            if let Some(payload) = ev.get("payload") {
                if let Some(obj) = payload.as_object() {
                    for (k, v) in obj {
                        row[k.clone()] = v.clone();
                    }
                }
            }
            matched.push(row);
        }

        // limit：尾部截取（最新 N 条），truncated 显式标注
        let matched_total = matched.len();
        let truncated = matched_total > limit;
        if truncated {
            matched.drain(..matched_total - limit);
        }

        Ok(json!({
            "session": ctx.session_id,
            "total_lines": total_lines,
            "matched": matched_total,
            "truncated": truncated,
            "events": matched,
        }))
    }
}

// =============================================================================
// query_trace —— 工具调用轨迹查询
// =============================================================================

/// `query_trace` 工具
pub struct QueryTraceTool {
    deps: AccountingDeps,
}

impl QueryTraceTool {
    /// 构造（deps 由 default_safe_toolkit 占位 / runner wire_accounting 重绑提供）
    pub fn new(deps: AccountingDeps) -> Self {
        Self { deps }
    }
}

#[async_trait::async_trait]
impl ToolFunction for QueryTraceTool {
    async fn call(&self, args: &Value) -> IoResult {
        let tool_filter = args
            .get("tool")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let status_filter = args
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let from_seq = args.get("from_seq").and_then(|v| v.as_i64());
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(100)
            .clamp(1, 500);

        let entries = self.deps.trace_snapshot()?;
        let total = entries.len();
        let mut matched: Vec<Value> = entries
            .into_iter()
            .filter(|e| {
                if let Some(t) = &tool_filter {
                    if e.get("tool_name").and_then(|v| v.as_str()) != Some(t.as_str()) {
                        return false;
                    }
                }
                if let Some(s) = &status_filter {
                    if e.get("status").and_then(|v| v.as_str()) != Some(s.as_str()) {
                        return false;
                    }
                }
                if let Some(fs) = from_seq {
                    if e.get("seq").and_then(|v| v.as_i64()).unwrap_or(i64::MIN) < fs {
                        return false;
                    }
                }
                true
            })
            .collect();

        let matched_total = matched.len();
        let truncated = matched_total > limit;
        if truncated {
            matched.drain(..matched_total - limit);
        }

        Ok(json!({
            "total": total,
            "matched": matched_total,
            "truncated": truncated,
            "traces": matched,
        }))
    }
}

// =============================================================================
// read_back —— 文件现势内容 + staleness 判定
// =============================================================================

/// `read_back` 工具
pub struct ReadBackTool {
    deps: AccountingDeps,
}

impl ReadBackTool {
    /// 构造（deps 由 default_safe_toolkit 占位 / runner wire_accounting 重绑提供）
    pub fn new(deps: AccountingDeps) -> Self {
        Self { deps }
    }
}

#[async_trait::async_trait]
impl ToolFunction for ReadBackTool {
    async fn call(&self, args: &Value) -> IoResult {
        let raw_path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: path".to_string())?;
        let norm = normalize_path(raw_path);

        // 1. staleness 判定（会话内纯轨迹推算）：最近一次 file_read 之后
        //    是否有对该路径的写（file_write/file_create/file_delete 命中
        //    path；file_move 命中源 path 或合成目标）。锚=trace_seq（轨迹
        //    collector 内部 0 起序号，与 query_trace 输出同域）——trace 条目
        //    无 journal seq/ts，不得伪造（E2E 修正：原形态输出 seq/ts 会被
        //    误读为 journal 全局账锚，ts 恒为缺省 0）
        let entries = self.deps.trace_snapshot()?;
        let mut last_read: Option<i64> = None;
        let mut writes_after: Vec<Value> = Vec::new();
        for e in &entries {
            let tool = e.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
            let paths = touched_paths(e);
            let is_read = READ_TOOLS.contains(&tool) && paths.contains(&norm);
            let is_write = WRITE_TOOLS.contains(&tool) && paths.contains(&norm);
            let entry_seq = e.get("seq").and_then(|v| v.as_i64()).unwrap_or(-1);
            if is_read {
                last_read = Some(entry_seq);
                // 读本身刷新判定基线；其后的写才构成 stale
                writes_after.clear();
            } else if is_write && last_read.is_some() {
                writes_after.push(json!({
                    "trace_seq": entry_seq,
                    "tool": tool,
                }));
            }
        }
        let is_stale = !writes_after.is_empty();

        // 2. 现势内容：从盘上重读（workdir 沙箱校验，与 file_read 同口径；
        //    10MB 上限同 file_read）
        let (content, content_bytes, note) = match &self.deps.workdir {
            Some(workdir) => {
                match crate::builtin_tools::fs_safety::resolve_existing(workdir, raw_path) {
                    Ok(resolved) => {
                        let meta = std::fs::metadata(&resolved)
                            .map_err(|e| format!("read_back metadata failed: {e}"))?;
                        if meta.len() > 10 * 1024 * 1024 {
                            (
                                Value::Null,
                                0u64,
                                Some("file larger than 10MB (same limit as file_read)".to_string()),
                            )
                        } else {
                            match std::fs::read_to_string(&resolved) {
                                Ok(text) => {
                                    let n = text.len() as u64;
                                    (Value::String(text), n, None)
                                }
                                Err(e) => (
                                    Value::Null,
                                    meta.len(),
                                    Some(format!(
                                        "file is not valid UTF-8 text or unreadable: {e}"
                                    )),
                                ),
                            }
                        }
                    }
                    // 盘上不存在（可能被删/改名）——staleness 结论照常返回
                    Err(_) => (
                        Value::Null,
                        0,
                        Some(
                            "file not found on disk (deleted, moved, or path rejected)".to_string(),
                        ),
                    ),
                }
            }
            None => (Value::Null, 0, Some("workdir not wired".to_string())),
        };

        let no_read_record = last_read.is_none();
        Ok(json!({
            "path": raw_path,
            "is_stale": is_stale,
            "last_read": last_read.map(|seq| json!({"trace_seq": seq})),
            "writes_after": writes_after,
            "content": content,
            "content_bytes": content_bytes,
            "note": note,
            "staleness_scope": if no_read_record {
                "no in-session read record: is_stale=false only means no known write after a known read (external modifications are not tracked)"
            } else {
                "in-session trace only: external process modifications are not tracked"
            },
        }))
    }
}

// =============================================================================
// diff_runs —— 两段文本的行级结构化比对（纯函数）
// =============================================================================

/// `diff_runs` 工具
pub struct DiffRunsTool {
    _deps: AccountingDeps,
}

impl DiffRunsTool {
    /// 构造（deps 由 default_safe_toolkit 占位 / runner wire_accounting 重绑提供）
    pub fn new(deps: AccountingDeps) -> Self {
        Self { _deps: deps }
    }
}

#[async_trait::async_trait]
impl ToolFunction for DiffRunsTool {
    async fn call(&self, args: &Value) -> IoResult {
        let left = args
            .get("left")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: left".to_string())?;
        let right = args
            .get("right")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: right".to_string())?;
        let left_label = args
            .get("left_label")
            .and_then(|v| v.as_str())
            .unwrap_or("left");
        let right_label = args
            .get("right_label")
            .and_then(|v| v.as_str())
            .unwrap_or("right");
        let context = args
            .get("context")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(3)
            .clamp(0, 50);

        let diff = similar::TextDiff::from_lines(left, right);
        let identical = diff.ratio() >= 1.0;

        let mut hunks: Vec<Value> = Vec::new();
        let mut added = 0usize;
        let mut removed = 0usize;
        for group in diff.grouped_ops(context) {
            let mut lines: Vec<Value> = Vec::new();
            let mut a_start = usize::MAX;
            let mut a_end = 0usize;
            let mut b_start = usize::MAX;
            let mut b_end = 0usize;
            for op in &group {
                for change in diff.iter_changes(op) {
                    let tag = match change.tag() {
                        similar::ChangeTag::Equal => " ",
                        similar::ChangeTag::Delete => "-",
                        similar::ChangeTag::Insert => "+",
                    };
                    match change.tag() {
                        similar::ChangeTag::Delete => removed += 1,
                        similar::ChangeTag::Insert => added += 1,
                        similar::ChangeTag::Equal => {}
                    }
                    a_start = a_start.min(change.old_index().unwrap_or(usize::MAX));
                    a_end = a_end.max(change.old_index().map(|i| i + 1).unwrap_or(0));
                    b_start = b_start.min(change.new_index().unwrap_or(usize::MAX));
                    b_end = b_end.max(change.new_index().map(|i| i + 1).unwrap_or(0));
                    let text: String = change.value().to_string();
                    lines.push(json!({
                        "tag": tag,
                        "old_line": change.old_index().map(|i| i + 1),
                        "new_line": change.new_index().map(|i| i + 1),
                        "text": text.trim_end_matches('\n'),
                    }));
                }
            }
            hunks.push(json!({
                "old_range": [if a_start == usize::MAX { 0 } else { a_start + 1 }, a_end],
                "new_range": [if b_start == usize::MAX { 0 } else { b_start + 1 }, b_end],
                "lines": lines,
            }));
        }

        Ok(json!({
            "identical": identical,
            "left_label": left_label,
            "right_label": right_label,
            "left_bytes": left.len(),
            "right_bytes": right.len(),
            "stats": {"added": added, "removed": removed},
            "hunks": if identical { Vec::<Value>::new() } else { hunks },
        }))
    }
}

// =============================================================================
// 测试
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool_trace::ToolTraceCollector;
    use std::sync::Mutex;

    fn wired_deps(workdir: &Path, collector: Arc<Mutex<ToolTraceCollector>>) -> AccountingDeps {
        AccountingDeps::wired(
            workdir,
            collector,
            Arc::new(RwLock::new(Some(JournalCtx {
                dir: workdir.join("sessions"),
                session_id: "s-test".to_string(),
            }))),
        )
    }

    fn collector_with(records: Vec<(&str, Value, &str)>) -> Arc<Mutex<ToolTraceCollector>> {
        let mut c = ToolTraceCollector::default();
        for (tool, args, status) in records {
            c.record(tool, &args, status, 1);
        }
        Arc::new(Mutex::new(c))
    }

    #[tokio::test]
    async fn read_back_fresh_after_read_without_write() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        let c = collector_with(vec![
            ("file_read", json!({"path": "a.txt"}), "ok"),
            ("file_read", json!({"path": "b.txt"}), "ok"),
        ]);
        let tool = ReadBackTool::new(wired_deps(tmp.path(), c));
        let out = tool
            .call(&json!({"path": "a.txt"}))
            .await
            .expect("read_back must succeed");
        assert!(!out["is_stale"].as_bool().unwrap());
        assert_eq!(out["content"], "hello");
        assert_eq!(out["last_read"]["trace_seq"], 0);
        assert!(out["writes_after"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn read_back_stale_after_write_with_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "v2").unwrap();
        let c = collector_with(vec![
            ("file_read", json!({"path": "a.txt"}), "ok"),
            ("file_read", json!({"path": "b.txt"}), "ok"), // 无关读不清基线
            (
                "file_write",
                json!({"path": "a.txt", "content": "v2"}),
                "ok",
            ),
        ]);
        let tool = ReadBackTool::new(wired_deps(tmp.path(), c));
        let out = tool
            .call(&json!({"path": "a.txt"}))
            .await
            .expect("read_back must succeed");
        assert!(out["is_stale"].as_bool().unwrap());
        assert_eq!(out["writes_after"][0]["tool"], "file_write");
        assert_eq!(out["writes_after"][0]["trace_seq"], 2);
        assert_eq!(out["content"], "v2", "content must be current on-disk text");
    }

    #[tokio::test]
    async fn read_back_no_read_record_reports_scope() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        let c = collector_with(vec![("file_write", json!({"path": "a.txt"}), "ok")]);
        let tool = ReadBackTool::new(wired_deps(tmp.path(), c));
        let out = tool.call(&json!({"path": "a.txt"})).await.unwrap();
        assert!(!out["is_stale"].as_bool().unwrap());
        assert!(out["last_read"].is_null());
        assert!(
            out["staleness_scope"]
                .as_str()
                .unwrap()
                .contains("no in-session read record"),
            "boundary declaration must be present"
        );
    }

    #[tokio::test]
    async fn read_back_move_counts_as_write() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "moved-away").unwrap_or(());
        let c = collector_with(vec![
            ("file_read", json!({"path": "a.txt"}), "ok"),
            (
                "file_move",
                json!({"path": "a.txt", "target_dir": ".", "new_name": "b.txt"}),
                "ok",
            ),
        ]);
        let tool = ReadBackTool::new(wired_deps(tmp.path(), c));
        let out = tool.call(&json!({"path": "a.txt"})).await.unwrap();
        assert!(
            out["is_stale"].as_bool().unwrap(),
            "move of the source path counts as a write"
        );
    }

    #[tokio::test]
    async fn read_back_unwired_errors_honestly() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ReadBackTool::new(AccountingDeps::unwired_placeholder(tmp.path()));
        let err = tool.call(&json!({"path": "a.txt"})).await.unwrap_err();
        assert!(err.contains("not wired"), "{err}");
    }

    #[tokio::test]
    async fn diff_runs_identical_and_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = DiffRunsTool::new(AccountingDeps::unwired_placeholder(tmp.path()));

        let same = tool
            .call(&json!({"left": "a\nb\n", "right": "a\nb\n"}))
            .await
            .unwrap();
        assert_eq!(same["identical"], true);
        assert_eq!(same["hunks"].as_array().unwrap().len(), 0);

        let changed = tool
            .call(&json!({"left": "build ok\nexit 0\n", "right": "build ok\nerror: E0432\n"}))
            .await
            .unwrap();
        assert_eq!(changed["identical"], false);
        assert_eq!(changed["stats"]["removed"], 1);
        assert_eq!(changed["stats"]["added"], 1);
        let hunk = &changed["hunks"][0];
        assert_eq!(hunk["lines"][0]["text"], "build ok", "context line");
        assert_eq!(hunk["lines"][1]["tag"], "-");
        assert_eq!(hunk["lines"][1]["text"], "exit 0");
        assert_eq!(hunk["lines"][2]["tag"], "+");
        assert_eq!(hunk["lines"][2]["text"], "error: E0432");
    }

    #[tokio::test]
    async fn diff_runs_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = DiffRunsTool::new(AccountingDeps::unwired_placeholder(tmp.path()));
        let args = json!({"left": "x\ny\nz\n", "right": "x\nw\nz\n"});
        let a = tool.call(&args).await.unwrap();
        let b = tool.call(&args).await.unwrap();
        assert_eq!(a, b, "same input must produce byte-identical output");
    }

    #[tokio::test]
    async fn query_trace_filters_and_reports_total() {
        let tmp = tempfile::tempdir().unwrap();
        let c = collector_with(vec![
            ("file_read", json!({"path": "a.txt"}), "ok"),
            ("file_read", json!({"path": "b.txt"}), "error"),
            ("shell_exec", json!({"command": "ls"}), "ok"),
        ]);
        let tool = QueryTraceTool::new(wired_deps(tmp.path(), c));

        let all = tool.call(&json!({})).await.unwrap();
        assert_eq!(all["total"], 3);
        assert_eq!(all["matched"], 3);

        let errs = tool.call(&json!({"status": "error"})).await.unwrap();
        assert_eq!(errs["matched"], 1);
        assert_eq!(errs["traces"][0]["tool_name"], "file_read");

        let by_tool = tool.call(&json!({"tool": "shell_exec"})).await.unwrap();
        assert_eq!(by_tool["matched"], 1);
    }

    #[tokio::test]
    async fn query_trace_readonly_snapshot_keeps_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let c = collector_with(vec![("file_read", json!({"path": "a.txt"}), "ok")]);
        let tool = QueryTraceTool::new(wired_deps(tmp.path(), c.clone()));
        let _ = tool.call(&json!({})).await.unwrap();
        // 只读纪律：查询后账面条数不变
        assert_eq!(c.lock().unwrap().len(), 1);
    }

    /// journal 行手写（与 encode_line 平铺形态一致：seq/ts + type/payload 键合并）
    fn write_journal_file(dir: &Path, session: &str, lines: &[String]) {
        std::fs::create_dir_all(dir).unwrap();
        let path = JournalWriter::path_for(dir, session);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    }

    #[tokio::test]
    async fn query_journal_filters_type_turn_and_seq() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        write_journal_file(
            &dir,
            "s-test",
            &[
                json!({"seq":1,"ts":1000,"type":"turn_started","payload":{"turn_seq":1,"goal":"g"}})
                    .to_string(),
                json!({"seq":2,"ts":2000,"type":"tool_invoked","payload":{"call_id":"t2","tool":"file_read","args_digest":"blake3:aa","evorule_request_id":null}})
                    .to_string(),
                json!({"seq":3,"ts":3000,"type":"tool_result","payload":{"call_id":"t2","status":"ok","size_bytes":5,"content_digest":"blake3:bb"}})
                    .to_string(),
                json!({"seq":4,"ts":4000,"type":"turn_started","payload":{"turn_seq":2,"goal":"g2"}})
                    .to_string(),
            ],
        );
        let tool = QueryJournalTool::new(wired_deps(
            tmp.path(),
            Arc::new(Mutex::new(ToolTraceCollector::default())),
        ));

        // 全量
        let all = tool.call(&json!({})).await.unwrap();
        assert_eq!(all["total_lines"], 4);
        assert_eq!(all["matched"], 4);
        // payload 平铺:tool_invoked 行顶层可取 tool 字段
        let ev2 = &all["events"][1];
        assert_eq!(ev2["type"], "tool_invoked");
        assert_eq!(ev2["tool"], "file_read");

        // 类型过滤
        let by_type = tool.call(&json!({"type": "tool_invoked"})).await.unwrap();
        assert_eq!(by_type["matched"], 1);
        assert_eq!(by_type["events"][0]["seq"], 2);

        // 回合过滤:turn 1 = seq 1..3
        let by_turn = tool.call(&json!({"turn": 1})).await.unwrap();
        assert_eq!(by_turn["matched"], 3);

        // seq 起点
        let since = tool.call(&json!({"since_seq": 3})).await.unwrap();
        assert_eq!(since["matched"], 2);

        // limit 尾部截取（最新 N 条）+ truncated 标注
        let limited = tool.call(&json!({"limit": 2})).await.unwrap();
        assert_eq!(limited["matched"], 4);
        assert_eq!(limited["truncated"], true);
        let evs = limited["events"].as_array().unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["seq"], 3);
        assert_eq!(evs[1]["seq"], 4);
    }

    #[tokio::test]
    async fn query_journal_missing_file_reports_empty_with_note() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = QueryJournalTool::new(wired_deps(
            tmp.path(),
            Arc::new(Mutex::new(ToolTraceCollector::default())),
        ));
        let out = tool.call(&json!({})).await.unwrap();
        assert_eq!(out["matched"], 0);
        assert!(
            out["note"].as_str().unwrap().contains("not found"),
            "empty ledger must be explicit, not silent"
        );
    }

    #[tokio::test]
    async fn query_journal_unwired_errors_honestly() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = QueryJournalTool::new(AccountingDeps::unwired_placeholder(tmp.path()));
        let err = tool.call(&json!({})).await.unwrap_err();
        assert!(
            err.contains("journal not available"),
            "unwired placeholder must fail explicitly: {err}"
        );
    }

    #[tokio::test]
    async fn manifest_count_and_accounting_specs_present() {
        // 快照锁随动（PR-11a）：静态计数 20/46/71 由
        // tool_manifest::test_static_manifest_count_locked 锁守；此处锁
        // default_tool_specs 侧 4 spec 在场 + manifest 查询一致
        let specs = super::super::default_tool_specs();
        for name in ["query_journal", "query_trace", "read_back", "diff_runs"] {
            let spec = specs
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("spec '{name}' must be in default_tool_specs"));
            assert!(!spec.description.is_empty());
            assert!(!spec.parameters.is_empty());
            let m = crate::agent::tool_manifest::lookup_static(name)
                .unwrap_or_else(|| panic!("manifest '{name}' must be in static table"));
            assert_eq!(
                m.adjudication_class,
                crate::agent::tool_manifest::AdjudicationClass::Standard
            );
            assert_eq!(
                m.approval_policy,
                crate::agent::tool_manifest::ApprovalPolicy::AutoPolicy
            );
            assert!(
                m.default_switch.is_none(),
                "accounting tools bind no switch"
            );
        }
    }
}
