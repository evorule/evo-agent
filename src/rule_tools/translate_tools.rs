// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 规则转译 + 校验工具（3 个）：rule_to_transform / rule_to_conditional / rule_validate

use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::api::workspace_client::WorkspaceApiClient;
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use crate::json_convert::{serde_to_tcb, tcb_to_serde};

// =============================================================================
// rule_to_transform —— 转译为 transform 格式
// =============================================================================

#[derive(Clone)]
pub struct RuleToTransformTool {
    client: WorkspaceApiClient,
}

impl RuleToTransformTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleToTransformTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let body = args
            .get("body")
            .ok_or_else(|| "missing required parameter: body".to_string())?;
        let serde_body = tcb_to_serde(body);
        let result = self
            .client
            .translate_to_transform(serde_body)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// rule_to_conditional —— 转译为可读视图
// =============================================================================

#[derive(Clone)]
pub struct RuleToConditionalTool {
    client: WorkspaceApiClient,
}

impl RuleToConditionalTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleToConditionalTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let body = args
            .get("body")
            .ok_or_else(|| "missing required parameter: body".to_string())?;
        let serde_body = tcb_to_serde(body);
        let result = self
            .client
            .translate_to_conditional(serde_body)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// rule_validate —— 校验规则（G1-G7）
// =============================================================================

#[derive(Clone)]
pub struct RuleValidateTool {
    client: WorkspaceApiClient,
}

impl RuleValidateTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RuleValidateTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let rules = args
            .get("rules")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: rules".to_string())?;
        let body = serde_json::json!({ "rules": rules });
        let result = self
            .client
            .validate_rules(body)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// register / specs
// =============================================================================

pub fn register(h: &mut ToolHandler, client: &WorkspaceApiClient) {
    h.register_tool(
        "rule_to_transform",
        Arc::new(RuleToTransformTool::new(client.clone())),
    );
    h.register_tool(
        "rule_to_conditional",
        Arc::new(RuleToConditionalTool::new(client.clone())),
    );
    h.register_tool(
        "rule_validate",
        Arc::new(RuleValidateTool::new(client.clone())),
    );
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "rule_to_transform".to_string(),
            description: "Translate a rule (condition + action_set) into transform format."
                .to_string(),
            parameters: vec![ParameterSpec {
                name: "body".to_string(),
                r#type: "object".to_string(),
                description: "Rule body to translate (JSON object).".to_string(),
                required: true,
            }],
        },
        ToolSpec {
            name: "rule_to_conditional".to_string(),
            description:
                "Translate a transform-format rule into a readable conditional view (lossy)."
                    .to_string(),
            parameters: vec![ParameterSpec {
                name: "body".to_string(),
                r#type: "object".to_string(),
                description: "Rule body to translate (JSON object).".to_string(),
                required: true,
            }],
        },
        ToolSpec {
            name: "rule_validate".to_string(),
            description: "Validate rules against G1-G7 constraints.".to_string(),
            parameters: vec![ParameterSpec {
                name: "rules".to_string(),
                r#type: "string".to_string(),
                description: "Rules to validate (JSON string).".to_string(),
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
        assert_eq!(specs().len(), 3);
    }

    #[test]
    fn test_register_tools() {
        let client = make_client();
        let mut h = ToolHandler::new();
        register(&mut h, &client);
        assert!(h.has_tool("rule_to_transform"));
        assert!(h.has_tool("rule_to_conditional"));
        assert!(h.has_tool("rule_validate"));
    }

    #[tokio::test]
    async fn test_rule_to_transform_missing_body() {
        let tool = RuleToTransformTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: body"));
    }

    #[tokio::test]
    async fn test_rule_to_conditional_missing_body() {
        let tool = RuleToConditionalTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: body"));
    }

    #[tokio::test]
    async fn test_rule_validate_missing_rules() {
        let tool = RuleValidateTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: rules"));
    }
}
