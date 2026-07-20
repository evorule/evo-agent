// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Tool I/O Handler -- invokes registered tool functions
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tier0_tcb::JsonValue;
use tracing::debug;

use crate::io_handler::{IoHandler, IoResult};

/// TODO: doc
pub trait ToolFunction: Send + Sync {
    /// TODO: doc
    fn call(&self, args: &JsonValue) -> IoResult;
}

impl<F> ToolFunction for F
where
    F: Fn(&JsonValue) -> IoResult + Send + Sync,
{
    fn call(&self, args: &JsonValue) -> IoResult {
        self(args)
    }
}

const TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// Tool I/O Handler -- invokes registered tool functions
#[derive(Clone)]
pub struct ToolHandler {
    tools: Arc<BTreeMap<String, Arc<dyn ToolFunction>>>,
}

impl std::fmt::Debug for ToolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolHandler")
            .field("tool_count", &self.tools.len())
            .finish()
    }
}

impl ToolHandler {
    /// Create new tool handler
    pub fn new() -> Self {
        Self {
            tools: Arc::new(BTreeMap::new()),
        }
    }

    /// Create handler with existing tools
    pub fn with_tools(tools: BTreeMap<String, Arc<dyn ToolFunction>>) -> Self {
        Self {
            tools: Arc::new(tools),
        }
    }

    /// Register tool function
    pub fn register_tool(&mut self, name: &str, func: Arc<dyn ToolFunction>) {
        Arc::make_mut(&mut self.tools).insert(name.to_string(), func);
    }

    /// Check whether tool is registered
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
}

#[async_trait::async_trait]
impl IoHandler for ToolHandler {
    /// Execute tool invocation
    async fn execute(&self, params: &JsonValue) -> IoResult {
        let tool_name = params
            .get("tool_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required param: tool_name".to_string())?;

        let args = params.get("args").cloned().unwrap_or(JsonValue::Null);

        let func = self
            .tools
            .get(tool_name)
            .ok_or_else(|| format!("tool not found: {tool_name}"))?;

        debug!(tool_name = tool_name, "ready to invoke tool");

        tokio::time::timeout(TOOL_TIMEOUT, async move { func.call(&args) })
            .await
            .map_err(|_| {
                format!(
                    "tool '{tool_name}' timed out after {}s",
                    TOOL_TIMEOUT.as_secs()
                )
            })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    impl ToolFunction for EchoTool {
        fn call(&self, _args: &JsonValue) -> IoResult {
            Ok(JsonValue::string("result"))
        }
    }

    #[test]
    fn test_tool_handler_new() {
        let handler = ToolHandler::new();
        assert!(!handler.has_tool("test"));
    }

    #[test]
    fn test_tool_handler_register_and_has() {
        let mut handler = ToolHandler::new();
        handler.register_tool("test", Arc::new(EchoTool));
        assert!(handler.has_tool("test"));
        assert!(!handler.has_tool("other"));
    }
}
