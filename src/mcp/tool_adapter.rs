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

/// 单次工具结果上限(治理接合表:call_tool 结果 >100KB 截断+提示)
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

    /// 终止底层传输(stdio=kill 子进程树;http=断开+drain pending)
    ///
    /// 执行超时契约的终止半边:超时不再是「放弃等待」,server 进程被杀、
    /// 其余在飞请求收到 transport-closed 真实结局。
    fn terminate_transport(&self) {
        let client = self.client.clone();
        tokio::spawn(async move {
            let _ = client.close().await;
        });
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

        let call = self
            .client
            .call_tool_structured(&self.tool_name, &args_json);
        let structured = if self.timeout_secs == 0 {
            call.await
        } else {
            match tokio::time::timeout(std::time::Duration::from_secs(self.timeout_secs), call)
                .await
            {
                Ok(r) => r,
                Err(_) => {
                    // 超时=终止并记账(非放弃):断开传输(stdio=kill 子进程树,
                    // 全部 pending 请求回传 transport-closed),server 不再带着
                    // 未完成的副作用继续跑
                    self.terminate_transport();
                    Err(format!(
                        "MCP tool '{}' timed out after {}s (server transport closed)",
                        self.tool_name, self.timeout_secs
                    ))
                }
            }
        };

        let structured =
            structured.map_err(|e| format!("MCP tool '{}' failed: {}", self.tool_name, e))?;

        // 结果上限:>100KB 截断+提示——LLM 上下文保护,原始字节数留痕;
        // 按 UTF-8 字符边界回退避免截断撕裂多字节字符
        // (G3:截断只作用 text 部分;图片不走文本截断,由 runner 侧 B1
        // 预算校验统一管——单图/单结果上限,超限丢弃计数 images_omitted)
        let mut text = structured.text;

        // 2025-06-18 additive 面：structuredContent/内嵌文本资源拼入模型可见文本
        // （旧版本 server 三者皆空 → 拼接零字节，存量语义逐字节不变）。
        // 链接只列 uri 不自动展开——取数由模型按需决策（防上下文自膨胀）。
        if let Some(sv) = &structured.structured {
            text.push_str(
                "

[structured]
",
            );
            text.push_str(&sv.to_string());
        }
        for rt in &structured.resource_texts {
            text.push_str(&format!(
                "

[resource {}]
{}",
                rt.uri, rt.text
            ));
        }
        if !structured.resource_links.is_empty() {
            text.push_str(&format!(
                "

[resource_links]
{}",
                structured.resource_links.join(
                    "
"
                )
            ));
        }

        // 单次合并截断（text+新面合计过限才截；提示文案保持存量口径）
        if text.len() > MAX_RESULT_BYTES {
            let original = text.len();
            let mut cut = MAX_RESULT_BYTES;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str(&format!(
                "

[truncated: original result was {original} bytes,                  showing first {cut} bytes (MCP tool result limit 100KB)]"
            ));
        }

        // G3 双形态:无图维持纯 string(存量 MCP server 逐字节零变化);
        // 有图组装 B1 提取协议对象(images 旁路)——下游 runner 统一提取点
        // 自动接住(校验/指纹/落盘/wire 组装/续话还原全复用,零新增管道)
        if structured.images.is_empty() {
            return Ok(Value::from(text));
        }
        let images_json: Vec<Value> = structured
            .images
            .iter()
            .map(|img| {
                serde_json::json!({
                    "mime_type": img.mime_type,
                    "data_base64": img.data_base64,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "text": text,
            "images": images_json,
        }))
    }

    /// 执行超时钩子(执行器生命周期契约):终止在飞 server(缺省 no-op 的
    /// 未实现方此前让执行超时退化为「放弃等待」——远端副作用可在超时报错
    /// 后继续完成)。close 对 stdio 传输 kill 子进程树并对全部 pending 请求
    /// 回传真实结局,[`Self::call`] 的在飞 future 随即收 transport-closed。
    fn on_execution_timeout(&self) {
        self.terminate_transport();
    }

    /// 管道⑤评估钩子(G8 接合):MCP 工具缺省
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

/// MCP 标识符(server 名/tool 名)注册边界校验
///
/// 字符集 `[A-Za-z0-9_-]`、非空(与工作台 PUT 面校验同语义镜像);tool 名
/// 另加 64 字节上限——tools/list 透传的 tool 名是**远端(可能不可信)内容
/// 直达 LLM 契约面/审批卡/日志的名字空间**,无边界即注入面,长度上限防
/// 组合注册名膨胀。server 名不加长度上限(与 PUT 面口径一致,避免
/// PUT-合法配置在注册面被二次拒绝)。
pub fn validate_mcp_identifier(kind: &str, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("MCP {kind} name must not be empty"));
    }
    if kind == "tool" && name.len() > 64 {
        return Err(format!(
            "MCP tool name '{}' exceeds the 64-byte limit ({} bytes); \
             shorten the tool name on the server",
            name,
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "MCP {kind} name '{}' contains characters outside [a-zA-Z0-9_-]; \
             rename it (the name lands verbatim in the LLM contract and approval cards)",
            name
        ));
    }
    Ok(())
}

