// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G12:MCP(Model Context Protocol)客户端 —— 接入 MCP 工具生态
//!
//! ## 作用
//!
//! 让 evo-agent 能连接外部 MCP server(如 Claude Desktop 的 stdio server、
//! `@modelcontextprotocol/server-filesystem` 等),把 MCP server 暴露的工具
//! 注册到 evo-agent 的 [`ToolHandler`](crate::io_handlers::tool_handler::ToolHandler) 中。
//!
//! ## 架构(3 层)
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │                   ToolHandler                            │
//! │  register_tool("mcp_filesystem_read_file", adapter)      │
//! └──────────────────────┬──────────────────────────────────┘
//!                        │ ToolFunction::call (async, G13)
//!                        ▼
//! ┌─────────────────────────────────────────────────────────┐
//! │              McpToolAdapter (tool_adapter.rs)            │
//! │  把 ToolFunction::call → McpClient::call_tool            │
//! └──────────────────────┬──────────────────────────────────┘
//!                        │ async call_tool
//!                        ▼
//! ┌─────────────────────────────────────────────────────────┐
//! │                McpClient (client.rs)                     │
//! │  connect() / list_tools() / call_tool()                  │
//! │  JSON-RPC 2.0 method: initialize / tools/list / tools/call │
//! └──────────────────────┬──────────────────────────────────┘
//!                        │ McpTransport::request / notify
//!                        ▼
//! ┌─────────────────────────────────────────────────────────┐
//! │              StdioTransport (transport.rs)                │
//! │  spawn 子进程 → stdin 写 JSON-RPC → stdout 读响应         │
//! │  reader loop 按 id 路由到 oneshot channel                 │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! ## P1 边界(见规范 §12.6)
//!
//! - **只实现 stdio 传输**(SSE 传输放 P2)
//! - **只做 client**(evo-agent 不暴露自己为 MCP server;P2 可考虑)
//! - **工具名加 `mcp_{server}_{tool}` 前缀**,避免与内置工具名冲突
//! - 依赖 G13 的 async `ToolFunction`(已完成)
//!
//! ## 配置示例
//!
//! ```toml
//! [[mcp.servers]]
//! name = "filesystem"
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
//! ```

pub mod client;
pub mod tool_adapter;
pub mod transport;

pub use client::{McpClient, McpToolSpec};
pub use tool_adapter::{mcp_tool_name, register_mcp_tools, McpToolAdapter};
pub use transport::{McpTransport, StdioTransport};
