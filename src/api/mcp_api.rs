// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! B16 PR-2:MCP 工作台 REST 面 —— server 配置/连接态/重启的管理端点
//!
//! 四端点全部挂 `agent_api::router` 的 route 链(G16 鉴权中间件自动覆盖),
//! 与 [`crate::api::git_api`] 同款主体语义:服务的是**人**在工作台 MCP 页的
//! 直接管理操作,不进 agent 会话审计链。
//!
//! ## 端点一览
//!
//! - `GET  /api/mcp/servers`              → 配置面 + 运行态合并视图(工具表)
//! - `PUT  /api/mcp/servers`              → 校验 → 写回 .evo/mcp.toml → 全量重载
//!   (env 字段省略 = 保留该 server 现有 env 值,显式提供含空对象 = 覆盖)
//! - `POST /api/mcp/servers/{name}/restart` → 断连重连重注册单 server
//! - `GET  /api/mcp/status`               → 聚合徽标(总/连/败/停 + 工具数)
//!
//! ## 锁与 await 纪律
//!
//! toolkit 为 `std::sync::RwLock<ToolHandler>`(B16 PR-2 升级),写 guard 非
//! Send **不得跨 await**——所有重载路径走两段式:async 段先完成连接
//! ([`McpManager::reconnect`](crate::mcp::McpManager::reconnect),不持
//! handler 引用),同步段再持写锁注册
//! ([`register_client_tools`](crate::mcp::register_client_tools)),临界区内无 await。
//!
//! ## 未启用态
//!
//! serve 启动未注入 MCP 管理器(无配置或 agentTools.mcp=false)时,
//! /api/mcp/* 一律 503(同 /api/index/* 关闭态惯例)。

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use tracing::info;

use crate::api::agent_api::AgentApiState;
use crate::config::{McpConfig, McpServerConfig};
use crate::mcp::{register_client_tools, McpConfigStore, McpManager, McpServerStatus};

/// MCP 面未启用(serve 启动未注入管理器)→ 503
fn not_enabled() -> (StatusCode, Json<Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": "MCP surface not enabled",
            "hint": "serve 启动时未注入 MCP 管理器(无 mcp 配置或 agentTools.mcp=false)"
        })),
    )
}

/// 配置校验(name/command 非空、name 字符集安全、唯一、timeout > 0)
///
/// name 直接进入工具名 `mcp_{name}_{tool}` 与 URL 路径,约束为
/// `[a-zA-Z0-9_-]` 防注入与歧义。
fn validate(cfg: &McpConfig) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for s in &cfg.servers {
        if s.name.trim().is_empty() {
            return Err("server name must not be empty".to_string());
        }
        if !s
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "server name '{}' must only contain [a-zA-Z0-9_-]",
                s.name
            ));
        }
        if !seen.insert(s.name.clone()) {
            return Err(format!("duplicate server name '{}'", s.name));
        }
        // B23:按传输方式分支校验(缺省 stdio)
        let transport = s.transport.as_deref().unwrap_or("stdio");
        match transport {
            "stdio" => {
                if s.command.trim().is_empty() {
                    return Err(format!("server '{}' command must not be empty", s.name));
                }
            }
            "streamable_http" => {
                let url = s.url.as_deref().unwrap_or("").trim();
                if url.is_empty() {
                    return Err(format!(
                        "server '{}' transport=streamable_http requires url",
                        s.name
                    ));
                }
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err(format!(
                        "server '{}' url must start with http:// or https://",
                        s.name
                    ));
                }
            }
            other => {
                return Err(format!(
                    "server '{}' has unsupported transport '{}' (stdio | streamable_http)",
                    s.name, other
                ));
            }
        }
        if s.timeout_secs == 0 {
            return Err(format!("server '{}' timeout_secs must be > 0", s.name));
        }
    }
    Ok(())
}