/// 把一个已连接 MCP client 的工具列表注册进 ToolHandler(B16 抽出共用)
///
/// CLI(`register_mcp_tools`)与 serve(`mcp::manager`)两条路径共用。
/// **两阶段注册(all-or-nothing)**:阶段一对全部工具做身份边界校验 +
/// manifest 契约校验 + 名称碰撞检查(零 mutation),任一被拒 = 整 server
/// 拒绝注册——注册循环中途契约拒绝不再残留前 k-1 工具(状态面 failed
/// 与实际可用面一致);阶段二统一注册(走 [`ToolHandler::register_guarded`]
/// 来源防覆盖防线,阶段一已查碰撞,此处 Err 即防线执法可见)。
///
/// 名称碰撞防线:`mcp_{server}_{tool}` 组合可撞(server a + tool b_c 与
/// server a_b + tool c 同产 `mcp_a_b_c`),同名覆盖会静默顶替既有工具
/// (换执行端点/借 always_allow 名单静默升档)——先注册者胜,后注册者
/// 显式失败;重启/重载路径由 manager 先清旧名再重注册(见
/// [`super::manager::McpManager::restart`])。
///
/// 治理接合(G8):manifest 缺省 ManualDefault
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
    // ---- 阶段一:全量校验(零 mutation;任一被拒 = 整 server 拒绝) ----
    validate_mcp_identifier("server", server_name)?;
    let mut prepared: Vec<(
        crate::agent::tool_manifest::ToolManifest,
        Arc<dyn crate::io_handlers::tool_handler::ToolFunction>,
    )> = Vec::with_capacity(tools.len());
    let mut names_in_batch = std::collections::HashSet::new();
    for tool in tools {
        validate_mcp_identifier("tool", &tool.name)?;
        let registered_name = mcp_tool_name(server_name, &tool.name);
        if !names_in_batch.insert(registered_name.clone()) {
            return Err(format!(
                "MCP server '{server_name}' exposes duplicate tool name '{}' \
                 (registered as '{registered_name}')",
                tool.name
            ));
        }
        if let Some(existing) = tool_handler.manifest(&registered_name) {
            let existing_source = format!("{:?}", existing.source);
            return Err(format!(
                "MCP tool name '{registered_name}' collides with an already-registered \
                 '{existing_source}' tool (server '{server_name}'); refusing shadow \
                 registration (first registrant wins; a later server must not swap the \
                 executor or escalate via always_allow)"
            ));
        }
        let active = always_allow.contains(&tool.name);
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
        let adapter = Arc::new(McpToolAdapter::with_governance(
            client.clone(),
            tool.name.clone(),
            registered_name,
            active,
            timeout_secs,
        ));
        prepared.push((manifest, adapter));
    }
    // ---- 阶段二:统一注册(来源防覆盖防线兜底;阶段一已查碰撞) ----
    for (manifest, func) in prepared {
        tool_handler.register_guarded(manifest, func).map_err(|e| {
            tracing::warn!(
                server = %server_name,
                error = %e,
                "MCP tool registration refused by source guard"
            );
            e
        })?;
    }
    tracing::info!(
        server = %server_name,
        tools_registered = tools.len(),
        "MCP tools registered (all-or-nothing)"
    );
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
    // 拉起装载门（来源认证面）：与 serve 面 manager 同一闸口
    crate::mcp::spawn_gate::ensure_spawn_allowed(
        &server_cfg.name,
        &server_cfg.command,
        &server_cfg.args,
    )?;

    let args: Vec<&str> = server_cfg.args.iter().map(|s| s.as_str()).collect();

    let limits = super::lifecycle::ResourceLimits::from_config(server_cfg);
    let transport = std::sync::Arc::new(
        crate::mcp::transport::StdioTransport::spawn_with_env_and_limits(
            &server_cfg.command,
            &args,
            &server_cfg.env,
            &limits,
        )
        .await?,
    );

    // CLI 路径握手+工具发现整体受 timeout_secs 约束(manager.connect_inner
    // 同语义镜像):原无界 await——hang 住的 server 可拖死 agent 启动
    let deadline = std::time::Duration::from_secs(server_cfg.timeout_secs.max(1));
    let (client, tools) = tokio::time::timeout(deadline, async {
        let client = McpClient::connect(transport).await?;
        let tools = client.list_tools().await?;
        Ok::<_, String>((client, tools))
    })
    .await
    .map_err(|_| {
        format!(
            "MCP server '{}' connect/discovery timed out after {}s",
            server_cfg.name,
            deadline.as_secs()
        )
    })??;
    let client = Arc::new(client);

    // 生命周期监控（资源边界设计（L1）；策略全关=不启用=现行为）。CLI 单发路径无
    // 重启监督——收割通知端就地丢弃（fail-soft：收割与事件面不受影响）
    {
        let policy = super::lifecycle::LifecyclePolicy::from_config(server_cfg);
        if policy.is_active() {
            let _ = client.enable_lifecycle(&server_cfg.name, policy);
        }
    }

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

    // ---- G3:图片面双形态 ----

    #[tokio::test]
    async fn test_adapter_call_with_images_returns_b1_object() {
        // 有图结果组装 B1 提取协议对象 {text, images:[{mime_type,data_base64}]}——
        // 下游 runner 统一提取点按此协议接住(截图类 GUI 桥闭环)
        let (client, transport) = make_client_with_tool_response().await;
        transport
            .set_response(
                "tools/call",
                serde_json::json!({
                    "content": [
                        {"type": "text", "text": "screenshot ok"},
                        {"type": "image", "data": "cG5nLWRhdGE=", "mimeType": "image/png"}
                    ]
                }),
            )
            .await;
        let adapter = McpToolAdapter::new(client, "screenshot".to_string());
        let result = adapter.call(&Value::Null).await.unwrap();
        let obj = result.as_object().expect("expected JSON object");
        assert_eq!(
            obj.get("text").and_then(|v| v.as_str()),
            Some("screenshot ok")
        );
        let images = obj.get("images").and_then(|v| v.as_array()).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(
            images[0].get("mime_type").and_then(|v| v.as_str()),
            Some("image/png")
        );
        assert_eq!(
            images[0].get("data_base64").and_then(|v| v.as_str()),
            Some("cG5nLWRhdGE=")
        );
    }

    #[tokio::test]
    async fn test_adapter_call_truncation_only_affects_text_with_images() {
        // 有图 + 超限 text:截断只作用 text(带标记),images 原样旁路
        let (client, transport) = make_client_with_tool_response().await;
        let big_text = "x".repeat(MAX_RESULT_BYTES + 1000);
        transport
            .set_response(
                "tools/call",
                serde_json::json!({
                    "content": [
                        {"type": "text", "text": big_text},
                        {"type": "image", "data": "cG5nLWRhdGE=", "mimeType": "image/png"}
                    ]
                }),
            )
            .await;
        let adapter = McpToolAdapter::new(client, "shot".to_string());
        let result = adapter.call(&Value::Null).await.unwrap();
        let obj = result.as_object().expect("expected JSON object");
        let text = obj.get("text").and_then(|v| v.as_str()).unwrap();
        assert!(text.len() < MAX_RESULT_BYTES + 1000);
        assert!(text.contains("[truncated:"));
        let images = obj.get("images").and_then(|v| v.as_array()).unwrap();
        assert_eq!(images.len(), 1);
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
                ..Default::default()
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
        // 缺省全审批:manifest=ManualDefault,
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
        // 结果上限:>100KB 截断+提示,提示含原始字节数
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

    // ===== 注册面完整性(o78 all-or-nothing / o79 名称边界与碰撞 / o80 跨来源冒名) =====

    /// 跨来源冒名测试替身:既有非 MCP 来源条目的执行器
    struct NoopTool;

    #[async_trait::async_trait]
    impl crate::io_handlers::tool_handler::ToolFunction for NoopTool {
        async fn call(&self, _args: &Value) -> IoResult {
            Ok(Value::from("noop"))
        }
    }

    #[test]
    fn test_mcp_identifier_validation_boundaries() {
        // o79:server/tool 名边界——字符集镜像 PUT 面,tool 名另加 64 字节上限
        assert!(validate_mcp_identifier("server", "fs").is_ok());
        assert!(validate_mcp_identifier("server", "web-scraper_v2").is_ok());
        assert!(validate_mcp_identifier("tool", "read_file").is_ok());
        assert!(validate_mcp_identifier("tool", "").is_err());
        assert!(validate_mcp_identifier("server", "").is_err());
        assert!(validate_mcp_identifier("tool", "a b").is_err());
        assert!(validate_mcp_identifier("tool", "a\nb").is_err());
        assert!(validate_mcp_identifier("tool", "中文工具").is_err());
        assert!(validate_mcp_identifier("tool", "a;b").is_err());
        let long = "x".repeat(65);
        assert!(validate_mcp_identifier("tool", &long).is_err());
        assert!(validate_mcp_identifier("tool", &"x".repeat(64)).is_ok());
        // server 名不加长度上限(与 PUT 面口径一致)
        assert!(validate_mcp_identifier("server", &"x".repeat(200)).is_ok());
    }

    #[tokio::test]
    async fn test_register_client_tools_contract_reject_is_all_or_nothing() {
        // o78:注册循环中途契约拒绝不得残留前 k-1 工具——两阶段注册,
        // 整 server 拒绝,状态面 failed 与实际可用面一致
        let (client, _transport) = make_client_with_tool_response().await;
        let mut handler = crate::io_handlers::tool_handler::ToolHandler::new();
        let good = spec("good_tool");
        let mut bad = spec("bad_tool");
        bad.description = String::new(); // 丢字段:manifest 契约拒绝
        let err = register_client_tools(&mut handler, "fs", &client, &[good, bad], &[], 60)
            .expect_err("contract rejection must fail the whole server batch");
        assert!(err.contains("bad_tool"), "{err}");
        assert!(
            !handler.has_tool("mcp_fs_good_tool"),
            "no partial residue allowed (failed server must not keep callable tools)"
        );
        assert!(!handler.has_tool("mcp_fs_bad_tool"));
    }

    #[tokio::test]
    async fn test_register_client_tools_rejects_nonconforming_names() {
        // o79:远端 tools/list 透传的非法工具名 = 整 server 拒绝(不留合法子集)
        let (client, _transport) = make_client_with_tool_response().await;
        let mut handler = crate::io_handlers::tool_handler::ToolHandler::new();
        let tools = [spec("ok_tool"), spec("bad name")];
        let err = register_client_tools(&mut handler, "fs", &client, &tools, &[], 60)
            .expect_err("nonconforming tool name must be rejected");
        assert!(err.contains("bad name"), "{err}");
        assert!(!handler.has_tool("mcp_fs_ok_tool"), "all-or-nothing");
    }

    #[tokio::test]
    async fn test_register_client_tools_name_collision_refuses_shadow() {
        // o79:`mcp_{server}_{tool}` 组合碰撞(a+b_c vs a_b+c 同产 mcp_a_b_c)
        // ——后者显式拒绝,先注册者保持原样(always_allow 升档路径封死)
        let (client, _transport) = make_client_with_tool_response().await;
        let mut handler = crate::io_handlers::tool_handler::ToolHandler::new();
        register_client_tools(&mut handler, "a", &client, &[spec("b_c")], &[], 60).unwrap();
        let err = register_client_tools(&mut handler, "a_b", &client, &[spec("c")], &[], 60)
            .expect_err("composition collision must be refused, not silently shadowed");
        assert!(err.contains("mcp_a_b_c"), "{err}");
        assert!(handler.has_tool("mcp_a_b_c"), "first registrant stays");
        let m = handler.manifest("mcp_a_b_c").unwrap();
        assert_eq!(
            m.approval_policy,
            crate::agent::tool_manifest::ApprovalPolicy::ManualDefault,
            "original policy must be intact (no always_allow escalation via overwrite)"
        );
    }

    #[tokio::test]
    async fn test_register_client_tools_refuses_cross_source_shadow() {
        // o80:同名冒名的结构根=覆盖无来源防线——MCP 注册撞上非 MCP 来源
        // 既有名时显式拒绝(先注册者胜),serve 面与 run/patrol 面两种装配序
        // 下等价安全,不再依赖注册顺序巧合
        let (client, _transport) = make_client_with_tool_response().await;
        let mut handler = crate::io_handlers::tool_handler::ToolHandler::new();
        // 服务代理面先注册(serve 面装配序:服务先、MCP 后)
        let svc_manifest = crate::agent::tool_manifest::dynamic_manifest(
            "mcp_fs_read_file",
            crate::agent::tool_manifest::ToolSource::ServiceProxy,
            "service proxy tool".to_string(),
            serde_json::json!({"type": "object", "properties": {}}),
        )
        .unwrap();
        handler.register(svc_manifest, std::sync::Arc::new(NoopTool));
        let err = register_client_tools(
            &mut handler,
            "fs",
            &client,
            &[spec("read_file")],
            &["read_file".to_string()],
            60,
        )
        .expect_err("MCP tool must not shadow a non-MCP tool of the same name");
        assert!(err.contains("mcp_fs_read_file"), "{err}");
        assert!(err.contains("ServiceProxy"), "{err}");
        // 被顶替面零残影:服务代理条目原样在场
        assert!(handler.has_tool("mcp_fs_read_file"));
        let m = handler.manifest("mcp_fs_read_file").unwrap();
        assert!(matches!(
            m.spec,
            crate::agent::tool_manifest::SpecSource::Inline { .. }
        ));
    }

    // ===== 执行超时契约(o83):超时=终止,非放弃 =====

    /// 握手可通过、tools/call 永不响应的挂起 server 替身(纯 Rust,无外部依赖):
    /// 模拟「accept 后握住不回」的缺陷/恶意 server
    struct HungOnCallTransport {
        closed: std::sync::atomic::AtomicBool,
    }

    impl HungOnCallTransport {
        fn new() -> Self {
            Self {
                closed: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn is_closed(&self) -> bool {
            self.closed.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl crate::mcp::transport::McpTransport for HungOnCallTransport {
        async fn request(&self, method: &str, _params: Value) -> Result<Value, String> {
            if method == "initialize" {
                return Ok(serde_json::json!({
                    "serverInfo": {"name": "hung", "version": "1"}
                }));
            }
            // tools/call 等:永不响应(server 挂死)
            std::future::pending().await
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<(), String> {
            Ok(())
        }

        async fn close(&self) -> Result<(), String> {
            self.closed
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    async fn await_closed(transport: &HungOnCallTransport) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if transport.is_closed() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        transport.is_closed()
    }

    #[tokio::test]
    async fn test_adapter_internal_timeout_terminates_transport() {
        // 适配器内建超时:超时报错且传输被断开(kill 子进程语义),server
        // 不再带着未完成副作用继续跑
        let transport = Arc::new(HungOnCallTransport::new());
        let client = Arc::new(McpClient::connect(transport.clone()).await.unwrap());
        let adapter = McpToolAdapter::with_timeout(client, "slow".to_string(), 1);
        let t0 = std::time::Instant::now();
        let result = adapter.call(&Value::Null).await;
        assert!(result.is_err(), "hung call must surface as timeout error");
        assert!(
            result.unwrap_err().contains("timed out after 1s"),
            "timeout message must carry the budget"
        );
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "timeout must fire near the budget, took {:?}",
            t0.elapsed()
        );
        assert!(
            await_closed(&transport).await,
            "timeout must close the transport (terminate the server), not abandon the future"
        );
    }

    #[tokio::test]
    async fn test_on_execution_timeout_terminates_transport() {
        // 管道执行超时钩子契约:on_execution_timeout 触发即终止在飞 server
        // (此前未实现 = 缺省 no-op,执行超时退化为放弃等待)
        let transport = Arc::new(HungOnCallTransport::new());
        let client = Arc::new(McpClient::connect(transport.clone()).await.unwrap());
        let adapter = McpToolAdapter::new(client, "slow".to_string());
        adapter.on_execution_timeout();
        assert!(
            await_closed(&transport).await,
            "execution-timeout hook must close the transport"
        );
    }

    #[tokio::test]
    async fn test_cli_connect_deadline_bounds_hung_server() {
        // CLI 路径握手+发现整体受 timeout_secs 约束:永不回 initialize 的
        // stdio server 在 deadline 处报超时,不再无界拖死 agent 启动。
        // 真子进程替身:Windows ping(-n 长轮询,stdout 输出非 JSON 行被
        // reader 逐行丢弃)、Unix sleep(纯静默)——两者都不回协议帧。
        let mut handler = crate::io_handlers::tool_handler::ToolHandler::new();
        let cfg = crate::config::McpServerConfig {
            name: "hung-stdio".to_string(),
            #[cfg(windows)]
            command: "ping".to_string(),
            #[cfg(windows)]
            args: vec!["-n".to_string(), "120".to_string(), "127.0.0.1".to_string()],
            #[cfg(not(windows))]
            command: "sleep".to_string(),
            #[cfg(not(windows))]
            args: vec!["120".to_string()],
            env: std::collections::HashMap::new(),
            disabled: false,
            timeout_secs: 1,
            always_allow: vec![],
            transport: None,
            url: None,
            headers: None,
            ..Default::default()
        };
        let t0 = std::time::Instant::now();
        let result = connect_and_register_one(&mut handler, &cfg).await;
        let err = result.expect_err("hung stdio server must hit the connect deadline");
        assert!(err.contains("timed out"), "got: {err}");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(10),
            "deadline must fire near timeout_secs=1, took {:?}",
            t0.elapsed()
        );
        assert!(!handler.has_tool("mcp_hung-stdio_anything"));
    }
}
