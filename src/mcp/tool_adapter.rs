// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G12:MCP 工具适配器 —— 把 MCP server 工具包装成 evo-agent 的 [`ToolFunction`]
//!
//! ## 作用
//!
//! 一个 MCP server 可能暴露多个工具(如 `read_file` / `write_file`)。
//! 每个工具被包装成一个 [`McpToolAdapter`],注册到 `ToolHandler` 中,
//! 工具名加 `mcp_{server}_{tool}` 前缀(见 [`mcp_tool_name`])避免与内置工具冲突。
//!
//! ## 数据流
//!
//! ```text
//! LLM tool_call("mcp_filesystem_read_file", {path: "/tmp/x"})
//!   → ToolHandler::execute_by_name
//!   → McpToolAdapter::call (ToolFunction, async)
//!   → McpClient::call_tool("read_file", {path: "/tmp/x"})
//!   → McpTransport::request("tools/call", ...)
//!   → MCP server 子进程
//!   → 返回 text content
//!   → 包装成 JsonValue::string 返回给 runner
//! ```

use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;
use crate::mcp::client::McpClient;

/// 构造注册到 ToolHandler 的工具名:`mcp_{server}_{tool}`
///
/// `server` 是配置中的 `mcp.servers[].name`,`tool` 是 MCP server 暴露的工具名。
/// 前缀 `mcp_` 避免与内置工具(file_read / http_get 等)冲突。
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp_{}_{}", server, tool)
}

/// 把单个 MCP 工具适配为 evo-agent 的 [`ToolFunction`]
///
/// 多个 adapter 共享同一个 `Arc<McpClient>`(同一 server 的所有工具共用一条传输层)。
#[derive(Clone)]
pub struct McpToolAdapter {
    client: Arc<McpClient>,
    /// MCP server 内的工具名(不含 `mcp_{server}_` 前缀)
    tool_name: String,
}

impl std::fmt::Debug for McpToolAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolAdapter")
            .field("tool_name", &self.tool_name)
            .finish()
    }
}

impl McpToolAdapter {
    /// 创建一个 MCP 工具适配器
    ///
    /// `client` 是已连接的 MCP client(多个 adapter 共享)。
    /// `tool_name` 是 MCP server 暴露的工具名(不含前缀)。
    pub fn new(client: Arc<McpClient>, tool_name: String) -> Self {
        Self { client, tool_name }
    }

    /// 只读访问工具名(MCP server 内的原始名)
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }
}

#[async_trait::async_trait]
impl ToolFunction for McpToolAdapter {
    async fn call(&self, args: &JsonValue) -> IoResult {
        // JsonValue → serde_json::Value(MCP 协议用 serde_json)
        let args_json = crate::json_convert::tcb_to_serde(args);

        tracing::debug!(
            tool = %self.tool_name,
            args = %args_json,
            "MCP tool adapter invoked"
        );

        let result = self
            .client
            .call_tool(&self.tool_name, &args_json)
            .await
            .map_err(|e| format!("MCP tool '{}' failed: {}", self.tool_name, e))?;

        // MCP 返回的是 text 字符串,包装成 JsonValue::string
        Ok(JsonValue::string(result))
    }
}

