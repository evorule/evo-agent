// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! MCP(Model Context Protocol)双面接入 —— client 接入外部工具生态 + server 暴露自有工具面
//!
//! ## 作用（client / server 双面）
//!
//! - **client 面**：连接外部 MCP server（stdio 子进程或 Streamable HTTP 远端），
//!   把其工具注册进 [`ToolHandler`](crate::io_handlers::tool_handler::ToolHandler)
//!   ——run CLI 走 [`register_mcp_tools`](tool_adapter::register_mcp_tools) 单发注册，
//!   serve 走 [`McpManager`](McpManager) 常驻连接态管理（B16）。
//! - **server 面**（B23）：把 serve 工具面以 MCP 协议暴露为 `POST /mcp` 端点，
//!   治理等价——外部 MCP 客户端可见/可调的工具面与 LLM 完全一致，审批链共用
//!   同一执行体；`mcpServer.enabled` 缺省 false fail-closed（server.rs）。
//! - **工作台面**（B16 PR-2）：`/api/mcp/*` 四端点（servers 配置读写 / restart /
//!   status），配置单一事实源 `.evo/mcp.toml`（store.rs 原子写 +
//!   evo-agent.toml `[mcp]` 段回落）。
//!
//! ## 架构（client 面 4 层 + server 面）
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
//! │  mcp_{server}_{tool} 命名 / always_allow 白名单 / 审批钩子 │
//! └──────────────────────┬──────────────────────────────────┘
//!                        │ async call_tool
//!                        ▼
//! ┌─────────────────────────────────────────────────────────┐
//! │                McpClient (client.rs)                     │
//! │  connect() / list_tools() / call_tool(_structured)()     │
//! │  JSON-RPC 2.0: initialize / tools/list / tools/call      │
//! │  （协议版本 2025-06-18，additive 回落旧版；图片块收集）    │
//! └──────────────────────┬──────────────────────────────────┘
//!                        │ McpTransport::request / notify
//!                        ▼
//! ┌─────────────────────────────────────────────────────────┐
//! │           双传输（transport.rs，trait McpTransport）      │
//! │  StdioTransport：spawn 子进程 → stdin/stdout JSON-RPC，   │
//! │    reader task 按 id 路由 oneshot（任务落连接时活跃 runtime）│
//! │  StreamableHttpTransport：POST + SSE 响应、会话头捕获、    │
//! │    headers 内 ${ENV_VAR} 缺变量 fail-closed 展开          │
//! └─────────────────────────────────────────────────────────┘
//!
//! serve 侧编排（B16）：McpManager（manager.rs）三态连接态
//! starting → connected | failed{error}，失败隔离不阻塞启动；
//! connect_and_register_all / reconnect / shutdown；server.rs 挂载
//! /mcp 暴露面（enabled 门禁），api/mcp_api.rs 挂工作台四端点。
//! ```
//!
//! ## 能力边界（现状；G12 P1 边界已随 B16/B23 演进）
//!
//! - **双传输**：stdio（缺省）+ streamable_http（P1 期「SSE 放 P2」已落）
//! - **双面**：client 接入 + `/mcp` server 暴露（P1 期「只做 client」已落；
//!   server 面缺省关闭 fail-closed，开启即治理等价）
//! - **工具名加 `mcp_{server}_{tool}` 前缀**，避免与内置工具名冲突
//! - 依赖 G13 的 async `ToolFunction`（已完成）
//!
//! ## 配置示例（`.evo/mcp.toml` 或 evo-agent.toml `[mcp]` 段）
//!
//! ```toml
//! [[mcp.servers]]
//! name = "filesystem"
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
//!
//! [[mcp.servers]]
//! name = "remote"
//! transport = "streamable_http"
//! url = "https://example.com/mcp"
//! # headers = { Authorization = "Bearer ${MCP_TOKEN}" }  # env 展开缺变量拒启
//! ```

pub mod client;
pub mod gov_proposal;
pub mod gov_source;
pub mod manager;
pub mod server;
pub mod spawn_gate;
pub mod store;
pub mod tool_adapter;
pub mod transport;

pub use client::{McpClient, McpToolSpec};
pub use manager::{McpManager, McpServerEntry, McpServerStatus};
pub use store::McpConfigStore;
pub use tool_adapter::{mcp_tool_name, register_client_tools, register_mcp_tools, McpToolAdapter};
pub use transport::{McpTransport, StdioTransport, StreamableHttpTransport};
