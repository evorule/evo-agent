// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! B16:MCP server 连接态管理器(serve 面专用)
//!
//! P1 的 MCP 注册只发生在 `evo-agent run`(见 [`register_mcp_tools`]);
//! serve 模式需要**长生命周期**的 client(子进程常驻,按请求 spawn 代价
//! 过高)——本管理器持有每 server 的连接与工具清单,并把工具注册进 serve
//! union toolkit,是 B16 工作台面(配置/状态/重启)的运行态基座。
//!
//! ## 连接态模型(16 号档 §3.2)
//!
//! `starting → connected | failed{error}`;restart = 断连(close,kill 子
//! 进程)后按同一配置重连重注册。失败隔离(D10):单 server 失败只落状态,
//! 不影响其他 server 与 serve 启动。
//!
//! ## 锁纪律
//!
//! `entries` 用 std Mutex,临界区内无 await(连接/关闭等 async 动作在锁外
//! 完成,结果回填时再短暂持锁)。

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::McpServerConfig;

use super::client::{McpClient, McpToolSpec};
use super::tool_adapter::register_client_tools;
use super::transport::{McpTransport, StdioTransport, StreamableHttpTransport};

/// 单 server 连接态(16 号档 §3.2 三态)
#[derive(Debug, Clone, PartialEq)]
pub enum McpServerStatus {
    /// 连接中(异步连接尚未落定)
    Starting,
    /// 已连接(工具清单就绪)
    Connected,
    /// 连接失败(含错误描述;配置保留可重启)
    Failed { error: String },
}

impl McpServerStatus {
    /// 状态名(状态端点序列化用)
    pub fn as_str(&self) -> &'static str {
        match self {
            McpServerStatus::Starting => "starting",
            McpServerStatus::Connected => "connected",
            McpServerStatus::Failed { .. } => "failed",
        }
    }
}

/// 单 server 运行态条目
#[derive(Debug)]
pub struct McpServerEntry {
    pub status: McpServerStatus,
    /// 已连接 client(connected 态必有;failed 态 None)
    pub client: Option<std::sync::Arc<McpClient>>,
    /// tools/list 发现的工具清单(connected 态填充,状态面/工具树数据源)
    pub tools: Vec<McpToolSpec>,
}

/// serve 级 MCP 管理器
#[derive(Debug, Default)]
pub struct McpManager {
    entries: Mutex<HashMap<String, McpServerEntry>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 连接单个 server(spawn → initialize 握手 → tools/list,受 timeout 约束)
    ///
    /// 状态先落 `starting`,连接结果落 `connected`/`failed`;返回连接产物
    /// 供调用方注册工具。失败隔离:错误只返回+落状态,不 panic。
    pub async fn connect_server(
        &self,
        cfg: &McpServerConfig,
    ) -> Result<(std::sync::Arc<McpClient>, Vec<McpToolSpec>), String> {
        self.set_entry(
            cfg.name.clone(),
            McpServerEntry {
                status: McpServerStatus::Starting,
                client: None,
                tools: Vec::new(),
            },
        );

        let result = self.connect_inner(cfg).await;
        match &result {
            Ok((client, tools)) => self.set_entry(
                cfg.name.clone(),
                McpServerEntry {
                    status: McpServerStatus::Connected,
                    client: Some(client.clone()),
                    tools: tools.clone(),
                },
            ),
            Err(error) => self.set_entry(
                cfg.name.clone(),
                McpServerEntry {
                    status: McpServerStatus::Failed {
                        error: error.clone(),
                    },
                    client: None,
                    tools: Vec::new(),
                },
            ),
        }
        result
    }

    /// 连接实现(按 transport 分派 + 握手 + 发现,整体受 timeout_secs 约束)
    ///
    /// B23:stdio(缺省)spawn 子进程;streamable_http 连远程 endpoint
    /// (headers 内 `${ENV_VAR}` 在 transport 构造时展开,缺变量 fail-closed)。
    async fn connect_inner(
        &self,
        cfg: &McpServerConfig,
    ) -> Result<(std::sync::Arc<McpClient>, Vec<McpToolSpec>), String> {
        let transport: std::sync::Arc<dyn McpTransport> =
            match cfg.transport.as_deref().unwrap_or("stdio") {
                "stdio" => {
                    let args: Vec<&str> = cfg.args.iter().map(|s| s.as_str()).collect();
                    std::sync::Arc::new(if cfg.env.is_empty() {
                        StdioTransport::spawn(&cfg.command, &args).await?
                    } else {
                        StdioTransport::spawn_with_env(&cfg.command, &args, &cfg.env).await?
                    })
                }
                "streamable_http" => {
                    let url = cfg.url.as_deref().unwrap_or("");
                    let headers = cfg.headers.clone().unwrap_or_default();
                    std::sync::Arc::new(StreamableHttpTransport::new(url, &headers)?)
                }
                other => return Err(format!("unsupported MCP transport '{}'", other)),
            };
        let client = std::sync::Arc::new(McpClient::connect(transport).await?);
        let tools = client.list_tools().await?;
        Ok((client, tools))
    }

