// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Audit 本地逻辑工具（1 个）：audit_verify
//!
//! 透传族 audit_get / session_rewind 已迁入 adapter 表驱动（rule_tools::adapter）；
//! 本文件仅保留带本地语义变换的工具——audit_verify 把 server 的 verified 布尔
//! 包装为 {"verified": bool} 返回。

use std::sync::Arc;

use serde_json::Value;

use crate::api::evorule_client::EvoruleApiClient;
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};

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
    async fn call(&self, args: &Value) -> IoResult {
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
        Ok(v.clone())
    }
}

// =============================================================================
// register / specs
// =============================================================================

pub fn register(h: &mut ToolHandler, client: &EvoruleApiClient) {
    h.register_tool(
        "audit_verify",
        Arc::new(AuditVerifyTool::new(client.clone())),
    );
}

pub fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "audit_verify".to_string(),
        description: "Verify the audit chain for a session. Returns {\"verified\": bool}."
            .to_string(),
        parameters: vec![ParameterSpec {
            name: "session_id".to_string(),
            r#type: "string".to_string(),
            description: "Session id.".to_string(),
            required: true,
        }],
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:0")
    }

    #[test]
    fn test_specs_count() {
        assert_eq!(specs().len(), 1);
    }

    #[test]
    fn test_register_tools() {
        let client = make_client();
        let mut h = ToolHandler::new();
        register(&mut h, &client);
        assert!(h.has_tool("audit_verify"));
    }

    #[tokio::test]
    async fn test_audit_verify_missing_session_id() {
        let tool = AuditVerifyTool::new(make_client());
        let args = Value::Object(serde_json::Map::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: session_id"));
    }
}