/// G12:把配置中的所有 MCP server 连上,把它们的工具注册到 `ToolHandler`
///
/// 对每个 server:
/// 1. spawn 子进程(StdioTransport)
/// 2. McpClient::connect 握手
/// 3. list_tools 发现工具
/// 4. 每个工具包装成 `McpToolAdapter`,以 `mcp_{server}_{tool}` 名注册
///
/// # 错误处理
///
/// - 某个 server 启动失败 → 记录 warning,跳过该 server,继续处理其他 server
///   (不让一个坏 server 阻止整个 agent 启动;见规范 §12.6)
/// - 某个 server 的工具发现失败 → 同样跳过该 server
///
/// # 返回
///
/// 成功连接的 server 数(便于日志/调试)。
pub async fn register_mcp_tools(
    tool_handler: &mut crate::io_handlers::tool_handler::ToolHandler,
    mcp_config: &crate::config::McpConfig,
) -> usize {
    let mut connected = 0usize;
    for server_cfg in &mcp_config.servers {
        match connect_and_register_one(tool_handler, server_cfg).await {
            Ok(tool_count) => {
                tracing::info!(
                    server = %server_cfg.name,
                    tools_registered = tool_count,
                    "MCP server connected"
                );
                connected += 1;
            }
            Err(e) => {
                tracing::warn!(
                    server = %server_cfg.name,
                    error = %e,
                    "failed to connect MCP server, skipping"
                );
            }
        }
    }
    connected
}

