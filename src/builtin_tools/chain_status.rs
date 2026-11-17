// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `chain_status` —— 自主会话链观测工具（P2 多链并行与观测闭环;design-105 A1/A3）。
//!
//! 与 `session_spawn` 启动面配对:spawn 脱管子会话后,父会话经本工具
//! 轮询子会话状态(server 会话真相单源)与链上在册清单——观测闭环补齐
//! 「spawn 后失联」缺口(design-105 缺口②③)。
//!
//! ## 形态
//!
//! - 启动期 unwired 注册(占位:调用 fail-visible);会话期重绑 wired
//!   (与 session_spawn 同款 handover 重绑点);
//! - `action=list`:链上在册子会话清单(spawn 时序)+server 实时状态;
//! - `action=query`+`child_id`:单子会话 server 元数据结构化返回;
//! - 纯查询零写面(ReadOnly);server 不可达=fail-visible 如实报错。

use serde_json::Value;
use std::sync::Arc;

use crate::agent::session_spawn_tool::ChainRuntimeState;
use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 工具名(LLM 契约面/manifest/注册三处同键)
pub const CHAIN_STATUS_TOOL: &str = "chain_status";

/// 会话期接线(重绑点注入;与 session_spawn 同款 handover 重绑形态)
#[derive(Clone)]
pub struct ChainStatusWiring {
    /// 父会话 ID(spawn 发起方)
    pub session_id: String,
    /// server 客户端(子会话元数据查询)
    pub client: EvoruleApiClient,
    /// 链共享运行态(在册子会话清单源)
    pub chain: Arc<ChainRuntimeState>,
}

/// `chain_status` 工具(占位/会话期重绑形态)
pub struct ChainStatusTool {
    wiring: Option<ChainStatusWiring>,
}

impl ChainStatusTool {
    /// 启动期占位构造(未接线调用 fail-visible)
    pub fn unwired() -> Self {
        Self { wiring: None }
    }

    /// 会话期重绑构造(与 session_spawn 同点注入)
    pub fn wired(wiring: ChainStatusWiring) -> Self {
        Self {
            wiring: Some(wiring),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for ChainStatusTool {
    async fn call(&self, args: &Value) -> IoResult {
        let wiring = self.wiring.as_ref().ok_or_else(|| {
            "chain_status: not wired to a session yet (registered as a startup \
                 placeholder; wired at session start with handover tools)"
                .to_string()
        })?;
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: action (list | query)".to_string())?;

        match action {
            "list" => {
                let children = wiring.chain.children_list();
                let mut items: Vec<Value> = Vec::with_capacity(children.len());
                for c in &children {
                    // server 实时状态逐子查询(在册数受宽度帽约束,有界);
                    // 不可达子项状态如实标注降级(概览面 fail-soft,不硬失败)
                    let status = match wiring
                        .client
                        .get_session_metadata(&c.child_session_id)
                        .await
                    {
                        Ok(m) if m.is_finished => "completed".to_string(),
                        Ok(_) => "running".to_string(),
                        Err(e) => format!("unreachable: {e}"),
                    };
                    items.push(serde_json::json!({
                        "child_session_id": c.child_session_id,
                        "first_goal": c.first_goal,
                        "spawned_at_epoch_ms": c.spawned_at_epoch_ms,
                        "server_status": status,
                    }));
                }
                Ok(serde_json::json!({
                    "action": "list",
                    "count": items.len(),
                    "children": items,
                }))
            }
            "query" => {
                let child_id = args
                    .get("child_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing required arg: child_id (string)".to_string())?;
                let known = wiring
                    .chain
                    .children_list()
                    .iter()
                    .any(|c| c.child_session_id == child_id);
                if !known {
                    return Err(format!(
                        "child_id \"{child_id}\" not spawned by this chain (jobs are \
                         chain-local; use the exact id from the spawn envelope)"
                    ));
                }
                let meta = wiring
                    .client
                    .get_session_metadata(child_id)
                    .await
                    .map_err(|e| format!("session metadata query failed for '{child_id}': {e}"))?;
                Ok(serde_json::json!({
                    "action": "query",
                    "child_session_id": meta.session_id,
                    "parent_session_id": meta.parent_session_id,
                    "is_finished": meta.is_finished,
                }))
            }
            other => Err(format!(
                "unknown action \"{other}\" (valid actions: list, query)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_unwired() -> ChainStatusTool {
        ChainStatusTool::unwired()
    }

    fn tool_wired() -> (ChainStatusTool, Arc<ChainRuntimeState>) {
        let client = EvoruleApiClient::new("http://localhost:8080");
        let chain = Arc::new(ChainRuntimeState::new(100_000));
        (
            ChainStatusTool::wired(ChainStatusWiring {
                session_id: "sess-parent".to_string(),
                client: client.clone(),
                chain: chain.clone(),
            }),
            chain,
        )
    }

    #[tokio::test]
    async fn unwired_call_is_fail_visible() {
        let err = tool_unwired()
            .call(&json!({"action": "list"}))
            .await
            .unwrap_err();
        assert!(err.contains("not wired to a session"), "got: {err}");
    }

    #[tokio::test]
    async fn missing_action_is_fail_visible() {
        let (tool, _chain) = tool_wired();
        let err = tool.call(&json!({})).await.unwrap_err();
        assert!(err.contains("missing required arg: action"), "got: {err}");
    }

    #[tokio::test]
    async fn unknown_action_is_fail_visible() {
        let (tool, _chain) = tool_wired();
        let err = tool.call(&json!({"action": "kill"})).await.unwrap_err();
        assert!(err.contains("valid actions: list, query"), "got: {err}");
    }

    #[tokio::test]
    async fn list_reflects_chain_children() {
        let (tool, chain) = tool_wired();
        chain.record_child("child-1", "goal one");
        chain.record_child("child-2", "goal two");
        let r = tool.call(&json!({"action": "list"})).await.unwrap();
        assert_eq!(r.get("count").and_then(|v| v.as_i64()), Some(2));
        let items = r.get("children").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            items[0].get("child_session_id").and_then(|v| v.as_str()),
            Some("child-1")
        );
        assert_eq!(
            items[1].get("first_goal").and_then(|v| v.as_str()),
            Some("goal two")
        );
    }

    #[tokio::test]
    async fn query_unknown_child_is_rejected() {
        let (tool, _chain) = tool_wired();
        let err = tool
            .call(&json!({"action": "query", "child_id": "child-none"}))
            .await
            .unwrap_err();
        assert!(err.contains("not spawned by this chain"), "got: {err}");
    }
}
