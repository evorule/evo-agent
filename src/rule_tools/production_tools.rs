// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 生产状态/审计工具（2 个）：prod_state / prod_audit

use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::api::workspace_client::WorkspaceApiClient;
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use crate::json_convert::serde_to_tcb;

// =============================================================================
// prod_state —— 查询当前生产状态
// =============================================================================

#[derive(Clone)]
pub struct ProdStateTool {
    client: WorkspaceApiClient,
}

impl ProdStateTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for ProdStateTool {
    async fn call(&self, _args: &JsonValue) -> IoResult {
        let result = self
            .client
            .get_production_state()
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// prod_audit —— 查询发布审计历史
// =============================================================================

#[derive(Clone)]
pub struct ProdAuditTool {
    client: WorkspaceApiClient,
}

impl ProdAuditTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for ProdAuditTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let limit = args.get("limit").and_then(|v| v.as_i64());
        let result = self
            .client
            .list_production_audit(limit)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// register / specs
// =============================================================================

pub fn register(h: &mut ToolHandler, client: &WorkspaceApiClient) {
    h.register_tool("prod_state", Arc::new(ProdStateTool::new(client.clone())));
    h.register_tool("prod_audit", Arc::new(ProdAuditTool::new(client.clone())));
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "prod_state".to_string(),
            description: "Get the current production state (active session + ruleset version)."
                .to_string(),
            parameters: vec![],
        },
        ToolSpec {
            name: "prod_audit".to_string(),
            description: "List publish/rollback audit history (most recent first).".to_string(),
            parameters: vec![ParameterSpec {
                name: "limit".to_string(),
                r#type: "integer".to_string(),
                description: "Optional max number of records to return (default 50).".to_string(),
                required: false,
            }],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_client() -> WorkspaceApiClient {
        WorkspaceApiClient::new("http://localhost:0")
    }

    #[test]
    fn test_specs_count() {
        assert_eq!(specs().len(), 2);
    }

    #[test]
    fn test_register_tools() {
        let client = make_client();
        let mut h = ToolHandler::new();
        register(&mut h, &client);
        assert!(h.has_tool("prod_state"));
        assert!(h.has_tool("prod_audit"));
    }

    #[tokio::test]
    async fn test_prod_state_no_args_is_network_error() {
        // prod_state 无参数；空 args 不应触发 missing-parameter，而是网络连接错误。
        let tool = ProdStateTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(!result.unwrap_err().contains("missing required parameter"));
    }

    #[tokio::test]
    async fn test_prod_audit_no_args_is_network_error() {
        // prod_audit 的 limit 是可选；空 args 不应触发 missing-parameter。
        let tool = ProdAuditTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(!result.unwrap_err().contains("missing required parameter"));
    }
}