/// 单 server 合并视图条目(配置面 + 运行态)
fn server_view(s: &McpServerConfig, snap: &HashMap<String, crate::mcp::McpServerEntry>) -> Value {
    let entry = snap.get(&s.name);
    let status = if s.disabled {
        "disabled".to_string()
    } else {
        match entry {
            Some(e) => e.status.as_str().to_string(),
            None => "unconnected".to_string(),
        }
    };
    let error = match entry {
        Some(e) => match &e.status {
            McpServerStatus::Failed { error } => Some(error.clone()),
            _ => None,
        },
        None => None,
    };
    let tools: Vec<Value> = match entry {
        Some(e) if e.status == McpServerStatus::Connected => e
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": crate::mcp::mcp_tool_name(&s.name, &t.name),
                    "server_tool": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                    "allowed": s.always_allow.contains(&t.name),
                })
            })
            .collect(),
        _ => Vec::new(),
    };
    json!({
        "name": s.name,
        "command": s.command,
        "args": s.args,
        "env_keys": s.env.keys().collect::<Vec<_>>(),
        "disabled": s.disabled,
        "timeout_secs": s.timeout_secs,
        "always_allow": s.always_allow,
        "transport": s.transport.as_deref().unwrap_or("stdio"),
        "url": s.url,
        "headers": s.headers,
        "status": status,
        "error": error,
        "tools": tools,
    })
}

/// 合并视图(配置 store.load() + 管理器 snapshot();GET 与 PUT 共用响应形)
fn merged_view(manager: &McpManager, store: &McpConfigStore) -> Value {
    let cfg = store.load();
    let snap: HashMap<String, crate::mcp::McpServerEntry> =
        manager.snapshot().into_iter().collect();
    let servers: Vec<Value> = cfg.servers.iter().map(|s| server_view(s, &snap)).collect();
    json!({
        "servers": servers,
        "config_path": store.path().display().to_string(),
    })
}

/// `GET /api/mcp/servers` 配置面 + 运行态合并视图
pub async fn list_servers(
    State(state): State<AgentApiState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(manager) = state.mcp() else {
        return Err(not_enabled());
    };
    let Some(store) = state.mcp_store() else {
        return Err(not_enabled());
    };
    Ok(Json(merged_view(manager, store)))
}

/// PUT 请求单 server 条目(与 [`McpServerConfig`] 同形,仅 env 可选)
///
/// env 缺省语义(工作台契约):GET 视图只回 `env_keys` 不回值(防泄露),
/// 前端永远拿不到 env 值——若 PUT 要求全量 env,前端任何写回都会误清密钥。
/// 故:省略 env 字段 = 保留该 server 现有 env 值;显式提供(含空对象)= 覆盖。
/// (pub 仅为满足 handler 签名可见性;字段全部私有,外部不可构造读取)
#[derive(serde::Deserialize)]
pub struct McpPutServer {
    name: String,
    /// 启动命令(仅 stdio 传输必填;streamable_http 条目省略=空串)
    #[serde(default)]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    #[serde(default)]
    disabled: bool,
    #[serde(default = "McpServerConfig::default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default)]
    always_allow: Vec<String>,
    /// 传输方式(B23):省略=stdio;streamable_http 需配合 url
    #[serde(default)]
    transport: Option<String>,
    /// streamable_http endpoint URL
    #[serde(default)]
    url: Option<String>,
    /// 附加请求头(值=${ENV_VAR} 引用形态,GET 视图原样回显引用不回明文)
    #[serde(default)]
    headers: Option<HashMap<String, String>>,
}

/// PUT 请求体(`{"servers":[...]}`;与 GET 视图的 servers 字段同位)
#[derive(serde::Deserialize)]
pub struct McpPutRequest {
    servers: Vec<McpPutServer>,
}

