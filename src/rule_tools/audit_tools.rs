// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Audit / Session 工具（3 个）：audit_get / audit_verify / session_rewind

use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::api::evorule_client::EvoruleApiClient;
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use crate::json_convert::serde_to_tcb;

// =============================================================================
// audit_get —— 获取审计报告
// =============================================================================

#[derive(Clone)]
pub struct AuditGetTool {
    client: EvoruleApiClient,
}

impl AuditGetTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for AuditGetTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: session_id".to_string())?;
        let result = self
            .client
            .get_audit_report(session_id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// audit_verify —— 验证审计 → {"verified": bool}
// =============================================================================

#[derive(Clone)]
pub struct AuditVerifyTool {
    client: EvoruleApiClient,
}

impl AuditVerifyTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for AuditVerifyTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: session_id".to_string())?;
        let verified = self
            .client
            .verify_audit(session_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::json!({ "verified": verified });
        Ok(serde_to_tcb(&v))
    }
}

// =============================================================================
// session_rewind —— 回退到指定版本
// =============================================================================

#[derive(Clone)]
pub struct SessionRewindTool {
    client: EvoruleApiClient,
}

impl SessionRewindTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for SessionRewindTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: session_id".to_string())?;
        let version = args
            .get("version")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "missing required parameter: version".to_string())?;
        if version < 0 {
            return Err("version must be non-negative".to_string());
        }
        let result = self
            .client
            .rewind(session_id, version as u64)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// register / specs
// =============================================================================

pub fn register(h: &mut ToolHandler, client: &EvoruleApiClient) {
    h.register_tool("audit_get", Arc::new(AuditGetTool::new(client.clone())));
    h.register_tool(
        "audit_verify",
        Arc::new(AuditVerifyTool::new(client.clone())),
    );
    h.register_tool(
        "session_rewind",
        Arc::new(SessionRewindTool::new(client.clone())),
    );
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "audit_get".to_string(),
            description: "Get the audit report for a session.".to_string(),
            parameters: vec![ParameterSpec {
                name: "session_id".to_string(),
                r#type: "string".to_string(),
                description: "Session id.".to_string(),
                required: true,
            }],
        },
        ToolSpec {
            name: "audit_verify".to_string(),
            description: "Verify the audit chain for a session. Returns {\"verified\": bool}."
                .to_string(),
            parameters: vec![ParameterSpec {
                name: "session_id".to_string(),
                r#type: "string".to_string(),
                description: "Session id.".to_string(),
                required: true,
            }],
        },
        ToolSpec {
            name: "session_rewind".to_string(),
            description: "Rewind a session to a specific version.".to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "session_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Session id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "version".to_string(),
                    r#type: "integer".to_string(),
                    description: "Target version to rewind to (non-negative integer).".to_string(),
                    required: true,
                },
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:0")
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
        assert!(h.has_tool("audit_get"));
        assert!(h.has_tool("audit_verify"));
        assert!(h.has_tool("session_rewind"));
    }

    #[tokio::test]
    async fn test_audit_get_missing_session_id() {
        let tool = AuditGetTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: session_id"));
    }

    #[tokio::test]
    async fn test_audit_verify_missing_session_id() {
        let tool = AuditVerifyTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: session_id"));
    }

    #[tokio::test]
    async fn test_session_rewind_missing_session_id() {
        let tool = SessionRewindTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: session_id"));
    }

    #[tokio::test]
    async fn test_session_rewind_missing_version() {
        let tool = SessionRewindTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("session_id".to_string(), JsonValue::string("s1"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: version"));
    }

    #[tokio::test]
    async fn test_session_rewind_negative_version() {
        let tool = SessionRewindTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("session_id".to_string(), JsonValue::string("s1"));
        m.insert("version".to_string(), JsonValue::Integer(-1));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("version must be non-negative"));
    }
}
