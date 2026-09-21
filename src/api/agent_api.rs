// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent API -- HTTP interface for managing Agent execution

use axum::{
    extract::State,
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    Json, Router,
};
use futures_core::Stream;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::agent::{
    AgentDefinitionManager, AgentError, AgentEvent, AgentRunner, ApprovalDecision, PendingApproval,
};
use crate::api::auth::AuthConfig;
use crate::api::evorule_client::EvoruleApiClient;
use crate::api::metrics::{Metrics, SharedMetrics, SseConnectionGuard};
use crate::api::workspace_client::WorkspaceApiClient;
use crate::config::LlmStatusSnapshot;
use crate::io_handlers::tool_handler::ToolHandler;

/// G6:正在运行的 session → 取消令牌的映射(SessionStore)
///
/// - 流式端点 `run_agent_stream` 在 `SessionCreated` 时插入,`Done`/`Error` 时移除
/// - `/cancel` 端点按 `session_id` 查找并触发 `cancel()`
///
/// 用 `std::sync::Mutex`(临界区是 O(1) HashMap 操作,无 await),避免在
/// 同步 `.map()` 闭包中无法 `await` `tokio::sync::Mutex` 的问题。
pub type SessionStore = Arc<Mutex<HashMap<String, CancellationToken>>>;

/// Agent run request
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentRunRequest {
    /// Agent type
    pub agent_type: String,
    /// Goal
    pub goal: String,
    /// Max steps (optional)
    pub max_steps: Option<usize>,
    /// Temperature parameter (optional)
    pub temperature: Option<f32>,
    /// Model name (optional)
    pub model: Option<String>,
}

/// Agent run response
#[derive(Debug, Serialize)]
pub struct AgentRunResponse {
    /// Whether successful
    pub success: bool,
    /// Result content
    pub content: String,
    /// Number of steps executed
    pub steps: usize,
    /// Execution duration (milliseconds)
    pub duration_ms: u64,
    /// Error message (if failed)
    pub error: Option<String>,
}

/// Agent list response
#[derive(Debug, Serialize)]
pub struct AgentListResponse {
    /// Agent list
    pub agents: Vec<AgentInfo>,
}

/// Agent info
#[derive(Debug, Serialize)]
pub struct AgentInfo {
    /// Agent type
    pub agent_type: String,
    /// Version number
    pub version: String,
    /// Description
    pub description: String,
    /// Tool list
    pub tools: Vec<String>,
}

/// Agent definition response
#[derive(Debug, Serialize)]
pub struct AgentDefinitionResponse {
    /// Agent type
    pub agent_type: String,
    /// Version number
    pub version: String,
    /// Description
    pub description: String,
    /// System prompt
    pub system_prompt: String,
    /// Model name
    pub model: String,
    /// Temperature parameter
    pub temperature: f32,
    /// Max steps
    pub max_steps: usize,
    /// Tool list
    pub tools: Vec<String>,
    /// Memory config
    pub memory_config: Option<crate::agent::MemoryConfig>,
}

/// G8:正在等待审批的 session → pending 审批项(ApprovalStore)
///
/// - 流式端点 `run_agent_stream` 在 `ApprovalRequired` 时由 `HttpApproval` 插入
///   `PendingApproval`(oneshot sender + 提案 ID)
/// - `/approve` 端点按 `session_id` 取出 pending 项,校验提案 ID 后发送
///   [`ApprovalDecision`](crate::agent::ApprovalDecision)
/// - 超时(`HTTP_APPROVAL_TIMEOUT_SECS` 秒)后 `HttpApproval` 自动清理 + 拒绝
///
/// 用 `std::sync::Mutex`(临界区是 O(1) HashMap 操作,无 await)。
pub type ApprovalStore = Arc<Mutex<HashMap<String, PendingApproval>>>;

/// Agent API state
#[derive(Debug, Clone)]
pub struct AgentApiState {
    definitions: AgentDefinitionManager,
    evorule_client: EvoruleApiClient,
    /// G6:正在运行的 session → 取消令牌(SessionStore)
    running: SessionStore,
    /// G8:正在等待审批的 session → oneshot sender(ApprovalStore)
    pending_approvals: ApprovalStore,
    /// G17:Prometheus 指标(自定义 Registry,非全局)
    metrics: SharedMetrics,
    /// G16:鉴权配置(WebSocket 升级前检查 ?token=,复用 G7 token 列表)
    ///
    /// HTTP 中间件(`auth_middleware`)对普通路由生效;WebSocket 升级发生在
    /// handler 内,需在 `ws_handler()` 中显式调用 `validate()` 做二次校验。
    auth_config: AuthConfig,
    /// E1:工作目录(工具沙箱基目录)
    workdir: std::path::PathBuf,
    /// E1:Workspace API 客户端(规则工具依赖)
    workspace_client: Arc<WorkspaceApiClient>,
    /// E1:预建 union toolkit(内置 + 规则,启动时组装一次)
    toolkit: Arc<ToolHandler>,
    /// LLM 配置脱敏快照(凭据可视化状态端点;不含任何密钥内容)
    llm_status: Arc<LlmStatusSnapshot>,
}

impl AgentApiState {
    /// Create new Agent API state
    pub fn new(definitions: AgentDefinitionManager, evorule_client: EvoruleApiClient) -> Self {
        let metrics = Arc::new(
            Metrics::new().unwrap_or_else(|e| panic!("failed to create metrics registry: {}", e)),
        );
        // E1:new 兼容路径——构造默认 workspace_client + 空 toolkit
        let workspace_client = Arc::new(WorkspaceApiClient::new(evorule_client.base_url()));
        let toolkit = Arc::new(ToolHandler::new());
        Self::new_with_metrics(
            definitions,
            evorule_client,
            metrics,
            std::path::PathBuf::from("."),
            workspace_client,
            toolkit,
        )
    }

    /// G17:Create Agent API state with an existing SharedMetrics
    ///
    /// 用于 `cmd_serve` 注入共享 metrics(供 `/metrics` 端点 + runner 插桩共用)。
    ///
    /// E1:新增 `workdir` / `workspace_client` / `toolkit` 参数,供 serve 模式
    /// 注入预建的 union toolkit(内置 6 + 规则 20 = 26 工具)。
    pub fn new_with_metrics(
        definitions: AgentDefinitionManager,
        evorule_client: EvoruleApiClient,
        metrics: SharedMetrics,
        workdir: std::path::PathBuf,
        workspace_client: Arc<WorkspaceApiClient>,
        toolkit: Arc<ToolHandler>,
    ) -> Self {
        Self {
            definitions,
            evorule_client,
            running: Arc::new(Mutex::new(HashMap::new())),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            metrics,
            auth_config: AuthConfig::disabled(),
            workdir,
            workspace_client,
            toolkit,
            llm_status: Arc::new(LlmStatusSnapshot::unconfigured()),
        }
    }

    /// 注入 LLM 配置脱敏快照(builder 风格,供 serve 启动时调用)
    pub fn with_llm_status(mut self, snapshot: Arc<LlmStatusSnapshot>) -> Self {
        self.llm_status = snapshot;
        self
    }

    /// 获取 LLM 配置脱敏快照(状态端点消费)
    pub fn llm_status(&self) -> &LlmStatusSnapshot {
        &self.llm_status
    }

