// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! todo 任务清单族（todo_write / todo_read）——会话内结构化任务状态的自管理。
//!
//! 缺位收口（批J 盘点 o14④）：记忆域 todo 类笔记管跨会话长期备忘、交接档
//! todo_next 管会话收尾快照，会话**进行中**的结构化任务清单此前无承载。本族
//! 补齐该缺位：LLM 以全量重写式维护当前清单，runner 每轮装配时把清单摘要经
//! 装配槽位（S3c_todo，配方声明）注入 system prompt——LLM 自写自读自锚定。
//!
//! 设计要点（批K 段4 设计稿）：
//! - **全量重写式**：`items` 数组整体替换旧清单（非增量 patch）——规避并发
//!   部分写歧义，journal 回放重建=取末条快照，零折叠逻辑；
//! - **进程内共享态**：`Arc<TodoState>` 两工具+runner 三方共享（runner 持同
//!   一 Arc 渲染装配摘要）；会话内态，跨会话不保；
//! - **占位/会话期重绑**（handover 同构）：启动期占位注册（未接线调用
//!   fail-visible 如实报错），runner 会话期重绑（state 在手）；G15 续跑从
//!   journal 回放最新 `todo_updated` 重建；
//! - **开关单键双绑**：agentTools.todo 过滤工具契约面（既有机制自动工作）+
//!   runner 装配点按 has_tool 同键判定是否渲染摘要——工具面与注入面同键，
//!   防「清单工具在、状态注入断」的单腿态；
//! - **账面**：todo_write 成功后由 runner 落 journal `todo_updated` 全量快照
//!   （runner 按工具名分支，handover_written 同款）；本工具不直接写 journal。
//!
//! 治理面：CapDomain=Memory（handover 先例）+ Standard 免裁决 + AutoPolicy
//! （进程内内存态读写，无沙箱/网络/子进程面）。

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::io_handlers::tool_handler::ToolFunction;

/// 清单条数上限（防清单膨胀挤占注入预算）
pub const TODO_MAX_ITEMS: usize = 10;
/// 单条内容字符上限
pub const TODO_MAX_CONTENT_CHARS: usize = 500;

/// 工具名
pub const TODO_WRITE_TOOL: &str = "todo_write";
/// 工具名
pub const TODO_READ_TOOL: &str = "todo_read";

/// 条目状态（三态；serde 小写）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// 待办
    Pending,
    /// 进行中
    InProgress,
    /// 已完成
    Completed,
}

impl TodoStatus {
    /// 解析（LLM 参数面字符串 → 枚举；fail-visible 报合法值域）
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            other => Err(format!(
                "invalid status '{other}' (allowed: pending | in_progress | completed)"
            )),
        }
    }

    /// 注入渲染用标记
    fn marker(self) -> &'static str {
        match self {
            Self::Pending => " ",
            Self::InProgress => "*",
            Self::Completed => "x",
        }
    }

    /// journal 快照字符串化（小写枚举名，与 parse 值域互逆）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// 条目优先级（serde 小写）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoPriority {
    /// 高
    High,
    /// 中
    Medium,
    /// 低
    Low,
}

impl TodoPriority {
    /// 解析（缺省=medium 由调用侧处理；显式非法值 fail-visible）
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "high" => Ok(Self::High),
            "medium" => Ok(Self::Medium),
            "low" => Ok(Self::Low),
            other => Err(format!(
                "invalid priority '{other}' (allowed: high | medium | low)"
            )),
        }
    }

    /// journal 快照字符串化（小写枚举名，与 parse 值域互逆）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// 清单条目（LLM 面全量重写式提交）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    /// 条目 id（LLM 命名，清单内唯一）
    pub id: String,
    /// 内容（≤500 字符）
    pub content: String,
    /// 状态
    pub status: TodoStatus,
    /// 优先级
    pub priority: TodoPriority,
}

/// 会话内清单共享态（Arc 工具族+runner 三方共享）
#[derive(Default)]
pub struct TodoState {
    items: Mutex<Vec<TodoItem>>,
}

impl TodoState {
    /// 全量重写（todo_write 唯一写入路径）
    pub fn replace(&self, items: Vec<TodoItem>) {
        if let Ok(mut guard) = self.items.lock() {
            *guard = items;
        }
    }

    /// 当前快照（渲染/落账读侧）
    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.items.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 是否为空（装配渲染缺席判定）
    pub fn is_empty(&self) -> bool {
        self.items.lock().map(|g| g.is_empty()).unwrap_or(true)
    }
}

