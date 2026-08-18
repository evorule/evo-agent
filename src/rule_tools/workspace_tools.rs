// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Workspace 管理工具（2 个）：ws_list / ws_create

use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::api::workspace_client::{CreateWorkspaceRequest, WorkspaceApiClient};
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use crate::json_convert::serde_to_tcb;

// =============================================================================
// ws_list —— 列出 workspace（可按 owner 过滤）
// =============================================================================

#[derive(Clone)]
pub struct WsListTool {
    client: WorkspaceApiClient,
}

impl WsListTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for WsListTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let owner_id = args.get("owner_id").and_then(|v| v.as_str());
        let result = self
            .client
            .list_workspaces(owner_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// ws_create —— 创建 workspace
// =============================================================================

#[derive(Clone)]
pub struct WsCreateTool {
    client: WorkspaceApiClient,
}

impl WsCreateTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for WsCreateTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: name".to_string())?;
        let owner_id = args
            .get("owner_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: owner_id".to_string())?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let req = CreateWorkspaceRequest {
            name: name.to_string(),
            owner_id: owner_id.to_string(),
            description,
        };
        let result = self
            .client
            .create_workspace(req)
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
    h.register_tool("ws_list", Arc::new(WsListTool::new(client.clone())));
    h.register_tool("ws_create", Arc::new(WsCreateTool::new(client.clone())));
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "ws_list".to_string(),
            description: "List all workspaces, optionally filtered by owner_id.".to_string(),
            parameters: vec![ParameterSpec {
                name: "owner_id".to_string(),
                r#type: "string".to_string(),
                description: "Optional owner id to filter workspaces.".to_string(),
                required: false,
            }],
        },
        ToolSpec {
            name: "ws_create".to_string(),
            description: "Create a new workspace.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "name".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace name.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "owner_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Owner id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "description".to_string(),
                    r#type: "string".to_string(),
                    description: "Optional workspace description.".to_string(),
                    required: false,
                },
            ],
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
        assert!(h.has_tool("ws_list"));
        assert!(h.has_tool("ws_create"));
    }

    #[tokio::test]
    async fn test_ws_create_missing_name() {
        let tool = WsCreateTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: name"));
    }

    #[tokio::test]
    async fn test_ws_create_missing_owner_id() {
        let tool = WsCreateTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("name".to_string(), JsonValue::string("ws"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: owner_id"));
    }
}