/// 连接单个 MCP server 并注册其所有工具
async fn connect_and_register_one(
    tool_handler: &mut crate::io_handlers::tool_handler::ToolHandler,
    server_cfg: &crate::config::McpServerConfig,
) -> Result<usize, String> {
    let args: Vec<&str> = server_cfg.args.iter().map(|s| s.as_str()).collect();

    let transport = if server_cfg.env.is_empty() {
        crate::mcp::transport::StdioTransport::spawn(&server_cfg.command, &args).await?
    } else {
        crate::mcp::transport::StdioTransport::spawn_with_env(
            &server_cfg.command,
            &args,
            &server_cfg.env,
        )
        .await?
    };

    let client = Arc::new(McpClient::connect(Arc::new(transport)).await?);

    let tools = client.list_tools().await?;
    let tool_count = tools.len();

    for tool in tools {
        let registered_name = mcp_tool_name(&server_cfg.name, &tool.name);
        tracing::info!(
            server = %server_cfg.name,
            mcp_tool = %tool.name,
            registered_as = %registered_name,
            "registering MCP tool"
        );
        tool_handler.register_tool(
            &registered_name,
            Arc::new(McpToolAdapter::new(client.clone(), tool.name)),
        );
    }

    Ok(tool_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::transport::tests::MockTransport;

    async fn make_client_with_tool_response() -> (Arc<McpClient>, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new());
        transport
            .set_response(
                "initialize",
                serde_json::json!({"serverInfo": {"name": "fs", "version": "1"}}),
            )
            .await;
        let client = Arc::new(McpClient::connect(transport.clone()).await.unwrap());
        (client, transport)
    }

    #[test]
    fn test_mcp_tool_name_format() {
        assert_eq!(
            mcp_tool_name("filesystem", "read_file"),
            "mcp_filesystem_read_file"
        );
        assert_eq!(
            mcp_tool_name("github", "create_issue"),
            "mcp_github_create_issue"
        );
    }

    #[test]
    fn test_mcp_tool_name_avoids_builtin_collision() {
        // 内置工具叫 file_read;MCP 工具加前缀后不会撞
        let name = mcp_tool_name("fs", "file_read");
        assert_eq!(name, "mcp_fs_file_read");
        assert_ne!(name, "file_read");
    }

    #[tokio::test]
    async fn test_adapter_call_returns_text_content() {
        let (client, transport) = make_client_with_tool_response().await;
        transport
            .set_response(
                "tools/call",
                serde_json::json!({"content": [{"type": "text", "text": "hello from MCP"}]}),
            )
            .await;
        let adapter = McpToolAdapter::new(client, "read_file".to_string());

        let args = {
            let mut m = std::collections::BTreeMap::new();
            m.insert("path".to_string(), JsonValue::string("/tmp/x"));
            JsonValue::object(m)
        };
        let result = adapter.call(&args).await.unwrap();
        assert_eq!(result.as_str(), Some("hello from MCP"));
    }

    #[tokio::test]
    async fn test_adapter_call_passes_arguments_as_json() {
        // 验证 args 被正确转发(MCP server 收到的 arguments 应含 path 字段)
        // MockTransport 不记录 params,这里只验证 call 成功且返回 text
        let (client, transport) = make_client_with_tool_response().await;
        transport
            .set_response(
                "tools/call",
                serde_json::json!({"content": [{"type": "text", "text": "ok"}]}),
            )
            .await;
        let adapter = McpToolAdapter::new(client, "write_file".to_string());

        // 复杂参数:嵌套对象 + 数组
        let args = {
            let mut path_val = std::collections::BTreeMap::new();
            path_val.insert("path".to_string(), JsonValue::string("/a/b"));
            path_val.insert(
                "options".to_string(),
                JsonValue::object({
                    let mut m = std::collections::BTreeMap::new();
                    m.insert("overwrite".to_string(), JsonValue::Bool(true));
                    m
                }),
            );
            JsonValue::object(path_val)
        };
        let result = adapter.call(&args).await.unwrap();
        assert_eq!(result.as_str(), Some("ok"));
    }

    #[tokio::test]
    async fn test_adapter_call_propagates_mcp_error() {
        let (client, transport) = make_client_with_tool_response().await;
        transport
            .set_response(
                "tools/call",
                serde_json::json!({"isError": true, "content": [{"type": "text", "text": "permission denied"}]}),
            )
            .await;
        let adapter = McpToolAdapter::new(client, "delete_file".to_string());

        let result = adapter.call(&JsonValue::Null).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("MCP tool 'delete_file' failed"));
        assert!(err.contains("permission denied"));
    }

    #[tokio::test]
    async fn test_adapter_call_transport_failure() {
        let (client, _transport) = make_client_with_tool_response().await;
        // 没注册 tools/call 响应 → transport 返回 Err
        let adapter = McpToolAdapter::new(client, "unknown".to_string());
        let result = adapter.call(&JsonValue::Null).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed"));
    }

    #[tokio::test]
    async fn test_adapter_call_empty_content_returns_empty_string() {
        let (client, transport) = make_client_with_tool_response().await;
        transport
            .set_response("tools/call", serde_json::json!({}))
            .await;
        let adapter = McpToolAdapter::new(client, "noop".to_string());
        let result = adapter.call(&JsonValue::Null).await.unwrap();
        assert_eq!(result.as_str(), Some(""));
    }

    #[tokio::test]
    async fn test_adapter_clone_and_debug() {
        // Clone + Debug 必须可用(ToolHandler 内部用 Arc,但 adapter 本身需 Clone)
        let (client, _transport) = make_client_with_tool_response().await;
        let adapter = McpToolAdapter::new(client, "read_file".to_string());
        let _cloned = adapter.clone();
        let debug_str = format!("{:?}", adapter);
        assert!(debug_str.contains("McpToolAdapter"));
        assert!(debug_str.contains("read_file"));
    }

    #[tokio::test]
    async fn test_adapter_tool_name_accessor() {
        let (client, _transport) = make_client_with_tool_response().await;
        let adapter = McpToolAdapter::new(client, "write_file".to_string());
        assert_eq!(adapter.tool_name(), "write_file");
    }

    #[tokio::test]
    async fn test_register_mcp_tools_empty_config() {
        let mut handler = crate::io_handlers::tool_handler::ToolHandler::new();
        let config = crate::config::McpConfig::default();
        let count = register_mcp_tools(&mut handler, &config).await;
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_register_mcp_tools_skips_failing_server() {
        // 用一个不存在的命令,验证该 server 被跳过、不 panic、返回 0
        let mut handler = crate::io_handlers::tool_handler::ToolHandler::new();
        let config = crate::config::McpConfig {
            servers: vec![crate::config::McpServerConfig {
                name: "broken".to_string(),
                command: "this-command-does-not-exist-xyz999".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
            }],
        };
        let count = register_mcp_tools(&mut handler, &config).await;
        assert_eq!(count, 0);
        // 没有工具被注册
        assert!(!handler.has_tool("mcp_broken_anything"));
    }
}