/// 参数校验 + 解析（纯函数；fail-visible 一次列明全部问题后拒写）
///
/// 规则：items 必须为数组（空数组=清空清单，合法）；每条 id 非空且清单内
/// 唯一、content 非空且 ≤500 字符、status 必填枚举、priority 可缺省为
/// medium（显式给值时须为合法枚举）。
pub fn validate_todo_items(args: &Value) -> Result<Vec<TodoItem>, String> {
    let arr = match args.get("items").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => {
            return Err(
                "missing required parameter 'items' (array; empty array clears the list)"
                    .to_string(),
            )
        }
    };
    if arr.len() > TODO_MAX_ITEMS {
        return Err(format!(
            "items exceeds limit: {} entries > max {TODO_MAX_ITEMS}",
            arr.len()
        ));
    }
    let mut problems: Vec<String> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();
    let mut items = Vec::with_capacity(arr.len());
    for (idx, entry) in arr.iter().enumerate() {
        let tag = format!("items[{idx}]");
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match id {
            Some(id) => {
                if seen_ids.iter().any(|s| s == id) {
                    problems.push(format!("{tag}: duplicate id '{id}'"));
                } else {
                    seen_ids.push(id.to_string());
                }
            }
            None => problems.push(format!("{tag}: 'id' must be a non-empty string")),
        }
        let content = entry
            .get("content")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match content {
            Some(c) if c.chars().count() > TODO_MAX_CONTENT_CHARS => problems.push(format!(
                "{tag}: content exceeds {TODO_MAX_CONTENT_CHARS} chars (got {})",
                c.chars().count()
            )),
            Some(_) => {}
            None => problems.push(format!("{tag}: 'content' must be a non-empty string")),
        }
        let status = match entry.get("status").and_then(|v| v.as_str()) {
            Some(s) => TodoStatus::parse(s).map_err(|e| format!("{tag}: {e}")),
            None => Err(format!("{tag}: 'status' is required")),
        };
        let priority = match entry.get("priority") {
            None | Some(Value::Null) => Ok(TodoPriority::Medium),
            Some(Value::String(s)) => TodoPriority::parse(s).map_err(|e| format!("{tag}: {e}")),
            Some(_) => Err(format!("{tag}: 'priority' must be a string")),
        };
        // status/priority 校验错误入收集(逐条列明,fail-visible 一次报全)
        if let Err(e) = &status {
            problems.push(e.clone());
        }
        if let Err(e) = &priority {
            problems.push(e.clone());
        }
        if let (Some(id), Some(c), Ok(status), Ok(priority)) = (id, content, status, priority) {
            items.push(TodoItem {
                id: id.to_string(),
                content: c.to_string(),
                status,
                priority,
            });
        }
    }
    if problems.is_empty() {
        Ok(items)
    } else {
        Err(problems.join("; "))
    }
}

/// 渲染清单摘要（确定性纯函数：同状态逐字节同输出——装配槽位注入物）
///
/// 格式：`## Current Todo List` 标题 + 未完成条目逐行（`- [ ]/* id content (priority)`）
/// + 完成态计数行。空清单返回 None（槽位静默跳过）。
pub fn render_todo_summary(items: &[TodoItem]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut out = String::from("## Current Todo List\n");
    let mut done = 0usize;
    for it in items {
        match it.status {
            TodoStatus::Completed => done += 1,
            _ => {
                out.push_str(&format!(
                    "- [{}] {} {} ({:?})\n",
                    it.status.marker(),
                    it.id,
                    it.content,
                    it.priority
                ));
            }
        }
    }
    out.push_str(&format!(
        "Completed: {done}/{}. Keep this list current via todo_write.",
        items.len()
    ));
    Some(out)
}

// ---------- 工具执行体 ----------

/// todo_write：全量重写式清单更新（active，进程内内存态）
pub struct TodoWriteTool {
    /// 共享态（None = 占位未接线）
    state: Option<Arc<TodoState>>,
}

impl TodoWriteTool {
    /// 占位实例（启动期注册；调用 fail-visible 如实报错）
    pub fn unwired() -> Self {
        Self { state: None }
    }

    /// 会话期接线实例（runner 重绑）
    pub fn wired(state: Arc<TodoState>) -> Self {
        Self { state: Some(state) }
    }
}