    /// G6:获取 SessionStore 的引用(供 G5 server 层做断开即取消等扩展)
    pub fn running_sessions(&self) -> &SessionStore {
        &self.running
    }

    /// G8:获取 ApprovalStore 的引用(供外部观察或测试)
    pub fn pending_approvals(&self) -> &ApprovalStore {
        &self.pending_approvals
    }

    /// G17:获取 SharedMetrics 的引用(供 runner 注入等)
    pub fn metrics(&self) -> &SharedMetrics {
        &self.metrics
    }

    /// G16:获取 AuthConfig 的引用(WebSocket 升级前检查 ?token=)
    pub fn auth_config(&self) -> &AuthConfig {
        &self.auth_config
    }

    /// G16:获取 AgentDefinitionManager 的引用(WebSocket handler 构造 runner)
    pub fn definitions(&self) -> &AgentDefinitionManager {
        &self.definitions
    }

    /// G16:获取 EvoruleApiClient 的引用(WebSocket handler 调用 rewind 等 API)
    pub fn evorule_client(&self) -> &EvoruleApiClient {
        &self.evorule_client
    }

    /// E1:获取工作目录(工具沙箱基目录)
    pub fn workdir(&self) -> &std::path::Path {
        &self.workdir
    }

    /// E1:获取 WorkspaceApiClient 的引用(规则工具依赖)
    pub fn workspace_client(&self) -> &WorkspaceApiClient {
        &self.workspace_client
    }

    /// E1:获取预建 union toolkit 的引用(26 工具,启动时组装)
    pub fn toolkit(&self) -> &ToolHandler {
        &self.toolkit
    }

    /// G16:注入鉴权配置(builder pattern,供 `router_with_auth` 链式调用)
    pub fn with_auth_config(mut self, auth_config: AuthConfig) -> Self {
        self.auth_config = auth_config;
        self
    }
}

/// Create Agent API routes(无鉴权,向后兼容 / 测试用)
///
/// 生产环境请用 [`router_with_auth`] 传入 `AuthConfig`。
pub fn router(state: AgentApiState) -> Router {
    router_with_auth(state, crate::api::auth::AuthConfig::disabled())
}

/// G7:Create Agent API routes with auth middleware
///
/// 鉴权中间件对 `/health` 豁免,其余路由需要 Bearer token(或 `?token=` query param)。
/// `auth_config.enabled() == false` 时等价于无鉴权。
pub fn router_with_auth(state: AgentApiState, auth_config: crate::api::auth::AuthConfig) -> Router {
    // G16:把 auth_config 也注入到 state,供 WebSocket handler 在升级前做二次校验
    let state = state.with_auth_config(auth_config.clone());
    Router::new()
        .route("/health", axum::routing::get(health))
        .route("/metrics", axum::routing::get(metrics_handler))
        // 凭据可视化:LLM 配置只读状态(脱敏,响应体不携带任何密钥内容)
        .route("/admin/llm-status", axum::routing::get(get_llm_status))
        .route("/agents", axum::routing::get(list_agents))
        .route("/agents/{agent_type}", axum::routing::get(get_agent))
        .route("/agents/{agent_type}/run", axum::routing::post(run_agent))
        // G4:流式执行端点 — 把 AgentEvent 流转成 SSE 帧
        .route(
            "/agents/{agent_type}/run/stream",
            axum::routing::post(run_agent_stream),
        )
        // G6:取消正在运行的 session(按 session_id 查 SessionStore)
        .route(
            "/agents/{agent_type}/cancel",
            axum::routing::post(cancel_agent),
        )
        // G8:审批正在等待的 candidate 工具调用(按 session_id 查 ApprovalStore)
        .route(
            "/agents/{agent_type}/approve",
            axum::routing::post(approve_agent),
        )
        // G16:WebSocket 双向流端点 — ?token= 鉴权 + 复用 run_continuation
        .route(
            "/api/sessions/{id}/ws",
            axum::routing::get(crate::api::ws_handler::ws_handler),
        )
        // G14:记忆事件查询 — 返回 session 的所有结构化事件
        .route(
            "/api/sessions/{id}/events",
            axum::routing::get(list_session_events),
        )
        // G14:记忆事件回放 — 因果链回溯 + 可选 LLM 叙述
        .route(
            "/api/sessions/{id}/replay",
            axum::routing::get(replay_session_events),
        )
        // E2 §4.1b:记忆证据端点 — 出示某 KV 记忆的三段式证明
        .route(
            "/agents/{type}/memory/evidence",
            axum::routing::get(agent_memory_evidence),
        )
        // E2 §4.1b:记忆召回端点 — 三层召回(可选附带证据)
        .route(
            "/agents/{type}/memory/recall",
            axum::routing::get(agent_memory_recall),
        )
        // 历史批次(既定设计决策):LLM 命名操作端点 — 供 evorule-rule 作为命名操作契约消费
        // draft_rule / gen_tests / explain_rule(MVP);受同一鉴权中间件保护(LLM 按租户隔离)
        .route(
            "/ops/{operation}",
            axum::routing::post(crate::api::llm_ops::run_operation),
        )
        .with_state(state)
        // G7:鉴权中间件(对 /health、/metrics 豁免)
        .layer(axum::middleware::from_fn_with_state(
            auth_config,
            crate::api::auth::auth_middleware,
        ))
}

/// G5:健康检查端点
///
/// 路由:`GET /health` → 200 `"ok"`
/// 供 load balancer / Kubernetes liveness probe 使用。
async fn health() -> &'static str {
    "ok"
}

/// G17:Prometheus 指标端点
///
/// 路由:`GET /metrics` → 200 Prometheus 文本格式
/// 供 Prometheus / Grafana 抓取,豁免鉴权(参见 [`crate::api::auth::auth_middleware`] 的 PUBLIC_PATHS)。
async fn metrics_handler(State(state): State<AgentApiState>) -> String {
    state.metrics.render()
}

/// LLM 配置只读状态端点(脱敏)
///
/// 路由:`GET /admin/llm-status` → [`LlmStatusSnapshot`]。
/// 凭据可视化消费面:报告 provider/model/端点与 API key 存在性、末 4 位提示、
/// 来源变量名。响应体**不携带任何密钥内容**(见 [`LlmStatusSnapshot`] 设计铁律)。
async fn get_llm_status(State(state): State<AgentApiState>) -> Json<LlmStatusSnapshot> {
    Json(state.llm_status().clone())
}

async fn list_agents(State(state): State<AgentApiState>) -> Json<AgentListResponse> {
    let types = match state.definitions.list_types() {
        Ok(t) => t,
        Err(e) => {
            warn!("Failed to list agents: {}", e);
            return Json(AgentListResponse { agents: Vec::new() });
        }
    };

    let mut agents = Vec::new();
    for agent_type in types {
        if let Ok(def) = state.definitions.load(&agent_type) {
            agents.push(AgentInfo {
                agent_type: def.agent_type,
                version: def.version,
                description: def.description,
                tools: def.tools,
            });
        }
    }

    Json(AgentListResponse { agents })
}

async fn get_agent(
    State(state): State<AgentApiState>,
    axum::extract::Path(agent_type): axum::extract::Path<String>,
) -> Result<Json<AgentDefinitionResponse>, StatusCode> {
    let def = state
        .definitions
        .load(&agent_type)
        .map_err(|_| StatusCode::NOT_FOUND)?;

    Ok(Json(AgentDefinitionResponse {
        agent_type: def.agent_type,
        version: def.version,
        description: def.description,
        system_prompt: def.system_prompt,
        model: def.model,
        temperature: def.temperature,
        max_steps: def.max_steps,
        tools: def.tools,
        memory_config: Some(def.memory),
    }))
}