    /// 断连单 server(close → kill 子进程)并清除运行态条目
    pub async fn disconnect(&self, name: &str) {
        let client = self.remove_entry(name);
        if let Some(entry) = client {
            if let Some(client) = entry.client {
                if let Err(e) = client.close().await {
                    tracing::warn!(server = %name, error = %e, "MCP server close failed");
                }
            }
        }
    }

    /// 重连单 server(断连 → 按配置重连),**不注册工具**
    ///
    /// B16 PR-2:供 serve 端点「先 async 连接 → 再同步持 toolkit 写锁注册」
    /// 两段式使用(写锁 guard 非 Send 不得跨 await)。失败隔离语义同
    /// [`connect_server`](Self::connect_server)(状态面已落 connected/failed)。
    pub async fn reconnect(
        &self,
        cfg: &McpServerConfig,
    ) -> Result<(std::sync::Arc<McpClient>, Vec<McpToolSpec>), String> {
        self.disconnect(&cfg.name).await;
        self.connect_server(cfg).await
    }

    /// 重启单 server(断连 → 按配置重连重注册进 handler)
    ///
    /// 返回注册的工具数;连接失败时错误返回(状态面已落 failed)。
    pub async fn restart(
        &self,
        cfg: &McpServerConfig,
        tool_handler: &mut crate::io_handlers::tool_handler::ToolHandler,
    ) -> Result<usize, String> {
        let (client, tools) = self.reconnect(cfg).await?;
        register_client_tools(
            tool_handler,
            &cfg.name,
            &client,
            &tools,
            &cfg.always_allow,
            cfg.timeout_secs,
        )
    }

    /// 连接全部 server 并把工具注册进 union toolkit(serve 启动接入)
    ///
    /// disabled 的 server 跳过(不连接不注册,配置保留);失败隔离(D10):
    /// 单 server 失败 warn + 落 failed 状态,继续其余。返回成功连接数。
    pub async fn connect_and_register_all(
        &self,
        tool_handler: &mut crate::io_handlers::tool_handler::ToolHandler,
        cfgs: &[McpServerConfig],
    ) -> usize {
        let mut connected = 0usize;
        for cfg in cfgs {
            if cfg.disabled {
                tracing::info!(server = %cfg.name, "MCP server disabled, skipping");
                continue;
            }
            match self.connect_server(cfg).await {
                Ok((client, tools)) => {
                    match register_client_tools(
                        tool_handler,
                        &cfg.name,
                        &client,
                        &tools,
                        &cfg.always_allow,
                        cfg.timeout_secs,
                    ) {
                        Ok(count) => {
                            tracing::info!(
                                server = %cfg.name,
                                tools_registered = count,
                                "MCP server connected"
                            );
                            connected += 1;
                        }
                        Err(e) => {
                            // 契约拒绝等注册期失败:状态面落 failed,隔离继续
                            tracing::warn!(server = %cfg.name, error = %e, "MCP tool registration failed");
                            self.set_entry(
                                cfg.name.clone(),
                                McpServerEntry {
                                    status: McpServerStatus::Failed { error: e },
                                    client: None,
                                    tools: Vec::new(),
                                },
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        server = %cfg.name,
                        error = %e,
                        "failed to connect MCP server, skipping"
                    );
                }
            }
        }
        connected
    }

    /// 全量关闭(serve 退出/重载前调用;逐 server close → kill 子进程)
    pub async fn shutdown(&self) {
        let names: Vec<String> = {
            let guard = self.entries.lock().expect("mcp manager lock");
            guard.keys().cloned().collect()
        };
        for name in names {
            self.disconnect(&name).await;
        }
    }

    /// 状态快照(状态端点数据源;按 server 名排序)
    pub fn snapshot(&self) -> Vec<(String, McpServerEntry)> {
        let guard = self.entries.lock().expect("mcp manager lock");
        let mut out: Vec<(String, McpServerEntry)> = guard
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    McpServerEntry {
                        status: v.status.clone(),
                        client: v.client.clone(),
                        tools: v.tools.clone(),
                    },
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn set_entry(&self, name: String, entry: McpServerEntry) {
        self.entries
            .lock()
            .expect("mcp manager lock")
            .insert(name, entry);
    }

    /// 显式落 failed 状态(B16 PR-2:端点侧注册期失败——连接已成功但工具
    /// 注册被拒/写锁竞争异常时,状态面回落 failed 供工作台可见)
    pub fn mark_failed(&self, name: &str, error: &str) {
        self.set_entry(
            name.to_string(),
            McpServerEntry {
                status: McpServerStatus::Failed {
                    error: error.to_string(),
                },
                client: None,
                tools: Vec::new(),
            },
        );
    }

    fn remove_entry(&self, name: &str) -> Option<McpServerEntry> {
        self.entries.lock().expect("mcp manager lock").remove(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: command.to_string(),
            args: vec![],
            env: HashMap::new(),
            disabled: false,
            timeout_secs: 60,
            always_allow: vec![],
            transport: None,
            url: None,
            headers: None,
        }
    }

    #[tokio::test]
    async fn test_connect_server_records_failed_status() {
        let mgr = McpManager::new();
        let result = mgr
            .connect_server(&cfg("broken", "this-command-does-not-exist-xyz999"))
            .await;
        assert!(result.is_err());
        let snap = mgr.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "broken");
        assert_eq!(snap[0].1.status.as_str(), "failed");
        assert!(snap[0].1.client.is_none());
    }

    #[tokio::test]
    async fn test_connect_server_unsupported_transport_fails() {
        let mgr = McpManager::new();
        let mut bad = cfg("bad-tp", "npx");
        bad.transport = Some("websocket".to_string());
        let result = mgr.connect_server(&bad).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported MCP transport"));
        let snap = mgr.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].1.status.as_str(), "failed");
    }

