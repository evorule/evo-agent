// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 规则管理工具（12 个，1:1 映射 A1 端点）

use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::api::workspace_client::{
    CreateRuleRequest, ForkRuleRequest, UpdateRuleContentRequest, WorkspaceApiClient,
};
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use crate::json_convert::serde_to_tcb;

// =============================================================================
// rule_list —— 列出规则
// =============================================================================

#[derive(Clone)]
pub struct RuleListTool {
    client: WorkspaceApiClient,
}

impl RuleListTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleListTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let result = self
            .client
            .list_rules(workspace_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// rule_get —— 获取规则详情
// =============================================================================

#[derive(Clone)]
pub struct RuleGetTool {
    client: WorkspaceApiClient,
}

impl RuleGetTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleGetTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let rule_id = args
            .get("rule_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: rule_id".to_string())?;
        let result = self
            .client
            .get_rule(workspace_id, rule_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// rule_create —— 创建规则
// =============================================================================

#[derive(Clone)]
pub struct RuleCreateTool {
    client: WorkspaceApiClient,
}

impl RuleCreateTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleCreateTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: name".to_string())?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: content".to_string())?;
        let created_by = args
            .get("created_by")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: created_by".to_string())?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let req = CreateRuleRequest {
            name: name.to_string(),
            content: content.to_string(),
            created_by: created_by.to_string(),
            description,
        };
        let result = self
            .client
            .create_rule(workspace_id, req)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// rule_update —— 更新规则内容
// =============================================================================

#[derive(Clone)]
pub struct RuleUpdateTool {
    client: WorkspaceApiClient,
}

impl RuleUpdateTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleUpdateTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let rule_id = args
            .get("rule_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: rule_id".to_string())?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: content".to_string())?;
        let updated_by = args
            .get("updated_by")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: updated_by".to_string())?;
        let req = UpdateRuleContentRequest {
            content: content.to_string(),
            updated_by: updated_by.to_string(),
        };
        let result = self
            .client
            .update_rule_content(workspace_id, rule_id, req)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// rule_versions —— 列出版本
// =============================================================================

#[derive(Clone)]
pub struct RuleVersionsTool {
    client: WorkspaceApiClient,
}

