// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G12:MCP 客户端 —— 握手 + 工具发现 + 工具调用
//!
//! [`McpClient`] 封装 MCP 协议的三个核心交互:
//! 1. [`connect`](McpClient::connect):`initialize` 握手 + 发送 `notifications/initialized`
//! 2. [`list_tools`](McpClient::list_tools):`tools/list` 发现 server 暴露的工具
//! 3. [`call_tool`](McpClient::call_tool):`tools/call` 调用某个工具
//!
//! client 持有 `Arc<dyn McpTransport>`,可被多个 [`McpToolAdapter`](crate::mcp::tool_adapter::McpToolAdapter)
//! 共享(每个 adapter 对应 server 上的一个工具,但共用同一个传输层)。

use std::sync::Arc;

use serde_json::Value;
use tracing::debug;

use crate::mcp::transport::McpTransport;

/// MCP 协议版本(2024-11-05;Streamable HTTP 传输的 MCP-Protocol-Version 头沿用)
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// MCP 客户端
///
/// 生命周期:一个 `McpClient` 对应一个已连接的 MCP server。
/// 通过 `Arc<McpClient>` 共享给多个 `McpToolAdapter`。
pub struct McpClient {
    transport: Arc<dyn McpTransport>,
    /// `initialize` 握手返回的 server info(含 server 名称/版本/能力)
    server_info: Value,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server_info", &self.server_info)
            .finish()
    }
}

impl McpClient {
    /// 连接 MCP server 并完成 initialize 握手
    ///
    /// 步骤(见 MCP 协议 §初始化):
    /// 1. 发送 `initialize` 请求(带协议版本 + client 信息)
    /// 2. 收到 server 返回的 capabilities + serverInfo
    /// 3. 发送 `notifications/initialized` 通知(告诉 server 握手完成)
    pub async fn connect(transport: Arc<dyn McpTransport>) -> Result<Self, String> {
        let result = transport
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "evo-agent",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .await?;

        // 发送 initialized 通知(协议要求)
        transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;

        debug!(server_info = %result, "MCP server connected");

        Ok(Self {
            transport,
            server_info: result,
        })
    }

    /// 只读访问 server info(initialize 握手返回值)
    pub fn server_info(&self) -> &Value {
        &self.server_info
    }

    /// 只读访问 transport(供 close 等操作)
    pub fn transport(&self) -> &Arc<dyn McpTransport> {
        &self.transport
    }

    /// 发现工具列表(`tools/list`)
    ///
    /// 返回 server 暴露的所有工具的 spec。
    pub async fn list_tools(&self) -> Result<Vec<McpToolSpec>, String> {
        let result = self
            .transport
            .request("tools/list", serde_json::json!({}))
            .await?;

        let tools = result
            .get("tools")
            .ok_or("MCP tools/list: missing 'tools' field")?
            .as_array()
            .ok_or("MCP tools/list: 'tools' is not an array")?;

        tools
            .iter()
            .map(|t| {
                let name = t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or("MCP tool: missing 'name'")?
                    .to_string();
                let description = t
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input_schema = t.get("inputSchema").cloned().unwrap_or(Value::Null);
                Ok(McpToolSpec {
                    name,
                    description,
                    input_schema,
                })
            })
            .collect()
    }

    /// 调用工具(`tools/call`)
    ///
    /// MCP 返回 `content` 数组(每个元素可能是 text/image/resource),
    /// P1 只取第一个 `text` 类型的内容(最常见场景)。
    /// 若无 text 内容,返回空字符串。
    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<String, String> {
        let result = self
            .transport
            .request(
                "tools/call",
                serde_json::json!({
                    "name": name,
                    "arguments": arguments,
                }),
            )
            .await?;

        // MCP 错误响应(isError=true)优先处理
        if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
            // 取 content 中第一个 text 作为错误描述
            let msg = extract_first_text(&result)
                .unwrap_or_else(|| "MCP tool returned error".to_string());
            return Err(format!("MCP tool '{}' returned error: {}", name, msg));
        }

        Ok(extract_first_text(&result).unwrap_or_default())
    }

    /// 结构化调用工具(G3:`call_tool` 的图片面扩展)
    ///
    /// text 语义与 [`McpClient::call_tool`] 完全一致(首个 text 块,无则空串;
    /// isError 错误路径同款);额外收集 `content` 数组内全部 image 块。
    /// 其余块类型(resource 等)维持忽略。旧方法原样保留,存量调用零变化。
    pub async fn call_tool_structured(
        &self,
        name: &str,
        arguments: &Value,
    ) -> Result<McpToolResult, String> {
        let result = self
            .transport
            .request(
                "tools/call",
                serde_json::json!({
                    "name": name,
                    "arguments": arguments,
                }),
            )
            .await?;

        // MCP 错误响应(isError=true)优先处理——与 call_tool 同款语义
        if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
            let msg = extract_first_text(&result)
                .unwrap_or_else(|| "MCP tool returned error".to_string());
            return Err(format!("MCP tool '{}' returned error: {}", name, msg));
        }

        Ok(McpToolResult {
            text: extract_first_text(&result).unwrap_or_default(),
            images: extract_images(&result),
        })
    }

    /// 关闭底层传输层
    pub async fn close(&self) -> Result<(), String> {
        self.transport.close().await
    }
}

