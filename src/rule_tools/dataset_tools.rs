// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 测试数据集工具（2 个）：dataset_create / dataset_list

use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::api::workspace_client::{CreateTestDatasetRequest, WorkspaceApiClient};
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use crate::json_convert::serde_to_tcb;

// =============================================================================
// dataset_create —— 创建合成测试数据集
// =============================================================================

#[derive(Clone)]
pub struct DatasetCreateTool {
    client: WorkspaceApiClient,
}

impl DatasetCreateTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for DatasetCreateTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: name".to_string())?;
        let cases_json = args
            .get("cases_json")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: cases_json".to_string())?;
        let created_by = args
            .get("created_by")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: created_by".to_string())?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let req = CreateTestDatasetRequest {
            name: name.to_string(),
            cases_json: cases_json.to_string(),
            created_by: created_by.to_string(),
            // body 中的 workspace_id 留空，由 server 按 path 校验归属
            workspace_id: None,
            description,
        };
        let result = self
            .client
            .create_test_dataset(workspace_id, req)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// dataset_list —— 列出测试数据集
// =============================================================================

#[derive(Clone)]
pub struct DatasetListTool {
    client: WorkspaceApiClient,
}

impl DatasetListTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for DatasetListTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let result = self
            .client
            .list_test_datasets(workspace_id)
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
    h.register_tool(
        "dataset_create",
        Arc::new(DatasetCreateTool::new(client.clone())),
    );
    h.register_tool(
        "dataset_list",
        Arc::new(DatasetListTool::new(client.clone())),
    );
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "dataset_create".to_string(),
            description: "Create a synthetic test dataset for sandbox testing.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id (path).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "name".to_string(),
                    r#type: "string".to_string(),
                    description: "Dataset name.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "cases_json".to_string(),
                    r#type: "string".to_string(),
                    description: "Test cases as a JSON array string.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "created_by".to_string(),
                    r#type: "string".to_string(),
                    description: "Creator user id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "description".to_string(),
                    r#type: "string".to_string(),
                    description: "Optional dataset description.".to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "dataset_list".to_string(),
            description: "List test datasets for a workspace.".to_string(),
            parameters: vec![ParameterSpec {
                name: "workspace_id".to_string(),
                r#type: "string".to_string(),
                description: "Workspace id.".to_string(),
                required: true,
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
        assert!(h.has_tool("dataset_create"));
        assert!(h.has_tool("dataset_list"));
    }

    #[tokio::test]
    async fn test_dataset_create_missing_name() {
        let tool = DatasetCreateTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: name"));
    }

    #[tokio::test]
    async fn test_dataset_create_missing_cases_json() {
        let tool = DatasetCreateTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        m.insert("name".to_string(), JsonValue::string("ds"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: cases_json"));
    }

    #[tokio::test]
    async fn test_dataset_create_missing_created_by() {
        let tool = DatasetCreateTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        m.insert("name".to_string(), JsonValue::string("ds"));
        m.insert("cases_json".to_string(), JsonValue::string("[]"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: created_by"));
    }

    #[tokio::test]
    async fn test_dataset_list_missing_workspace_id() {
        let tool = DatasetListTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: workspace_id"));
    }
}
