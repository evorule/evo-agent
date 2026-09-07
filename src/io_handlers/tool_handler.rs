// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Tool I/O Handler -- invokes registered tool functions
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use evorule_tcb::JsonValue;
use tracing::debug;

use crate::io_handler::{IoHandler, IoResult};

/// G13:工具函数 trait(异步)
///
/// `call` 是 `async fn`,支持网络请求/IO 操作/子进程等异步操作。
/// 同步工具(如 `std::fs`)在实现中用 `tokio::task::spawn_blocking` 包装。
///
/// **破坏性变更**(G13):从 `fn call(&self, args) -> IoResult` 改为 `async fn call`。
/// 旧代码需给 `impl ToolFunction` 加 `#[async_trait::async_trait]` 并把 `fn call` 改 `async fn call`。
#[async_trait::async_trait]
pub trait ToolFunction: Send + Sync {
    /// 执行工具(异步)
    ///
    /// # 参数
    /// - `args`:工具参数(JSON)
    ///
    /// # 返回
    /// - `Ok(JsonValue)`:工具执行结果
    /// - `Err(String)`:错误描述
    async fn call(&self, args: &JsonValue) -> IoResult;
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

    /// 已注册工具名列表(按字母序)
    ///
    /// 供 runner 组装 LLM 请求的 tools schema 时枚举执行器
    /// (schema 数据源 = 静态 spec 目录 ∩ 本列表)。
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// 按名取出工具实现(供 serve 按白名单过滤复用)
    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn ToolFunction>> {
        self.tools.get(name).cloned()
    }

    /// G13:直接按名称执行工具(不经 IoHandler::execute 的 params 解包)
    ///
    /// 供 runner 的并行执行路径(`execute_single_tool`)直接调用,
    /// 跳过 `params.get("tool_name")` 解包步骤。
    /// 包含 60s 超时(同 `IoHandler::execute`)。
    pub async fn execute_by_name(&self, tool_name: &str, args: &JsonValue) -> IoResult {
        let func = self
            .tools
            .get(tool_name)
            .ok_or_else(|| format!("tool not found: {tool_name}"))?;

        debug!(tool_name = tool_name, "ready to invoke tool (async)");

        tokio::time::timeout(TOOL_TIMEOUT, func.call(args))
            .await
            .map_err(|_| {
                format!(
                    "tool '{tool_name}' timed out after {}s",
                    TOOL_TIMEOUT.as_secs()
                )
            })?
    }
}

impl Default for ToolHandler {
    fn default() -> Self {
        Self::new()
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

        self.execute_by_name(tool_name, &args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait::async_trait]
    impl ToolFunction for EchoTool {
        async fn call(&self, _args: &JsonValue) -> IoResult {
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

    #[tokio::test]
    async fn test_tool_handler_execute_by_name() {
        let mut handler = ToolHandler::new();
        handler.register_tool("echo", Arc::new(EchoTool));
        let result = handler.execute_by_name("echo", &JsonValue::Null).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().to_string(), "\"result\"");
    }

    #[tokio::test]
    async fn test_tool_handler_execute_not_found() {
        let handler = ToolHandler::new();
        let result = handler
            .execute_by_name("nonexistent", &JsonValue::Null)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("tool not found"));
    }

    #[tokio::test]
    async fn test_tool_handler_execute_via_io_handler() {
        // 通过 IoHandler::execute 路径(params 含 tool_name + args)
        let mut handler = ToolHandler::new();
        handler.register_tool("echo", Arc::new(EchoTool));
        let params = JsonValue::object(
            std::iter::once(("tool_name".to_string(), JsonValue::string("echo"))).collect(),
        );
        let result = handler.execute(&params).await;
        assert!(result.is_ok());
    }
}