/// MCP image content block(G3 结构化提取产物)
///
/// MCP `tools/call` 响应中 `{"type":"image","data":"<base64>","mimeType":"..."}` 块。
#[derive(Debug, Clone, PartialEq)]
pub struct McpImage {
    /// MIME 类型(如 `image/png`)
    pub mime_type: String,
    /// 图片本体 base64(未解码,与 MCP 协议原样)
    pub data_base64: String,
}

/// MCP `tools/call` 结构化结果
///
/// G3:GUI 桥类工具(截图等)返回 image content block,P1 首个 text 语义接不住
/// ——本结构在不动 `call_tool` 的前提下补齐图片面。text 语义与 `call_tool`
/// 完全一致(首个 text 块,无则空串),存量调用零变化。
#[derive(Debug, Clone, Default)]
pub struct McpToolResult {
    pub text: String,
    pub images: Vec<McpImage>,
}

/// 从 MCP tools/call 响应中提取第一个 text content
fn extract_first_text(response: &Value) -> Option<String> {
    response
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("text"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// G3:收集响应 `content` 数组内全部 image content block
///
/// MCP image 块形态 `{"type":"image","data":"<base64>","mimeType":"image/png"}`;
/// 缺 data/mimeType、空值块跳过(不 fail,与 P1 忽略未知类型语义同族)。
fn extract_images(response: &Value) -> Vec<McpImage> {
    response
        .get("content")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|c| c.get("type").and_then(|v| v.as_str()) == Some("image"))
                .filter_map(|c| {
                    let data = c.get("data").and_then(|v| v.as_str())?;
                    let mime = c.get("mimeType").and_then(|v| v.as_str())?;
                    if data.is_empty() || mime.is_empty() {
                        return None;
                    }
                    Some(McpImage {
                        mime_type: mime.to_string(),
                        data_base64: data.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// MCP 工具 spec(`tools/list` 返回的单个工具描述)
#[derive(Debug, Clone)]
pub struct McpToolSpec {
    /// 工具名(server 内唯一)
    pub name: String,
    /// 工具描述(给 LLM 看)
    pub description: String,
    /// 输入 JSON Schema(MCP server 提供)
    pub input_schema: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::transport::tests::MockTransport;

    async fn make_connected_client() -> (McpClient, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new());
        transport
            .set_response(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "serverInfo": { "name": "test-server", "version": "1.0.0" },
                    "capabilities": {}
                }),
            )
            .await;
        let client = McpClient::connect(transport.clone()).await.unwrap();
        (client, transport)
    }

    #[tokio::test]
    async fn test_connect_initializes_and_notifies() {
        let (client, transport) = make_connected_client().await;
        // server_info 被保存
        let info = client.server_info();
        assert_eq!(
            info.get("serverInfo")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str()),
            Some("test-server")
        );
        // 发送了 initialized 通知
        let notifies = transport.notifies_received().await;
        assert_eq!(notifies.len(), 1);
        assert_eq!(notifies[0].0, "notifications/initialized");
    }

    #[tokio::test]
    async fn test_connect_fails_when_initialize_rejected() {
        let transport = Arc::new(MockTransport::new());
        // 不注册 initialize 响应 → MockTransport 返回 Err
        let result = McpClient::connect(transport).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no response registered"));
    }

    #[tokio::test]
    async fn test_list_tools_parses_tool_array() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response(
                "tools/list",
                serde_json::json!({
                    "tools": [
                        {
                            "name": "read_file",
                            "description": "Read a file",
                            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}}
                        },
                        {
                            "name": "write_file",
                            "description": "Write a file",
                            "inputSchema": {"type": "object"}
                        }
                    ]
                }),
            )
            .await;
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[0].description, "Read a file");
        assert_eq!(tools[1].name, "write_file");
        assert!(tools[1].input_schema.get("type").is_some());
    }

    #[tokio::test]
    async fn test_list_tools_missing_tools_field() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response("tools/list", serde_json::json!({"unexpected": true}))
            .await;
        let result = client.list_tools().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing 'tools'"));
    }

    #[tokio::test]
    async fn test_list_tools_tools_not_array() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response("tools/list", serde_json::json!({"tools": "not-an-array"}))
            .await;
        let result = client.list_tools().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not an array"));
    }

    #[tokio::test]
    async fn test_list_tools_tool_missing_name() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response(
                "tools/list",
                serde_json::json!({"tools": [{"description": "no name"}]}),
            )
            .await;
        let result = client.list_tools().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing 'name'"));
    }

    #[tokio::test]
    async fn test_list_tools_tool_optional_fields_default() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response(
                "tools/list",
                serde_json::json!({"tools": [{"name": "minimal"}]}),
            )
            .await;
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "minimal");
        assert_eq!(tools[0].description, "");
        assert!(tools[0].input_schema.is_null());
    }

    #[tokio::test]
    async fn test_call_tool_returns_first_text() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response(
                "tools/call",
                serde_json::json!({
                    "content": [
                        {"type": "text", "text": "file content here"},
                        {"type": "text", "text": "second (ignored)"}
                    ]
                }),
            )
            .await;
        let result = client
            .call_tool("read_file", &serde_json::json!({"path": "/tmp/x"}))
            .await
            .unwrap();
        assert_eq!(result, "file content here");
    }

    #[tokio::test]
    async fn test_call_tool_no_content_returns_empty() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response("tools/call", serde_json::json!({}))
            .await;
        let result = client
            .call_tool("noop", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result, "");
    }

    #[tokio::test]
    async fn test_call_tool_is_error_returns_err() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response(
                "tools/call",
                serde_json::json!({
                    "isError": true,
                    "content": [{"type": "text", "text": "file not found"}]
                }),
            )
            .await;
        let result = client
            .call_tool("read_file", &serde_json::json!({"path": "/missing"}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("returned error"));
        assert!(err.contains("file not found"));
    }

    #[tokio::test]
    async fn test_call_tool_is_error_no_content() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response("tools/call", serde_json::json!({"isError": true}))
            .await;
        let result = client.call_tool("bad", &serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("returned error"));
    }

    #[tokio::test]
    async fn test_call_tool_transport_error_propagates() {
        let (client, _transport) = make_connected_client().await;
        // 没注册 tools/call 响应 → MockTransport 返回 Err
        let result = client.call_tool("unknown", &serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no response registered"));
    }

    // ---- G3:call_tool_structured 图片面 ----

    #[tokio::test]
    async fn test_call_tool_structured_collects_image_blocks() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response(
                "tools/call",
                serde_json::json!({
                    "content": [
                        {"type": "text", "text": "screenshot taken"},
                        {"type": "image", "data": "cG5nLWRhdGE=", "mimeType": "image/png"},
                        {"type": "resource", "resource": {"uri": "file:///x"}},
                        {"type": "image", "data": "anYtZGF0YQ==", "mimeType": "image/jpeg"}
                    ]
                }),
            )
            .await;
        let r = client
            .call_tool_structured("screenshot", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(r.text, "screenshot taken");
        assert_eq!(r.images.len(), 2);
        assert_eq!(r.images[0].mime_type, "image/png");
        assert_eq!(r.images[0].data_base64, "cG5nLWRhdGE=");
        assert_eq!(r.images[1].mime_type, "image/jpeg");
    }

    #[tokio::test]
    async fn test_call_tool_structured_skips_invalid_image_blocks() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response(
                "tools/call",
                serde_json::json!({
                    "content": [
                        {"type": "text", "text": "only text survives"},
                        {"type": "image", "data": "", "mimeType": "image/png"},
                        {"type": "image", "data": "aGF2ZW1pbWU="},
                        {"type": "image", "mimeType": "image/png"}
                    ]
                }),
            )
            .await;
        let r = client
            .call_tool_structured("mixed", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(r.images.len(), 0);
        assert_eq!(r.text, "only text survives");
    }

    #[tokio::test]
    async fn test_call_tool_structured_no_content_matches_call_tool() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response("tools/call", serde_json::json!({}))
            .await;
        let r = client
            .call_tool_structured("noop", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(r.text, "");
        assert!(r.images.is_empty());
    }

    #[tokio::test]
    async fn test_call_tool_structured_is_error_same_semantics() {
        let (client, transport) = make_connected_client().await;
        transport
            .set_response(
                "tools/call",
                serde_json::json!({
                    "isError": true,
                    "content": [{"type": "text", "text": "boom"}]
                }),
            )
            .await;
        let result = client
            .call_tool_structured("bad", &serde_json::json!({}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("returned error"));
        assert!(err.contains("boom"));
    }

    #[test]
    fn test_extract_first_text_extracts_text() {
        let resp = serde_json::json!({
            "content": [{"type": "text", "text": "hello"}]
        });
        assert_eq!(extract_first_text(&resp), Some("hello".to_string()));
    }

    #[test]
    fn test_extract_first_text_no_content() {
        let resp = serde_json::json!({});
        assert_eq!(extract_first_text(&resp), None);
    }

    #[test]
    fn test_extract_first_text_empty_array() {
        let resp = serde_json::json!({"content": []});
        assert_eq!(extract_first_text(&resp), None);
    }

    #[test]
    fn test_extract_first_text_non_text_first() {
        // 第一个是 image,没有 text → None
        let resp = serde_json::json!({
            "content": [{"type": "image", "data": "..."}]
        });
        assert_eq!(extract_first_text(&resp), None);
    }

    #[tokio::test]
    async fn test_close_delegates_to_transport() {
        let (client, transport) = make_connected_client().await;
        assert!(!transport.is_closed());
        client.close().await.unwrap();
        assert!(transport.is_closed());
    }
}