/// `PUT /api/mcp/servers` 校验 → 写回 mcp.toml → 全量重载(重连全部)
///
/// 重载语义:先全量断连(旧子进程一律 kill),再按新配置逐 server 重连;
/// disabled 跳过(不连接不注册,配置保留);单 server 失败隔离落 failed
/// 状态,不阻塞其余。env 省略条目按 name 对齐现有配置继承 env 值。
/// 响应 = 重载后的合并视图。
pub async fn put_servers(
    State(state): State<AgentApiState>,
    Json(body): Json<McpPutRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(manager) = state.mcp() else {
        return Err(not_enabled());
    };
    let Some(store) = state.mcp_store() else {
        return Err(not_enabled());
    };

    // env 保留语义:按 name 对齐现有配置,省略 env 字段的条目继承现有值
    let existing: HashMap<String, McpServerConfig> = store
        .load()
        .servers
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();
    let servers = body
        .servers
        .into_iter()
        .map(|s| {
            let env = s.env.unwrap_or_else(|| {
                existing
                    .get(&s.name)
                    .map(|e| e.env.clone())
                    .unwrap_or_default()
            });
            McpServerConfig {
                name: s.name,
                command: s.command,
                args: s.args,
                env,
                disabled: s.disabled,
                timeout_secs: s.timeout_secs,
                always_allow: s.always_allow,
                transport: s.transport,
                url: s.url,
                headers: s.headers,
            }
        })
        .collect();
    let cfg = McpConfig { servers };

    validate(&cfg).map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))))?;

    // 1. 写回配置(原子写;mcp.toml 从此为单一事实源,回落态终止)
    store.save(&cfg).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
    })?;

    // 2. 全量断连(async 段,不持 toolkit 锁)
    manager.shutdown().await;

    // 3. 逐 enabled server 重连(async 段,失败隔离继续)
    let mut connections: Vec<(
        McpServerConfig,
        std::sync::Arc<crate::mcp::McpClient>,
        Vec<crate::mcp::McpToolSpec>,
    )> = Vec::new();
    let mut connect_errors: Vec<(String, String)> = Vec::new();
    for s in cfg.servers.iter().filter(|s| !s.disabled) {
        match manager.reconnect(s).await {
            Ok((client, tools)) => connections.push((s.clone(), client, tools)),
            Err(e) => connect_errors.push((s.name.clone(), e)),
        }
    }

    // 4. 同步段:持写锁注册全部连接产物(临界区内无 await)
    let mut registered_servers = 0usize;
    let mut registered_tools = 0usize;
    {
        let mut toolkit = state.toolkit_lock().write().expect("toolkit lock poisoned");
        for (s, client, tools) in &connections {
            match register_client_tools(
                &mut toolkit,
                &s.name,
                client,
                tools,
                &s.always_allow,
                s.timeout_secs,
            ) {
                Ok(n) => {
                    registered_servers += 1;
                    registered_tools += n;
                }
                Err(e) => {
                    tracing::warn!(server = %s.name, error = %e, "MCP tool registration failed on reload");
                    manager.mark_failed(&s.name, &e);
                }
            }
        }
    }
    info!(
        servers = registered_servers,
        tools = registered_tools,
        failed = connect_errors.len(),
        "MCP servers reloaded via workbench PUT"
    );

    Ok(Json(merged_view(manager, store)))
}

/// `POST /api/mcp/servers/{name}/restart` 断连重连重注册单 server
pub async fn restart_server(
    State(state): State<AgentApiState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(manager) = state.mcp() else {
        return Err(not_enabled());
    };
    let Some(store) = state.mcp_store() else {
        return Err(not_enabled());
    };
    // 配置里必须存在该 server(重启以持久化配置为准,不认运行态幽灵)
    let cfg = store.load();
    let Some(s) = cfg.servers.iter().find(|s| s.name == name) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("server '{name}' not found in mcp config") })),
        ));
    };

    // async 段:断连重连(失败已落 failed 状态)
    let (client, tools) = manager.reconnect(s).await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("MCP server '{}' reconnect failed: {}", s.name, e) })),
        )
    })?;

    // 同步段:持写锁重注册
    let count = {
        let mut toolkit = state.toolkit_lock().write().expect("toolkit lock poisoned");
        register_client_tools(
            &mut toolkit,
            &s.name,
            &client,
            &tools,
            &s.always_allow,
            s.timeout_secs,
        )
    };
    let count = match count {
        Ok(n) => n,
        Err(e) => {
            manager.mark_failed(&s.name, &e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({ "error": format!("MCP server '{}' tool registration failed: {}", s.name, e) }),
                ),
            ));
        }
    };
    info!(server = %s.name, tools = count, "MCP server restarted via workbench");
    Ok(Json(json!({
        "name": s.name,
        "status": "connected",
        "tools_registered": count,
    })))
}

