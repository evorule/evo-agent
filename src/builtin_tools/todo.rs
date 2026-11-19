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
//! 处置表四桶扩展（todo v2 disposition）：三态基础上增 Deferred（挂起，登记
//! 不修）/Reworked（返工）/Transformed（转形态）——挂起/转形态**必附**
//! `disposition_note`（处置判据+恢复条件，缺注拒写 fail-visible）；既有三态
//! 行为与序列化对 v1 完全不变。另附**表项-动作对账计数器**（observe 态纯
//! 函数：只计数不拦截，计划内/表外两计数，供漂移度量器后续批消费）。
//!
//! 治理面：CapDomain=Memory（handover 先例）+ Standard 免裁决 + AutoPolicy
//! （进程内内存态读写，无沙箱/网络/子进程面）。

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::tool_intent::ToolIntentV1;
use crate::io_handlers::tool_handler::ToolFunction;

/// 清单条数上限（防清单膨胀挤占注入预算）
pub const TODO_MAX_ITEMS: usize = 10;
/// 单条内容字符上限
pub const TODO_MAX_CONTENT_CHARS: usize = 500;
/// 单条处置注记字符上限（与 content 同帽，防注记膨胀挤占注入预算）
pub const TODO_MAX_NOTE_CHARS: usize = 500;

/// 工具名
pub const TODO_WRITE_TOOL: &str = "todo_write";
/// 工具名
pub const TODO_READ_TOOL: &str = "todo_read";

/// 条目状态（v2 六态四桶；serde 小写）
///
/// 四桶口径（处置表制度泛化）：进桶=Pending/InProgress/Completed 既有三态
/// （行为与序列化对 v1 完全不变）；patch/处置桶=Reworked（返工）；登记桶=
/// Deferred（挂起，必附 [`TodoItem::disposition_note`]，登记不修）；转形态桶=
/// Transformed（派生到他形态，同样必附处置注记）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// 待办
    Pending,
    /// 进行中
    InProgress,
    /// 已完成
    Completed,
    /// 挂起（登记不修：必附处置注记=处置判据+恢复条件）
    Deferred,
    /// 返工（patch 桶）
    Reworked,
    /// 转形态（派生到他形态：必附处置注记）
    Transformed,
}