#[async_trait::async_trait]
impl ToolFunction for TodoWriteTool {
    async fn call(&self, args: &Value) -> crate::io_handler::IoResult {
        let state = self.state.as_ref().ok_or_else(|| {
            "todo_write is not wired to session state (placeholder; wired at session start)"
                .to_string()
        })?;
        let items = validate_todo_items(args)?;
        state.replace(items);
        let count = state.snapshot().len();
        Ok(json!({
            "applied": true,
            "count": count,
            "note": "full rewrite applied; list summary is injected into the system prompt each turn"
        }))
    }
}

/// todo_read：读取当前清单（只读）
pub struct TodoReadTool {
    /// 共享态（None = 占位未接线）
    state: Option<Arc<TodoState>>,
}

impl TodoReadTool {
    /// 占位实例
    pub fn unwired() -> Self {
        Self { state: None }
    }

    /// 会话期接线实例
    pub fn wired(state: Arc<TodoState>) -> Self {
        Self { state: Some(state) }
    }
}

#[async_trait::async_trait]
impl ToolFunction for TodoReadTool {
    async fn call(&self, _args: &Value) -> crate::io_handler::IoResult {
        let state = self.state.as_ref().ok_or_else(|| {
            "todo_read is not wired to session state (placeholder; wired at session start)"
                .to_string()
        })?;
        let items = state.snapshot();
        Ok(json!({ "items": items, "count": items.len() }))
    }
}