/// `GET /api/mcp/status` 聚合徽标(总/连/败/停 + 工具总数)
pub async fn status_brief(
    State(state): State<AgentApiState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(manager) = state.mcp() else {
        return Err(not_enabled());
    };
    let Some(store) = state.mcp_store() else {
        return Err(not_enabled());
    };
    let cfg = store.load();
    let snap = manager.snapshot();
    let connected = snap
        .iter()
        .filter(|(_, e)| e.status == McpServerStatus::Connected)
        .count();
    let failed = snap
        .iter()
        .filter(|(_, e)| matches!(e.status, McpServerStatus::Failed { .. }))
        .count();
    let disabled = cfg.servers.iter().filter(|s| s.disabled).count();
    let tools: usize = snap
        .iter()
        .filter(|(_, e)| e.status == McpServerStatus::Connected)
        .map(|(_, e)| e.tools.len())
        .sum();
    Ok(Json(json!({
        "total": cfg.servers.len(),
        "connected": connected,
        "failed": failed,
        "disabled": disabled,
        "tools": tools,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::evorule_client::EvoruleApiClient;
    use crate::api::metrics::Metrics;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn test_validate_transport_branches() {
        let mk = |transport: Option<&str>, url: Option<&str>, command: &str| McpServerConfig {
            name: "t".to_string(),
            command: command.to_string(),
            args: vec![],
            env: std::collections::HashMap::new(),
            disabled: false,
            timeout_secs: 60,
            always_allow: vec![],
            transport: transport.map(|s| s.to_string()),
            url: url.map(|s| s.to_string()),
            headers: None,
        };
        let one = |s: McpServerConfig| McpConfig { servers: vec![s] };
        // 缺省 stdio:command 必填
        assert!(validate(&one(mk(None, None, ""))).is_err());
        assert!(validate(&one(mk(None, None, "npx"))).is_ok());
        // streamable_http:url 必填且需 http(s) 前缀
        assert!(validate(&one(mk(Some("streamable_http"), None, ""))).is_err());
        assert!(validate(&one(mk(Some("streamable_http"), Some("ftp://x"), ""))).is_err());
        assert!(validate(&one(mk(
            Some("streamable_http"),
            Some("http://127.0.0.1:9/mcp"),
            ""
        )))
        .is_ok());
        // 非法 transport 值
        assert!(validate(&one(mk(Some("websocket"), None, "npx"))).is_err());
    }

    /// 构造注入 MCP 管理器 + 配置存储的测试 state(空 toolkit)
    fn mcp_state(dir: &std::path::Path) -> AgentApiState {
        let manager = McpManager::new();
        let store = McpConfigStore::new(dir, McpConfig::default());
        AgentApiState::new_with_metrics(
            crate::agent::AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new("http://localhost:0"),
            std::sync::Arc::new(Metrics::new().unwrap()),
            dir.to_path_buf(),
            std::sync::Arc::new(crate::api::workspace_client::WorkspaceApiClient::new(
                "http://localhost:0",
            )),
            std::sync::Arc::new(std::sync::RwLock::new(
                crate::io_handlers::tool_handler::ToolHandler::new(),
            )),
        )
        .with_mcp(std::sync::Arc::new(manager), std::sync::Arc::new(store))
    }

    fn app(state: AgentApiState) -> axum::Router {
        crate::api::agent_api::router(state)
    }

    async fn json_request(
        app: axum::Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = match body {
            Some(v) => Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        };
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    #[tokio::test]
    async fn test_mcp_disabled_returns_503() {
        let tmp = tempfile::tempdir().unwrap();
        // 不注入 with_mcp:未启用态
        let state = AgentApiState::new_with_metrics(
            crate::agent::AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new("http://localhost:0"),
            std::sync::Arc::new(Metrics::new().unwrap()),
            tmp.path().to_path_buf(),
            std::sync::Arc::new(crate::api::workspace_client::WorkspaceApiClient::new(
                "http://localhost:0",
            )),
            std::sync::Arc::new(std::sync::RwLock::new(
                crate::io_handlers::tool_handler::ToolHandler::new(),
            )),
        );
        let a = app(state);
        for (method, uri, body) in [
            ("GET", "/api/mcp/servers", None),
            ("PUT", "/api/mcp/servers", Some(json!({"servers": []}))),
            ("POST", "/api/mcp/servers/x/restart", None),
            ("GET", "/api/mcp/status", None),
        ] {
            let (status, body) = json_request(a.clone(), method, uri, body).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method} {uri}");
            assert_eq!(body["error"], "MCP surface not enabled");
        }
    }

    #[tokio::test]
    async fn test_list_servers_merges_config_and_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let state = mcp_state(tmp.path());
        // 预置运行态:向 manager 造一条 failed 记录(不真连接)
        let mgr = state.mcp().unwrap().clone();
        mgr.mark_failed("broken", "spawn failed: no such command");
        let a = app(state);

        // PUT 写入:disabled + broken 两个 server(全 disabled/broken 避免真 spawn)
        let body = json!({"servers": [
            {"name": "off", "command": "echo", "disabled": true},
            {"name": "broken", "command": "echo"}
        ]});
        let (status, _) = json_request(a.clone(), "PUT", "/api/mcp/servers", Some(body)).await;
        assert_eq!(status, StatusCode::OK);

        let (status, v) = json_request(a, "GET", "/api/mcp/servers", None).await;
        assert_eq!(status, StatusCode::OK);
        let servers = v["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 2);
        let off = servers.iter().find(|s| s["name"] == "off").unwrap();
        assert_eq!(
            off["status"], "disabled",
            "disabled server 不落运行态,视图标 disabled"
        );
        let broken = servers.iter().find(|s| s["name"] == "broken").unwrap();
        assert_eq!(
            broken["status"], "failed",
            "manager 里的 failed 记录应合并进视图"
        );
        assert!(
            !broken["error"].as_str().unwrap_or("").is_empty(),
            "failed 态应带错误描述"
        );
        // 配置写回落盘
        assert!(tmp.path().join(".evo").join("mcp.toml").exists());
    }

    #[tokio::test]
    async fn test_put_streamable_http_view_persistence_and_reject() {
        let tmp = tempfile::tempdir().unwrap();
        let a = app(mcp_state(tmp.path()));
        // PUT:stdio 缺省 + streamable_http(均 disabled 避免真连接)
        let body = json!({"servers": [
            {"name": "local", "command": "npx", "disabled": true},
            {"name": "remote", "transport": "streamable_http",
             "url": "http://127.0.0.1:9/mcp", "disabled": true,
             "headers": {"Authorization": "Bearer ${B23_TOK}"}}
        ]});
        let (status, _) = json_request(a.clone(), "PUT", "/api/mcp/servers", Some(body)).await;
        assert_eq!(status, StatusCode::OK);

        // GET 视图透出 transport/url/headers(stdio 条目缺省 "stdio")
        let (status, v) = json_request(a, "GET", "/api/mcp/servers", None).await;
        assert_eq!(status, StatusCode::OK);
        let servers = v["servers"].as_array().unwrap();
        let local = servers.iter().find(|s| s["name"] == "local").unwrap();
        assert_eq!(local["transport"], "stdio");
        let remote = servers.iter().find(|s| s["name"] == "remote").unwrap();
        assert_eq!(remote["transport"], "streamable_http");
        assert_eq!(remote["url"], "http://127.0.0.1:9/mcp");
        assert_eq!(remote["headers"]["Authorization"], "Bearer ${B23_TOK}");

        // mcp.toml 落盘含新字段,重启装载形态一致(serde 回读)
        let text = std::fs::read_to_string(tmp.path().join(".evo").join("mcp.toml")).unwrap();
        assert!(text.contains("streamable_http"));
        let cfg = crate::mcp::McpConfigStore::new(tmp.path(), Default::default()).load();
        let remote = cfg.servers.iter().find(|s| s.name == "remote").unwrap();
        assert_eq!(remote.transport.as_deref(), Some("streamable_http"));
        assert_eq!(remote.url.as_deref(), Some("http://127.0.0.1:9/mcp"));
        // stdio 条目 command 落盘照旧
        let local = cfg.servers.iter().find(|s| s.name == "local").unwrap();
        assert_eq!(local.command, "npx");
    }

    #[tokio::test]
    async fn test_put_servers_validation_rejects() {
        let tmp = tempfile::tempdir().unwrap();
        let a = app(mcp_state(tmp.path()));
        let cases: Vec<(Value, &str)> = vec![
            (
                json!({"servers": [{"name": "", "command": "echo"}]}),
                "empty name",
            ),
            (
                json!({"servers": [{"name": "a", "command": "echo"}, {"name": "a", "command": "echo"}]}),
                "duplicate name",
            ),
            (
                json!({"servers": [{"name": "a", "command": " "}]}),
                "empty command",
            ),
            (
                json!({"servers": [{"name": "a", "command": "echo", "timeout_secs": 0}]}),
                "zero timeout",
            ),
            (
                json!({"servers": [{"name": "bad name!", "command": "echo"}]}),
                "unsafe name",
            ),
            (
                json!({"servers": [{"name": "a", "transport": "streamable_http"}]}),
                "http transport without url",
            ),
            (
                json!({"servers": [{"name": "a", "command": "npx", "transport": "websocket"}]}),
                "unsupported transport",
            ),
        ];
        for (body, why) in cases {
            let (status, v) = json_request(a.clone(), "PUT", "/api/mcp/servers", Some(body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{why}");
            assert!(v["error"].is_string(), "{why} 应带 error 文案");
        }
    }

    #[tokio::test]
    async fn test_put_servers_persists_toml_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let a = app(mcp_state(tmp.path()));
        let body = json!({"servers": [
            {"name": "fs", "command": "npx", "args": ["-y", "server-filesystem"],
             "disabled": true, "timeout_secs": 30, "always_allow": ["read_file"]}
        ]});
        let (status, _) = json_request(a.clone(), "PUT", "/api/mcp/servers", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        // mcp.toml 落盘且可回读
        let text = std::fs::read_to_string(tmp.path().join(".evo").join("mcp.toml")).unwrap();
        assert!(text.contains("[[mcp.servers]]"));
        let (_, v) = json_request(a, "GET", "/api/mcp/servers", None).await;
        let fs = v["servers"].as_array().unwrap()[0].clone();
        assert_eq!(fs["name"], "fs");
        assert_eq!(fs["disabled"], true);
        assert_eq!(fs["timeout_secs"], 30);
        assert_eq!(fs["always_allow"], json!(["read_file"]));
    }

    #[tokio::test]
    async fn test_put_servers_env_omitted_preserves_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let a = app(mcp_state(tmp.path()));
        // 初次 PUT:显式 env 写入
        let body =
            json!({"servers": [{"name": "fs", "command": "npx", "env": {"API_KEY": "secret"}}]});
        let (status, _) = json_request(a.clone(), "PUT", "/api/mcp/servers", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        // 二次 PUT:省略 env → 保留现有值(前端拿不到值,启停/勾白名单不得误清)
        let body = json!({"servers": [{"name": "fs", "command": "npx", "disabled": true}]});
        let (status, _) = json_request(a.clone(), "PUT", "/api/mcp/servers", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        let text = std::fs::read_to_string(tmp.path().join(".evo").join("mcp.toml")).unwrap();
        assert!(text.contains("secret"), "省略 env 字段应保留现有 env 值");
        // 三次 PUT:显式空 env → 覆盖清空
        let body = json!({"servers": [{"name": "fs", "command": "npx", "env": {}}]});
        let (status, _) = json_request(a.clone(), "PUT", "/api/mcp/servers", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        let text = std::fs::read_to_string(tmp.path().join(".evo").join("mcp.toml")).unwrap();
        assert!(!text.contains("secret"), "显式空 env 应覆盖清空");
        // 改名不继承:新 name 无对齐条目,env 落空
        let body = json!({"servers": [{"name": "fs2", "command": "npx", "disabled": true}]});
        let (status, _) = json_request(a.clone(), "PUT", "/api/mcp/servers", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        let text = std::fs::read_to_string(tmp.path().join(".evo").join("mcp.toml")).unwrap();
        assert!(
            text.contains("fs2") && !text.contains("secret"),
            "新 name 不继承旧 server env"
        );
    }

    #[tokio::test]
    async fn test_restart_unknown_server_404() {
        let tmp = tempfile::tempdir().unwrap();
        let a = app(mcp_state(tmp.path()));
        let (status, v) = json_request(a, "POST", "/api/mcp/servers/nope/restart", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(v["error"].as_str().unwrap().contains("nope"));
    }

    #[tokio::test]
    async fn test_status_brief_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let a = app(mcp_state(tmp.path()));
        let body = json!({"servers": [
            {"name": "off", "command": "echo", "disabled": true}
        ]});
        let _ = json_request(a.clone(), "PUT", "/api/mcp/servers", Some(body)).await;
        let (status, v) = json_request(a, "GET", "/api/mcp/status", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["total"], 1);
        assert_eq!(v["connected"], 0);
        assert_eq!(v["failed"], 0);
        assert_eq!(v["disabled"], 1);
        assert_eq!(v["tools"], 0);
    }
}
