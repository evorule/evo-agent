// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 沙盒编排工具（5 个）：sandbox_start / sandbox_list / sandbox_get / sandbox_close / sandbox_report

use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::api::workspace_client::{StartSandboxRequest, WorkspaceApiClient};
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use crate::json_convert::serde_to_tcb;

// =============================================================================
// sandbox_start —— 启动沙盒测试
// =============================================================================

#[derive(Clone)]
pub struct SandboxStartTool {
    client: WorkspaceApiClient,
}

impl SandboxStartTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for SandboxStartTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        // rule_version_ids 为数组：用 as_array 解析
        let rule_version_ids = args
            .get("rule_version_ids")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "missing required parameter: rule_version_ids".to_string())?
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect::<Vec<String>>();
        let test_dataset_id = args
            .get("test_dataset_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "missing required parameter: test_dataset_id".to_string())?;
        let started_by = args
            .get("started_by")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: started_by".to_string())?;
        let parent_version = args
            .get("parent_version")
            .and_then(|v| v.as_i64())
            .map(|i| i as u64);
        let req = StartSandboxRequest {
            rule_version_ids,
            test_dataset_id,
            parent_version,
        };
        let result = self
            .client
            .start_sandbox(workspace_id, req, started_by)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// sandbox_list —— 列出沙盒历史
// =============================================================================

#[derive(Clone)]
pub struct SandboxListTool {
    client: WorkspaceApiClient,
}

impl SandboxListTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for SandboxListTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let requester = args
            .get("requester")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: requester".to_string())?;
        let result = self
            .client
            .list_sandboxes(workspace_id, requester)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// sandbox_get —— 沙盒详情
// =============================================================================

#[derive(Clone)]
pub struct SandboxGetTool {
    client: WorkspaceApiClient,
}

impl SandboxGetTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for SandboxGetTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let sandbox_id = args
            .get("sandbox_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "missing required parameter: sandbox_id".to_string())?;
        let requester = args
            .get("requester")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: requester".to_string())?;
        let result = self
            .client
            .get_sandbox(workspace_id, sandbox_id, requester)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// sandbox_close —— 关闭沙盒
// =============================================================================

#[derive(Clone)]
pub struct SandboxCloseTool {
    client: WorkspaceApiClient,
}

impl SandboxCloseTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for SandboxCloseTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let sandbox_id = args
            .get("sandbox_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "missing required parameter: sandbox_id".to_string())?;
        let closed_by = args
            .get("closed_by")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: closed_by".to_string())?;
        let result = self
            .client
            .close_sandbox(workspace_id, sandbox_id, closed_by)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// sandbox_report —— 测试报告
// =============================================================================

#[derive(Clone)]
pub struct SandboxReportTool {
    client: WorkspaceApiClient,
}

impl SandboxReportTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for SandboxReportTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let sandbox_id = args
            .get("sandbox_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "missing required parameter: sandbox_id".to_string())?;
        let result = self
            .client
            .get_sandbox_report(workspace_id, sandbox_id)
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
        "sandbox_start",
        Arc::new(SandboxStartTool::new(client.clone())),
    );
    h.register_tool(
        "sandbox_list",
        Arc::new(SandboxListTool::new(client.clone())),
    );
    h.register_tool("sandbox_get", Arc::new(SandboxGetTool::new(client.clone())));
    h.register_tool(
        "sandbox_close",
        Arc::new(SandboxCloseTool::new(client.clone())),
    );
    h.register_tool(
        "sandbox_report",
        Arc::new(SandboxReportTool::new(client.clone())),
    );
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "sandbox_start".to_string(),
            description: "Start a sandbox test session (fork production session + load draft rules + inject test cases)."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_version_ids".to_string(),
                    r#type: "array".to_string(),
                    description: "Rule version ids to test (array of strings).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "test_dataset_id".to_string(),
                    r#type: "integer".to_string(),
                    description: "Synthetic test dataset id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "started_by".to_string(),
                    r#type: "string".to_string(),
                    description: "User id starting the sandbox (must be a workspace member).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "parent_version".to_string(),
                    r#type: "integer".to_string(),
                    description: "Optional production session version to fork from (default: latest).".to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "sandbox_list".to_string(),
            description: "List sandbox test history for a workspace.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "requester".to_string(),
                    r#type: "string".to_string(),
                    description: "Requester user id (for workspace member check).".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "sandbox_get".to_string(),
            description: "Get a sandbox session's details.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "sandbox_id".to_string(),
                    r#type: "integer".to_string(),
                    description: "Sandbox id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "requester".to_string(),
                    r#type: "string".to_string(),
                    description: "Requester user id (for workspace member check).".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "sandbox_close".to_string(),
            description: "Close a sandbox session (export test facts + close session).".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "sandbox_id".to_string(),
                    r#type: "integer".to_string(),
                    description: "Sandbox id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "closed_by".to_string(),
                    r#type: "string".to_string(),
                    description: "User id closing the sandbox (must be a workspace member).".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "sandbox_report".to_string(),
            description: "Get the test report (BLAKE3-signed) for a closed/running sandbox.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "sandbox_id".to_string(),
                    r#type: "integer".to_string(),
                    description: "Sandbox id.".to_string(),
                    required: true,
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
        assert_eq!(specs().len(), 5);
    }

    #[test]
    fn test_register_tools() {
        let client = make_client();
        let mut h = ToolHandler::new();
        register(&mut h, &client);
        for name in [
            "sandbox_start",
            "sandbox_list",
            "sandbox_get",
            "sandbox_close",
            "sandbox_report",
        ] {
            assert!(h.has_tool(name), "tool {} should be registered", name);
        }
    }

    #[tokio::test]
    async fn test_sandbox_start_missing_workspace_id() {
        let tool = SandboxStartTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: workspace_id"));
    }

    #[tokio::test]
    async fn test_sandbox_start_missing_rule_version_ids() {
        let tool = SandboxStartTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: rule_version_ids"));
    }

    #[tokio::test]
    async fn test_sandbox_start_missing_test_dataset_id() {
        let tool = SandboxStartTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        m.insert("rule_version_ids".to_string(), JsonValue::array(vec![]));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: test_dataset_id"));
    }

    #[tokio::test]
    async fn test_sandbox_get_missing_sandbox_id() {
        let tool = SandboxGetTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: sandbox_id"));
    }

    #[tokio::test]
    async fn test_sandbox_close_missing_closed_by() {
        let tool = SandboxCloseTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        m.insert("sandbox_id".to_string(), JsonValue::Integer(1));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: closed_by"));
    }

    #[tokio::test]
    async fn test_sandbox_report_missing_sandbox_id() {
        let tool = SandboxReportTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: sandbox_id"));
    }

    #[tokio::test]
    async fn test_sandbox_list_missing_requester() {
        let tool = SandboxListTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: requester"));
    }
}