// ---------- 测试 ----------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item_args(id: &str, content: &str, status: &str, priority: Value) -> Value {
        let mut v = json!({"id": id, "content": content, "status": status});
        if !priority.is_null() {
            v["priority"] = priority;
        }
        v
    }

    #[test]
    fn test_validate_ok_with_default_priority() {
        let args = json!({"items": [item_args("t1", "实现校验", "in_progress", Value::Null)]});
        let items = validate_todo_items(&args).expect("valid");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].priority, TodoPriority::Medium);
        assert_eq!(items[0].status, TodoStatus::InProgress);
    }

    #[test]
    fn test_validate_missing_items_field_fails() {
        assert!(validate_todo_items(&json!({})).is_err());
        assert!(validate_todo_items(&json!({"items": "nope"})).is_err());
    }

    #[test]
    fn test_validate_empty_array_is_legal_clear() {
        let items = validate_todo_items(&json!({"items": []})).expect("empty clears");
        assert!(items.is_empty());
    }

    #[test]
    fn test_validate_over_limit_fails() {
        let items: Vec<Value> = (0..=TODO_MAX_ITEMS)
            .map(|i| item_args(&format!("t{i}"), "x", "pending", Value::Null))
            .collect();
        let err = validate_todo_items(&json!({ "items": items })).unwrap_err();
        assert!(err.contains("exceeds limit"), "got: {err}");
    }

    #[test]
    fn test_validate_content_cap_and_required_fields_fail_visible_once() {
        let long = "字".repeat(TODO_MAX_CONTENT_CHARS + 1);
        let args = json!({"items": [
            item_args("", "no id", "pending", Value::Null),
            item_args("t2", "", "pending", Value::Null),
            item_args("t3", &long, "pending", Value::Null),
            json!({"id": "t4", "content": "x"}),
            item_args("t5", "x", "doing", Value::Null),
            item_args("t6", "x", "pending", json!("urgent")),
            item_args("t1", "dup", "pending", Value::Null),
            item_args("t1", "dup again", "pending", Value::Null),
        ]});
        let err = validate_todo_items(&args).unwrap_err();
        for frag in [
            "items[0]",
            "items[1]",
            "items[2]",
            "items[3]",
            "items[4]",
            "items[5]",
            "duplicate id 't1'",
        ] {
            assert!(err.contains(frag), "missing '{frag}' in: {err}");
        }
    }

    #[test]
    fn test_render_summary_deterministic_and_empty_absent() {
        assert!(render_todo_summary(&[]).is_none());
        let items = vec![
            TodoItem {
                id: "a".into(),
                content: "先做".into(),
                status: TodoStatus::InProgress,
                priority: TodoPriority::High,
            },
            TodoItem {
                id: "b".into(),
                content: "后做".into(),
                status: TodoStatus::Pending,
                priority: TodoPriority::Low,
            },
            TodoItem {
                id: "c".into(),
                content: "完成".into(),
                status: TodoStatus::Completed,
                priority: TodoPriority::Medium,
            },
        ];
        let r1 = render_todo_summary(&items).expect("some");
        let r2 = render_todo_summary(&items).expect("some");
        assert_eq!(r1, r2, "same state must render byte-identical");
        assert!(r1.starts_with("## Current Todo List\n"));
        assert!(r1.contains("- [*] a 先做 (High)"));
        assert!(r1.contains("- [ ] b 后做 (Low)"));
        assert!(
            !r1.contains("完成"),
            "completed items collapse to count line"
        );
        assert!(r1.contains("Completed: 1/3"));
    }

    #[tokio::test]
    async fn test_unwired_fail_visible_and_wired_full_rewrite() {
        let unwired = TodoWriteTool::unwired();
        assert!(unwired
            .call(&json!({"items": []}))
            .await
            .unwrap_err()
            .contains("not wired"));

        let state = Arc::new(TodoState::default());
        let write = TodoWriteTool::wired(state.clone());
        let read = TodoReadTool::wired(state.clone());

        let args = json!({"items": [item_args("t1", "a", "pending", Value::Null)]});
        write.call(&args).await.expect("write ok");
        let snap = read.call(&Value::Null).await.expect("read ok");
        assert_eq!(snap["count"], 1);

        // 全量重写：第二次写整体替换（非追加）
        let args2 = json!({"items": [item_args("t2", "b", "completed", Value::Null)]});
        write.call(&args2).await.expect("rewrite ok");
        let snap2 = read.call(&Value::Null).await.expect("read ok");
        assert_eq!(snap2["count"], 1);
        assert_eq!(snap2["items"][0]["id"], "t2");

        // 清空（空数组）后注入面缺席
        write.call(&json!({"items": []})).await.expect("clear ok");
        assert!(state.is_empty());
        assert!(render_todo_summary(&state.snapshot()).is_none());
    }

    #[test]
    fn test_todo_state_replace_and_snapshot() {
        let state = TodoState::default();
        assert!(state.is_empty());
        state.replace(vec![TodoItem {
            id: "x".into(),
            content: "y".into(),
            status: TodoStatus::Pending,
            priority: TodoPriority::Low,
        }]);
        assert_eq!(state.snapshot().len(), 1);
        state.replace(vec![]);
        assert!(state.is_empty());
    }

    /// 验收③:todo_write 落账 → journal 回放 → resume 重建一致性（全量重写式,
    /// 取末条即终态——模拟 runner wire 的回放重建路径）
    #[test]
    fn test_journal_roundtrip_rebuild_takes_latest_snapshot() {
        use crate::agent::journal::{JournalEvent, JournalWriter, TodoItemSnapshot};
        let dir = std::env::temp_dir().join(format!("jf-todo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let snap = |id: &str, content: &str, status: TodoStatus, priority: TodoPriority| {
            TodoItemSnapshot {
                id: id.to_string(),
                content: content.to_string(),
                status: status.as_str().to_string(),
                priority: priority.as_str().to_string(),
            }
        };
        // 第一次全量重写落账,随后第二次重写(终态)落账
        {
            let w = JournalWriter::open(&dir, "s-todo").unwrap();
            w.todo_updated(vec![snap(
                "t1",
                "旧条目",
                TodoStatus::InProgress,
                TodoPriority::High,
            )])
            .unwrap();
            w.todo_updated(vec![
                snap("t2", "终态甲", TodoStatus::Completed, TodoPriority::Medium),
                snap("t3", "终态乙", TodoStatus::Pending, TodoPriority::Low),
            ])
            .unwrap();
        }
        // 回放重建(与 runner wire 同构:filter TodoUpdated → last → parse)
        let w = JournalWriter::open(&dir, "s-todo").unwrap();
        let lines = w.read_lines().unwrap();
        let latest = lines
            .into_iter()
            .filter_map(|l| match l.event {
                JournalEvent::TodoUpdated { items } => Some(items),
                _ => None,
            })
            .next_back()
            .expect("latest todo_updated present");
        let rebuilt: Vec<TodoItem> = latest
            .into_iter()
            .map(|s| TodoItem {
                id: s.id,
                content: s.content,
                status: TodoStatus::parse(&s.status).unwrap_or(TodoStatus::Pending),
                priority: TodoPriority::parse(&s.priority).unwrap_or(TodoPriority::Medium),
            })
            .collect();
        assert_eq!(rebuilt.len(), 2, "重建取末条快照=终态(非折叠)");
        assert_eq!(rebuilt[0].id, "t2");
        assert_eq!(rebuilt[0].status, TodoStatus::Completed);
        assert_eq!(rebuilt[1].id, "t3");
        assert_eq!(rebuilt[1].priority, TodoPriority::Low);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
