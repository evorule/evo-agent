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
//!   → 包装成 Value::string 返回给 runner
//! ```

use std::sync::Arc;

use serde_json::Value;

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

/// 单次工具结果上限(16 号档 §3.3 治理接合表:call_tool 结果 >100KB 截断+提示)
const MAX_RESULT_BYTES: usize = 100 * 1024;

/// 审批卡命令摘要上限(bytes;args 序列化截断,防巨型参数撑爆审批卡)
const PROPOSAL_SUMMARY_BYTES: usize = 2048;

/// 把单个 MCP 工具适配为 evo-agent 的 [`ToolFunction`]
///
/// 多个 adapter 共享同一个 `Arc<McpClient>`(同一 server 的所有工具共用一条传输层)。
#[derive(Clone)]
pub struct McpToolAdapter {
    client: Arc<McpClient>,
    /// MCP server 内的工具名(不含 `mcp_{server}_` 前缀)
    tool_name: String,
    /// 注册名 `mcp_{server}_{tool}`(审批卡 command 展示用;B16 G8 接合)
    registered_name: String,
    /// 白名单直跑(B16 决策点 3:`mcp.servers[].always_allow` 名单内 = active,
    /// 免审批;缺省 false = candidate 全审批,经 [`ToolFunction::evaluate_proposal`]
    /// 出 needs_approval 提案)
    active: bool,
    /// 单次调用超时秒数(B16;0 = 不限时;缺省 60)
    timeout_secs: u64,
}

impl std::fmt::Debug for McpToolAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolAdapter")
            .field("tool_name", &self.tool_name)
            .field("registered_name", &self.registered_name)
            .field("active", &self.active)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

impl McpToolAdapter {
    /// 创建一个 MCP 工具适配器(缺省 60s 调用超时;candidate 审批级)
    ///
    /// `client` 是已连接的 MCP client(多个 adapter 共享)。
    /// `tool_name` 是 MCP server 暴露的工具名(不含前缀)。
    pub fn new(client: Arc<McpClient>, tool_name: String) -> Self {
        Self {
            client,
            registered_name: tool_name.clone(),
            tool_name,
            active: false,
            timeout_secs: 60,
        }
    }

    /// 创建带治理元数据的适配器(B16 G8 接合;`mcp.servers[].timeout_secs` 覆盖)
    ///
    /// `registered_name` 是注册名 `mcp_{server}_{tool}`(审批卡展示);
    /// `active` = 该工具在 `always_allow` 白名单内(直跑,免审批)。
    pub fn with_governance(
        client: Arc<McpClient>,
        tool_name: String,
        registered_name: String,
        active: bool,
        timeout_secs: u64,
    ) -> Self {
        Self {
            client,
            registered_name,
            tool_name,
            active,
            timeout_secs,
        }
    }

    /// 创建带显式调用超时的适配器(B16:`mcp.servers[].timeout_secs` 覆盖;
    /// candidate 审批级,治理字段取缺省——单测/临时装配用)
    pub fn with_timeout(client: Arc<McpClient>, tool_name: String, timeout_secs: u64) -> Self {
        Self {
            client,
            registered_name: tool_name.clone(),
            tool_name,
            active: false,
            timeout_secs,
        }
    }

    /// 只读访问工具名(MCP server 内的原始名)
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }
}

#[async_trait::async_trait]
impl ToolFunction for McpToolAdapter {
    async fn call(&self, args: &Value) -> IoResult {
        // Value → serde_json::Value(MCP 协议用 serde_json)
        let args_json = args.clone();

        tracing::debug!(
            tool = %self.tool_name,
            args = %args_json,
            "MCP tool adapter invoked"
        );

        let call = self.client.call_tool(&self.tool_name, &args_json);
        let result = if self.timeout_secs == 0 {
            call.await
        } else {
            match tokio::time::timeout(std::time::Duration::from_secs(self.timeout_secs), call)
                .await
            {
                Ok(r) => r,
                Err(_) => Err(format!(
                    "MCP tool '{}' timed out after {}s",
                    self.tool_name, self.timeout_secs
                )),
            }
        };

        let result = result.map_err(|e| format!("MCP tool '{}' failed: {}", self.tool_name, e))?;

        // 结果上限(16 号档 §3.3):>100KB 截断+提示——LLM 上下文保护,原始
        // 字节数留痕;按 UTF-8 字符边界回退避免截断撕裂多字节字符
        let mut text = result;
        if text.len() > MAX_RESULT_BYTES {
            let original = text.len();
            let mut cut = MAX_RESULT_BYTES;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str(&format!(
                "\n\n[truncated: original result was {original} bytes, \
                 showing first {cut} bytes (MCP tool result limit 100KB)]"
            ));
        }

        // MCP 返回的是 text 字符串,包装成 Value::string
        Ok(Value::from(text))
    }