/// serve 模式定义加载 —— general agent 缺省补记忆基础档
///
/// - 按 agent_type 加载 agent 定义,失败返回 404
/// - `general` 定义未配置 memory(或配为 none)时,注入最小持久记忆档
///   (persistent + `general` 命名空间),使多轮 continuation 能回注历史;
///   CLI 路径与显式配置过 memory 的定义不受影响
fn load_serve_definition(
    state: &AgentApiState,
    agent_type: &str,
) -> Result<crate::agent::definition::AgentDefinition, (StatusCode, String)> {
    let mut def = state.definitions.load(agent_type).map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            format!("agent '{}' not found", agent_type),
        )
    })?;
    if agent_type == "general"
        && (def.memory.memory_type == "none" || def.memory.memory_type.is_empty())
    {
        def.memory.memory_type = "persistent".to_string();
        def.memory.namespace = "general".to_string();
    }
    Ok(def)
}

async fn run_agent(
    State(state): State<AgentApiState>,
    axum::extract::Path(agent_type): axum::extract::Path<String>,
    Json(req): Json<AgentRunRequest>,
) -> Result<Json<AgentRunResponse>, (StatusCode, String)> {
    let mut def = load_serve_definition(&state, &agent_type)?;

    // E1:per-request 覆盖直接改 def(from_definition 内部会调 to_agent_config)
    if let Some(max_steps) = req.max_steps {
        def.max_steps = max_steps;
    }
    if let Some(temperature) = req.temperature {
        def.temperature = temperature;
    }
    if let Some(model) = req.model {
        def.model = model;
    }

    info!(
        agent_type = agent_type,
        goal = %req.goal,
        "Starting agent execution"
    );

    // E1:按白名单过滤 toolkit(serve 模式安全隔离)
    let filtered = crate::api::serve_tools::build_filtered_toolkit(&state.toolkit, &def.tools);
    let mut runner = AgentRunner::from_definition(
        def,
        state.evorule_client.clone(),
        filtered,
        None, // llm_handler — serve 模式从 env 自动读
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .with_metrics(state.metrics.clone());

    let result = runner.run(&req.goal).await;

    match result {
        Ok(r) => {
            info!(
                agent_type = agent_type,
                steps = r.steps,
                duration_ms = r.duration_ms,
                "Agent execution completed"
            );
            Ok(Json(AgentRunResponse {
                success: r.success,
                content: r.content,
                steps: r.steps,
                duration_ms: r.duration_ms,
                error: r.error,
            }))
        }
        Err(e) => {
            warn!(agent_type = agent_type, error = %e, "Agent execution failed");
            Ok(Json(AgentRunResponse {
                success: false,
                content: String::new(),
                steps: 0,
                duration_ms: 0,
                error: Some(e.to_string()),
            }))
        }
    }
}

/// G4:流式执行 agent — 把 `AgentEvent` 流逐个转成 SSE 帧
///
/// 路由:`POST /agents/{agent_type}/run/stream`
///
/// 请求体同 `POST /agents/{t}/run`(`AgentRunRequest`),响应是 `text/event-stream`。
/// 每个 SSE 帧的 `event:` 名对应 `AgentEvent` 变体(snake_case),`data:` 是 JSON。
async fn run_agent_stream(
    State(state): State<AgentApiState>,
    axum::extract::Path(agent_type): axum::extract::Path<String>,
    Json(req): Json<AgentRunRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>> + Send + 'static>, (StatusCode, String)>
{
    let mut def = load_serve_definition(&state, &agent_type)?;

    // E1:per-request 覆盖直接改 def(from_definition 内部会调 to_agent_config)
    if let Some(max_steps) = req.max_steps {
        def.max_steps = max_steps;
    }
    if let Some(temperature) = req.temperature {
        def.temperature = temperature;
    }
    if let Some(model) = req.model {
        def.model = model;
    }

    info!(
        agent_type = agent_type,
        goal = %req.goal,
        "Starting agent streaming execution"
    );

    // E1:按白名单过滤 toolkit(serve 模式安全隔离)
    let filtered = crate::api::serve_tools::build_filtered_toolkit(&state.toolkit, &def.tools);
    let runner = AgentRunner::from_definition(
        def,
        state.evorule_client.clone(),
        filtered,
        None, // llm_handler — serve 模式从 env 自动读
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    // G8:注入 HttpApproval — candidate 工具返回 needs_approval 时,
    // runner 通过 oneshot channel 等 POST /approve(60s 超时自动拒绝)
    .with_approval_callback(std::sync::Arc::new(
        crate::agent::approval::HttpApproval::new(state.pending_approvals.clone()),
    ))
    // G17:注入 metrics — runner 在 session/step/LLM/工具关键路径插桩
    .with_metrics(state.metrics.clone());

    // G6:clone token(run_streaming 会 move runner),存入 SessionStore 供 /cancel 使用
    let cancel_token = runner.cancel_token().clone();
    let store = state.running.clone();
    // G17:clone metrics 供 SSE 连接守卫使用
    let sse_metrics = state.metrics.clone();

    // 用 stream! 包裹,在流入口创建 SseConnectionGuard(RAII),
    // 流结束(正常 / error / 客户端断开)时自动 dec sse_connections。
    let sse_stream = async_stream::stream! {
        let _sse_guard = SseConnectionGuard::new(sse_metrics);
        let mut event_stream = runner.run_streaming(req.goal);

        // G6:在 map 中拦截 SessionCreated(注册 token)/ Done / Error / Err(注销 token)
        let mut current_session: Option<String> = None;
        while let Some(result) = event_stream.next().await {
            match &result {
                Ok(AgentEvent::SessionCreated { session_id, .. }) => {
                    current_session = Some(session_id.clone());
                    if let Ok(mut map) = store.lock() {
                        map.insert(session_id.clone(), cancel_token.clone());
                    }
                }
                Ok(AgentEvent::Done(_)) | Ok(AgentEvent::Error(_)) | Err(_) => {
                    if let Some(sid) = current_session.take() {
                        if let Ok(mut map) = store.lock() {
                            map.remove(&sid);
                        }
                    }
                }
                _ => {}
            }
            yield agent_event_to_sse(result);
        }
    };

    Ok(Sse::new(sse_stream).keep_alive(KeepAlive::default()))
}

/// G6:取消请求的 query 参数
#[derive(Debug, Deserialize)]
pub struct CancelParams {
    /// 要取消的 session ID
    pub session_id: String,
}

/// G6:取消正在运行的 agent session
///
/// 路由:`POST /agents/{agent_type}/cancel?session_id=xxx`
///
/// 按 `session_id` 在 SessionStore 中查找取消令牌并触发 `cancel()`。
/// runner 在下一个 event/chunk 边界优雅退出(提交 error io_response、flush 消息)。
/// 200 = 已触发取消;404 = session 不在运行中(已结束或不存在)。
async fn cancel_agent(
    State(state): State<AgentApiState>,
    axum::extract::Path(_agent_type): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<CancelParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let token = {
        let map = state
            .running
            .lock()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        map.get(&params.session_id).cloned()
    };
    match token {
        Some(t) => {
            t.cancel();
            // 从 store 移除(runner 退出时也会移除,这里是双保险)
            if let Ok(mut map) = state.running.lock() {
                map.remove(&params.session_id);
            }
            info!(session_id = %params.session_id, "Cancel triggered by HTTP request");
            Ok(Json(serde_json::json!({
                "success": true,
                "message": "cancelled",
                "session_id": params.session_id,
            })))
        }
        None => {
            warn!(session_id = %params.session_id, "Cancel requested but session not found");
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// G8:审批请求的 body
#[derive(Debug, Deserialize)]
pub struct ApproveRequest {
    /// 要审批的 session ID
    pub session_id: String,
    /// 是否批准(true = 批准执行,false = 拒绝)
    pub approved: bool,
    /// 提案 ID(ApprovalRequired 帧携带;不匹配时请求被拒绝)
    pub proposal_id: Option<String>,
    /// 审批人平台令牌(可选;提供并校验通过时审批留痕带已验证身份)
    ///
    /// ⚠️ 该字段只用于换取用户名,不落审计链、不进日志。
    pub approver_token: Option<String>,
    /// 审批理由(可选,随决定写入审批留痕)
    pub reason: Option<String>,
}

/// G8:审批正在等待的 candidate 工具调用
///
/// 路由:`POST /agents/{agent_type}/approve`
/// Body:`{"session_id":"xxx","approved":true|false,"proposal_id":"ap-...","approver_token":"...","reason":"..."}`
///
/// 按 `session_id` 在 ApprovalStore 中查找 pending 审批项:
/// - 提案 ID 不匹配 → 400(审批保持等待,可携带正确 ID 重试)
/// - 提供 `approver_token` → 调认证端点换取平台用户名作为已验证审批人;
///   未提供或校验失败 → 审批人记 `unverified`(审批照常送达,不阻断)
///
/// runner 的 `HttpApproval::request_approval()` 收到决定后:
/// - `approved:true` → 带已批准状态重新调用工具
/// - 其他 → 返回 `{"status":"rejected"}`
///
/// 200 = 审批结果已送达;404 = session 不在等待审批(已超时/不存在/未触发审批)。
async fn approve_agent(
    State(state): State<AgentApiState>,
    axum::extract::Path(_agent_type): axum::extract::Path<String>,
    Json(req): Json<ApproveRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let pending = {
        let mut map = state.pending_approvals.lock().map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal error"})),
            )
        })?;
        map.remove(&req.session_id)
    };
    let Some(pending) = pending else {
        warn!(
            session_id = %req.session_id,
            "G8: approval requested but session not pending (timed out / not found / no approval needed)"
        );
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "session not pending approval"})),
        ));
    };

    // 提案 ID 校验:不匹配时把 pending 放回,允许携带正确 ID 重试
    if let Some(pid) = &req.proposal_id {
        if pid != &pending.proposal_id {
            if let Ok(mut map) = state.pending_approvals.lock() {
                map.insert(req.session_id.clone(), pending);
            }
            warn!(
                session_id = %req.session_id,
                "G8: proposal_id mismatch on approval request"
            );
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "proposal_id mismatch",
                    "session_id": req.session_id,
                })),
            ));
        }
    }

    // 审批人身份:令牌校验通过 → 平台用户名;未提供/校验失败 → unverified
    // (令牌本身不落日志、不落审计链,这里只保留换取到的用户名)
    let (approver, verified) = match &req.approver_token {
        Some(token) => match state.evorule_client().verify_platform_token(token).await {
            Ok(username) => (username, true),
            Err(e) => {
                info!(
                    session_id = %req.session_id,
                    error = %e,
                    "G8: approver token verification failed; degrading to unverified"
                );
                ("unverified".to_string(), false)
            }
        },
        None => ("unverified".to_string(), false),
    };

    let decision = ApprovalDecision {
        approved: req.approved,
        approver: approver.clone(),
        verified,
        reason: req.reason.unwrap_or_default(),
        auto_rejected: false,
    };
    // 发送审批决定(接收方已 drop 时返回 Err,但 HTTP 仍返回 200 表示"已处理")
    let _ = pending.tx.send(decision);
    info!(
        session_id = %req.session_id,
        approved = req.approved,
        approver = %approver,
        verified = verified,
        "G8: approval decision delivered"
    );
    Ok(Json(serde_json::json!({
        "success": true,
        "message": "approval delivered",
        "session_id": req.session_id,
        "approved": req.approved,
        "approver": approver,
        "verified": verified,
    })))
}

