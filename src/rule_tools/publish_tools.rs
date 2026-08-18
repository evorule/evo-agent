// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 发布队列工具（5 个）：publish_submit / publish_list / publish_queue_get / publish_review / publish_rollback

use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::api::workspace_client::{
    ReviewPublishRequest, RollbackRequest, SubmitPublishRequest, WorkspaceApiClient,
};
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use crate::json_convert::serde_to_tcb;

// =============================================================================
// publish_submit —— 提交到发布队列（DepartmentHead 权限）
// =============================================================================

#[derive(Clone)]
pub struct PublishSubmitTool {
    client: WorkspaceApiClient,
}

impl PublishSubmitTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for PublishSubmitTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let workspace_id = args
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: workspace_id".to_string())?;
        // rule_version_ids 为数组
        let rule_version_ids = args
            .get("rule_version_ids")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "missing required parameter: rule_version_ids".to_string())?
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect::<Vec<String>>();
        let submitted_by = args
            .get("submitted_by")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: submitted_by".to_string())?;
        let role = args
            .get("role")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: role".to_string())?;
        let test_report_sandbox_id = args.get("test_report_sandbox_id").and_then(|v| v.as_i64());
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let req = SubmitPublishRequest {
            workspace_id: workspace_id.to_string(),
            rule_version_ids,
            test_report_sandbox_id,
            description,
        };
        let result = self
            .client
            .submit_publish(req, submitted_by, role)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// publish_list —— 列出发布队列
// =============================================================================

#[derive(Clone)]
pub struct PublishListTool {
    client: WorkspaceApiClient,
}

impl PublishListTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for PublishListTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let status = args.get("status").and_then(|v| v.as_str());
        let result = self
            .client
            .list_publish_queue(status)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// publish_queue_get —— 队列项详情
// =============================================================================

#[derive(Clone)]
pub struct PublishQueueGetTool {
    client: WorkspaceApiClient,
}