impl RuleVersionsTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleVersionsTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let rule_id = args
            .get("rule_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: rule_id".to_string())?;
        let result = self
            .client
            .list_rule_versions(workspace_id, rule_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// rule_version_get —— 获取版本
// =============================================================================

#[derive(Clone)]
pub struct RuleVersionGetTool {
    client: WorkspaceApiClient,
}

impl RuleVersionGetTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleVersionGetTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let rule_id = args
            .get("rule_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: rule_id".to_string())?;
        let version_id = args
            .get("version_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: version_id".to_string())?;
        let result = self
            .client
            .get_rule_version(workspace_id, rule_id, version_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// rule_submit —— 提交候选（Draft → Candidate）
// =============================================================================

#[derive(Clone)]
pub struct RuleSubmitTool {
    client: WorkspaceApiClient,
}

impl RuleSubmitTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleSubmitTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let rule_id = args
            .get("rule_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: rule_id".to_string())?;
        let result = self
            .client
            .submit_rule(workspace_id, rule_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// rule_activate —— 激活规则
// =============================================================================

#[derive(Clone)]
pub struct RuleActivateTool {
    client: WorkspaceApiClient,
}

impl RuleActivateTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleActivateTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let rule_id = args
            .get("rule_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: rule_id".to_string())?;
        let result = self
            .client
            .activate_rule(workspace_id, rule_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// rule_block —— 阻塞规则
// =============================================================================

#[derive(Clone)]
pub struct RuleBlockTool {
    client: WorkspaceApiClient,
}

impl RuleBlockTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleBlockTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let rule_id = args
            .get("rule_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: rule_id".to_string())?;
        let result = self
            .client
            .block_rule(workspace_id, rule_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// rule_archive —— 归档规则
// =============================================================================

#[derive(Clone)]
pub struct RuleArchiveTool {
    client: WorkspaceApiClient,
}

impl RuleArchiveTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleArchiveTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let rule_id = args
            .get("rule_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: rule_id".to_string())?;
        let result = self
            .client
            .archive_rule(workspace_id, rule_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// rule_fork —— fork 规则
// =============================================================================

#[derive(Clone)]
pub struct RuleForkTool {
    client: WorkspaceApiClient,
}

impl RuleForkTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleForkTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        let rule_id = args
            .get("rule_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: rule_id".to_string())?;
        let new_name = args
            .get("new_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: new_name".to_string())?;
        let created_by = args
            .get("created_by")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: created_by".to_string())?;
        let req = ForkRuleRequest {
            new_name: new_name.to_string(),
            created_by: created_by.to_string(),
        };
        let result = self
            .client
            .fork_rule(workspace_id, rule_id, req)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// rule_reload —— 热重载规则（无参数）
// =============================================================================

#[derive(Clone)]
pub struct RuleReloadTool {
    client: WorkspaceApiClient,
}

impl RuleReloadTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleReloadTool {
    async fn call(&self, _args: &JsonValue) -> IoResult {
        let result = self
            .client
            .reload_rules()
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// register / specs
// =============================================================================

pub fn register(h: &mut ToolHandler, client: &WorkspaceApiClient) {
    h.register_tool("rule_list", Arc::new(RuleListTool::new(client.clone())));
    h.register_tool("rule_get", Arc::new(RuleGetTool::new(client.clone())));
    h.register_tool("rule_create", Arc::new(RuleCreateTool::new(client.clone())));
    h.register_tool("rule_update", Arc::new(RuleUpdateTool::new(client.clone())));
    h.register_tool(
        "rule_versions",
        Arc::new(RuleVersionsTool::new(client.clone())),
    );
    h.register_tool(
        "rule_version_get",
        Arc::new(RuleVersionGetTool::new(client.clone())),
    );
    h.register_tool("rule_submit", Arc::new(RuleSubmitTool::new(client.clone())));
    h.register_tool(
        "rule_activate",
        Arc::new(RuleActivateTool::new(client.clone())),
    );
    h.register_tool("rule_block", Arc::new(RuleBlockTool::new(client.clone())));
    h.register_tool(
        "rule_archive",
        Arc::new(RuleArchiveTool::new(client.clone())),
    );
    h.register_tool("rule_fork", Arc::new(RuleForkTool::new(client.clone())));
    h.register_tool("rule_reload", Arc::new(RuleReloadTool::new(client.clone())));
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "rule_list".to_string(),
            description: "List all rules in a workspace.".to_string(),
            parameters: vec![ParameterSpec {
                name: "workspace_id".to_string(),
                r#type: "string".to_string(),
                description: "Workspace id.".to_string(),
                required: true,
            }],
        },
        ToolSpec {
            name: "rule_get".to_string(),
            description: "Get a rule's details.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule id.".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "rule_create".to_string(),
            description: "Create a new rule in a workspace.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "name".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule name.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "content".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule content (JSON string).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "created_by".to_string(),
                    r#type: "string".to_string(),
                    description: "Creator id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "description".to_string(),
                    r#type: "string".to_string(),
                    description: "Optional rule description.".to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "rule_update".to_string(),
            description: "Update rule content (only allowed in Draft state).".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "content".to_string(),
                    r#type: "string".to_string(),
                    description: "New rule content (JSON string).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "updated_by".to_string(),
                    r#type: "string".to_string(),
                    description: "Updater id.".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "rule_versions".to_string(),
            description: "List all versions of a rule (descending by version).".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule id.".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "rule_version_get".to_string(),
            description: "Get a specific rule version.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "version_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Version id.".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "rule_submit".to_string(),
            description: "Submit a rule (Draft -> Candidate).".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule id.".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "rule_activate".to_string(),
            description: "Activate a rule.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule id.".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "rule_block".to_string(),
            description: "Block a rule.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule id.".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "rule_archive".to_string(),
            description: "Archive a rule.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule id.".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "rule_fork".to_string(),
            description: "Fork a rule into a new rule.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule id to fork from.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "new_name".to_string(),
                    r#type: "string".to_string(),
                    description: "Name for the forked rule.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "created_by".to_string(),
                    r#type: "string".to_string(),
                    description: "Creator id.".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "rule_reload".to_string(),
            description: "Hot-reload all rules from disk (TCB constitution + business rules)."
                .to_string(),
            parameters: vec![],
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
        assert_eq!(specs().len(), 12);
    }

    #[test]
    fn test_register_tools() {
        let client = make_client();
        let mut h = ToolHandler::new();
        register(&mut h, &client);
        for name in [
            "rule_list",
            "rule_get",
            "rule_create",
            "rule_update",
            "rule_versions",
            "rule_version_get",
            "rule_submit",
            "rule_activate",
            "rule_block",
            "rule_archive",
            "rule_fork",
            "rule_reload",
        ] {
            assert!(h.has_tool(name), "tool {} should be registered", name);
        }
    }

    #[tokio::test]
    async fn test_rule_list_missing_workspace_id() {
        let tool = RuleListTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: workspace_id"));
    }

    #[tokio::test]
    async fn test_rule_get_missing_rule_id() {
        let tool = RuleGetTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: rule_id"));
    }

    #[tokio::test]
    async fn test_rule_create_missing_content() {
        let tool = RuleCreateTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        m.insert("name".to_string(), JsonValue::string("rule1"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: content"));
    }

    #[tokio::test]
    async fn test_rule_update_missing_updated_by() {
        let tool = RuleUpdateTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        m.insert("rule_id".to_string(), JsonValue::string("r1"));
        m.insert("content".to_string(), JsonValue::string("{}"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: updated_by"));
    }

    #[tokio::test]
    async fn test_rule_fork_missing_new_name() {
        let tool = RuleForkTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        m.insert("rule_id".to_string(), JsonValue::string("r1"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: new_name"));
    }
}