/// G4:把单个 `AgentEvent`(或流级错误)转成 axum SSE `Event`
///
/// 约定:`event:` 名 = 变体名 snake_case;`data:` 是 JSON 字符串。
/// `Done` / `Error` 帧是终帧,前端收到后应关闭流。
fn agent_event_to_sse(event: Result<AgentEvent, AgentError>) -> Result<Event, Infallible> {
    let ev = match event {
        Ok(AgentEvent::SessionCreated {
            session_id,
            memory_enabled,
        }) => Event::default().event("session_created").data(
            serde_json::json!({
                "session_id": session_id,
                "memory_enabled": memory_enabled,
            })
            .to_string(),
        ),
        Ok(AgentEvent::Step { step }) => Event::default()
            .event("step")
            .data(serde_json::json!({ "step": step }).to_string()),
        Ok(AgentEvent::LlmDelta { text }) => Event::default()
            .event("llm_delta")
            .data(serde_json::json!({ "text": text }).to_string()),
        Ok(AgentEvent::ToolCall { name, args }) => Event::default()
            .event("tool_call")
            .data(serde_json::json!({ "name": name, "args": args }).to_string()),
        Ok(AgentEvent::ToolResult { name, result }) => Event::default()
            .event("tool_result")
            .data(serde_json::json!({ "name": name, "result": result }).to_string()),
        Ok(AgentEvent::LlmDone {
            content,
            finish_reason,
        }) => Event::default().event("llm_done").data(
            serde_json::json!({
                "content": content,
                "finish_reason": finish_reason,
            })
            .to_string(),
        ),
        Ok(AgentEvent::Done(result)) => Event::default()
            .event("done")
            .data(serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string())),
        Ok(AgentEvent::Error(err)) => Event::default()
            .event("error")
            .data(serde_json::json!({ "error": err.to_string() }).to_string()),
        Ok(AgentEvent::Info(msg)) => Event::default()
            .event("info")
            .data(serde_json::json!({ "message": msg }).to_string()),
        Ok(AgentEvent::ApprovalRequired {
            tool_name,
            command,
            risk,
            alternative,
            proposal_id,
        }) => Event::default().event("approval_required").data(
            serde_json::json!({
                "tool_name": tool_name,
                "command": command,
                "risk": risk,
                "alternative": alternative,
                "proposal_id": proposal_id,
            })
            .to_string(),
        ),
        // G8:审批结果(已决定)— 前端据此更新 UI,runner 会继续执行或返回 rejected
        Ok(AgentEvent::ApprovalResult {
            tool_name,
            approved,
            approver,
            auto_rejected,
        }) => Event::default().event("approval_result").data(
            serde_json::json!({
                "tool_name": tool_name,
                "approved": approved,
                "approver": approver,
                "auto_rejected": auto_rejected,
            })
            .to_string(),
        ),
        // 流级错误(底层 stream yield Err) — 当作可恢复错误帧发出
        Err(err) => Event::default()
            .event("error")
            .data(serde_json::json!({ "error": err.to_string() }).to_string()),
    };
    Ok(ev)
}