impl PublishQueueGetTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for PublishQueueGetTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let queue_id = args
            .get("queue_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "missing required parameter: queue_id".to_string())?;
        let result = self
            .client
            .get_publish_queue_item(queue_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// publish_review —— 审批发布（Admin 权限）
// =============================================================================

#[derive(Clone)]
pub struct PublishReviewTool {
    client: WorkspaceApiClient,
}

impl PublishReviewTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for PublishReviewTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let queue_id = args
            .get("queue_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "missing required parameter: queue_id".to_string())?;
        let decision = args
            .get("decision")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: decision".to_string())?;
        let reviewed_by = args
            .get("reviewed_by")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: reviewed_by".to_string())?;
        let role = args
            .get("role")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: role".to_string())?;
        let comment = args
            .get("comment")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let req = ReviewPublishRequest {
            decision: decision.to_string(),
            comment,
        };
        let result = self
            .client
            .review_publish(queue_id, req, reviewed_by, role)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&result).unwrap_or_default();
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// publish_rollback —— 紧急回滚（Admin 权限）
// =============================================================================

#[derive(Clone)]
pub struct PublishRollbackTool {
    client: WorkspaceApiClient,
}

impl PublishRollbackTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for PublishRollbackTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let target_version = args
            .get("target_version")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "missing required parameter: target_version".to_string())?;
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: reason".to_string())?;
        let operated_by = args
            .get("operated_by")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: operated_by".to_string())?;
        let role = args
            .get("role")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: role".to_string())?;
        let req = RollbackRequest {
            target_version,
            reason: reason.to_string(),
        };
        let result = self
            .client
            .emergency_rollback(req, operated_by, role)
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
        "publish_submit",
        Arc::new(PublishSubmitTool::new(client.clone())),
    );
    h.register_tool(
        "publish_list",
        Arc::new(PublishListTool::new(client.clone())),
    );
    h.register_tool(
        "publish_queue_get",
        Arc::new(PublishQueueGetTool::new(client.clone())),
    );
    h.register_tool(
        "publish_review",
        Arc::new(PublishReviewTool::new(client.clone())),
    );
    h.register_tool(
        "publish_rollback",
        Arc::new(PublishRollbackTool::new(client.clone())),
    );
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "publish_submit".to_string(),
            description: "Submit rules to the publish queue (requires DepartmentHead role)."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Source workspace id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_version_ids".to_string(),
                    r#type: "array".to_string(),
                    description: "Candidate rule version ids to publish (array of strings)."
                        .to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "submitted_by".to_string(),
                    r#type: "string".to_string(),
                    description: "Submitter user id (department head).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "role".to_string(),
                    r#type: "string".to_string(),
                    description: "Publish role (doctor / department_head / admin).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "test_report_sandbox_id".to_string(),
                    r#type: "integer".to_string(),
                    description: "Optional attached test report sandbox id.".to_string(),
                    required: false,
                },
                ParameterSpec {
                    name: "description".to_string(),
                    r#type: "string".to_string(),
                    description: "Optional release description.".to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "publish_list".to_string(),
            description: "List the publish queue, optionally filtered by status.".to_string(),
            parameters: vec![ParameterSpec {
                name: "status".to_string(),
                r#type: "string".to_string(),
                description:
                    "Optional status filter (pending/approved/published/rejected/cancelled)."
                        .to_string(),
                required: false,
            }],
        },
        ToolSpec {
            name: "publish_queue_get".to_string(),
            description: "Get a publish queue item's details.".to_string(),
            parameters: vec![ParameterSpec {
                name: "queue_id".to_string(),
                r#type: "integer".to_string(),
                description: "Queue item id.".to_string(),
                required: true,
            }],
        },
        ToolSpec {
            name: "publish_review".to_string(),
            description: "Review (approve/reject) a publish queue item (requires Admin role)."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "queue_id".to_string(),
                    r#type: "integer".to_string(),
                    description: "Queue item id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "decision".to_string(),
                    r#type: "string".to_string(),
                    description: "Review decision (approved / rejected).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "reviewed_by".to_string(),
                    r#type: "string".to_string(),
                    description: "Reviewer user id (admin).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "role".to_string(),
                    r#type: "string".to_string(),
                    description: "Publish role (must be admin).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "comment".to_string(),
                    r#type: "string".to_string(),
                    description: "Optional review comment.".to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "publish_rollback".to_string(),
            description: "Emergency rollback to a target ruleset version (requires Admin role)."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "target_version".to_string(),
                    r#type: "integer".to_string(),
                    description: "Target ruleset version to roll back to.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "reason".to_string(),
                    r#type: "string".to_string(),
                    description: "Rollback reason.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "operated_by".to_string(),
                    r#type: "string".to_string(),
                    description: "Operator user id (admin).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "role".to_string(),
                    r#type: "string".to_string(),
                    description: "Publish role (must be admin).".to_string(),
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
            "publish_submit",
            "publish_list",
            "publish_queue_get",
            "publish_review",
            "publish_rollback",
        ] {
            assert!(h.has_tool(name), "tool {} should be registered", name);
        }
    }

    #[tokio::test]
    async fn test_publish_submit_missing_rule_version_ids() {
        let tool = PublishSubmitTool::new(make_client());
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
    async fn test_publish_submit_missing_role() {
        let tool = PublishSubmitTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("workspace_id".to_string(), JsonValue::string("ws1"));
        m.insert("rule_version_ids".to_string(), JsonValue::array(vec![]));
        m.insert("submitted_by".to_string(), JsonValue::string("u1"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: role"));
    }

    #[tokio::test]
    async fn test_publish_queue_get_missing_queue_id() {
        let tool = PublishQueueGetTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: queue_id"));
    }

    #[tokio::test]
    async fn test_publish_review_missing_decision() {
        let tool = PublishReviewTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("queue_id".to_string(), JsonValue::Integer(1));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: decision"));
    }

    #[tokio::test]
    async fn test_publish_rollback_missing_reason() {
        let tool = PublishRollbackTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("target_version".to_string(), JsonValue::Integer(2));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: reason"));
    }

    #[tokio::test]
    async fn test_publish_rollback_missing_target_version() {
        let tool = PublishRollbackTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: target_version"));
    }

    #[tokio::test]
    async fn test_publish_list_no_args_ok_schema() {
        // publish_list 无必填参数；不传 args 会尝试连 localhost:0 失败（连接错误），不是 missing-param 错误。
        let tool = PublishListTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        // 应为 Err（网络错误），但不是 missing-parameter 错误
        assert!(result.is_err());
        assert!(!result.unwrap_err().contains("missing required parameter"));
    }
}