    /// 管道⑤评估钩子(B16 G8 接合,16 号档 §3.3/决策点 3):MCP 工具缺省
    /// candidate 全审批——非白名单调用返回 needs_approval 提案交 G8 决策端;
    /// `always_allow` 白名单内(active)返回 None 直通执行。
    /// 提案 command = 注册名 + args 摘要(截断防巨型参数撑爆审批卡)。
    fn evaluate_proposal(&self, args: &Value) -> Option<Value> {
        if self.active {
            return None;
        }
        let args_str = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
        let summary = if args_str.len() > PROPOSAL_SUMMARY_BYTES {
            let mut cut = PROPOSAL_SUMMARY_BYTES;
            while cut > 0 && !args_str.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}…(args truncated)", &args_str[..cut])
        } else {
            args_str
        };
        Some(serde_json::json!({
            "status": "needs_approval",
            "command": format!("{} {}", self.registered_name, summary),
            "risk": "external MCP server tool invocation (out-of-process remote \
                     side effects, not covered by host sandbox guarantees)",
            "alternative": "",
            "instructions": "Ask the user. The request pauses for approval and executes once the user approves.",
        }))
    }
}

/// 把一个已连接 MCP client 的工具列表注册进 ToolHandler(B16 抽出共用)
///
/// CLI(`register_mcp_tools`)与 serve(`mcp::manager`)两条路径共用:
/// 逐工具 `mcp_{server}_{tool}` 命名 → manifest 契约校验(description/
/// inputSchema 透传,一表硬规则 3)→ `McpToolAdapter` 注册(带超时覆盖)。
///
/// 治理接合(B16 G8,16 号档 §3.3/决策点 3):manifest 缺省 ManualDefault
/// (dynamic_manifest Mcp 分支);`always_allow` 名单内工具覆写 AutoPolicy
/// (active 直跑=用户显式授权)并给 adapter 打 active 标——评估钩子与
/// manifest 策略同源,每次调用按名单判定,不依赖注册次序。
pub fn register_client_tools(
    tool_handler: &mut crate::io_handlers::tool_handler::ToolHandler,
    server_name: &str,
    client: &Arc<McpClient>,
    tools: &[crate::mcp::client::McpToolSpec],
    always_allow: &[String],
    timeout_secs: u64,
) -> Result<usize, String> {
    for tool in tools {
        let registered_name = mcp_tool_name(server_name, &tool.name);
        let active = always_allow.contains(&tool.name);
        tracing::info!(
            server = %server_name,
            mcp_tool = %tool.name,
            registered_as = %registered_name,
            active,
            "registering MCP tool"
        );
        // 一表硬规则 3:MCP 握手后 tools/list 的 description/inputSchema 透传
        // 进 manifest(Inline 契约)——丢字段即注册失败(设计档 §3.1,消灭
        // MCP 空 schema 降级)
        let mut manifest = crate::agent::tool_manifest::dynamic_manifest(
            &registered_name,
            crate::agent::tool_manifest::ToolSource::Mcp,
            tool.description.clone(),
            tool.input_schema.clone(),
        )
        .map_err(|e| {
            tracing::warn!(
                server = %server_name,
                mcp_tool = %tool.name,
                error = %e,
                "MCP tool registration rejected by manifest contract"
            );
            e
        })?;
        // 白名单直跑(B16 决策点 3):manifest 策略与 adapter 评估钩子同源覆写
        if active {
            manifest.approval_policy = crate::agent::tool_manifest::ApprovalPolicy::AutoPolicy;
        }
        tool_handler.register(
            manifest,
            Arc::new(McpToolAdapter::with_governance(
                client.clone(),
                tool.name.clone(),
                registered_name,
                active,
                timeout_secs,
            )),
        );
    }
    Ok(tools.len())
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
    register_client_tools(
        tool_handler,
        &server_cfg.name,
        &client,
        &tools,
        &server_cfg.always_allow,
        server_cfg.timeout_secs,
    )
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
            let mut m = serde_json::Map::new();
            m.insert("path".to_string(), Value::from("/tmp/x"));
            Value::Object(m)
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
            let mut path_val = serde_json::Map::new();
            path_val.insert("path".to_string(), Value::from("/a/b"));
            path_val.insert(
                "options".to_string(),
                Value::Object({
                    let mut m = serde_json::Map::new();
                    m.insert("overwrite".to_string(), Value::Bool(true));
                    m
                }),
            );
            Value::Object(path_val)
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

        let result = adapter.call(&Value::Null).await;
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
        let result = adapter.call(&Value::Null).await;
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
        let result = adapter.call(&Value::Null).await.unwrap();
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
                disabled: false,
                timeout_secs: 60,
                always_allow: vec![],
                transport: None,
                url: None,
                headers: None,
            }],
        };
        let count = register_mcp_tools(&mut handler, &config).await;
        assert_eq!(count, 0);
        // 没有工具被注册
        assert!(!handler.has_tool("mcp_broken_anything"));
    }

    // ===== B16 G8 接合:审批钩子 + 白名单 + 结果截断 =====

    fn spec(name: &str) -> crate::mcp::client::McpToolSpec {
        crate::mcp::client::McpToolSpec {
            name: name.to_string(),
            description: format!("{name} test tool"),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    #[tokio::test]
    async fn test_register_client_tools_default_candidate_policy() {
        // 缺省全审批(16 号档 §3.3 决策点 3):manifest=ManualDefault,
        // 评估钩子对任意调用返回 needs_approval 提案
        let (client, _transport) = make_client_with_tool_response().await;
        let mut handler = crate::io_handlers::tool_handler::ToolHandler::new();
        register_client_tools(
            &mut handler,
            "fs",
            &client,
            &[spec("read_file"), spec("write_file")],
            &[],
            60,
        )
        .unwrap();
        let m = handler.manifest("mcp_fs_read_file").unwrap();
        assert_eq!(
            m.approval_policy,
            crate::agent::tool_manifest::ApprovalPolicy::ManualDefault
        );
        let proposal = handler
            .evaluate_proposal_for("mcp_fs_read_file", &serde_json::json!({"path": "/tmp/x"}))
            .expect("default candidate must produce a proposal");
        assert_eq!(proposal["status"], "needs_approval");
        let command = proposal["command"].as_str().unwrap();
        assert!(
            command.starts_with("mcp_fs_read_file "),
            "proposal command must carry the registered name, got: {command}"
        );
        assert!(
            command.contains("/tmp/x"),
            "proposal command must carry args summary"
        );
        assert!(proposal["risk"].as_str().unwrap().contains("MCP"));
    }

    #[tokio::test]
    async fn test_register_client_tools_whitelist_active_direct_run() {
        // 白名单直跑(决策点 3):always_allow 名单内 manifest=AutoPolicy +
        // 评估钩子返回 None(免审批);名单外保持 candidate
        let (client, _transport) = make_client_with_tool_response().await;
        let mut handler = crate::io_handlers::tool_handler::ToolHandler::new();
        register_client_tools(
            &mut handler,
            "fs",
            &client,
            &[spec("read_file"), spec("write_file")],
            &["read_file".to_string()],
            60,
        )
        .unwrap();
        let m = handler.manifest("mcp_fs_read_file").unwrap();
        assert_eq!(
            m.approval_policy,
            crate::agent::tool_manifest::ApprovalPolicy::AutoPolicy
        );
        assert!(
            handler
                .evaluate_proposal_for("mcp_fs_read_file", &serde_json::json!({}))
                .is_none(),
            "whitelisted tool must run directly (no proposal)"
        );
        // 名单外仍是 candidate
        let m2 = handler.manifest("mcp_fs_write_file").unwrap();
        assert_eq!(
            m2.approval_policy,
            crate::agent::tool_manifest::ApprovalPolicy::ManualDefault
        );
        assert!(handler
            .evaluate_proposal_for("mcp_fs_write_file", &serde_json::json!({}))
            .is_some());
    }

    #[tokio::test]
    async fn test_evaluate_proposal_args_summary_truncated() {
        // 巨型参数:审批卡 command 摘要截断(2KB 上限,防撑爆审批卡)
        let (client, _transport) = make_client_with_tool_response().await;
        let adapter = McpToolAdapter::new(client, "read_file".to_string());
        let big = "x".repeat(10_000);
        let proposal = adapter
            .evaluate_proposal(&serde_json::json!({"data": big}))
            .unwrap();
        let command = proposal["command"].as_str().unwrap();
        assert!(command.contains("(args truncated)"));
        assert!(command.len() < PROPOSAL_SUMMARY_BYTES + 200);
    }

    #[tokio::test]
    async fn test_adapter_call_truncates_over_100kb() {
        // 结果上限(16 号档 §3.3):>100KB 截断+提示,提示含原始字节数
        let (client, transport) = make_client_with_tool_response().await;
        let big = "y".repeat(150 * 1024);
        transport
            .set_response(
                "tools/call",
                serde_json::json!({"content": [{"type": "text", "text": big}]}),
            )
            .await;
        let adapter = McpToolAdapter::new(client, "dump".to_string());
        let result = adapter.call(&Value::Null).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("[truncated: original result was 153600 bytes"));
        assert!(text.contains("100KB"));
        // 截断后总长 = 保留字节 + 提示(远小于原始 150KB)
        assert!(text.len() < 110 * 1024, "truncated result must be capped");
    }

    #[tokio::test]
    async fn test_adapter_call_within_limit_untouched() {
        // 上限内结果原样返回(零改动语义)
        let (client, transport) = make_client_with_tool_response().await;
        transport
            .set_response(
                "tools/call",
                serde_json::json!({"content": [{"type": "text", "text": "small"}]}),
            )
            .await;
        let adapter = McpToolAdapter::new(client, "read_file".to_string());
        let result = adapter.call(&Value::Null).await.unwrap();
        assert_eq!(result.as_str(), Some("small"));
    }
}