// =============================================================================
// G14:记忆事件 HTTP 端点
// =============================================================================

/// `GET /api/sessions/{id}/events` 的查询参数
#[derive(Debug, serde::Deserialize)]
struct EventsQueryParams {
    /// 按实体 ID 过滤(如 `?entity=pet_doudou`)
    entity: Option<String>,
}

/// G14:查询 session 的所有记忆事件
///
/// 路由:`GET /api/sessions/{id}/events`
///
/// 查询参数:
/// - `?entity=<entity_id>`:按实体过滤
///
/// 返回:JSON 数组,每个元素是一个 MemoryEvent
async fn list_session_events(
    State(state): State<AgentApiState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<EventsQueryParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    use crate::agent::memory_event::MemoryEventStore;

    let client = state.evorule_client.clone();
    let mut store = MemoryEventStore::new("default", client);
    store.set_session_id(&session_id);

    // best-effort 同步(HTTP 失败返回空列表)
    if let Err(e) = store.sync_from_evorule().await {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "memory event sync failed before list; serving possibly stale local data"
        );
    }

    let events = if let Some(entity_id) = &params.entity {
        store.events_for_entity(entity_id)
    } else {
        store.list_events_sorted()
    };

    let events_json: Vec<serde_json::Value> = events
        .iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .collect();

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "count": events_json.len(),
        "events": events_json,
    })))
}

/// `GET /api/sessions/{id}/replay` 的查询参数
#[derive(Debug, serde::Deserialize)]
struct ReplayQueryParams {
    /// 从指定事件 ID 出发回溯/前进
    event: Option<String>,
    /// 按实体回放
    entity: Option<String>,
    /// 回放方向:backward(默认)或 forward
    direction: Option<String>,
    /// 是否启用 LLM 自然语言叙述
    narrate: Option<bool>,
}

/// G14:回放 session 的记忆事件链
///
/// 路由:`GET /api/sessions/{id}/replay`
///
/// 查询参数:
/// - `?event=<event_id>`:从指定事件出发,沿因果链回溯/前进
/// - `?entity=<entity_id>`:按实体回放
/// - `?direction=backward|forward`:回放方向(默认 backward)
/// - `?narrate=true`:启用 LLM 自然语言叙述(temperature=0)
async fn replay_session_events(
    State(state): State<AgentApiState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<ReplayQueryParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    use crate::agent::memory_event::{MemoryEventStore, ReplayDirection, ReplayEngine};

    let client = state.evorule_client.clone();
    let mut store = MemoryEventStore::new("default", client);
    store.set_session_id(&session_id);

    // best-effort 同步
    if let Err(e) = store.sync_from_evorule().await {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "memory event sync failed before replay; replaying possibly stale local data"
        );
    }

    if store.event_count() == 0 {
        return Ok(Json(serde_json::json!({
            "session_id": session_id,
            "count": 0,
            "events": [],
            "message": "no events found",
        })));
    }

    let mut engine = ReplayEngine::new(store);

    // 可选注入 LLM(narrate=true 时)
    let do_narrate = params.narrate.unwrap_or(false);
    if do_narrate {
        let llm = crate::io_handlers::LlmHandler::with_defaults();
        engine = engine.with_llm(llm);
    }

    let direction = params.direction.as_deref().unwrap_or("backward");
    let dir = if direction == "forward" {
        ReplayDirection::Forward
    } else {
        ReplayDirection::Backward
    };

    let events_result = if let Some(event_id) = &params.event {
        engine.replay_from(event_id, dir).await
    } else if let Some(entity_id) = &params.entity {
        engine.replay_by_entity(entity_id).await
    } else {
        Ok(engine.store().list_events_sorted())
    };

    let events = match events_result {
        Ok(e) => e,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("replay error: {}", e),
            ));
        }
    };

    if do_narrate {
        match engine.narrate(&events).await {
            Ok(narrative) => {
                let events_json: Vec<serde_json::Value> = events
                    .iter()
                    .filter_map(|e| serde_json::to_value(e).ok())
                    .collect();
                Ok(Json(serde_json::json!({
                    "session_id": session_id,
                    "count": events_json.len(),
                    "events": events_json,
                    "narrative": narrative.text,
                    "cited_events": narrative.cited_events,
                    "cited_facts": narrative.cited_facts,
                })))
            }
            Err(e) => {
                // 叙述失败时降级为结构化输出
                warn!(session_id = %session_id, error = %e, "narration failed, falling back to structured output");
                let events_json: Vec<serde_json::Value> = events
                    .iter()
                    .filter_map(|e| serde_json::to_value(e).ok())
                    .collect();
                Ok(Json(serde_json::json!({
                    "session_id": session_id,
                    "count": events_json.len(),
                    "events": events_json,
                    "narrative_error": e,
                })))
            }
        }
    } else {
        let events_json: Vec<serde_json::Value> = events
            .iter()
            .filter_map(|e| serde_json::to_value(e).ok())
            .collect();
        Ok(Json(serde_json::json!({
            "session_id": session_id,
            "count": events_json.len(),
            "events": events_json,
        })))
    }
}

// ===== E2 §4.1b: 记忆证据 / 召回端点 =====

/// E2: 记忆证据查询参数
#[derive(Debug, Deserialize)]
pub struct MemoryEvidenceQuery {
    /// 目标 session_id（用于 Session 作用域 + audit verify）
    pub session_id: String,
    /// 作用域:`shared` 或 `session`
    pub scope: String,
    /// KV 键
    pub key: String,
}

/// E2: 记忆召回查询参数
#[derive(Debug, Deserialize)]
pub struct MemoryRecallQuery {
    /// 召回目标（语义匹配用）
    pub goal: String,
    /// L1 摘要上限（缺省走 AgentDefinition.memory）
    pub max_summaries: Option<usize>,
    /// L2 事件上限（缺省走 AgentDefinition.memory）
    pub max_events: Option<usize>,
    /// 是否附带证据（B4 attach_evidence）
    #[serde(default)]
    pub with_evidence: Option<bool>,
}

/// E2 §4.1b: GET /agents/{type}/memory/evidence — 出示某 KV 记忆的证据
///
/// 三段式证明:源 FactId + 整链 verify + 因果链（KV 无 cause,chain 为空）。
async fn agent_memory_evidence(
    State(state): State<AgentApiState>,
    axum::extract::Path(agent_type): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<MemoryEvidenceQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // 1. 校验 agent_type 存在
    state
        .definitions
        .load(&agent_type)
        .map_err(|_| StatusCode::NOT_FOUND)?;

    // 2. 构造 MemoryManager（带 session_id,供 audit verify）
    let memory =
        crate::agent::memory::MemoryManager::new(&agent_type, state.evorule_client.clone())
            .with_session_id(&params.session_id);

    // 3. scope 解析
    let scope = match params.scope.as_str() {
        "shared" => crate::agent::memory::MemoryScope::Shared,
        "session" => crate::agent::memory::MemoryScope::Session(params.session_id.clone()),
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    // 4. evidence_for
    match memory.evidence_for(&scope, &params.key).await {
        Ok(Some(ev)) => Ok(Json(serde_json::to_value(&ev).unwrap_or_default())),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::BAD_GATEWAY),
    }
}

