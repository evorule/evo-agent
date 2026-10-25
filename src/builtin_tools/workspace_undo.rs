// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! `workspace_undo` —— 工作区写面单步回退(写面撤销批)
//!
//! 零参工具薄壳:语义=弹栈恢复最近一次写面快照(content 字节级写回 /
//! tombstone 删除),多步回退=多次调用。执行序列、路径复验、blob hash
//! 复算核对、fail-visible 语义全部在 [`CheckpointStore::undo`]
//! (crate::agent::workspace_checkpoint);本层只做参数校验与结果组装。
//!
//! 治理形态:manifest Standard + AutoPolicy(免裁决+免审+落账——undo 不
//! 引入新内容,只回退,低于 file_write 的 Sentineled);红线面由
//! `hits_red_line` 写面分支自然拦截(名单含本工具,恢复路径指向治理配置
//! 同样强制人工);开关 `agentTools.workspaceUndo`(默认开——安全网能力)。

use std::sync::Arc;

use serde_json::Value;

use crate::agent::workspace_checkpoint::{CheckpointStore, UndoOutcome};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 工具名(注册面/manifest/开关常量同源)
pub const WORKSPACE_UNDO_TOOL: &str = "workspace_undo";

/// `workspace_undo` 工具
#[derive(Clone)]
pub struct WorkspaceUndoTool {
    store: Arc<CheckpointStore>,
}

impl WorkspaceUndoTool {
    /// 构造(store 与 runner 快照钩子实例可各自持有,同 root 盘面 SSOT)
    pub fn new(store: Arc<CheckpointStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl ToolFunction for WorkspaceUndoTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 操作
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("workspace_undo tool panicked: {e}"))?
    }
}

impl WorkspaceUndoTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &Value) -> IoResult {
        // 零参契约:语义=回退最近一步(参数面最小化,防模型乱序指定 seq
        // 造成状态跳跃);多余参数显式拒绝
        if let Some(obj) = args.as_object() {
            if !obj.is_empty() {
                return Err(format!(
                    "workspace_undo takes no arguments (got: {})",
                    obj.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }

        let outcome = self.store.undo(None)?;

        Ok(outcome_json(&outcome))
    }
}

/// 结果组装:content=恢复形态;tombstone=移除形态(设计批 §5.3 字面)
fn outcome_json(outcome: &UndoOutcome) -> Value {
    let mut map = serde_json::Map::new();
    if outcome.kind == crate::agent::workspace_checkpoint::KIND_TOMBSTONE {
        map.insert(
            "removed_path".to_string(),
            Value::from(outcome.path.clone()),
        );
    } else {
        map.insert(
            "restored_path".to_string(),
            Value::from(outcome.path.clone()),
        );
        map.insert("bytes".to_string(), Value::from(outcome.bytes as i64));
    }
    map.insert("kind".to_string(), Value::from(outcome.kind.clone()));
    map.insert(
        "restored_hash".to_string(),
        Value::from(outcome.restored_hash.clone()),
    );
    map.insert(
        "popped_seq".to_string(),
        Value::from(outcome.popped_seq as i64),
    );
    map.insert(
        "remaining_depth".to_string(),
        Value::from(outcome.remaining_depth as i64),
    );
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handlers::tool_handler::ToolFunction as _;

    fn temp_store() -> (tempfile::TempDir, Arc<CheckpointStore>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("workspace")).unwrap();
        let store = Arc::new(CheckpointStore::new(dir.path(), "workspace"));
        (dir, store)
    }

    #[tokio::test]
    async fn undo_restores_content_via_tool_call() {
        let (dir, store) = temp_store();
        std::fs::write(dir.path().join("workspace/a.txt"), b"old bytes").unwrap();
        let p = store.pre_capture("workspace/a.txt", "file_write").unwrap();
        store.commit(&p, None).unwrap();
        std::fs::write(dir.path().join("workspace/a.txt"), b"new bytes").unwrap();

        let tool = WorkspaceUndoTool::new(store);
        let result = tool.call(&serde_json::json!({})).await.unwrap();
        assert_eq!(result["restored_path"], "workspace/a.txt");
        assert_eq!(result["kind"], "content");
        assert_eq!(result["bytes"], 9);
        assert_eq!(result["popped_seq"], 1);
        assert_eq!(result["remaining_depth"], 0);
        let restored = std::fs::read(dir.path().join("workspace/a.txt")).unwrap();
        assert_eq!(restored, b"old bytes");
    }

    #[tokio::test]
    async fn undo_tombstone_reports_removed_path() {
        let (dir, store) = temp_store();
        let p = store.pre_capture("workspace/new.txt", "file_create").unwrap();
        store.commit(&p, None).unwrap();
        std::fs::write(dir.path().join("workspace/new.txt"), b"x").unwrap();

        let tool = WorkspaceUndoTool::new(store);
        let result = tool.call(&serde_json::json!({})).await.unwrap();
        assert_eq!(result["removed_path"], "workspace/new.txt");
        assert_eq!(result["kind"], "tombstone");
        assert!(!dir.path().join("workspace/new.txt").exists());
    }

    #[tokio::test]
    async fn empty_stack_fails_visibly() {
        let (_dir, store) = temp_store();
        let tool = WorkspaceUndoTool::new(store);
        let err = tool.call(&serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("no checkpoint to undo"), "got: {err}");
    }

    #[tokio::test]
    async fn rejects_extra_arguments() {
        let (_dir, store) = temp_store();
        let tool = WorkspaceUndoTool::new(store);
        let err = tool
            .call(&serde_json::json!({"seq": 3}))
            .await
            .unwrap_err();
        assert!(err.contains("takes no arguments"), "got: {err}");
    }
}