impl TodoStatus {
    /// 解析（LLM 参数面字符串 → 枚举；fail-visible 报合法值域）
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "deferred" => Ok(Self::Deferred),
            "reworked" => Ok(Self::Reworked),
            "transformed" => Ok(Self::Transformed),
            other => Err(format!(
                "invalid status '{other}' (allowed: pending | in_progress | completed | \
                 deferred | reworked | transformed)"
            )),
        }
    }

    /// 挂起/转形态桶必附处置注记（登记不修纪律：缺注拒写，fail-visible）
    pub fn requires_disposition_note(self) -> bool {
        matches!(self, Self::Deferred | Self::Transformed)
    }

    /// 注入渲染用标记
    fn marker(self) -> &'static str {
        match self {
            Self::Pending => " ",
            Self::InProgress => "*",
            Self::Completed => "x",
            Self::Deferred => "D",
            Self::Reworked => "R",
            Self::Transformed => "T",
        }
    }

    /// journal 快照字符串化（小写枚举名，与 parse 值域互逆）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Deferred => "deferred",
            Self::Reworked => "reworked",
            Self::Transformed => "transformed",
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
///
/// v1 兼容：`disposition_note` 缺省反序列化为 None，且 None 时不参与序列化
/// （`skip_serializing_if`）——v1 账面 JSON（四字段形态）逐字节不变。
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
    /// 处置注记（挂起/转形态必附：处置判据+恢复条件，登记不修形态；
    /// Other statuses: optional note. <=500 chars, non-empty when present)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition_note: Option<String>,
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
/// medium（显式给值时须为合法枚举）；`disposition_note` 可选，但在场即须
/// 非空字符串且 ≤500 字符，**挂起（deferred）/转形态（transformed）条目必附**
/// （处置判据+恢复条件，登记不修纪律：缺注拒写，handover 六字段先例同款
/// fail-visible）。
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
        let note = match entry.get("disposition_note") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => {
                let t = s.trim();
                if t.is_empty() {
                    Err(format!(
                        "{tag}: 'disposition_note' must be a non-empty string when present"
                    ))
                } else if t.chars().count() > TODO_MAX_NOTE_CHARS {
                    Err(format!(
                        "{tag}: disposition_note exceeds {TODO_MAX_NOTE_CHARS} chars (got {})",
                        t.chars().count()
                    ))
                } else {
                    Ok(Some(t.to_string()))
                }
            }
            Some(_) => Err(format!("{tag}: 'disposition_note' must be a string")),
        };
        // status/priority/note 校验错误入收集(逐条列明,fail-visible 一次报全)
        if let Err(e) = &status {
            problems.push(e.clone());
        }
        if let Err(e) = &priority {
            problems.push(e.clone());
        }
        if let Err(e) = &note {
            problems.push(e.clone());
        }
        // 登记不修纪律:挂起/转形态必附处置注记(缺注拒写,fail-visible)
        let usable_note = matches!(&note, Ok(Some(n)) if !n.is_empty());
        if let Ok(st) = &status {
            if st.requires_disposition_note() && !usable_note {
                problems.push(format!(
                    "{tag}: status '{}' requires a non-empty 'disposition_note' \
                     (handling criteria + recovery condition; such entries are \
                     registered only, not worked)",
                    st.as_str()
                ));
            }
        }
        if let (Some(id), Some(c), Ok(status), Ok(priority), Ok(note)) =
            (id, content, status, priority, note)
        {
            items.push(TodoItem {
                id: id.to_string(),
                content: c.to_string(),
                status,
                priority,
                disposition_note: note,
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
/// 格式：`## Current Todo List` 标题；未完成条目逐行
/// （`- [ ]/* id content (priority)`）；挂起桶单列段（在场时：
/// `Deferred (registered only, not being worked):` 标题，逐行携带处置注记）；
/// 完成态计数行。空清单返回 None（槽位静默跳过）。
pub fn render_todo_summary(items: &[TodoItem]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut out = String::from("## Current Todo List\n");
    let mut done = 0usize;
    let mut deferred: Vec<&TodoItem> = Vec::new();
    for it in items {
        match it.status {
            TodoStatus::Completed => done += 1,
            TodoStatus::Deferred => deferred.push(it),
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
    // 挂起桶单列渲染（登记不修：与在工条目分段，注记=恢复条件随行可见）
    if !deferred.is_empty() {
        out.push_str("Deferred (registered only, not being worked):\n");
        for it in deferred {
            out.push_str(&format!(
                "- [{}] {} {} ({:?}){}\n",
                it.status.marker(),
                it.id,
                it.content,
                it.priority,
                match it.disposition_note.as_deref() {
                    Some(n) => format!(" | note: {n}"),
                    None => String::new(),
                }
            ));
        }
    }
    out.push_str(&format!(
        "Completed: {done}/{}. Keep this list current via todo_write.",
        items.len()
    ));
    Some(out)
}

// ---------- 表项-动作对账计数器（observe 态纯函数） ----------

/// 对账计数（observe 态输出；供漂移度量器结算面消费）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileCounts {
    /// 计划内动作数（归到至少一个表项——含返工/转形态桶条目——或表自维护）
    pub planned_actions: usize,
    /// 表外动作数（无法归到任何表项的动作）
    pub out_of_table_actions: usize,
}

/// 表项-动作对账计数器（纯函数；**observe 态：只计数，不拦截**）
///
/// 归属规则 v1（确定性、可复算）：
/// 1. 表自维护动作（`todo_write`/`todo_read`，作用于表本身）恒归属计划内；
/// 2. 其余动作按 `target_scope`（缺省视为空）与表项 id 集合做大小写不敏感
///    子串匹配——**全桶 id 皆锚**（返工/转形态条目仍是归属锚，即其 patch/
///    转形态派生动作同样归属）；命中任一锚=计划内，否则=表外。
///
/// observe 纪律（外泄扫描三态同款）：本函数零副作用、无拒绝路径，输出不流
/// 入任何写门/审批判定——计数变化不改变任何行为；阈值与 enforce 态归漂移
/// 度量器后续批（先 observe 攒数据再定阈值）。id 子串匹配是 v1 启发式（短
/// id 可能过匹配/漏匹配）——观测态正是为量化该误报率而设，归属规则富化
/// （如注记引用的派生锚、args 面匹配）随数据一起后置。
pub fn reconcile_actions(items: &[TodoItem], signals: &[ToolIntentV1]) -> ReconcileCounts {
    let anchors: Vec<String> = items
        .iter()
        .map(|it| it.id.trim().to_lowercase())
        .filter(|a| !a.is_empty())
        .collect();
    let mut counts = ReconcileCounts {
        planned_actions: 0,
        out_of_table_actions: 0,
    };
    for sig in signals {
        if sig.tool_name == TODO_WRITE_TOOL || sig.tool_name == TODO_READ_TOOL {
            counts.planned_actions += 1;
            continue;
        }
        let target = sig
            .target_scope
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_lowercase();
        if !target.is_empty() && anchors.iter().any(|a| target.contains(a)) {
            counts.planned_actions += 1;
        } else {
            counts.out_of_table_actions += 1;
        }
    }
    counts
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
    fn is_wired(&self) -> bool {
        self.state.is_some()
    }

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
    fn is_wired(&self) -> bool {
        self.state.is_some()
    }

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
                disposition_note: None,
            },
            TodoItem {
                id: "b".into(),
                content: "后做".into(),
                status: TodoStatus::Pending,
                priority: TodoPriority::Low,
                disposition_note: None,
            },
            TodoItem {
                id: "c".into(),
                content: "完成".into(),
                status: TodoStatus::Completed,
                priority: TodoPriority::Medium,
                disposition_note: None,
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
            disposition_note: None,
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
                // journal 快照 v1 字段形态无注记面：回放重建注记置空（注记为
                // 会话内存态+读面可见；快照面增量归快照 schema 批次）
                disposition_note: None,
            })
            .collect();
        assert_eq!(rebuilt.len(), 2, "重建取末条快照=终态(非折叠)");
        assert_eq!(rebuilt[0].id, "t2");
        assert_eq!(rebuilt[0].status, TodoStatus::Completed);
        assert_eq!(rebuilt[1].id, "t3");
        assert_eq!(rebuilt[1].priority, TodoPriority::Low);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 验收①:v1 三态账面序列化零红——无注记条目序列化不含 disposition_note
    /// 键(四字段形态逐字节同 v1),v1 JSON(含显式 null 注记)反序列化无损。
    #[test]
    fn test_v1_three_state_serde_bytes_unchanged() {
        let v1_item = TodoItem {
            id: "t1".into(),
            content: "既有三态条目".into(),
            status: TodoStatus::InProgress,
            priority: TodoPriority::High,
            disposition_note: None,
        };
        let s = serde_json::to_string(&v1_item).expect("serialize");
        assert_eq!(
            s, r#"{"id":"t1","content":"既有三态条目","status":"in_progress","priority":"high"}"#,
            "v1 form must be byte-identical (no disposition_note key)"
        );
        // v1 账面 JSON(无注记字段)反序列化 → 注记 None
        let back: TodoItem = serde_json::from_str(&s).expect("v1 json parses");
        assert_eq!(back, v1_item);
        assert!(back.disposition_note.is_none());
        // 显式 null 注记亦接受(v1 客户端形态)
        let with_null: TodoItem = serde_json::from_value(json!({
            "id": "t2", "content": "x", "status": "pending",
            "priority": "low", "disposition_note": null
        }))
        .expect("null note parses");
        assert!(with_null.disposition_note.is_none());
    }

    /// 验收②:四桶全形态 parse/as_str/serde 读写回放(六态值域互逆+注记保留)。
    #[test]
    fn test_four_bucket_full_form_roundtrip() {
        let cases = [
            ("pending", TodoStatus::Pending),
            ("in_progress", TodoStatus::InProgress),
            ("completed", TodoStatus::Completed),
            ("deferred", TodoStatus::Deferred),
            ("reworked", TodoStatus::Reworked),
            ("transformed", TodoStatus::Transformed),
        ];
        for (word, st) in cases {
            assert_eq!(TodoStatus::parse(word).expect("parse"), st);
            assert_eq!(st.as_str(), word, "as_str 与 parse 值域互逆");
            assert_eq!(TodoStatus::parse(st.as_str()).expect("re-parse"), st);
        }
        // 非法值 fail-visible 列全六态合法值域
        let err = TodoStatus::parse("doing").unwrap_err();
        for frag in [
            "pending",
            "in_progress",
            "completed",
            "deferred",
            "reworked",
            "transformed",
        ] {
            assert!(
                err.contains(frag),
                "allowed list must contain {frag}: {err}"
            );
        }
        // 挂起+注记 serde 往返无损
        let item = TodoItem {
            id: "d1".into(),
            content: "挂起条目".into(),
            status: TodoStatus::Deferred,
            priority: TodoPriority::Medium,
            disposition_note: Some("恢复条件:上游 schema 批合入后重开".into()),
        };
        let v = serde_json::to_value(&item).expect("serialize");
        assert_eq!(v["status"], "deferred");
        assert!(v.get("disposition_note").is_some());
        let back: TodoItem = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, item, "注记往返无损");
        // 转形态同理（显式给状态——结构更新语法仅覆写注记字段）
        let t = TodoItem {
            status: TodoStatus::Transformed,
            disposition_note: Some("转形态:并入 rule_set 批跟踪".into()),
            ..item
        };
        let tv = serde_json::to_value(t.clone()).expect("serialize");
        assert_eq!(tv["status"], "transformed");
        let tb: TodoItem = serde_json::from_value(tv).expect("deserialize");
        assert_eq!(tb, t);
    }

    /// 验收③:挂起/转形态缺 disposition_note 拒写(fail-visible,一次列明);
    /// 附注后放行;返工/三态注记可选(不强制)。
    #[test]
    fn test_deferred_missing_note_rejected_fail_visible() {
        // 挂起缺注 → 拒
        let args = json!({"items": [item_args("t1", "登记不修", "deferred", Value::Null)]});
        let err = validate_todo_items(&args).unwrap_err();
        assert!(err.contains("items[0]"), "tagged: {err}");
        assert!(
            err.contains("disposition_note"),
            "must name the missing field: {err}"
        );
        // 转形态缺注 → 拒
        let args = json!({"items": [item_args("t2", "转形态", "transformed", Value::Null)]});
        let err = validate_todo_items(&args).unwrap_err();
        assert!(err.contains("disposition_note"), "{err}");
        // 在场但空串注记 → 拒(两问题并列,一次报全)
        let args = json!({"items": [{"id": "t3", "content": "x", "status": "deferred", "disposition_note": "   "}]});
        let err = validate_todo_items(&args).unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
        // 挂起附注 → 放行
        let args = json!({"items": [{
            "id": "t4", "content": "登记不修", "status": "deferred",
            "disposition_note": "处置判据:非阻塞;恢复条件:依赖批合入"
        }]});
        let items = validate_todo_items(&args).expect("note present passes");
        assert_eq!(items[0].status, TodoStatus::Deferred);
        assert!(items[0].disposition_note.is_some());
        // 返工(注记可选)与三态不强制注记 → 放行
        for status in ["reworked", "pending", "in_progress", "completed"] {
            let args = json!({"items": [item_args("t5", "x", status, Value::Null)]});
            assert!(
                validate_todo_items(&args).is_ok(),
                "{status} must not require a note"
            );
        }
        // 注记超帽 → 拒
        let long = "字".repeat(TODO_MAX_NOTE_CHARS + 1);
        let args = json!({"items": [{"id": "t6", "content": "x", "status": "deferred", "disposition_note": long}]});
        let err = validate_todo_items(&args).unwrap_err();
        assert!(err.contains("disposition_note exceeds"), "{err}");
    }

    /// 表项 fixture(对账计数器用:含返工/转形态/挂起桶各一)
    fn fixture_table() -> Vec<TodoItem> {
        vec![
            TodoItem {
                id: "parser-fix".into(),
                content: "修复解析器".into(),
                status: TodoStatus::InProgress,
                priority: TodoPriority::High,
                disposition_note: None,
            },
            TodoItem {
                id: "registry-sweep".into(),
                content: "登记扫尾".into(),
                status: TodoStatus::Reworked,
                priority: TodoPriority::Medium,
                disposition_note: None,
            },
            TodoItem {
                id: "doc-handover".into(),
                content: "交接文档".into(),
                status: TodoStatus::Transformed,
                priority: TodoPriority::Medium,
                disposition_note: Some("转形态:并入规则批;恢复条件:规则批开工".into()),
            },
            TodoItem {
                id: "lsp-probe".into(),
                content: "探针登记".into(),
                status: TodoStatus::Deferred,
                priority: TodoPriority::Low,
                disposition_note: Some("登记不修;恢复条件:上游目录扩域后重开".into()),
            },
        ]
    }

    /// 信号构造(走真实 args 摘要口径;counter 只读 tool_name/target_scope)
    fn sig(tool: &str, target: Option<&str>) -> ToolIntentV1 {
        ToolIntentV1 {
            tool_name: tool.to_string(),
            target_scope: target.map(str::to_string),
            args: None,
            args_digest: crate::agent::tool_intent::args_digest(&json!({})),
            session_ref: None,
            schema_ver: crate::agent::tool_intent::TOOL_INTENT_SCHEMA_VER,
        }
    }

    /// 验收④:对账计数器手工 fixture(10 动作/8 归表/2 表外)计数=2;
    /// 同输入两次结算相等(确定性);planned+out=总信号数守恒。
    #[test]
    fn test_reconcile_fixture_10_actions_8_on_table_2_out() {
        let table = fixture_table();
        let signals = vec![
            sig("file_write", Some("src/parser-fix/core.rs")), // 归(子串)
            sig("file_edit", Some("SRC/PARSER-FIX/Lexer.rs")), // 归(大小写不敏感)
            sig("shell_exec", Some("cargo test parser-fix")),  // 归
            sig("file_write", Some("docs/registry-sweep.md")), // 归(返工桶仍锚)
            sig("file_edit", Some("rules/doc-handover.json")), // 归(转形态桶仍锚=派生动作)
            sig("file_read", Some("notes/lsp-probe.md")),      // 归(挂起桶仍锚)
            sig("todo_write", None),                           // 归(表自维护)
            sig("todo_read", None),                            // 归(表自维护)
            sig("file_write", Some("out_of_sandbox:/tmp/x")),  // 表外
            sig("shell_exec", None),                           // 表外(无 scope 不可归)
        ];
        let c1 = reconcile_actions(&table, &signals);
        let c2 = reconcile_actions(&table, &signals);
        assert_eq!(c1, c2, "同输入两次结算逐字段相等(确定性)");
        assert_eq!(c1.planned_actions, 8, "8 on-table");
        assert_eq!(c1.out_of_table_actions, 2, "fixture 表外计数=2");
        assert_eq!(
            c1.planned_actions + c1.out_of_table_actions,
            signals.len(),
            "守恒"
        );
        // 空表 → 除表自维护外全表外(无锚可归,observe 如实计数)
        let empty = reconcile_actions(&[], &signals);
        assert_eq!(empty.out_of_table_actions, 8);
        assert_eq!(empty.planned_actions, 2);
    }

    /// 验收④(observe 语义):计数变化零拦截——计数器输出不流入写门,
    /// 全表外信号在场的同批写路径照常放行;计数器零副作用。
    #[test]
    fn test_reconcile_observe_no_interception() {
        let table = fixture_table();
        let hostile = vec![sig("shell_exec", None); 5];
        let c = reconcile_actions(&table, &hostile);
        assert_eq!(c.out_of_table_actions, 5, "observe 如实计数");
        // 写门不消费计数:挂起+附注的写照常放行(与计数无关)
        let args = json!({"items": [{
            "id": "t9", "content": "照常登记", "status": "deferred",
            "disposition_note": "恢复条件:下一批开工"
        }]});
        assert!(
            validate_todo_items(&args).is_ok(),
            "write gate ignores counts"
        );
        // 纯函数零副作用:重放同输入结果不变,无隐藏态
        let again = reconcile_actions(&table, &hostile);
        assert_eq!(c, again);
    }

    /// 注入面:挂起桶单列渲染(与在工条目分段,注记随行可见;确定性;v1 形态
    /// 清单不含挂起段=零红)。
    #[test]
    fn test_render_deferred_bucket_separate_with_note() {
        let items = fixture_table();
        let r1 = render_todo_summary(&items).expect("some");
        let r2 = render_todo_summary(&items).expect("some");
        assert_eq!(r1, r2, "byte-identical");
        // 挂起段单列
        assert!(
            r1.contains("Deferred (registered only, not being worked):"),
            "{r1}"
        );
        let dline = r1
            .lines()
            .find(|l| l.contains("lsp-probe"))
            .expect("deferred line present");
        assert!(dline.starts_with("- [D]"), "{dline}");
        assert!(
            dline.contains("note: 登记不修"),
            "note rides the line: {dline}"
        );
        // 在工条目仍内联(返工/转形态标记),挂起条目不混入在工段
        assert!(r1.contains("- [*] parser-fix"));
        assert!(r1.contains("- [R] registry-sweep"));
        assert!(r1.contains("- [T] doc-handover"));
        let work_head = r1.split("Deferred (registered").next().expect("head");
        assert!(
            !work_head.contains("lsp-probe"),
            "deferred not in work section"
        );
        assert!(r1.contains("Completed: 0/4"));
        // v1 形态清单(无挂起)不含挂起段=渲染零红
        let v1_only = vec![TodoItem {
            id: "a".into(),
            content: "先做".into(),
            status: TodoStatus::InProgress,
            priority: TodoPriority::High,
            disposition_note: None,
        }];
        let rv = render_todo_summary(&v1_only).expect("some");
        assert!(!rv.contains("Deferred (registered"), "no deferred section");
        assert!(rv.contains("- [*] a 先做 (High)"));
    }

    /// 四桶经既有 journal 快照形态(v1 字段:status 字符串)落账回放——状态六态
    /// 互逆恢复;注记非快照字段,重建如实置空(快照面增量归快照 schema 批次)。
    #[test]
    fn test_journal_v1_shape_snapshot_four_bucket_replay() {
        use crate::agent::journal::{JournalEvent, JournalWriter, TodoItemSnapshot};
        let dir = std::env::temp_dir().join(format!("jf-todo-v2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let snap = |id: &str, status: TodoStatus| TodoItemSnapshot {
            id: id.to_string(),
            content: "四桶条目".to_string(),
            status: status.as_str().to_string(),
            priority: "medium".to_string(),
        };
        {
            let w = JournalWriter::open(&dir, "s-todo-v2").unwrap();
            w.todo_updated(vec![
                snap("t1", TodoStatus::Deferred),
                snap("t2", TodoStatus::Reworked),
                snap("t3", TodoStatus::Transformed),
            ])
            .unwrap();
        }
        let w = JournalWriter::open(&dir, "s-todo-v2").unwrap();
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
                disposition_note: None,
            })
            .collect();
        assert_eq!(rebuilt[0].status, TodoStatus::Deferred);
        assert_eq!(rebuilt[1].status, TodoStatus::Reworked);
        assert_eq!(rebuilt[2].status, TodoStatus::Transformed);
        assert!(
            rebuilt.iter().all(|it| it.disposition_note.is_none()),
            "v1 snapshot shape carries no note; rebuild is honestly empty"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