/// E2 §4.1b: GET /agents/{type}/memory/recall — 三层召回（可选附带证据）
async fn agent_memory_recall(
    State(state): State<AgentApiState>,
    axum::extract::Path(agent_type): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<MemoryRecallQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // 1. 校验 agent_type 存在 + 取默认值
    let def = state
        .definitions
        .load(&agent_type)
        .map_err(|_| StatusCode::NOT_FOUND)?;

    // 2. 构造 MemoryManager
    let memory =
        crate::agent::memory::MemoryManager::new(&agent_type, state.evorule_client.clone());

    // 3. 参数缺省（走 AgentDefinition.memory 配置）
    let max_summaries = params
        .max_summaries
        .unwrap_or(def.memory.max_session_summaries);
    let max_events = params.max_events.unwrap_or(def.memory.max_injected_events);

    // 4. recall（with_evidence=true 时走 C2 带证据路径）
    let ctx = if params.with_evidence.unwrap_or(false) {
        memory
            .recall_context_with_evidence(&params.goal, max_summaries, max_events)
            .await
            .unwrap_or_default()
    } else {
        memory
            .recall_context(&params.goal, max_summaries, max_events)
            .await
    };

    Ok(Json(serde_json::to_value(&ctx).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    // G4 测试需要 AgentResult 构造 Done 帧
    use crate::agent::AgentResult;

    fn make_test_state() -> AgentApiState {
        AgentApiState::new(
            AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new("http://localhost:8080"),
        )
    }

    #[tokio::test]
    async fn test_list_agents_empty() {
        let state = make_test_state();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        // G5:/health 端点返回 200 "ok"
        let state = make_test_state();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_llm_status_endpoint_unconfigured() {
        // 兼容路径默认 state → 未配置快照(configured=false / present=false)
        let state = make_test_state();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/llm-status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["configured"], serde_json::Value::Bool(false));
        assert_eq!(json["api_key"]["present"], serde_json::Value::Bool(false));
    }

    #[tokio::test]
    async fn test_llm_status_endpoint_masks_key() {
        // 脱敏铁律:响应体不得包含 key 全值,只允许末 4 位提示
        let secret = "sk-test-abcd1234wxyz";
        let cfg = crate::config::LlmConfig {
            api_key: secret.to_string(),
            ..crate::config::LlmConfig::default()
        };
        let snapshot = cfg.status_snapshot(Some("MINIMAX_API_KEY"));
        assert_eq!(snapshot.api_key.hint.as_deref(), Some("wxyz"));

        let state = make_test_state().with_llm_status(std::sync::Arc::new(snapshot));
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/llm-status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains(secret), "response body leaked the API key");
        assert!(text.contains("wxyz"), "last-4 hint should be present");
    }

    #[tokio::test]
    async fn test_metrics_endpoint() {
        // G17:/metrics 端点返回 200 + Prometheus 文本格式
        let state = make_test_state();
        // 触发一些指标(含 Vec 指标子项初始化,否则惰性创建的 Vec 不会出现在输出中)
        state.metrics.inc_sessions_total();
        state.metrics.inc_steps();
        state
            .metrics
            .observe_llm_call("test_model", std::time::Duration::from_millis(1), true);
        state
            .metrics
            .observe_tool_call("test_tool", std::time::Duration::from_millis(1), true);
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("evo_agent_sessions_total"));
        assert!(text.contains("evo_agent_steps_total"));
        assert!(text.contains("evo_agent_llm_calls_total"));
        assert!(text.contains("evo_agent_tool_calls_total"));
    }

    #[tokio::test]
    async fn test_metrics_exempt_from_auth() {
        // G17:/metrics 豁免鉴权(即使 auth enabled 也不需要 token)
        let state = make_test_state();
        let auth_config = crate::api::auth::AuthConfig::new(vec!["secret".to_string()], true);
        let app = router_with_auth(state, auth_config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // 无 token 也应返回 200(豁免鉴权)
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_agent_not_found() {
        let state = make_test_state();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/agents/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_run_agent_not_found() {
        let state = make_test_state();
        let app = router(state);

        let request = AgentRunRequest {
            agent_type: "nonexistent".to_string(),
            goal: "test".to_string(),
            max_steps: None,
            temperature: None,
            model: None,
        };

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/nonexistent/run")
                    .header("Content-Type", "application/json")
                    .body(Body::from(serde_json::to_string(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_agent_run_request_deserialize() {
        let json = r#"{
            "agent_type": "researcher",
            "goal": "search for AI news",
            "max_steps": 5,
            "temperature": 0.3,
            "model": "gpt-4"
        }"#;
        let req: AgentRunRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.agent_type, "researcher");
        assert_eq!(req.goal, "search for AI news");
        assert_eq!(req.max_steps, Some(5));
        assert_eq!(req.temperature, Some(0.3));
        assert_eq!(req.model, Some("gpt-4".to_string()));
    }

    #[test]
    fn test_agent_run_response_serialize() {
        let resp = AgentRunResponse {
            success: true,
            content: "hello".to_string(),
            steps: 3,
            duration_ms: 100,
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("success"));
        assert!(json.contains("hello"));
        assert!(json.contains("3"));
    }

    // ===== G4 SSE 测试 =====
    // axum 的 Event 没有公开 getter,测试通过 format!("{:?}", ev) 检查 event 名 + data 内容

    #[test]
    fn test_agent_event_to_sse_session_created() {
        let ev = agent_event_to_sse(Ok(AgentEvent::SessionCreated {
            session_id: "s-123".to_string(),
            memory_enabled: false,
        }))
        .unwrap();
        let s = format!("{:?}", ev);
        assert!(s.contains("session_created"));
        assert!(s.contains("s-123"));
        assert!(s.contains("memory_enabled"));
    }

    #[test]
    fn test_agent_event_to_sse_llm_delta() {
        let ev = agent_event_to_sse(Ok(AgentEvent::LlmDelta {
            text: "hello".to_string(),
        }))
        .unwrap();
        let s = format!("{:?}", ev);
        assert!(s.contains("llm_delta"));
        assert!(s.contains("hello"));
    }

    #[test]
    fn test_agent_event_to_sse_step() {
        let ev = agent_event_to_sse(Ok(AgentEvent::Step { step: 7 })).unwrap();
        let s = format!("{:?}", ev);
        assert!(s.contains("step"));
        assert!(s.contains("7"));
    }

    #[test]
    fn test_agent_event_to_sse_tool_call_and_result() {
        let call = agent_event_to_sse(Ok(AgentEvent::ToolCall {
            name: "search".to_string(),
            args: serde_json::json!({"q": "rust"}),
        }))
        .unwrap();
        let s = format!("{:?}", call);
        assert!(s.contains("tool_call"));
        assert!(s.contains("search"));

        let res = agent_event_to_sse(Ok(AgentEvent::ToolResult {
            name: "search".to_string(),
            result: serde_json::json!({"hits": 3}),
        }))
        .unwrap();
        let s = format!("{:?}", res);
        assert!(s.contains("tool_result"));
    }

    #[test]
    fn test_agent_event_to_sse_llm_done() {
        let ev = agent_event_to_sse(Ok(AgentEvent::LlmDone {
            content: "done text".to_string(),
            finish_reason: Some("stop".to_string()),
        }))
        .unwrap();
        let s = format!("{:?}", ev);
        assert!(s.contains("llm_done"));
        assert!(s.contains("done text"));
        assert!(s.contains("stop"));
    }

    #[test]
    fn test_agent_event_to_sse_done() {
        let result = AgentResult::success("final".to_string(), 2, 50, vec![]);
        let ev = agent_event_to_sse(Ok(AgentEvent::Done(result))).unwrap();
        let s = format!("{:?}", ev);
        assert!(s.contains("done"));
        assert!(s.contains("final"));
    }

    #[test]
    fn test_agent_event_to_sse_error_and_info() {
        let err_ev = agent_event_to_sse(Ok(AgentEvent::Error(AgentError::LlmError(
            "boom".to_string(),
        ))))
        .unwrap();
        let s = format!("{:?}", err_ev);
        assert!(s.contains("error"));
        assert!(s.contains("boom"));

        let info_ev = agent_event_to_sse(Ok(AgentEvent::Info("rewind".to_string()))).unwrap();
        let s = format!("{:?}", info_ev);
        assert!(s.contains("info"));
        assert!(s.contains("rewind"));
    }

    #[test]
    fn test_agent_event_to_sse_stream_level_error() {
        // 流级 Err(AgentError) 也应映射成 error 帧,不能 panic
        let ev = agent_event_to_sse(Err(AgentError::Internal("stream broke".to_string()))).unwrap();
        let s = format!("{:?}", ev);
        assert!(s.contains("error"));
        assert!(s.contains("stream broke"));
    }

    #[test]
    fn test_stream_route_registered() {
        // 验证 stream 路由已注册:对不存在的 agent 发 POST 应返回 404(说明路由匹配到了 handler)
        let state = make_test_state();
        let app = router(state);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let request = AgentRunRequest {
            agent_type: "nonexistent".to_string(),
            goal: "test".to_string(),
            max_steps: None,
            temperature: None,
            model: None,
        };
        let response = rt.block_on(async {
            app.oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/nonexistent/run/stream")
                    .header("Content-Type", "application/json")
                    .body(Body::from(serde_json::to_string(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
        });
        // 路由匹配成功 → handler 返回 404(agent 不存在)
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ===== G6 /cancel 端点测试 =====

    #[tokio::test]
    async fn test_cancel_unknown_session_returns_404() {
        // 不在运行中的 session → 404
        let state = make_test_state();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/cancel?session_id=nonexistent-session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_cancel_registered_session_triggers_cancel() {
        // 手动往 SessionStore 插入一个 token,验证 /cancel 能触发它
        let state = make_test_state();
        let token = CancellationToken::new();
        {
            let mut map = state.running.lock().unwrap();
            map.insert("test-session-001".to_string(), token.clone());
        }
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/cancel?session_id=test-session-001")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        // token 应已被取消
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancel_removes_session_from_store() {
        // /cancel 后 session 应从 store 移除(再次 cancel 同一 session → 404)
        let state = make_test_state();
        let token = CancellationToken::new();
        state
            .running
            .lock()
            .unwrap()
            .insert("sess-002".to_string(), token.clone());

        let app = router(state.clone());

        // 第一次 cancel → 200
        let r1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/cancel?session_id=sess-002")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        assert!(token.is_cancelled());

        // 第二次 cancel 同一 session → 404(已移除)
        let r2 = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/cancel?session_id=sess-002")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_cancel_params_deserialize() {
        let json = r#"{"session_id":"abc-123"}"#;
        let params: CancelParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.session_id, "abc-123");
    }

    #[test]
    fn test_session_store_starts_empty() {
        let state = make_test_state();
        let map = state.running.lock().unwrap();
        assert!(map.is_empty(), "SessionStore should start empty");
    }

    // ===== G8 SSE 审批事件测试 =====

    #[test]
    fn test_agent_event_to_sse_approval_required() {
        let ev = agent_event_to_sse(Ok(AgentEvent::ApprovalRequired {
            tool_name: "shell_exec".to_string(),
            command: "rm -rf /tmp/test".to_string(),
            risk: "high".to_string(),
            alternative: "use trash instead".to_string(),
            proposal_id: "ap-test-1".to_string(),
        }))
        .unwrap();
        let s = format!("{:?}", ev);
        assert!(s.contains("approval_required"));
        assert!(s.contains("shell_exec"));
        assert!(s.contains("rm -rf /tmp/test"));
        assert!(s.contains("high"));
        assert!(s.contains("use trash instead"));
        assert!(s.contains("ap-test-1"));
    }

    #[test]
    fn test_agent_event_to_sse_approval_result_approved() {
        let ev = agent_event_to_sse(Ok(AgentEvent::ApprovalResult {
            tool_name: "shell_exec".to_string(),
            approved: true,
            approver: "alice".to_string(),
            auto_rejected: false,
        }))
        .unwrap();
        let s = format!("{:?}", ev);
        assert!(s.contains("approval_result"));
        assert!(s.contains("shell_exec"));
        assert!(s.contains("true"));
        assert!(s.contains("alice"));
    }

    #[test]
    fn test_agent_event_to_sse_approval_result_denied() {
        let ev = agent_event_to_sse(Ok(AgentEvent::ApprovalResult {
            tool_name: "http_get".to_string(),
            approved: false,
            approver: "auto".to_string(),
            auto_rejected: true,
        }))
        .unwrap();
        let s = format!("{:?}", ev);
        assert!(s.contains("approval_result"));
        assert!(s.contains("http_get"));
        assert!(s.contains("false"));
        assert!(s.contains("auto"));
    }

    // ===== G8 /approve 端点测试 =====

    #[test]
    fn test_approve_request_deserialize() {
        let json = r#"{"session_id":"sess-abc","approved":true}"#;
        let req: ApproveRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.session_id, "sess-abc");
        assert!(req.approved);
    }

    #[test]
    fn test_approve_request_deserialize_denied() {
        let json = r#"{"session_id":"sess-xyz","approved":false}"#;
        let req: ApproveRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.session_id, "sess-xyz");
        assert!(!req.approved);
    }

    #[test]
    fn test_approval_store_starts_empty() {
        let state = make_test_state();
        let map = state.pending_approvals.lock().unwrap();
        assert!(map.is_empty(), "ApprovalStore should start empty");
    }

    #[tokio::test]
    async fn test_approve_unknown_session_returns_404() {
        // 不在等待审批的 session → 404
        let state = make_test_state();
        let app = router(state);

        let body = serde_json::json!({"session_id": "nonexistent", "approved": true});
        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/approve")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_approve_registered_session_delivers_decision() {
        // 手动往 ApprovalStore 插入一个 pending 项,验证 /approve 能送达决定
        let state = make_test_state();
        let (tx, rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        {
            let mut map = state.pending_approvals.lock().unwrap();
            map.insert(
                "approval-session-001".to_string(),
                PendingApproval {
                    tx,
                    proposal_id: "ap-001".to_string(),
                },
            );
        }
        let app = router(state);

        let body = serde_json::json!({
            "session_id": "approval-session-001",
            "approved": true,
            "proposal_id": "ap-001",
            "reason": "okay",
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/approve")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        // sender 应已收到批准决定
        let decision = rx.await.unwrap();
        assert!(decision.approved);
        assert_eq!(decision.approver, "unverified");
        assert!(!decision.verified);
        assert_eq!(decision.reason, "okay");
        assert!(!decision.auto_rejected);
    }

    #[tokio::test]
    async fn test_approve_delivers_denial() {
        // 验证 approved:false 也能正确送达
        let state = make_test_state();
        let (tx, rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        state.pending_approvals.lock().unwrap().insert(
            "approval-session-002".to_string(),
            PendingApproval {
                tx,
                proposal_id: "ap-002".to_string(),
            },
        );
        let app = router(state);

        let body = serde_json::json!({"session_id": "approval-session-002", "approved": false});
        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/approve")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let decision = rx.await.unwrap();
        assert!(!decision.approved);
    }

    #[tokio::test]
    async fn test_approve_removes_session_from_store() {
        // /approve 后 session 应从 store 移除(再次 approve 同一 session → 404)
        let state = make_test_state();
        let (tx, _rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        state.pending_approvals.lock().unwrap().insert(
            "approval-session-003".to_string(),
            PendingApproval {
                tx,
                proposal_id: "ap-003".to_string(),
            },
        );
        let app = router(state.clone());

        let body = serde_json::json!({"session_id": "approval-session-003", "approved": true});
        let r1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/approve")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::OK);

        // 第二次 approve 同一 session → 404(已移除)
        let body2 = serde_json::json!({"session_id": "approval-session-003", "approved": false});
        let r2 = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/approve")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body2.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_approve_route_registered() {
        // 验证 /approve 路由已注册:对已注册 session 发 POST 应返回 200(非 405 Method Not Allowed)
        let state = make_test_state();
        let (tx, _rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        state.pending_approvals.lock().unwrap().insert(
            "route-test-sess".to_string(),
            PendingApproval {
                tx,
                proposal_id: "ap-route".to_string(),
            },
        );
        let app = router(state);

        let body = serde_json::json!({"session_id": "route-test-sess", "approved": true});
        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/approve")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        // 路由匹配成功 → handler 返回 200
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ===== 审批人身份验证(降级 / 提案 ID 校验) =====

    #[tokio::test]
    async fn test_approve_without_token_degrades_to_unverified() {
        // 未携带 approver_token → 审批照常送达,审批人记 unverified
        let state = make_test_state();
        let (tx, rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        state.pending_approvals.lock().unwrap().insert(
            "unverified-sess".to_string(),
            PendingApproval {
                tx,
                proposal_id: "ap-uv".to_string(),
            },
        );
        let app = router(state);

        let body = serde_json::json!({"session_id": "unverified-sess", "approved": true});
        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/approve")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let decision = rx.await.unwrap();
        assert!(decision.approved);
        assert_eq!(decision.approver, "unverified");
        assert!(!decision.verified);
    }

    #[tokio::test]
    async fn test_approve_token_verify_failure_degrades_to_unverified() {
        // approver_token 校验失败(服务不可达)→ 不阻断审批,降级 unverified
        let state = make_test_state();
        let (tx, rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        state.pending_approvals.lock().unwrap().insert(
            "badtoken-sess".to_string(),
            PendingApproval {
                tx,
                proposal_id: "ap-bt".to_string(),
            },
        );
        let app = router(state);

        let body = serde_json::json!({
            "session_id": "badtoken-sess",
            "approved": true,
            "approver_token": "some-invalid-token",
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/approve")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let decision = rx.await.unwrap();
        assert_eq!(decision.approver, "unverified");
        assert!(!decision.verified);
    }

    #[tokio::test]
    async fn test_approve_token_verify_success_carries_username() {
        // approver_token 校验通过 → 审批人 = 平台用户名,verified = true
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/platform/auth/me")
            .match_header("authorization", "Bearer good-token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"success":true,"user":{"username":"bob"},"permissions":[],"permissions_version":1}"#,
            )
            .create_async()
            .await;

        let state = AgentApiState::new(
            crate::agent::AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new(&server.url()),
        );
        let (tx, rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        state.pending_approvals.lock().unwrap().insert(
            "verify-sess".to_string(),
            PendingApproval {
                tx,
                proposal_id: "ap-vf".to_string(),
            },
        );
        let app = router(state);

        let body = serde_json::json!({
            "session_id": "verify-sess",
            "approved": true,
            "approver_token": "good-token",
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/approve")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let decision = rx.await.unwrap();
        assert_eq!(decision.approver, "bob");
        assert!(decision.verified);
    }

    #[tokio::test]
    async fn test_approve_proposal_id_mismatch_returns_400_and_keeps_pending() {
        // 提案 ID 不匹配 → 400,且 pending 保留(可携带正确 ID 重试成功)
        let state = make_test_state();
        let (tx, rx) = tokio::sync::oneshot::channel::<ApprovalDecision>();
        state.pending_approvals.lock().unwrap().insert(
            "mismatch-sess".to_string(),
            PendingApproval {
                tx,
                proposal_id: "ap-correct".to_string(),
            },
        );
        let app = router(state.clone());

        // 第一次:错误提案 ID → 400
        let bad = serde_json::json!({
            "session_id": "mismatch-sess",
            "approved": true,
            "proposal_id": "ap-wrong",
        });
        let r1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/approve")
                    .header("Content-Type", "application/json")
                    .body(Body::from(bad.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::BAD_REQUEST);

        // pending 应被放回,可重试
        assert!(
            state
                .pending_approvals
                .lock()
                .unwrap()
                .contains_key("mismatch-sess"),
            "pending approval must survive a mismatched proposal_id"
        );

        // 第二次:正确提案 ID → 200,决定送达
        let good = serde_json::json!({
            "session_id": "mismatch-sess",
            "approved": true,
            "proposal_id": "ap-correct",
        });
        let r2 = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/general/approve")
                    .header("Content-Type", "application/json")
                    .body(Body::from(good.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::OK);
        let decision = rx.await.unwrap();
        assert!(decision.approved);
    }

    // ===== E2: 记忆证据/召回 query DTO 反序列化测试 =====

    #[test]
    fn test_memory_evidence_query_deserialize() {
        let json = r#"{"session_id":"sess-1","scope":"shared","key":"stable.fact"}"#;
        let q: MemoryEvidenceQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.session_id, "sess-1");
        assert_eq!(q.scope, "shared");
        assert_eq!(q.key, "stable.fact");
    }

    #[test]
    fn test_memory_evidence_query_deserialize_session_scope() {
        let json = r#"{"session_id":"s2","scope":"session","key":"k"}"#;
        let q: MemoryEvidenceQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.scope, "session");
    }

    #[test]
    fn test_memory_recall_query_deserialize_full() {
        let json = r#"{"goal":"find user","max_summaries":5,"max_events":8,"with_evidence":true}"#;
        let q: MemoryRecallQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.goal, "find user");
        assert_eq!(q.max_summaries, Some(5));
        assert_eq!(q.max_events, Some(8));
        assert_eq!(q.with_evidence, Some(true));
    }

    #[test]
    fn test_memory_recall_query_deserialize_minimal() {
        // 仅必填 goal,其余缺省
        let json = r#"{"goal":"hello"}"#;
        let q: MemoryRecallQuery = serde_json::from_str(json).unwrap();
        assert_eq!(q.goal, "hello");
        assert!(q.max_summaries.is_none());
        assert!(q.max_events.is_none());
        assert_eq!(q.with_evidence, None); // #[serde(default)]
    }
}