    #[tokio::test]
    async fn test_connect_server_streamable_http_unreachable_fails() {
        let mgr = McpManager::new();
        let mut remote = cfg("remote", "");
        remote.transport = Some("streamable_http".to_string());
        // 未监听端口:连接失败隔离落 failed,不 panic
        remote.url = Some("http://127.0.0.1:9/mcp".to_string());
        let result = mgr.connect_server(&remote).await;
        assert!(result.is_err());
        let snap = mgr.snapshot();
        assert_eq!(snap[0].1.status.as_str(), "failed");
    }

    #[tokio::test]
    async fn test_connect_and_register_all_skips_disabled() {
        let mgr = McpManager::new();
        let broken = cfg("broken", "this-command-does-not-exist-xyz999");
        let mut off = cfg("off", "whatever");
        off.disabled = true;
        let mut handler = crate::io_handlers::tool_handler::ToolHandler::new();
        // disabled 跳过(不落状态),broken 落 failed——都算不上 connected
        let count = mgr
            .connect_and_register_all(&mut handler, &[broken.clone(), off])
            .await;
        assert_eq!(count, 0);
        assert!(!handler.has_tool("mcp_off_anything"));
        let snap = mgr.snapshot();
        assert_eq!(snap.len(), 1, "disabled server 不落运行态条目");
        assert_eq!(snap[0].0, "broken");
        let _ = broken;
    }

    #[tokio::test]
    async fn test_snapshot_sorted_and_empty() {
        let mgr = McpManager::new();
        assert!(mgr.snapshot().is_empty());
        let mut b = cfg("b-server", "nope-1");
        let mut a = cfg("a-server", "nope-2");
        a.args = vec![];
        b.args = vec![];
        let _ = mgr.connect_server(&b).await;
        let _ = mgr.connect_server(&a).await;
        let snap = mgr.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap[0].0 < snap[1].0, "快照按 server 名排序");
    }

    #[tokio::test]
    async fn test_shutdown_clears_entries() {
        let mgr = McpManager::new();
        let _ = mgr.connect_server(&cfg("s", "nope")).await;
        assert_eq!(mgr.snapshot().len(), 1);
        mgr.shutdown().await;
        assert!(mgr.snapshot().is_empty());
    }

    #[test]
    fn test_status_as_str() {
        assert_eq!(McpServerStatus::Starting.as_str(), "starting");
        assert_eq!(McpServerStatus::Connected.as_str(), "connected");
        assert_eq!(
            McpServerStatus::Failed { error: "x".into() }.as_str(),
            "failed"
        );
    }
}
