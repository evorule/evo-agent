// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent API -- HTTP interface for managing Agent execution

use axum::{
    extract::State,
    http::StatusCode,
    response::{sse::{Event, KeepAlive, Sse}, IntoResponse},
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
    AgentDefinitionManager, AgentError, AgentEvent, AgentResult, AgentRunner, ApprovalDecision,
    PendingApproval,
};
// 批K 段7:人在环问答(AskUser 注入 + /answer 端点共享 QuestionStore)
use crate::api::auth::AuthConfig;
use crate::api::evorule_client::EvoruleApiClient;
use crate::api::metrics::{Metrics, SharedMetrics, SseConnectionGuard};
use crate::api::serve_tools::RuntimeIdentity;
use crate::api::workspace_client::WorkspaceApiClient;
use crate::builtin_tools::ask_user::{AskUserAnswer, CliAskUser, HttpAskUser, QuestionStore};
use crate::config::{LlmConfig, LlmStatusSnapshot};
use crate::io_handlers::tool_handler::ToolHandler;

/// G6:正在运行的 session → 取消令牌的映射(SessionStore)
///
/// - 流式端点 `run_agent_stream` 在 `SessionCreated` 时插入,`Done`/`Error` 时移除
/// - `/cancel` 端点按 `session_id` 查找并触发 `cancel()`
///
/// 用 `std::sync::Mutex`(临界区是 O(1) HashMap 操作,无 await),避免在
/// 同步 `.map()` 闭包中无法 `await` `tokio::sync::Mutex` 的问题。
pub type SessionStore = Arc<Mutex<HashMap<String, CancellationToken>>>;

/// plan-execute 执行模式声明（可选扩展字段）
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecutionSpec {
    /// 执行模式：`react`（缺省）| `plan_execute`——plan_execute 走外层驱动
    /// 循环（planning probe → PlanFact v1 → 物化执行 → replan），
    /// react = 既有单代理 ReAct 行为零变化
    pub mode: String,
}

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
    /// P1 执行桥扩展字段(可选):shell_exec 转入 docker-exec 后端,命令在
    /// 指定任务容器内执行。None = 宿主后端,行为与既有消费者完全一致
    /// (参赛兼容层三原则②:删配置即下线)。LLM 面不可见该字段——仅 HTTP
    /// 请求方(如评测 harness 的 agent 壳)可设。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// G1 执行环境 profile 扩展字段(可选):"sandbox-trusted" = 一次性隔离
    /// 执行环境语义(shell_exec/python_exec 按 trusted 分类基线运行);优先级
    /// 高于 agent 定义级 execution_profile;未知值 fail-fast 400。None = 既有
    /// 语义零变化。LLM 面不可见该字段——仅 HTTP 请求方(如评测 harness)可设。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_profile: Option<String>,
    /// 请求级工具子集扩展字段(可选):allowlist 语义——只保留列表命中的工具,
    /// 与 def 白名单求交(只收不放,请求不能越过定义面重新放开);条目支持
    /// 尾缀 `*` 前缀通配(与 def.tools 通配语义同款);可用面上零命中的条目
    /// fail-fast 400(显式意志拼错名不静默缩权)。None = 既有语义零变化。
    /// LLM 面不可见该字段——仅 HTTP 请求方(如评测 harness)可设。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// P2 沙箱根映射扩展字段(可选):file 类工具沙箱根重绑到指定绝对路径
    /// (校验域随执行域迁移,fs_safety 判据零改动;None = 既有语义零变化,
    /// 删字段即下线)。LLM 面不可见该字段——仅 HTTP 请求方可设。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// plan-execute 执行模式扩展字段(可选):mode=plan_execute 时请求由外层
    /// 驱动循环执行,None/缺省/react = 既有单代理 ReAct 行为零变化
    /// (参赛兼容层三原则②:删配置即下线)。LLM 面不可见该字段——仅 HTTP
    /// 请求方可设。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionSpec>,
    /// 恢复此前中断的 run(可选),按 execution.mode 分流:plan_execute =
    /// 恢复 planrun- 前缀的 run 级账本(值由上次响应 run_session_id 透出或
    /// GET /api/sessions/resumable 列出);react/缺省 = 崩溃恢复(值为普通
    /// 会话名,走 resume_crashed 管线——无新输入,加载历史+悬挂工具处置后
    /// 自然续完中断轮;WS 面等价入口为 {"type":"resume"} 帧)。恢复仅经
    /// 显式入参——无人监督的自动续跑不做(人类持剑)。LLM 面不可见该
    /// 字段——仅 HTTP 请求方可设。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_session_id: Option<String>,
    /// 恢复语义分流(可选,仅 react 面消费;plan_execute 面携带忽略,同
    /// step_timeout_secs 先例):"crash"(缺省)=既有崩溃恢复——无新输入,
    /// 悬挂工具处置后续完中断轮;"continue"=多轮续聊——活会话走
    /// run_continuation(要求新输入),死会话(引擎侧 404)自动降级为重建:
    /// 取两级历史(活投影→本地快照)合成重建回注块后以新会话续跑,响应
    /// rebuilt_from_session 透出旧会话名。非法值 400;恢复仅经显式入参,
    /// 不自动续跑(人类持剑)。LLM 面不可见该字段——仅 HTTP 请求方可设。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_mode: Option<String>,
    /// plan-execute 粒执行器步超时覆写(可选;秒)。粒定义步超时
    /// (如 general 300s)小于容器命令预算 600s 时,超时长的容器内命令
    /// (pip/构建类)被步超时先杀——请求级覆写粒定义 step_timeout_secs。
    /// 仅 execution.mode=plan_execute 路径消费,react 面携带忽略(语义
    /// 无冲突故不报 400)。
    /// None = 粒定义值零变化(删配置即下线)。LLM 面不可见该字段——仅
    /// HTTP 请求方可设。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_timeout_secs: Option<u64>,
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
    /// Created session ID (if the run established one; 会话关联收口:
    /// 消费者可凭此查询 18080 权威面或工作台回放,旧消费者不受影响)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// plan-execute 驱动循环统计(mode=plan_execute 时透出;react/缺省不序列化,
    /// 字段语义见 `driver::PlanLoopStats`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_stats: Option<PlanStatsResponse>,
    /// run 级账本会话名(plan-execute 面透出;调用方留存即可在中断后凭
    /// resume_session_id 显式恢复。react/缺省不序列化)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_session_id: Option<String>,
    /// 重建回退发生时的旧会话名(resume_mode=continue 且引擎侧会话已失效
    /// → 两级取历史合成重建回注块后以新会话续跑;此时 session_id=新会话
    /// 名,本字段=旧会话名供消费端对账。react 其他路径/plan_execute 不序列化)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebuilt_from_session: Option<String>,
}

/// plan-execute 驱动循环统计(stats 七项透出;字段语义见 `driver::PlanLoopStats`)
#[derive(Debug, Serialize)]
pub struct PlanStatsResponse {
    /// 执行过的计划版本数(Dsl 无 replan = 1;plan-execute = 1 + replans)
    pub plan_versions: u32,
    /// 实际发生的 replan 次数
    pub replans: u32,
    /// 全部版本累计完成节点数
    pub nodes_executed: u64,
    /// 全部版本累计墙钟毫秒
    pub wall_ms: u64,
    /// 静态拦截命中的重复执行节点数(幂等重复放行,仅埋点)
    pub repeated_nodes: u64,
    /// 全部版本累计 token
    pub tokens_used: u64,
    /// v2+ 版本消耗 token(replan 重复执行成本埋点)
    pub replan_tokens: u64,
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
    /// G2:生效上下文窗口 token 数(定义未声明时为默认 8192)
    ///
    /// 窗口值是裁剪行为的核心参数,运行时须有权威读口——
    /// 换模型/调窗口后的验收、裁剪误触发排障的第一手核对面。
    pub context_window_tokens: usize,
    /// 实际可用输入预算 = `context_window_tokens - context_window_tokens / 4`
    /// (1/4 预留给响应,与 `ContextWindowManager` 裁剪口径一致)
    pub input_budget_tokens: usize,
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
    /// 批K 段7:正在等待用户回答的 session → oneshot sender(QuestionStore)
    pending_questions: QuestionStore,
    /// G17:Prometheus 指标(自定义 Registry,非全局)
    metrics: SharedMetrics,
    /// G16:鉴权配置(Bearer / 一次性 ticket,复用 G7 token 列表)
    ///
    /// HTTP 中间件(`auth_middleware`)对所有受保护路由单点执法:校验
    /// Authorization Bearer 或 `?ticket=` 一次性短票,成功后向 request
    /// extensions 注入 AuthVia(handler 可见经由,审计同权)。
    auth_config: AuthConfig,
    /// E1:工作目录(工具沙箱基目录,写域根)
    workdir: std::path::PathBuf,
    /// B 批「读宽写窄」:读域根(serve `--read-root` 注入;None=读写同域,
    /// 行为与 v1 一致)。读面工具绑定发生在启动期 toolkit 构建,wire 面仅
    /// 消费此值合成边界自报
    read_root: Option<std::path::PathBuf>,
    /// 38 号方案 X:agent 能力边界根(缺省=workdir;None=未显式指定)
    agent_root: Option<std::path::PathBuf>,
    /// E1:Workspace API 客户端(规则工具依赖)
    workspace_client: Arc<WorkspaceApiClient>,
    /// E1:预建 union toolkit(内置 + 规则,启动时组装一次)
    ///
    /// B16:升级为 RwLock —— serve 运行期 MCP 重载/restart 需向 union 注册
    /// 新工具;读路径(白名单过滤/工具查询)取读锁,写路径(mcp_api 端点)
    /// 取写锁,临界区内无 await。
    toolkit: Arc<std::sync::RwLock<ToolHandler>>,
    /// LLM 配置脱敏快照(凭据可视化状态端点;不含任何密钥内容)
    ///
    /// S5 模型切换:切换端点写回新 model 后同步更新快照 model 字段,
    /// RwLock 支撑运行期可变(临界区内无 await)。
    llm_status: Arc<std::sync::RwLock<LlmStatusSnapshot>>,
    /// LLM 配置统一解析产物(env 优先、toml 兜底;serve 与 CLI 共用同一解析链)
    ///
    /// serve 面各请求路径的 handler 构造统一走
    /// `LlmHandler::from_config(&state.llm_config())`,不再绕过配置体系
    /// 直读裸环境变量。
    /// S5 模型切换:切换端点写回新 model 后,handler 每请求现取现构即对
    /// 后续所有请求生效(运行期热切换,进程内存态,重启回落 env/toml 基线)。
    llm_config: Arc<std::sync::RwLock<LlmConfig>>,
    /// 会话索引(JSONL 本地持久化;对话与历史阶段的枚举面)
    session_index: Arc<crate::api::session_index::SessionIndex>,
    /// 会话消息本地快照(展示层非权威副本;TTL 回收后历史仍可见)
    snapshots: Arc<crate::api::snapshots::SnapshotStore>,
    /// 工作台配置(快照保留期;data/workbench_config.json)
    workbench_config: Arc<crate::api::snapshots::WorkbenchConfigStore>,
    /// 工作台设置(两级合并;data/workbench_settings.json + <workdir>/.evo/settings.json)
    workbench_settings: Arc<crate::api::settings::WorkbenchSettingsStore>,
    /// 文件系统事件广播 hub(WS 连接订阅;serve 启动时 spawn_watcher 填充)
    fs_events: crate::api::fs_watch::FsEventHub,
    /// 人工审计账(HumanGate 写面操作 JSONL;data/human_gate_ledger.jsonl)
    human_gate: Arc<crate::api::human_gate::HumanGateLedger>,
    /// LSP 语言服务器活动连接注册表(每语言单连接单实例)
    lsp_registry: Arc<crate::lsp::LangRegistry>,
    /// PTY 终端会话计数护栏(并发上限 8)
    pty_registry: Arc<crate::pty::PtyRegistry>,
    /// 任务运行注册表(run_id→运行记录;并发上限 4)
    task_manager: Arc<crate::tasks::TaskManager>,
    /// B14 语义索引服务(代码库符号图谱;serve 按 `index.enabled` 注入,
    /// None = 关闭态,/api/index/* 一律 503)
    index: Option<Arc<crate::index::IndexService>>,
    /// B16 MCP 连接态管理器(serve 按 mcp 配置注入,None = 未启用态,
    /// /api/mcp/* 一律 503;工具注册已在启动期并入 union toolkit)
    mcp: Option<Arc<crate::mcp::McpManager>>,
    /// B16 MCP 配置存储(.evo/mcp.toml 读写 + evo-agent.toml 回落;与 mcp
    /// 管理器成对注入,PUT 端点写回配置后重载生效)
    mcp_store: Option<Arc<crate::mcp::store::McpConfigStore>>,
    /// B23 MCP server 暴露面开关(缺省 false;/mcp 路由仅在 true 时挂载,
    /// fail-closed;serve 装配点从工作台设置 `mcpServer.enabled` 读取注入)
    mcp_server_enabled: bool,
    /// B24:/mcp 面共享事件轨道(治理等价补账——LLM 面工具调用有 EL 事件
    /// 轨道,/mcp tools/call 同权同账;专用 namespace "mcp" 与会话面账显式
    /// 分域,无会话语境 session_id=None;进程级开关 event_ledger_enabled
    /// 由 serve 启动期自 settings 初始化,关闭时记录 API 自行 no-op)
    mcp_ledger: std::sync::Arc<crate::agent::event_ledger::EventLedger>,
}

// =============================================================================
// S5-b: 会话模型持久化(data/llm_state.json;「最后一次人工显式切换」记录)
// 范式与 workbench settings user 层同构:固定相对路径过白名单门、tmp+rename
// 原子写、损坏件读侧降级(fail-soft)。仅 model 单键,零凭据物料。
// =============================================================================

/// S5-b: 持久化状态文件路径(workdir 相对固定路径,过白名单门防演化越界)
fn llm_state_path(workdir: &std::path::Path) -> Result<std::path::PathBuf, String> {
    crate::api::settings::ensure_fixed_relative(workdir, "data/llm_state.json")
}

/// S5-b: 读持久化模型名(缺文件/损坏/非对象/空值 → None 降级回静态配置)
pub fn read_llm_state_model(workdir: &std::path::Path) -> Option<String> {
    let path = llm_state_path(workdir).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("model")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// S5-b: 原子写持久化模型名(tmp+rename,父目录确保;与 settings save_atomic 同款)
fn write_llm_state_model(workdir: &std::path::Path, model: &str) -> Result<(), String> {
    let path = llm_state_path(workdir)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create llm state dir failed: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    let doc = serde_json::json!({ "model": model });
    let text = serde_json::to_string_pretty(&doc)
        .map_err(|e| format!("serialize llm state failed: {e}"))?;
    {
        use std::io::Write as _;
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("write llm state tmp failed: {e}"))?;
        f.write_all(text.as_bytes())
            .map_err(|e| format!("write llm state tmp failed: {e}"))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename llm state failed: {e}"))
}

impl AgentApiState {
    /// Create new Agent API state
    pub fn new(definitions: AgentDefinitionManager, evorule_client: EvoruleApiClient) -> Self {
        let metrics = Arc::new(
            Metrics::new().unwrap_or_else(|e| panic!("failed to create metrics registry: {}", e)),
        );
        // E1:new 兼容路径——构造默认 workspace_client + 空 toolkit
        let workspace_client = Arc::new(WorkspaceApiClient::new(evorule_client.base_url()));
        let toolkit = Arc::new(std::sync::RwLock::new(ToolHandler::new()));
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
    /// 注入预建的 union toolkit(内置 15 + 规则 26 = 41 工具)。
    pub fn new_with_metrics(
        definitions: AgentDefinitionManager,
        evorule_client: EvoruleApiClient,
        metrics: SharedMetrics,
        workdir: std::path::PathBuf,
        workspace_client: Arc<WorkspaceApiClient>,
        toolkit: Arc<std::sync::RwLock<ToolHandler>>,
    ) -> Self {
        // B24:/mcp 共享账构造前预取 client 副本(struct 表达式内原参数将被 move)
        let evorule_client_b24 = evorule_client.clone();
        let state = Self {
            definitions,
            evorule_client,
            running: Arc::new(Mutex::new(HashMap::new())),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            pending_questions: Arc::new(Mutex::new(HashMap::new())),
            metrics,
            auth_config: AuthConfig::disabled(),
            // B24:/mcp 共享事件轨道(专用 namespace "mcp";client 与 state 同源;
            // 构造廉价,EventLedger 内部惰性落链,离线/失败静默不阻塞执行面)
            mcp_ledger: {
                let store = crate::agent::memory_event::MemoryEventStore::new(
                    "mcp",
                    evorule_client_b24.clone(),
                );
                std::sync::Arc::new(crate::agent::event_ledger::EventLedger::new(
                    store,
                    "mcp",
                    evorule_client_b24.clone(),
                ))
            },
            session_index: Arc::new(crate::api::session_index::SessionIndex::new(
                workdir.join("data").join("session_index.jsonl"),
            )),
            snapshots: Arc::new(crate::api::snapshots::SnapshotStore::new(
                workdir.join("data").join("snapshots"),
            )),
            workbench_config: Arc::new(crate::api::snapshots::WorkbenchConfigStore::new(
                workdir.join("data").join("workbench_config.json"),
            )),
            workbench_settings: Arc::new(
                crate::api::settings::WorkbenchSettingsStore::new(&workdir)
                    .unwrap_or_else(|e| panic!("failed to init workbench settings store: {e}")),
            ),
            human_gate: Arc::new(crate::api::human_gate::HumanGateLedger::new(&workdir)),
            lsp_registry: Arc::new(crate::lsp::LangRegistry::new()),
            pty_registry: Arc::new(crate::pty::PtyRegistry::new()),
            task_manager: Arc::new(crate::tasks::TaskManager::new()),
            // B14 索引服务缺省不注入(关闭态 503);serve 启动按设置 with_index
            index: None,
            // B16 MCP 管理器缺省不注入;serve 启动按 mcp 配置 with_mcp
            mcp: None,
            mcp_store: None,
            mcp_server_enabled: false,
            fs_events: crate::api::fs_watch::FsEventHub::new(),
            workdir,
            // B 批「读宽写窄」:缺省 None(读写同域),serve 启动按 --read-root
            // with_read_root 注入
            read_root: None,
            // 38 号方案 X:agent 能力边界根(缺省=workdir;serve 启动按 --agent-root with_agent_root 注入)
            agent_root: None,
            workspace_client,
            toolkit,
            llm_status: Arc::new(std::sync::RwLock::new(LlmStatusSnapshot::unconfigured())),
            // 默认态:api_key 置空(SG-1 件1 key 归边后 handler 不消费此字段,
            // 仅配置层脱敏回显保留),待 cmd_serve 注入经统一解析的完整配置
            llm_config: Arc::new(std::sync::RwLock::new(LlmConfig {
                api_key: String::new(),
                ..LlmConfig::default()
            })),
        };
        // 治理写权开关启动期观测:敏感部署(agentTools.governanceWrite=false)
        // 一开服即显式曝光只读模式,避免「写工具静默消失」被误判为故障。
        // 沿用 provider-key 缺失告警的 eprintln 风格(stdout 启动日志面)。
        let (merged, _) = state.workbench_settings().merged();
        if !crate::api::serve_tools::governance_write_enabled(&merged) {
            eprintln!(
                "[governance] readonly mode: agentTools.governanceWrite=false -> 21 governance write tools gated off \
                 (rule write x8, rule_promote, ws_create, sandbox_start/close, dataset_create, publish x5, bundle export/import x3)"
            );
        }
        state
    }

    /// 注入 LLM 配置脱敏快照(builder 风格,供 serve 启动时调用)
    pub fn with_llm_status(self, snapshot: LlmStatusSnapshot) -> Self {
        *self.llm_status.write().expect("llm_status lock poisoned") = snapshot;
        self
    }

    /// 获取 LLM 配置脱敏快照(状态端点消费;克隆值,S5 切换端点同步更新)
    pub fn llm_status(&self) -> LlmStatusSnapshot {
        self.llm_status
            .read()
            .expect("llm_status lock poisoned")
            .clone()
    }

    /// 注入统一解析后的 LLM 配置(builder 风格,供 serve 启动时调用)
    ///
    /// `config.llm` 来自 `Config::load`(env 优先、toml 兜底的统一解析),
    /// serve 面各请求路径的 handler 构造与 CLI 共用同一语义。
    pub fn with_llm_config(self, config: LlmConfig) -> Self {
        *self.llm_config.write().expect("llm_config lock poisoned") = config;
        self
    }

    /// 获取统一解析后的 LLM 配置(请求路径 handler 构造消费)
    ///
    /// S5:返回克隆值——调用侧 `LlmHandler::from_config(&state.llm_config())`;
    /// 切换端点写回新 model 后,后续每请求现取即得新值(运行期热切换)。
    pub fn llm_config(&self) -> LlmConfig {
        self.llm_config
            .read()
            .expect("llm_config lock poisoned")
            .clone()
    }

    /// S5:当前生效模型名(清单中继端点 current 字段消费)
    pub fn current_llm_model(&self) -> String {
        self.llm_config().model
    }

    /// S5:切换会话模型(切换端点消费;白名单校验在中继侧完成,此处只做
    /// 写回配置+联动快照 model 字段,两锁无嵌套)
    pub fn switch_llm_model(&self, model: &str) -> LlmStatusSnapshot {
        self.llm_config
            .write()
            .expect("llm_config lock poisoned")
            .model = model.to_string();
        let mut snap = self.llm_status.write().expect("llm_status lock poisoned");
        snap.model = model.to_string();
        let snapshot = snap.clone();
        drop(snap);
        // S5-b:内存态切换成功后落盘(fail-soft:写失败仅告警不回滚内存态;
        // 唯一写入口=切换端点,与 workbench settings「端点唯一写入口」同构)
        self.persist_llm_state_model(model);
        snapshot
    }

    /// S5-b: 持久化「最后一次人工显式切换」到 data/llm_state.json,serve 重启
    /// 后由启动段读回恢复;写失败仅告警——下次启动回退静态配置值(可接受)
    fn persist_llm_state_model(&self, model: &str) {
        if let Err(e) = write_llm_state_model(&self.workdir, model) {
            eprintln!(
                "[llm] persist model state failed (in-memory switch kept): {}",
                e
            );
        }
    }

    /// 会话索引的引用(WS 处理器记录会话活动)
    pub fn session_index(&self) -> &crate::api::session_index::SessionIndex {
        &self.session_index
    }

    /// 快照存储的引用(WS 处理器 TurnEnd 落快照;transcript 回落读)
    pub fn snapshots(&self) -> &crate::api::snapshots::SnapshotStore {
        &self.snapshots
    }

    /// 工作台配置存储的引用(保留期 GET/PUT 端点 + 清理任务现读)
    pub fn workbench_config(&self) -> &crate::api::snapshots::WorkbenchConfigStore {
        &self.workbench_config
    }

    /// 工作台设置存储的引用(设置 GET/PUT/schema 端点)
    pub fn workbench_settings(&self) -> &crate::api::settings::WorkbenchSettingsStore {
        &self.workbench_settings
    }

    /// 文件系统事件 hub 的引用(WS 连接订阅 + serve 启动时传给 spawn_watcher)
    pub fn fs_events(&self) -> &crate::api::fs_watch::FsEventHub {
        &self.fs_events
    }

    /// G6:获取 SessionStore 的引用(供 G5 server 层做断开即取消等扩展)
    pub fn running_sessions(&self) -> &SessionStore {
        &self.running
    }

    /// G8:获取 ApprovalStore 的引用(供外部观察或测试)
    pub fn pending_approvals(&self) -> &ApprovalStore {
        &self.pending_approvals
    }

    /// 批K 段7:获取 QuestionStore 的引用(/answer 端点与 HttpAskUser 共享)
    pub fn pending_questions(&self) -> &QuestionStore {
        &self.pending_questions
    }

    /// G17:获取 SharedMetrics 的引用(供 runner 注入等)
    pub fn metrics(&self) -> &SharedMetrics {
        &self.metrics
    }

    /// G16:获取 AuthConfig 的引用(ticket 换票端点等使用)
    pub fn auth_config(&self) -> &AuthConfig {
        &self.auth_config
    }

    /// HumanGate:获取人工审计账的引用(写面操作/拒绝留痕)
    pub fn human_gate(&self) -> &crate::api::human_gate::HumanGateLedger {
        &self.human_gate
    }

    /// LSP:获取语言服务器活动连接注册表的引用(每语言单连接单实例)
    pub fn lsp_registry(&self) -> &crate::lsp::LangRegistry {
        &self.lsp_registry
    }

    /// PTY:获取终端会话计数护栏的引用(并发上限)
    pub fn pty_registry(&self) -> &crate::pty::PtyRegistry {
        &self.pty_registry
    }

    /// 任务:获取运行注册表的共享句柄(run_id→运行记录)
    pub fn task_manager(&self) -> Arc<crate::tasks::TaskManager> {
        self.task_manager.clone()
    }

    /// G16:获取 AgentDefinitionManager 的引用(WebSocket handler 构造 runner)
    pub fn definitions(&self) -> &AgentDefinitionManager {
        &self.definitions
    }

    /// G16:获取 EvoruleApiClient 的引用(WebSocket handler 调用 rewind 等 API)
    pub fn evorule_client(&self) -> &EvoruleApiClient {
        &self.evorule_client
    }

    /// E1:获取工作目录(工具沙箱基目录,写域根)
    pub fn workdir(&self) -> &std::path::Path {
        &self.workdir
    }

    /// B 批「读宽写窄」:获取读域根(None=读写同域,行为与 v1 一致)
    pub fn read_root(&self) -> Option<&std::path::Path> {
        self.read_root.as_deref()
    }

    /// B 批「读宽写窄」:注入读域根(builder pattern,供 cmd_serve 按
    /// `--read-root` 启动参数调用)
    pub fn with_read_root(mut self, read_root: std::path::PathBuf) -> Self {
        self.read_root = Some(read_root);
        self
    }

    /// 38 号方案 X:注入 agent 能力边界根(builder pattern,供 cmd_serve 按
    /// `--agent-root` 启动参数调用;None=缺省 workdir,行为零变化)
    pub fn with_agent_root(mut self, agent_root: std::path::PathBuf) -> Self {
        self.agent_root = Some(agent_root);
        self
    }

    /// 38 号方案 X:agent 能力边界根(None=未显式指定,语义=workdir)
    pub fn agent_root(&self) -> Option<&std::path::Path> {
        self.agent_root.as_deref()
    }

    /// E1:获取 WorkspaceApiClient 的引用(规则工具依赖)
    pub fn workspace_client(&self) -> &WorkspaceApiClient {
        &self.workspace_client
    }

    /// E1:获取预建 union toolkit(41 工具,启动时组装)
    ///
    /// B16:toolkit 为 RwLock 包装,返回读锁 guard(Deref 到 ToolHandler,
    /// owned 查询 get_tool/manifest/tool_names 调用方式不变)。guard 为
    /// 临时值,不得跨 await 持有;写路径经 mcp_api 端点取 write()。
    pub fn toolkit(&self) -> std::sync::RwLockReadGuard<'_, ToolHandler> {
        self.toolkit.read().expect("toolkit lock poisoned")
    }

    /// G16:注入鉴权配置(builder pattern,供 `router_with_auth` 链式调用)
    pub fn with_auth_config(mut self, auth_config: AuthConfig) -> Self {
        self.auth_config = auth_config;
        self
    }

    /// B14:注入语义索引服务(builder pattern,供 serve 启动按 `index.enabled` 调用;
    /// 未注入 = 关闭态,/api/index/* 一律 503)
    pub fn with_index(mut self, index: Arc<crate::index::IndexService>) -> Self {
        self.index = Some(index);
        self
    }

    /// B14:获取语义索引服务(关闭态为 None)
    pub fn index(&self) -> Option<&Arc<crate::index::IndexService>> {
        self.index.as_ref()
    }

    /// B16:注入 MCP 连接态管理器与配置存储(builder pattern,供 serve 启动
    /// 按 mcp 配置成对调用;未注入 = 未启用态,/api/mcp/* 一律 503)
    pub fn with_mcp(
        mut self,
        mcp: Arc<crate::mcp::McpManager>,
        store: Arc<crate::mcp::store::McpConfigStore>,
    ) -> Self {
        self.mcp = Some(mcp);
        self.mcp_store = Some(store);
        self
    }

    /// B16:获取 MCP 连接态管理器(未启用态为 None)
    pub fn mcp(&self) -> Option<&Arc<crate::mcp::McpManager>> {
        self.mcp.as_ref()
    }

    /// B16:获取 MCP 配置存储(未启用态为 None)
    pub fn mcp_store(&self) -> Option<&Arc<crate::mcp::store::McpConfigStore>> {
        self.mcp_store.as_ref()
    }

    /// B23:注入 MCP server 暴露面开关(builder pattern;缺省 false)
    pub fn with_mcp_server_enabled(mut self, enabled: bool) -> Self {
        self.mcp_server_enabled = enabled;
        self
    }

    /// B23:MCP server 暴露面开关(缺省 false)
    pub fn mcp_server_enabled(&self) -> bool {
        self.mcp_server_enabled
    }

    /// B16:获取 union toolkit 底层锁(mcp_api 写路径 PUT/reload/restart
    /// 经 write() 注册工具;读路径一律走 [`Self::toolkit`] 读 guard)
    pub fn toolkit_lock(&self) -> &std::sync::RwLock<ToolHandler> {
        &self.toolkit
    }

    /// B24:/mcp 面共享事件轨道(tools/call 治理等价留痕消费)
    pub fn mcp_ledger(&self) -> &std::sync::Arc<crate::agent::event_ledger::EventLedger> {
        &self.mcp_ledger
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
/// 鉴权中间件对 `/health` 豁免,其余路由需要 Authorization Bearer
/// (WS/SSE 握手不能带头时经 `POST /api/auth/ticket` 换一次性票走 `?ticket=`)。
/// `auth_config.enabled() == false` 时等价于无鉴权。
pub fn router_with_auth(state: AgentApiState, auth_config: crate::api::auth::AuthConfig) -> Router {
    // G16:把 auth_config 也注入到 state(ticket 换票端点读之)
    let state = state.with_auth_config(auth_config.clone());
    // B23:MCP server 暴露面开关(enabled=false 缺省 → /mcp 不挂载即 404,
    // fail-closed;true 时挂载,鉴权由下方 G16 中间件统一覆盖,与 /api/mcp/*
    // 工作台面同语义)
    let mcp_server_exposed = state.mcp_server_enabled();
    Router::new()
        .route("/health", axum::routing::get(health))
        .route("/metrics", axum::routing::get(metrics_handler))
        // 运行体身份查询(与启动横幅共享正本;鉴权内)
        .route("/version", axum::routing::get(get_version))
        // 凭据可视化:LLM 配置只读状态(脱敏,响应体不携带任何密钥内容)
        .route("/admin/llm-status", axum::routing::get(get_llm_status))
        .route("/admin/llm-models", axum::routing::get(get_llm_models))
        .route("/admin/llm-model", axum::routing::post(post_llm_model))
        // G1:serve 日志环形缓冲尾部(纯内存,进程重启即清;前端控制台日志 tab 轮询)
        .route("/admin/logs", axum::routing::get(get_admin_logs))
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
        // 批K 段7:回答正在等待的用户提问(按 session_id 查 QuestionStore)
        .route(
            "/agents/{agent_type}/answer",
            axum::routing::post(answer_agent),
        )
        // G16:WebSocket 双向流端点 — ?token= 鉴权 + 复用 run_continuation
        .route(
            "/api/sessions/{id}/ws",
            axum::routing::get(crate::api::ws_handler::ws_handler),
        )
        // 对话与历史:会话枚举(本地索引)+ 消息历史投影(evorule facts 权威读)
        .route("/api/sessions", axum::routing::get(list_sessions))
        // 扫尾查询:可恢复 run 列表(计划检查点在账且无终态标记;人类持剑——
        // 只列不续,续跑决策权在显式 resume 调用方)
        .route(
            "/api/sessions/resumable",
            axum::routing::get(list_resumable_runs),
        )
        .route(
            "/api/sessions/{id}/transcript",
            axum::routing::get(get_transcript),
        )
        // B21 PR-4/G2:ATIF v1.8 轨迹导出(journal × 审计链 × transcript 三源 join)
        .route("/api/sessions/{id}/atif", axum::routing::get(get_atif))
        // 会话检查点:版本时间线 / 手动回滚 / 打点·列举·恢复(与 WS Rewind 同源)
        .route(
            "/api/sessions/{id}/versions",
            axum::routing::get(get_session_versions),
        )
        .route(
            "/api/sessions/{id}/rewind",
            axum::routing::post(post_session_rewind),
        )
        .route(
            "/api/sessions/{id}/checkpoints",
            axum::routing::get(list_session_checkpoints).post(create_session_checkpoint),
        )
        .route(
            "/api/sessions/{id}/checkpoints/{cp_id}/restore",
            axum::routing::post(restore_session_checkpoint),
        )
        // 工作台配置(快照保留期;展示层,不触引擎面)
        .route(
            "/api/workbench/config",
            axum::routing::get(get_workbench_config).put(put_workbench_config),
        )
        // 工作台设置(两级合并;工作台数据面,不触引擎面;G16 中间件自动覆盖)
        .route(
            "/api/workbench/settings",
            axum::routing::get(crate::api::settings::get_settings)
                .put(crate::api::settings::put_settings),
        )
        .route(
            "/api/workbench/settings/schema",
            axum::routing::get(crate::api::settings::get_settings_schema),
        )
        // 治理叠加:进化信号只读代理(工作台信号徽标数据源;evorule-server 零改动)
        .route(
            "/api/sessions/{id}/evolution-signals",
            axum::routing::get(get_evolution_signals),
        )
        // 工作台文件面(IDE 消费):目录列表 / 读 / 写 —— 全部委托 builtin_tools
        // 的 file 工具实现(同一沙箱与校验);写面为人工编辑语义,见 file_api 模块文档
        .route(
            "/api/files/list",
            axum::routing::get(crate::api::file_api::list_dir),
        )
        // 目录浏览(批 I 段 2,首页/工作台目录选择器消费):自由列文件系统目录,
        // 只列目录名不触文件内容;鉴权同文件面(G16 中间件覆盖)
        .route(
            "/api/fs/browse",
            axum::routing::get(crate::api::file_api::browse_dirs),
        )
        // 公告列表(批 J1 段 1,主页公告卡消费):读 data/notices/notices.json,
        // fail-soft;鉴权同文件面(G16 中间件覆盖);不消费 X-Evo-Workspace(全局面)
        .route(
            "/api/notices",
            axum::routing::get(crate::api::notices_api::list_notices),
        )
        .route(
            "/api/files/read",
            axum::routing::get(crate::api::file_api::read_file),
        )
        .route(
            "/api/files/write",
            axum::routing::put(crate::api::file_api::write_file),
        )
        // 文件增删改(人工面):同沙箱委托实现 + 树写互斥;软删除进 .evo-trash
        .route(
            "/api/files/create",
            axum::routing::post(crate::api::file_api::create_file),
        )
        .route(
            "/api/files/move",
            axum::routing::post(crate::api::file_api::move_file),
        )
        .route(
            "/api/files",
            axum::routing::delete(crate::api::file_api::delete_file),
        )
        // 全局搜索与替换(人工面,委托 grep_core/replace_core 核心层):
        // 替换 apply=true 原子写,树写互斥由核心层内部持有
        .route(
            "/api/files/search",
            axum::routing::post(crate::api::file_api::search_files),
        )
        .route(
            "/api/files/replace",
            axum::routing::post(crate::api::file_api::replace_files),
        )
        // 附件上传(人工面)不在主链:见文末 upload_router_with_auth
        // (全局 1MB 钳制之后独立 merge,自带 14MB 限层与同款 G7 鉴权)
        // LSP 代理面(人工编辑辅助,零 agent 工具面):两语言门状态检测 + WS
        // 透明转发(每语言单连接单实例);G16 中间件自动覆盖,WS 内二次校验
        .route(
            "/api/lsp/status",
            axum::routing::get(crate::api::lsp_api::status),
        )
        .route(
            "/api/lsp/ws",
            axum::routing::get(crate::api::lsp_api::lsp_ws),
        )
        // DAP 代理面(运行和调试板块):适配器白名单检测 + WS 透明转发(每调试
        // 会话单连接单适配器;fail-closed 缺省白名单空=拒绝);G16 中间件自动
        // 覆盖,WS 内二次校验
        .route(
            "/api/dap/adapters",
            axum::routing::get(crate::api::dap_api::adapters),
        )
        .route(
            "/api/dap/ws",
            axum::routing::get(crate::api::dap_api::dap_ws),
        )
        // 工作台终端面(人的终端,治理分级=会话留痕+会话内零拦截;独立端点
        // 与会话 WS/LSP WS 互不牵连;G16 中间件自动覆盖,WS 内二次校验)
        .route(
            "/api/pty/ws",
            axum::routing::get(crate::api::pty_api::pty_ws),
        )
        // 工作台任务面(人的显式动作,永不自动运行;两段式取消+树杀;G16 中间件
        // 自动覆盖,WS 内二次校验)
        .route(
            "/api/tasks",
            axum::routing::get(crate::api::task_api::list_tasks),
        )
        .route(
            "/api/tasks/run",
            axum::routing::post(crate::api::task_api::run_task),
        )
        .route(
            "/api/tasks/{run_id}/cancel",
            axum::routing::post(crate::api::task_api::cancel_task),
        )
        .route(
            "/api/tasks/ws",
            axum::routing::get(crate::api::task_api::task_ws),
        )
        // AI 补全/Inline Chat(B12,人的显式/半显式触发;LLM 外发面,G16 中间件
        // 覆盖;prompt 边界与 .evoignore 排除在前端组装层执行)
        .route(
            "/api/completion",
            axum::routing::post(crate::api::completion_api::completion),
        )
        .route(
            "/api/inline-chat",
            axum::routing::post(crate::api::completion_api::inline_chat),
        )
        // B14 语义索引(结构面 v1):符号检索/就绪度/全量重建 —— 索引服务未注入
        // (index.enabled 关)时一律 503;人工触发的重建写面,无 agent 工具接点
        .route(
            "/api/index/search",
            axum::routing::get(crate::index::search_api::search),
        )
        .route(
            "/api/index/status",
            axum::routing::get(crate::index::search_api::status),
        )
        .route(
            "/api/index/rebuild",
            axum::routing::post(crate::index::search_api::rebuild),
        )
        // 工作台 Git 面(B3,人工面):SCM 视图/diff/暂存/丢弃/提交 —— 薄委托
        // git::GitOps 核心层;身份缺失只引导不代写 config;G16 中间件自动覆盖
        .route(
            "/api/git/status",
            axum::routing::get(crate::api::git_api::status),
        )
        .route(
            "/api/git/diff",
            axum::routing::get(crate::api::git_api::diff),
        )
        .route(
            "/api/git/stage",
            axum::routing::post(crate::api::git_api::stage),
        )
        .route(
            "/api/git/unstage",
            axum::routing::post(crate::api::git_api::unstage),
        )
        .route(
            "/api/git/discard",
            axum::routing::post(crate::api::git_api::discard),
        )
        .route(
            "/api/git/commit",
            axum::routing::post(crate::api::git_api::commit),
        )
        .route(
            "/api/git/identity",
            axum::routing::get(crate::api::git_api::identity),
        )
        // B15:AI commit message 生成(staged diff → 三级截断 → LLM 薄委托;只读不提交)
        .route(
            "/api/git/commit-message/generate",
            axum::routing::post(crate::api::git_api::generate),
        )
        // B16:MCP 工作台面(配置/连接态/重启;未启用态一律 503,G16 中间件自动覆盖)
        .route(
            "/api/mcp/servers",
            axum::routing::get(crate::api::mcp_api::list_servers)
                .put(crate::api::mcp_api::put_servers),
        )
        .route(
            "/api/mcp/servers/{name}/restart",
            axum::routing::post(crate::api::mcp_api::restart_server),
        )
        .route(
            "/api/mcp/status",
            axum::routing::get(crate::api::mcp_api::status_brief),
        )
        // 批次一 PR-4:审批策略工作台面(条件文件状态+试判定机械预检,零 LLM)
        .route(
            "/api/approval-policy",
            axum::routing::get(crate::api::approval_api::get_policy),
        )
        .route(
            "/api/approval-policy/trial",
            axum::routing::post(crate::api::approval_api::trial_judge),
        )
        // B25:编排可视化谱系面(只读 journal 投影;树 + 全局归属链 + 可委托
        // agent 清单,前端编排面板与会话列表徽标数据源)
        .route(
            "/api/orchestration/sessions",
            axum::routing::get(crate::api::orchestration_api::list_orchestration_sessions),
        )
        .route(
            "/api/orchestration/agents",
            axum::routing::get(crate::api::orchestration_api::list_orchestration_agents),
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
        // skills 注册审批管理面(C 形态):全清单/待审预览/批准/撤销 —— 定义期
        // 供应链准入,留痕在 registry.json 账本(不走会话期 ApprovalCallback)
        .merge(crate::api::skill_api::skills_routes())
        // B23:MCP server 暴露面(enabled=false → 空 Router,/mcp 404 fail-closed)
        .merge(mcp_server_routes(mcp_server_exposed))
        // auth ticket 换票端点:Bearer 换一次性短票(WS/SSE query 通行用),
        // 消费式校验执法在 auth_middleware
        .route(
            "/api/auth/ticket",
            axum::routing::post(issue_auth_ticket),
        )
        .with_state(state)
        // G7:鉴权中间件(对 /health、/metrics 豁免)
        .layer(axum::middleware::from_fn_with_state(
            auth_config,
            crate::api::auth::auth_middleware,
        ))
}

/// auth ticket 换票 handler — `POST /api/auth/ticket`
///
/// Bearer 凭据(经 auth_middleware 校验并注入 AuthVia::Bearer)换一次性短票,
/// 供不能携带自定义 Authorization 头的 WS/SSE 握手经 `?ticket=` 通行。
/// 票 TTL 30s、单次消费即焚(见 [`crate::api::auth::TicketStore`])。
pub async fn issue_auth_ticket(State(state): State<AgentApiState>) -> impl IntoResponse {
    let (ticket, ttl) = state.auth_config().issue_ticket();
    Json(serde_json::json!({ "ticket": ticket, "expires_in": ttl.as_secs() }))
}

/// S4:附件上传独立子路由 —— 自带 14MB 请求体限层与同款 G7 鉴权
///
/// 限层机制(实测定谳):
/// - 全局 1MB 真实钳制(evo-agent.rs serve 装配尾 `RequestBodyLimitLayer`)
///   罩住主链全部路由,`Router::layer` 只约束此前添加的路由,故本子路由须
///   由装配点在主链 1MB 层**之后** merge(后 merge 的路由不受既有 layer
///   约束)逃逸之——实测 1.4MB 请求体通过即证;
/// - axum Router 对每条路由**内建**缺省 2MB `DefaultBodyLimit`(不随 merge
///   时序变化,per-route 机制),实测 4.2MB 请求体仍 413 即证;以
///   `DefaultBodyLimit::max(14MB)` 抬限(14MB = 10MB 解码上限 ≈ 13.7MB
///   base64 + JSON 信封余量),该层即 axum 自身限流实现,单层足够。
///
/// 鉴权:与 [`router_with_auth`] 同款 —— G16 步骤把 auth_config 注入 state
/// (HumanGate 写面身份门读 state 内 auth_config,缺省 disabled 会误判
/// anonymous 403)+ 中间件层 token 校验;CORS 由装配点随子路由补挂。
pub fn upload_router_with_auth(state: AgentApiState, auth_config: AuthConfig) -> axum::Router {
    // G16:auth_config 注入 state(同 router_with_auth 口径)
    let state = state.with_auth_config(auth_config.clone());
    axum::Router::new()
        .route(
            "/api/files/upload",
            axum::routing::post(crate::api::file_api::upload_file)
                .layer(axum::extract::DefaultBodyLimit::max(14 * 1024 * 1024)),
        )
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            auth_config,
            crate::api::auth::auth_middleware,
        ))
}

/// B23:`/mcp` 暴露面子路由(enabled=true 才含路由;false 返回空 Router)
///
/// 挂载后鉴权由 G16 中间件统一覆盖(不在 PUBLIC_PATHS 豁免清单内)。
fn mcp_server_routes(enabled: bool) -> axum::Router<AgentApiState> {
    if enabled {
        axum::Router::new().route("/mcp", axum::routing::post(crate::mcp::server::mcp_post))
    } else {
        axum::Router::new()
    }
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

/// 运行体身份查询端点(工作台可见的版本面)
///
/// 路由:`GET /version` → [`RuntimeIdentity`] JSON(version / exe_mtime_epoch /
/// workdir)。与 `cmd_serve` 启动横幅共用 [`serve_tools::runtime_identity`]
/// 正本(单一事实源,排障时核对运行中 exe 与源码 HEAD 是否一致)。
/// 鉴权内(不进 PUBLIC_PATHS——workdir 绝对路径不出无鉴权面)。
async fn get_version(State(state): State<AgentApiState>) -> Json<RuntimeIdentity> {
    Json(crate::api::serve_tools::runtime_identity(state.workdir()))
}

/// LLM 配置只读状态端点(脱敏)
///
/// 路由:`GET /admin/llm-status` → [`LlmStatusSnapshot`]。
/// 凭据可视化消费面:报告 provider/model/端点与 API key 存在性、末 4 位提示、
/// 来源变量名。响应体**不携带任何密钥内容**(见 [`LlmStatusSnapshot`] 设计铁律)。
async fn get_llm_status(State(state): State<AgentApiState>) -> Json<LlmStatusSnapshot> {
    Json(state.llm_status())
}

/// `GET /admin/logs` 的查询参数
#[derive(Debug, serde::Deserialize)]
struct AdminLogsQuery {
    /// 取尾部条数(1..=500,缺省 200)
    n: Option<usize>,
}

/// G1:serve 日志环形缓冲尾部(纯内存,进程重启即清)
///
/// 路由:`GET /admin/logs?n=200`
///
/// 返回:`{count, lines: [..]}`——lines 为 tracing 格式化条目(通常一行,
/// 消息内含换行则多行)。前端控制台日志 tab 2s 轮询呈现;排障视图,
/// 不充当审计链。
async fn get_admin_logs(
    axum::extract::Query(params): axum::extract::Query<AdminLogsQuery>,
) -> Json<serde_json::Value> {
    let n = params.n.unwrap_or(200).clamp(1, 500);
    let lines = crate::api::log_buffer::tail_lines(n);
    let count = lines.len();
    Json(serde_json::json!({ "count": count, "lines": lines }))
}

/// S5:执行体模型清单中继 URL——executor_url 为 `/llm/execute` 全 URL 形态
/// (配置契约),去尾换 `/llm/models`;其余形态按原样拼接 `/llm/models`。
fn executor_models_url(executor_url: &str) -> String {
    format!(
        "{}/llm/models",
        executor_url.trim_end_matches("/llm/execute")
    )
}

/// S5:向执行体中继取模型清单(短超时,不挂 UI)。
///
/// 成功 → 执行体响应原值(`{ready, models[, note]}`);失败 → Err(人读详情)。
async fn relay_llm_models(cfg: &crate::config::LlmConfig) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("relay client build: {e}"))?;
    let mut req = client.get(executor_models_url(&cfg.executor_url));
    if let Some(tok) = &cfg.executor_token {
        req = req.header("Authorization", format!("Bearer {tok}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("executor unreachable ({}): {e}", cfg.executor_url))?;
    let status = resp.status();
    let payload: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("executor responded {status} with non-JSON body: {e}"))?;
    if !status.is_success() {
        return Err(format!("executor responded {status}: {payload}"));
    }
    Ok(payload)
}

/// 路由:`GET /admin/llm-models` → 执行体白名单清单中继(附 current)。
///
/// 正源唯一=执行体白名单物料(serve 不复制清单,杜绝双真相);执行体
/// 不可达 → 503 自诊断(fail-visible)。
async fn get_llm_models(State(state): State<AgentApiState>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let cfg = state.llm_config();
    match relay_llm_models(&cfg).await {
        Err(e) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({ "error": format!("模型清单中继失败: {e}") })),
        )
            .into_response(),
        Ok(mut payload) => {
            payload["current"] = serde_json::json!(state.current_llm_model());
            axum::Json(payload).into_response()
        }
    }
}

/// 路由:`POST /admin/llm-model` → 切换会话模型(body `{"model": "..."}`)。
///
/// 校验:①body 形状;②在执行体白名单内(正源执法面同一清单);③执行体
/// 不可达/未就绪一律拒切(不盲切,与执行体 fail-closed 姿态一致)。成功
/// → 写回共享配置+联动脱敏快照,后续每请求即生效。模型切换=纯配置态
/// 人工显式操作,不挂 HumanGate(审批链语义不动;白名单执法面不变)。
async fn post_llm_model(
    State(state): State<AgentApiState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(model) = body
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty())
    else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "body 需 {\"model\": \"<模型名>\"}(非空字符串)"
            })),
        )
            .into_response();
    };
    let cfg = state.llm_config();
    match relay_llm_models(&cfg).await {
        Err(e) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": format!("模型清单不可达,拒绝切换(不盲切): {e}")
            })),
        )
            .into_response(),
        Ok(payload) => {
            if payload["ready"] != serde_json::json!(true) {
                let note = payload["note"].as_str().unwrap_or("执行体未就绪");
                return (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(serde_json::json!({
                        "error": format!("扩展执行体未就绪,拒绝切换: {note}")
                    })),
                )
                    .into_response();
            }
            let allowlisted = payload["models"]
                .as_array()
                .map(|a| a.iter().filter_map(|m| m.as_str()).any(|m| m == model))
                .unwrap_or(false);
            if !allowlisted {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": format!(
                            "模型不在执行体准入白名单内: {model}(白名单以执行体执法面为准)"
                        ),
                        "models": payload["models"],
                    })),
                )
                    .into_response();
            }
            let snapshot = state.switch_llm_model(model);
            axum::Json(serde_json::json!({ "ok": true, "status": snapshot })).into_response()
        }
    }
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

    let (context_window_tokens, input_budget_tokens) = {
        // 与 AgentRunner 装配同口径:定义未声明 → 默认 8192;1/4 预留响应
        let window = def.context_window_tokens.unwrap_or(8192);
        (window, window - window / 4)
    };

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
        context_window_tokens,
        input_budget_tokens,
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

/// 消费流式事件至 Done,聚合为最终 [`AgentResult`]
///
/// 流式路径的终止语义(`run_streaming_inner` 契约):正常完成 / LLM 流中断 /
/// evorule 错误 auto_rewind 失败 / max_steps 超限**均以 `Done(AgentResult)`
/// 收尾**,故 Done 即权威结果。中间 `Error` 事件(如 auto_rewind 提示)不提前
/// 返回,其后的 Done 承载最终判定;`Err(AgentError)` 形态与「流意外结束且无
/// Done」(防御性,现实现不应发生)均组装为失败结果——不 panic 不静默。
///
/// 修法先例:delegate 同款(bb172b2「consumes run_streaming to Done」)。
///
/// 同时捕获流中的 `SessionCreated` 事件携带的会话 ID 并随结果返回
/// (run 端点据此挂工作台本地索引 + 填充响应 `session_id` 字段)。
async fn consume_to_done(
    mut stream: std::pin::Pin<Box<dyn Stream<Item = Result<AgentEvent, AgentError>> + Send>>,
) -> (AgentResult, Option<String>) {
    let mut session_id: Option<String> = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(AgentEvent::SessionCreated {
                session_id: sid, ..
            }) => {
                session_id = Some(sid);
            }
            Ok(AgentEvent::Done(r)) => return (r, session_id),
            Ok(AgentEvent::Error(e)) => {
                tracing::debug!(error = %e, "intermediate error event before Done");
            }
            Ok(_) => {}
            Err(e) => return (AgentResult::error(e.to_string(), 0, 0), session_id),
        }
    }
    (
        AgentResult::error("stream ended without Done event".to_string(), 0, 0),
        session_id,
    )
}

async fn run_agent(
    State(state): State<AgentApiState>,
    axum::extract::Path(agent_type): axum::extract::Path<String>,
    Json(req): Json<AgentRunRequest>,
) -> Result<Json<AgentRunResponse>, (StatusCode, String)> {
    // B17 持久会话:恢复语义预校验(fail-visible,先于定义加载与 runner 构造;
    // 请求形状级校验最前置,agent 定义是否存在不影响 400 判定)。仅 react 面
    // 消费——plan_execute 面携带忽略(同 step_timeout_secs 先例)。
    let exec_mode = req
        .execution
        .as_ref()
        .map(|e| e.mode.as_str())
        .unwrap_or("react");
    if exec_mode != "plan_execute" {
        if let Some(mode) = req.resume_mode.as_deref() {
            if mode != "crash" && mode != "continue" {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("unknown resume_mode '{mode}' (expected \"crash\" or \"continue\")"),
                ));
            }
            if req.resume_session_id.is_none() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "resume_mode requires resume_session_id".to_string(),
                ));
            }
            if mode == "continue" && req.goal.trim().is_empty() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "resume_mode=continue requires a non-empty goal (new input)".to_string(),
                ));
            }
        }
    }

    let mut def = load_serve_definition(&state, &agent_type)?;

    // 执行模式分流（先于 per-request 覆盖——覆盖块按值取走 req 字段；plan_execute
    // 分支不消费 def，行为等价于「覆盖之后、AgentRunner 构造之前」）：
    // mode=plan_execute 走外层驱动循环（plan-execute 挂 driver），react（缺省/
    // None）走既有单代理路径（下方逐字节不动）。未知 mode = 400。
    // 恢复旗标合法态:plan_execute=计划级恢复;react/缺省=崩溃恢复,或经
    // resume_mode=continue 选续聊语义(见下方恢复分支)。恢复仅经显式入参,
    // 不自动续跑。
    match exec_mode {
        "react" => {} // 现路径零变化
        "plan_execute" => return run_plan_execute_request(&state, &agent_type, &req).await,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "unknown execution.mode '{other}' (expected \"react\" or \"plan_execute\")"
                ),
            ))
        }
    }

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

    // E1:按白名单过滤 toolkit(serve 模式安全隔离);agentTools.* 开关裁剪
    // 文件增删改工具面(file_create/move/delete,开关关=契约同步消失)
    let (merged_settings, _) = state.workbench_settings().merged();
    let mut filtered = crate::api::serve_tools::build_filtered_toolkit_with_switches(
        &state.toolkit(),
        &def.tools,
        &merged_settings,
    );
    // 请求级工具子集(allowlist 只收不放,与 def 面求交;零命中条目 400;
    // None = 零变化)。先于开关收紧步——restrict_tools_to_surface 随后把
    // def.tools 同步收敛(白名单声明=执行器=LLM 契约三者一致自动保持)
    if let Some(req_tools) = &req.tools {
        crate::api::serve_tools::apply_request_tool_allowlist(&mut filtered, req_tools)
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }
    // 开关裁剪后同步收紧 def.tools(白名单声明=执行器=LLM 契约三者一致,
    // 防 from_definition 早失败校验把「声明了默认关工具」误判为配置错误)
    crate::api::serve_tools::restrict_tools_to_surface(&mut def.tools, &filtered);
    // P1 执行桥:run 请求携带容器名 → shell_exec 替换为 docker-exec 后端
    // (per-request 替换,不影响启动期 union toolkit;None = 宿主语义零变化)
    // G1 执行环境 profile:请求级 > 定义级(未知值 fail-fast 400;None = 零变化)
    let execution_profile = crate::api::serve_tools::resolve_execution_profile(
        req.execution_profile.as_deref(),
        def.execution_profile.as_deref(),
    )
    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    if let Some(container) = &req.container {
        crate::api::serve_tools::with_shell_exec_backend(
            &mut filtered,
            container,
            execution_profile
                .unwrap_or(crate::builtin_tools::shell_exec::ExecutionProfile::Workbench),
        );
    } else if let Some(profile) = execution_profile {
        crate::api::serve_tools::with_shell_exec_profile(&mut filtered, profile);
    }
    // P2 沙箱根映射:请求携带 workspace → 以声明态注入能力边界
    // (先于 M5-a 接线,fs_safety 判据零改动;None = 零变化)
    if let Some(ws) = &req.workspace {
        crate::api::serve_tools::apply_workspace_boundary(&mut def, ws)
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }
    // M5-a:能力边界接线(显式声明重绑 file 工具沙箱 + 生效边界注入 runner)
    // B 批「读宽写窄」:读域根经 state 注入(--read-root);请求带 workspace 时
    // 声明优先,read_root 不参与(workspace=请求级完整意志,同域边界)
    // 38 号方案 X:能力边界根优先取 --agent-root(缺省=workdir)
    let boundary_root = state.agent_root().unwrap_or(state.workdir());
    let capability_boundary = crate::api::serve_tools::wire_capability_boundary_with_read_root(
        &mut filtered,
        &def,
        boundary_root,
        state.read_root(),
    );
    // S2 治理门禁段(S2 前移批):三段合并构造,经 runner 配置进组装层
    // (v2 序=权威紧跟 S1 身份段之后;不再改写 def.system_prompt——K-17②)
    let governance_segment =
        crate::api::serve_tools::build_governance_segment(&state.evorule_client, &def.tools).await;
    // 人工审查开合(2026-09-28):def move 前捕获审批模式
    // 档位解析单源:定义级优先,未指定回落设置键 approval.mode.shellExec
    let approval_mode =
        crate::api::ws_handler::resolve_approval_mode(&state, def.approval_mode.as_deref());
    // 39 号项 A:项目记忆段(AGENTS.md 层级,随 serve workdir 解析)
    let agents_md = crate::agent::runner::load_agents_md_segment(state.workdir());
    let runner = AgentRunner::from_definition(
        def,
        state.evorule_client.clone(),
        filtered,
        Some(crate::io_handlers::LlmHandler::from_config(
            &state.llm_config(),
        )), // llm_handler — serve 与 CLI 共用统一解析产物(env 优先、toml 兜底)
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .with_agents_md_segment(agents_md)
    .with_user_hooks(
        match crate::agent::user_hooks::WiredHooks::load_from_workdir(state.workdir()) {
            Ok(v) => v.map(std::sync::Arc::new),
            Err(e) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("user hooks invalid: {e}"),
                ))
            }
        },
    )
    .with_capability_boundary(capability_boundary)
    // 执行环境 profile 标记随 tool_result 落账(trusted 审计明文可追溯;
    // 未声明=None=落账无标记,零变化)
    .with_execution_profile_marker(execution_profile.as_ref().map(|p| p.as_str()))
    // serve 面 delegate 执行能力接线(B25 批顺手修,CLI run 主路径同款镜像):
    // from_definition 对 "delegate" 仅预放行校验,此前 serve 三路径(run/stream/ws)
    // 均无人调用 with_delegate_context → 声明了 delegate 的定义(general 等)
    // LLM 不可见、tool_call 无处解析。子代理 journal 同目录落账,谱系面可见。
    .with_delegate_context(
        crate::agent::delegate::DelegateContext::new(
            &agent_type,
            state.definitions().clone(),
            state.evorule_client.clone(),
        )
        .with_journal_dir(state.workdir.join("data").join("sessions")),
    )
    // 查账工具族接线（PR-11a）：以本会话态重绑 handler 内查账工具实例
    .wire_accounting(&state.workdir)
    // S2 治理门禁段(v2 前移批:治理段进组装层独立槽位)
    .with_governance_segment(governance_segment.clone());
    // G8:注入审批决策端(人工审查开合,与 WS 面 construct_runner 三态同口径;
    // 补接前本端点仅 auto_policy 二元路由,policy 档静默缺失):
    // auto_policy=PolicyApproval 判定式(无人值守,理由必产,现状不变);
    // policy=PolicyReview 评审式(独立 LLM 对照条件清单逐条判定),并同产出
    // Runaway 护栏策略注入(本端点改消费 run_streaming,护栏
    // 挂点在流式面随之生效);manual 态维持本端点既定行为(不注入=缺省
    // 拒绝,安全优先,行为零变化)
    let runner = match approval_mode.as_deref() {
        Some("auto_policy") => {
            // 红线豁免前置闸(与 WS 面同口径):命中且未显式关闭时回落人工审批卡
            let override_flag = crate::api::ws_handler::approval_exemption_override(&state);
            let guarded = crate::agent::approval::RedLineGuardApproval::new(
                std::sync::Arc::new(crate::agent::approval::PolicyApproval),
                std::sync::Arc::new(crate::governance::GovAuditedApproval::new(
                    std::sync::Arc::new(crate::agent::approval::HttpApproval::new(
                        state.pending_approvals().clone(),
                    )),
                )),
                override_flag,
            );
            let callback: std::sync::Arc<dyn crate::agent::approval::ApprovalCallback> =
                std::sync::Arc::new(guarded);
            runner.with_approval_callback(callback)
        }
        Some("policy") => {
            let (approval_callback, runaway_policy) =
                crate::api::ws_handler::build_approval_callback(&state, Some("policy"));
            runner
                .with_approval_callback(approval_callback)
                .with_runaway_policy(runaway_policy)
        }
        // manual/缺省:现状行为零变化
        _ => runner,
    };
    // 批K 段7:人在环问答端注入(与审批模式同口径分派):
    // auto_policy=无人值守语义 → CliAskUser 立即返回无人态(不模拟回答、不干等);
    // manual/policy/缺省 → HttpAskUser,pending 项经 POST /agents/{t}/answer 送达
    let runner = match approval_mode.as_deref() {
        Some("auto_policy") => {
            runner.with_ask_user_callback(std::sync::Arc::new(CliAskUser { auto: true }))
        }
        _ => runner.with_ask_user_callback(std::sync::Arc::new(HttpAskUser {
            pending: state.pending_questions().clone(),
        })),
    };
    // 执行桥后端上下文注入轨迹采集器:请求携带容器名(docker-exec 后端)时
    // shell_exec 轨迹成形分流 program 旗标(分流≠删检);宿主后端零变化
    let runner = if let Some(container) = &req.container {
        runner.with_trace_exec_backend(container)
    } else {
        runner
    };
    let runner = runner
        .with_metrics(state.metrics.clone())
        // B21 PR-1:注入 journal 目录 — 会话事件流落盘(会话唯一真相源)
        .with_journal_dir(state.workdir.join("data").join("sessions"))
        .with_workspace_checkpoint(&state.workdir)
        // B21 D3:注入主动压缩策略(longSession.compaction.* 设置映射;
        // CLI 面不读设置=None=不启用,行为零变化)
        .with_compaction_policy(crate::agent::runner::CompactionPolicy::from_settings(
            &merged_settings,
        ))
        // 注入 delegate 上下文——serve 模式多代理委托通路接线
        // 治理门禁段随上下文下放(子代理 S2 槽位与主 runner 同源,治理
        // 纪律无豁免面);journal 目录同源传入=子代理事件流落同目录
        .with_delegate_context(
            crate::agent::delegate::DelegateContext::new(
                &agent_type,
                state.definitions.clone(),
                state.evorule_client.clone(),
            )
            .with_journal_dir(state.workdir.join("data").join("sessions"))
            .with_governance_segment(governance_segment.clone())
            // 子代理工具契约:union toolkit 快照下放(此前 react 面子代理
            // 零工具契约=纯文本执行器)
            .with_toolkit((*state.toolkit()).clone(), &state.workdir)
            // 委托窗口 token 归账(差值随 SpawnRecord 落 journal)
            .with_token_counter(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0))),
        );

    // 改消费流式 ReAct 回路(delegate 同款修法,bb172b2 先例)——
    // 非流式 run() 是单发桥接(LLM 返 tool_calls 即返、工具不执行=能力面假象,
    // s113 实证 success=true+content=""),本端点改为消费 run_streaming 至 Done:
    // 多轮工具回喂在服务端执行完毕后聚合返回,对外 AgentRunResponse 结构零变化。
    // approval 注入随 approval_mode(auto_policy→PolicyApproval,见上;manual 态
    // 不注入=candidate 缺省拒绝);cancel token 不注册
    // (原 run() 路径亦不可 cancel,行为等价)。
    // 捕获流中 SessionCreated 的会话 ID,挂工作台本地索引(与 WS 面同
    // 口径,fail-soft;record 读时去重合并)+ 响应携带 session_id(会话关联收口)。
    let index_title: String = req.goal.chars().take(60).collect();
    // 恢复旗标(react 面):resume_session_id 在场按 resume_mode 分流——
    // crash(缺省)=崩溃恢复,无新输入(goal 字段忽略),悬挂工具处置后自然
    // 续完当前 turn,响应 session_id 取恢复会话名(continuation 不发
    // SessionCreated);continue=续聊,先 get_state 活性预检(单 runner 不被
    // 消费;与 resume_crashed 的预检先例同源):活会话 → run_continuation,
    // 死会话(引擎侧 404)→ 重建回退——两级取历史(活投影→本地快照)合成
    // 重建回注块后以新会话续跑,响应 rebuilt_from_session=旧会话名;
    // 其他预检错误 → 502 fail-visible
    let mut rebuilt_from_session: Option<String> = None;
    let (result, session_id) = if let Some(sid) = &req.resume_session_id {
        match req.resume_mode.as_deref().unwrap_or("crash") {
            "continue" => {
                // 空 goal 校验已在最前置块完成(continue 无新输入即 400)
                match state.evorule_client().get_state(sid).await {
                    Ok(_) => {
                        let (r, _) = consume_to_done(runner.run_continuation(
                            sid.clone(),
                            req.goal,
                            Vec::new(),
                        ))
                        .await;
                        (r, Some(sid.clone()))
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if crate::api::snapshots::is_session_gone(&err_str) {
                            // 死会话重建回退:不静默丢上下文——历史不可得即 fail-visible
                            let history = load_rebuild_history(&state, sid, &agent_type).await?;
                            if history.is_empty() {
                                return Err((
                                    StatusCode::BAD_REQUEST,
                                    format!(
                                        "cannot continue session '{sid}': engine session is gone and no recoverable history (no local snapshot); use a fresh run"
                                    ),
                                ));
                            }
                            let composed = compose_rebuild_context(&history, &req.goal);
                            let (r, new_sid) =
                                consume_to_done(runner.run_streaming(composed, Vec::new())).await;
                            rebuilt_from_session = Some(sid.clone());
                            (r, new_sid)
                        } else {
                            return Err((
                                StatusCode::BAD_GATEWAY,
                                format!("resume precheck failed: {err_str}"),
                            ));
                        }
                    }
                }
            }
            _ => {
                let (r, _) = consume_to_done(runner.resume_crashed(sid.clone())).await;
                (r, Some(sid.clone()))
            }
        }
    } else {
        consume_to_done(runner.run_streaming(req.goal, Vec::new())).await
    };
    if let Some(sid) = &session_id {
        state
            .session_index()
            .record(&crate::api::session_index::SessionIndexEntry {
                session_id: sid.clone(),
                agent_type: agent_type.clone(),
                created_at: crate::api::session_index::unix_now(),
                last_active: crate::api::session_index::unix_now(),
                title: index_title,
            });
    }

    if result.success {
        info!(
            agent_type = agent_type,
            steps = result.steps,
            duration_ms = result.duration_ms,
            "Agent execution completed"
        );
    } else {
        warn!(
            agent_type = agent_type,
            error = ?result.error,
            "Agent execution failed"
        );
    }
    Ok(Json(AgentRunResponse {
        success: result.success,
        content: result.content,
        steps: result.steps,
        duration_ms: result.duration_ms,
        error: result.error,
        session_id,
        rebuilt_from_session,
        plan_stats: None,
        run_session_id: None,
    }))
}

/// plan-execute 模式请求处理（serve 挂 driver）：serve 面 toolkit + 治理门禁段
/// 装配 → 外层驱动循环 → 响应映射。
///
/// - 成功：`session_id` = marks_session（会话关联收口），`plan_stats` 透出七项；
/// - 失败：与 react 路径执行失败同封套（HTTP 200 + success=false + error 原文），
///   消费者处理口径统一；marks_session 创建失败即断（fail-fast，留痕是硬义务）。
///
/// 扫尾查询:可恢复 run 列表(只读投影;恢复仅经显式 resume_session_id,
/// 不自动续跑)。单文件读败跳过并告警——列表面健壮性优先。
async fn list_resumable_runs(
    State(state): State<AgentApiState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let dir = state.workdir.join("data").join("sessions");
    match crate::agent::driver::scan_resumable_runs(&dir) {
        Ok(runs) => Ok(Json(serde_json::json!({ "resumable": runs }))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

async fn run_plan_execute_request(
    state: &AgentApiState,
    agent_type: &str,
    req: &AgentRunRequest,
) -> Result<Json<AgentRunResponse>, (StatusCode, String)> {
    let started = std::time::Instant::now();
    // serve 安全隔离语义优先：E1 union + agentTools.* 开关裁剪（白名单传全量
    // =仅裁开关；delegate 子代理再按各自 def.tools 白名单过滤），不重建 CLI
    // union toolkit
    let (merged_settings, _) = state.workbench_settings().merged();
    let all_tools = state.toolkit().tool_names();
    let mut delegate_toolkit = crate::api::serve_tools::build_filtered_toolkit_with_switches(
        &state.toolkit(),
        &all_tools,
        &merged_settings,
    );
    // Delegate-node exec bridge: with a run-request container name, delegate
    // nodes' shell_exec follows the same docker-exec backend as the react path
    // (per-agent filtered copies inherit the backend through entry cloning);
    // None = host backend, existing behavior unchanged.
    if let Some(container) = &req.container {
        crate::api::serve_tools::with_shell_exec_backend(
            &mut delegate_toolkit,
            container,
            crate::builtin_tools::shell_exec::ExecutionProfile::Workbench,
        );
    }
    // 治理门禁段随上下文下放（与 react 路径 DelegateContext 同源——治理纪律
    // 无豁免面）；L2 前馈按 serve union 全量工具面触发
    let governance_segment =
        crate::api::serve_tools::build_governance_segment(&state.evorule_client, &all_tools).await;
    // 驱动限额与 CLI workflow 子命令缺省一致（阈值禁入 PlanFact）
    let limits = crate::agent::driver::DriverLimits {
        max_replan: 3,
        max_recuts: 3,
        max_wall_ms: Some(1_800_000),
        max_tokens: None,
    };
    // 粒执行器步超时请求级覆写(秒→Duration;0=不限步超时，与定义面
    // 校验语义一致;None = 粒定义值零变化)
    let step_timeout_override = req.step_timeout_secs.map(std::time::Duration::from_secs);
    let run = crate::agent::driver::run_plan_execute(
        state.evorule_client.clone(),
        state.definitions.clone(),
        delegate_toolkit,
        &state.workdir,
        governance_segment,
        &req.goal,
        limits,
        3,                                // max_depth（与 CLI workflow 子命令缺省一致）
        5,                     // max_concurrent（与 CLI workflow 子命令缺省一致，0=不限流）
        req.container.clone(), // 判据 v0：run 请求容器名透传（P1 执行桥同源）
        req.resume_session_id.as_deref(), // 恢复旗标（None = 全新跑）
        step_timeout_override, // 粒执行器步超时覆写（None = 定义值零变化）
    )
    .await;
    let (outcome, marks_session) = match run {
        Ok(pair) => pair,
        Err(e) => {
            warn!(
                agent_type = agent_type,
                error = %e,
                "plan-execute execution failed"
            );
            return Ok(Json(AgentRunResponse {
                success: false,
                content: String::new(),
                steps: 0,
                duration_ms: started.elapsed().as_millis() as u64,
                error: Some(e),
                session_id: None,
                rebuilt_from_session: None,
                plan_stats: None,
                run_session_id: None,
            }));
        }
    };
    info!(
        agent_type = agent_type,
        plan_versions = outcome.stats.plan_versions,
        replans = outcome.stats.replans,
        nodes_executed = outcome.stats.nodes_executed,
        "plan-execute execution completed"
    );
    // 会话关联收口：marks_session 挂工作台本地索引（与 react 路径同口径，fail-soft）
    let index_title: String = req.goal.chars().take(60).collect();
    state
        .session_index()
        .record(&crate::api::session_index::SessionIndexEntry {
            session_id: marks_session.clone(),
            agent_type: agent_type.to_string(),
            created_at: crate::api::session_index::unix_now(),
            last_active: crate::api::session_index::unix_now(),
            title: index_title,
        });
    Ok(Json(AgentRunResponse {
        success: true,
        content: outcome.content,
        steps: outcome.stats.nodes_executed as usize,
        duration_ms: started.elapsed().as_millis() as u64,
        error: None,
        session_id: Some(marks_session),
        rebuilt_from_session: None,
        run_session_id: outcome.run_session_id.clone(),
        plan_stats: Some(PlanStatsResponse {
            plan_versions: outcome.stats.plan_versions,
            replans: outcome.stats.replans,
            nodes_executed: outcome.stats.nodes_executed,
            wall_ms: outcome.stats.wall_ms,
            repeated_nodes: outcome.stats.repeated_nodes,
            tokens_used: outcome.stats.tokens_used,
            replan_tokens: outcome.stats.replan_tokens,
        }),
    }))
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

    // E1:按白名单过滤 toolkit(serve 模式安全隔离);agentTools.* 开关裁剪
    // 文件增删改工具面(同 run_agent 口径,三路径共用开关语义)
    let (merged_settings, _) = state.workbench_settings().merged();
    let mut filtered = crate::api::serve_tools::build_filtered_toolkit_with_switches(
        &state.toolkit(),
        &def.tools,
        &merged_settings,
    );
    // 请求级工具子集(同 run_agent 口径:allowlist 只收不放,零命中条目 400;
    // None = 零变化;先于开关收紧步保三者一致)
    if let Some(req_tools) = &req.tools {
        crate::api::serve_tools::apply_request_tool_allowlist(&mut filtered, req_tools)
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }
    // 开关裁剪后同步收紧 def.tools(同 run_agent 口径)
    crate::api::serve_tools::restrict_tools_to_surface(&mut def.tools, &filtered);
    // P1 执行桥:同 run_agent 口径(请求携带容器名 → shell_exec 转 docker-exec)
    // G1 执行环境 profile:同 run_agent 口径(请求级 > 定义级,未知值 fail-fast 400)
    let execution_profile = crate::api::serve_tools::resolve_execution_profile(
        req.execution_profile.as_deref(),
        def.execution_profile.as_deref(),
    )
    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    if let Some(container) = &req.container {
        crate::api::serve_tools::with_shell_exec_backend(
            &mut filtered,
            container,
            execution_profile
                .unwrap_or(crate::builtin_tools::shell_exec::ExecutionProfile::Workbench),
        );
    } else if let Some(profile) = execution_profile {
        crate::api::serve_tools::with_shell_exec_profile(&mut filtered, profile);
    }
    // P2 沙箱根映射:同 run_agent 口径(请求携带 workspace → 声明态注入边界)
    if let Some(ws) = &req.workspace {
        crate::api::serve_tools::apply_workspace_boundary(&mut def, ws)
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }
    // M5-a:能力边界接线(同 run_agent 口径;B 批「读宽写窄」读域根经 state 注入)
    // 38 号方案 X:能力边界根优先取 --agent-root(缺省=workdir)
    let boundary_root = state.agent_root().unwrap_or(state.workdir());
    let capability_boundary = crate::api::serve_tools::wire_capability_boundary_with_read_root(
        &mut filtered,
        &def,
        boundary_root,
        state.read_root(),
    );
    // S2 治理门禁段(S2 前移批):同 run_agent 口径
    let governance_segment =
        crate::api::serve_tools::build_governance_segment(&state.evorule_client, &def.tools).await;
    // 人工审查开合(2026-09-28):def move 前捕获审批模式
    // G8 审批决策端 + Runaway 护栏(与 WS 面 construct_runner 同口径;
    // 补接前本端点为 auto_policy/其余二元路由,policy 档静默降 manual=缺口):
    // build_approval_callback 三态一次判别双产出——auto_policy=PolicyApproval/
    // policy=PolicyReview 评审式+Runaway 护栏策略(本端点消费 run_streaming,
    // 护栏挂点随之生效)/其余=HttpApproval 60s 人工审批窗(manual 态现状行为)
    let resolved_mode =
        crate::api::ws_handler::resolve_approval_mode(&state, def.approval_mode.as_deref());
    let (approval_callback, runaway_policy) =
        crate::api::ws_handler::build_approval_callback(&state, resolved_mode.as_deref());
    // 39 号项 A:项目记忆段(AGENTS.md 层级,随 serve workdir 解析)
    let agents_md = crate::agent::runner::load_agents_md_segment(state.workdir());
    let runner = AgentRunner::from_definition(
        def,
        state.evorule_client.clone(),
        filtered,
        Some(crate::io_handlers::LlmHandler::from_config(
            &state.llm_config(),
        )), // llm_handler — serve 与 CLI 共用统一解析产物(env 优先、toml 兜底)
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .with_agents_md_segment(agents_md)
    .with_user_hooks(
        match crate::agent::user_hooks::WiredHooks::load_from_workdir(state.workdir()) {
            Ok(v) => v.map(std::sync::Arc::new),
            Err(e) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("user hooks invalid: {e}"),
                ))
            }
        },
    )
    .with_capability_boundary(capability_boundary)
    // 执行环境 profile 标记随 tool_result 落账(同 run_agent 口径)
    .with_execution_profile_marker(execution_profile.as_ref().map(|p| p.as_str()))
    // serve 面 delegate 执行能力接线(同 run_agent 端点;SSE 流式面)
    .with_delegate_context(
        crate::agent::delegate::DelegateContext::new(
            &agent_type,
            state.definitions().clone(),
            state.evorule_client.clone(),
        )
        .with_journal_dir(state.workdir.join("data").join("sessions"))
        // 子代理工具契约:union toolkit 快照下放(此前 SSE 面子代理零工具契约)
        .with_toolkit((*state.toolkit()).clone(), &state.workdir)
        // 治理段同源下放(下方主 runner 消费同源值)
        .with_governance_segment(governance_segment.clone())
        // 委托窗口 token 归账(差值随 SpawnRecord 落 journal)
        .with_token_counter(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0))),
    )
    // 查账工具族接线（PR-11a）：以本会话态重绑 handler 内查账工具实例
    .wire_accounting(&state.workdir)
    // S2 治理门禁段(v2 前移批:治理段进组装层独立槽位)
    .with_governance_segment(governance_segment)
    // G8:注入审批决策端(三态同口径,构造期已双产出,见上)
    .with_approval_callback(approval_callback)
    // Runaway 护栏(policy 档;None=不启用,行为零变化)
    .with_runaway_policy(runaway_policy)
    // G17:注入 metrics — runner 在 session/step/LLM/工具关键路径插桩
    .with_metrics(state.metrics.clone())
    // B21 PR-1:注入 journal 目录 — 会话事件流落盘(会话唯一真相源)
    .with_journal_dir(state.workdir.join("data").join("sessions"))
    .with_workspace_checkpoint(&state.workdir)
    // B21 D3:注入主动压缩策略(longSession.compaction.* 设置映射;同 run_agent 口径)
    .with_compaction_policy(crate::agent::runner::CompactionPolicy::from_settings(
        &merged_settings,
    ));

    // 执行桥后端上下文注入轨迹采集器(同 run_agent 口径):请求携带容器名
    // (docker-exec 后端)时 shell_exec 轨迹成形分流 program 旗标(分流≠删检);
    // 宿主后端零变化
    let runner = if let Some(container) = &req.container {
        runner.with_trace_exec_backend(container)
    } else {
        runner
    };

    // 批K 段7:人在环问答端注入(与审批模式同口径分派,同 run_agent/WS 面):
    // auto_policy=无人值守语义 → CliAskUser 立即返回无人态(不模拟回答、不干等);
    // manual/policy/缺省 → HttpAskUser,pending 项经 POST /agents/{t}/answer 送达
    let runner = match resolved_mode.as_deref() {
        Some("auto_policy") => {
            runner.with_ask_user_callback(std::sync::Arc::new(CliAskUser { auto: true }))
        }
        _ => runner.with_ask_user_callback(std::sync::Arc::new(HttpAskUser {
            pending: state.pending_questions().clone(),
        })),
    };

    // G6:clone token(run_streaming 会 move runner),存入 SessionStore 供 /cancel 使用
    let cancel_token = runner.cancel_token().clone();
    let store = state.running.clone();
    // G17:clone metrics 供 SSE 连接守卫使用
    let sse_metrics = state.metrics.clone();

    // 用 stream! 包裹,在流入口创建 SseConnectionGuard(RAII),
    // 流结束(正常 / error / 客户端断开)时自动 dec sse_connections。
    // SSE 面同挂工作台本地索引(SessionCreated 时机,对齐 ws_handler;
    // title=goal 截 60 字符;record 自身 fail-soft)。
    let index_title: String = req.goal.chars().take(60).collect();
    let agent_type_for_index = agent_type.clone();
    let sse_stream = async_stream::stream! {
        let _sse_guard = SseConnectionGuard::new(sse_metrics);
        let mut event_stream = runner.run_streaming(req.goal, Vec::new());

        // G6:在 map 中拦截 SessionCreated(注册 token)/ Done / Error / Err(注销 token)
        let mut current_session: Option<String> = None;
        while let Some(result) = event_stream.next().await {
            match &result {
                Ok(AgentEvent::SessionCreated { session_id, .. }) => {
                    current_session = Some(session_id.clone());
                    if let Ok(mut map) = store.lock() {
                        map.insert(session_id.clone(), cancel_token.clone());
                    }
                    // 会话索引落一行(fail-soft,仅展示辅助)
                    state.session_index().record(
                        &crate::api::session_index::SessionIndexEntry {
                            session_id: session_id.clone(),
                            agent_type: agent_type_for_index.clone(),
                            created_at: crate::api::session_index::unix_now(),
                            last_active: crate::api::session_index::unix_now(),
                            title: index_title.clone(),
                        },
                    );
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

/// 批K 段7:问答回传请求的 body
#[derive(Debug, Deserialize)]
pub struct AnswerRequest {
    /// 要回答的 session ID
    pub session_id: String,
    /// 问题 ID(ask_user 帧携带;不匹配时请求被拒绝)
    pub question_id: String,
    /// 回答内容(单选=1 项且 ∈options;多选=1..=N 项 ⊆options;自由文本=1 项;
    /// 二阶段校验在工具侧重做,不合法 fail-visible 回喂 agent)
    pub answer: Vec<String>,
    /// 回答人平台令牌(可选;提供并校验通过时回执带已验证身份)
    ///
    /// ⚠️ 该字段只用于换取用户名,不落审计链、不进日志。
    pub approver_token: Option<String>,
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
        verdict_json: None,
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

/// 批K 段7:回答正在等待的用户提问
///
/// 路由:`POST /agents/{agent_type}/answer`
/// Body:`{"session_id":"xxx","question_id":"au-...","answer":["选项文本"],"approver_token":"..."}`
///
/// 按 `session_id` 在 QuestionStore 中查找 pending 问答项:
/// - 问题 ID 不匹配 → 400(等待保持,可携带正确 ID 重试)
/// - 提供 `approver_token` → 调认证端点换取平台用户名作为已验证回答人;
///   未提供或校验失败 → 回答人记 `unverified`(回答照常送达,不阻断)
///
/// runner 的 `HttpAskUser::request_answer()` 收到回答后,把 answer 注入
/// args 重调 ask_user 工具做二阶段校验(单选 ∈options/多选 ⊆options),
/// 不合法则 fail-visible 回喂,agent 自行澄清续跑。
///
/// 200 = 回答已送达;404 = session 不在等待问答(已超时/不存在/未发问)。
async fn answer_agent(
    State(state): State<AgentApiState>,
    axum::extract::Path(_agent_type): axum::extract::Path<String>,
    Json(req): Json<AnswerRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let pending = {
        let mut map = state.pending_questions.lock().map_err(|_| {
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
            "K7: answer delivered but session not pending question (timed out / not found / no question asked)"
        );
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "session not pending question"})),
        ));
    };

    // 问题 ID 校验:不匹配时把 pending 放回,允许携带正确 ID 重试
    if req.question_id != pending.question_id {
        if let Ok(mut map) = state.pending_questions.lock() {
            map.insert(req.session_id.clone(), pending);
        }
        warn!(
            session_id = %req.session_id,
            "K7: question_id mismatch on answer request"
        );
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "question_id mismatch",
                "session_id": req.session_id,
            })),
        ));
    }

    // 回答人身份:与 /approve 同口径(令牌换用户名;失败降级 unverified)
    let (answered_by, verified) = match &req.approver_token {
        Some(token) => match state.evorule_client().verify_platform_token(token).await {
            Ok(username) => (username, true),
            Err(e) => {
                info!(
                    session_id = %req.session_id,
                    error = %e,
                    "K7: answerer token verification failed; degrading to unverified"
                );
                ("unverified".to_string(), false)
            }
        },
        None => ("unverified".to_string(), false),
    };

    let answer = AskUserAnswer {
        answer: req.answer,
        answered_by: answered_by.clone(),
        verified,
        timed_out: false,
    };
    // 发送回答(接收方已 drop 时返回 Err,但 HTTP 仍返回 200 表示"已处理")
    let _ = pending.tx.send(answer);
    info!(
        session_id = %req.session_id,
        question_id = %pending.question_id,
        answered_by = %answered_by,
        verified = verified,
        "K7: question answer delivered"
    );
    Ok(Json(serde_json::json!({
        "success": true,
        "message": "answer delivered",
        "session_id": req.session_id,
        "question_id": pending.question_id,
        "answered_by": answered_by,
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
            constitution,
        }) => Event::default().event("session_created").data(
            serde_json::json!({
                "session_id": session_id,
                "memory_enabled": memory_enabled,
                "constitution": constitution,
            })
            .to_string(),
        ),
        Ok(AgentEvent::Step { step }) => Event::default()
            .event("step")
            .data(serde_json::json!({ "step": step }).to_string()),
        Ok(AgentEvent::LlmDelta { text }) => Event::default()
            .event("llm_delta")
            .data(serde_json::json!({ "text": text }).to_string()),
        Ok(AgentEvent::LlmThinkingDelta { text }) => Event::default()
            .event("llm_thinking_delta")
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
        // 批K 段7:人在环问答帧 —— 前端弹问答卡,经 POST /agents/{t}/answer 回传
        Ok(AgentEvent::AskUser {
            question_id,
            question,
            options,
            multi_select,
            timeout_secs,
        }) => Event::default().event("ask_user").data(
            serde_json::json!({
                "question_id": question_id,
                "question": question,
                "options": options,
                "multi_select": multi_select,
                "timeout_secs": timeout_secs,
            })
            .to_string(),
        ),
        // 批K 段7:问答结果(已回答/超时) —— 前端据此收卡置态
        Ok(AgentEvent::AskUserResult {
            question_id,
            answered,
            timed_out,
        }) => Event::default().event("ask_user_result").data(
            serde_json::json!({
                "question_id": question_id,
                "answered": answered,
                "timed_out": timed_out,
            })
            .to_string(),
        ),
        Ok(AgentEvent::PlanSubmitted { plan_id, plan }) => {
            Event::default().event("plan_submitted").data(
                serde_json::json!({
                    "plan_id": plan_id,
                    "plan": plan,
                })
                .to_string(),
            )
        }
        // 流级错误(底层 stream yield Err) — 当作可恢复错误帧发出
        Err(err) => Event::default()
            .event("error")
            .data(serde_json::json!({ "error": err.to_string() }).to_string()),
    };
    Ok(ev)
}

// =============================================================================
// 对话与历史:会话枚举 + 消息历史投影
// =============================================================================

/// `GET /api/sessions` —— 会话列表(本地索引,按最近活跃降序)
///
/// 索引由 WS 面在 SessionCreated / TurnEnd 时记录;只覆盖经过 serve WS 面
/// 创建的会话(工作台消费面),见 session_index 模块文档的边界说明。
async fn list_sessions(
    State(state): State<AgentApiState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sessions = state.session_index().list();
    let count = sessions.len();
    Ok(Json(serde_json::json!({
        "count": count,
        "sessions": sessions,
    })))
}

/// `GET /api/sessions/{id}/transcript` 查询参数
#[derive(Debug, serde::Deserialize)]
struct TranscriptQuery {
    /// agent 类型(缺省从索引回查,再缺省 general——决定记忆 namespace)
    agent_type: Option<String>,
    /// 分页:单页条数上限;缺省(不传)=全量(向后兼容)
    limit: Option<usize>,
    /// 分页游标:仅返回 idx 小于该值的消息(取最新一段);缺省=不限
    before_idx: Option<usize>,
}

/// transcript 分页就地收缩:先按 before_idx 游标过滤(仅保留更早侧),再自
/// 最新端截取 limit 条;返回 has_more(被截者=更早还有)。缺省不传=全量、
/// false(向后兼容);快照源同构。
fn apply_transcript_pagination(
    messages: &mut Vec<crate::agent::memory::MessageRecord>,
    before_idx: Option<usize>,
    limit: Option<usize>,
) -> bool {
    if let Some(before) = before_idx {
        messages.retain(|m| m.idx < before);
    }
    match limit {
        Some(l) if messages.len() > l => {
            let at = messages.len() - l;
            messages.drain(..at);
            true
        }
        _ => false,
    }
}

/// `GET /api/sessions/{id}/transcript` —— 会话消息历史
///
/// 数据源两级(快照机制定稿):
/// 1. **live**(缺省):`MemoryManager` 持久化到 evorule payload 的消息
///    (P0 短期记忆持久化,进 FactsLog 审计链);零写入,前缀读 + 同 idx 后写覆盖;
/// 2. **snapshot**(回落):evorule 会话 30min 闲置 TTL 回收后活投影报
///    `Session not found`,回落 serve 本地快照(`authoritative:false`,
///    展示层副本;TurnEnd 时与 live 同一投影路径落盘)。
///
/// 分页:`limit`+`before_idx` 游标(见 [`apply_transcript_pagination`]);缺省不传=全量。
async fn get_transcript(
    State(state): State<AgentApiState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<TranscriptQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // 解析 agent_type:query 参数 > 索引回查 > general(决定 namespace)
    let agent_type = q.agent_type.clone().or_else(|| {
        state
            .session_index()
            .list()
            .into_iter()
            .find(|e| e.session_id == session_id)
            .map(|e| e.agent_type)
    });
    let agent_type = agent_type.unwrap_or_else(|| "general".to_string());

    let namespace = resolve_memory_namespace(&state, &agent_type);

    match crate::api::session_index::load_transcript(
        state.evorule_client(),
        &session_id,
        &namespace,
    )
    .await
    {
        Ok(mut messages) => {
            let has_more = apply_transcript_pagination(&mut messages, q.before_idx, q.limit);
            let count = messages.len();
            Ok(Json(serde_json::json!({
                "session_id": session_id,
                "agent_type": agent_type,
                "namespace": namespace,
                "source": "live",
                "count": count,
                "has_more": has_more,
                "messages": messages,
            })))
        }
        // 会话已被 evorule TTL 回收 → 回落本地快照(非权威副本明示)
        Err(e) if crate::api::snapshots::is_session_gone(&e) => {
            match state.snapshots().load(&session_id) {
                Some(snap) => {
                    let mut messages = snap.messages;
                    let has_more =
                        apply_transcript_pagination(&mut messages, q.before_idx, q.limit);
                    let count = messages.len();
                    Ok(Json(serde_json::json!({
                        "session_id": session_id,
                        "agent_type": snap.agent_type,
                        "source": "snapshot",
                        "authoritative": false,
                        "saved_at": snap.saved_at,
                        "count": count,
                        "has_more": has_more,
                        "messages": messages,
                    })))
                }
                // 无快照 → 保持原有 502 语义
                None => Err((StatusCode::BAD_GATEWAY, e)),
            }
        }
        Err(e) => Err((StatusCode::BAD_GATEWAY, e)),
    }
}

/// agent_type → memory namespace(与 serve 侧 general 缺省注入同口径);
/// transcript 活投影与 TurnEnd 快照采集共用,保证两路读同一 namespace。
pub(crate) fn resolve_memory_namespace(state: &AgentApiState, agent_type: &str) -> String {
    match state.definitions().load(agent_type) {
        Ok(def) if !def.memory.namespace.is_empty() => def.memory.namespace,
        // 定义加载失败/未配置 → 与 serve 侧 general 注入口径一致
        _ => "general".to_string(),
    }
}

// =============================================================================
// 会话检查点(用户手动存档 + 引擎版本回滚;与 WS Rewind 通道同源)
// =============================================================================

/// 打点/回滚请求体
#[derive(Debug, serde::Deserialize)]
struct SessionOpRequest {
    /// 检查点标签(打点)或目标版本号(回滚)
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    version: Option<u64>,
}

/// REST 面活跃轮守卫(与 WS Rewind 同语义:活跃轮内拒绝结构性变更)
fn session_turn_active(state: &AgentApiState, session_id: &str) -> bool {
    state
        .running_sessions()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(session_id)
}

/// 版本列举:引擎 fact 链可回滚目标投影(每条 fact 的 version 即回滚锚)
async fn get_session_versions(
    State(state): State<AgentApiState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let facts = state
        .evorule_client()
        .get_facts(&session_id, None)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let mut versions: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
    for f in &facts {
        // 版本 v 时链上事实数 = version <= v 的条目数(时间线进度语义)
        let count = versions.entry(f.version).or_insert(0);
        *count += 1;
    }
    let list: Vec<serde_json::Value> = versions
        .into_iter()
        .scan(0usize, |acc, (version, n)| {
            *acc += n;
            Some(serde_json::json!({ "version": version, "facts": *acc }))
        })
        .collect();
    let latest = list.last().and_then(|v| v["version"].as_u64()).unwrap_or(0);
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "latest": latest,
        "versions": list,
    })))
}

/// 手动回滚(引擎 fact 链版本回退;与 WS Rewind 同一底层端点)
async fn post_session_rewind(
    State(state): State<AgentApiState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    Json(body): Json<SessionOpRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let Some(version) = body.version else {
        return Err((StatusCode::BAD_REQUEST, "version is required".to_string()));
    };
    if session_turn_active(&state, &session_id) {
        return Err((
            StatusCode::CONFLICT,
            "cannot rewind during active turn; send interrupt first".to_string(),
        ));
    }
    let reply = state
        .evorule_client()
        .rewind(&session_id, version)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let actual = reply["actual_version"].as_u64().unwrap_or(version);
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "requested_version": version,
        "actual_version": actual,
    })))
}

/// 创建检查点(打点):transcript 快照 + 当前引擎版本锚定三元组落盘
async fn create_session_checkpoint(
    State(state): State<AgentApiState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    Json(body): Json<SessionOpRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let Some(label) = body
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err((StatusCode::BAD_REQUEST, "label is required".to_string()));
    };
    if session_turn_active(&state, &session_id) {
        return Err((
            StatusCode::CONFLICT,
            "cannot take a checkpoint during an active turn".to_string(),
        ));
    }
    let agent_type = state
        .session_index()
        .list()
        .into_iter()
        .find(|e| e.session_id == session_id)
        .map(|e| e.agent_type)
        .unwrap_or_else(|| "general".to_string());
    let namespace = resolve_memory_namespace(&state, &agent_type);
    // 版本锚:当前引擎 fact 链最新版本
    let facts = state
        .evorule_client()
        .get_facts(&session_id, None)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let engine_version = facts.iter().map(|f| f.version).max().unwrap_or(0);
    let (cp_id, _path) = crate::api::snapshots::capture_checkpoint(
        state.evorule_client(),
        state.snapshots(),
        &session_id,
        &agent_type,
        &namespace,
        label,
        engine_version,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let count = state
        .snapshots()
        .load_checkpoint(&session_id, cp_id)
        .map(|s| s.count)
        .unwrap_or(0);
    Ok(Json(serde_json::json!({
        "cp_id": cp_id,
        "session_id": session_id,
        "label": label,
        "engine_version": engine_version,
        "count": count,
    })))
}

/// 列举某会话的手动检查点(按打点时刻升序)
async fn list_session_checkpoints(
    State(state): State<AgentApiState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let checkpoints: Vec<serde_json::Value> = state
        .snapshots()
        .list_checkpoints(&session_id)
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "cp_id": s.saved_at,
                "label": s.label,
                "saved_at": s.saved_at,
                "count": s.count,
                "engine_version": s.engine_version,
            })
        })
        .collect();
    Json(serde_json::json!({
        "session_id": session_id,
        "checkpoints": checkpoints,
    }))
}

/// 恢复检查点:引擎回滚到打点锚定版本 + 返回打点时刻消息(前端重建视图)
async fn restore_session_checkpoint(
    State(state): State<AgentApiState>,
    axum::extract::Path((session_id, cp_id)): axum::extract::Path<(String, u64)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if session_turn_active(&state, &session_id) {
        return Err((
            StatusCode::CONFLICT,
            "cannot restore during an active turn; send interrupt first".to_string(),
        ));
    }
    let snap = state
        .snapshots()
        .load_checkpoint(&session_id, cp_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("checkpoint {cp_id} not found for session {session_id}"),
            )
        })?;
    let version = snap.engine_version.ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            "checkpoint has no engine version anchor (legacy snapshot)".to_string(),
        )
    })?;
    let reply = state
        .evorule_client()
        .rewind(&session_id, version)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let actual = reply["actual_version"].as_u64().unwrap_or(version);
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "cp_id": cp_id,
        "label": snap.label,
        "restored_version": actual,
        "messages": snap.messages,
    })))
}

// =============================================================================
// B17 持久会话:死会话重建回退(两级历史 → 重建回注块)
// =============================================================================

/// 两级取历史(与 `GET /api/sessions/{id}/transcript` 同口径):
/// 活投影(facts)→ 本地快照(TurnEnd 落盘)。两源皆不可得 → 空表
/// (调用方 fail-visible 指引 fresh run,不静默丢上下文续跑)。
async fn load_rebuild_history(
    state: &AgentApiState,
    session_id: &str,
    agent_type: &str,
) -> Result<Vec<crate::agent::memory::MessageRecord>, (StatusCode, String)> {
    let namespace = resolve_memory_namespace(state, agent_type);
    match crate::api::session_index::load_transcript(state.evorule_client(), session_id, &namespace)
        .await
    {
        Ok(messages) => Ok(messages),
        // 会话已被引擎回收(或读取失败)→ 回落本地快照
        Err(_) => Ok(state
            .snapshots()
            .load(session_id)
            .map(|snap| snap.messages)
            .unwrap_or_default()),
    }
}

/// assistant 记录的调用轮廓:带 tool_calls 时在内容行后追加「(随后调用 A、B)」,
/// 工具名三源兼容(引擎持久化 tool_name / 内部 name / OpenAI function.name)。
/// 与前端 sessionRebuild.assistantOutline 同格式同口径。
fn rebuild_assistant_outline(m: &crate::agent::memory::MessageRecord) -> String {
    let Some(tc) = &m.tool_calls else {
        return String::new();
    };
    let empty = Vec::new();
    let arr: &[serde_json::Value] = match tc {
        serde_json::Value::Array(a) => a,
        v if v.is_null() => &empty,
        v => std::slice::from_ref(v),
    };
    if arr.is_empty() {
        return String::new();
    }
    let names: Vec<String> = arr
        .iter()
        .map(|c| {
            c.get("tool_name")
                .and_then(serde_json::Value::as_str)
                .or_else(|| c.get("name").and_then(serde_json::Value::as_str))
                .or_else(|| {
                    c.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(serde_json::Value::as_str)
                })
                .unwrap_or("tool")
                .to_string()
        })
        .collect();
    format!("(随后调用 {})", names.join("、"))
}

/// 历史消息 → 重建回注块+新输入合成(纯函数,无 IO)。
///
/// 压缩口径(与前端 sessionRebuild.buildRebuildBlock 同格式同口径,
/// 保证 HTTP 面与工作台面续聊上下文一致):user/assistant/tool 三角色
/// (tool 以名称+结果摘要入块,续聊模型可引用先前工具轮廓);assistant 带
/// tool_calls 追加调用轮廓;最近 60 条;单行 4000 字符截断;总预算 24000
/// 字符(自最新往回收集,至少保 1 行)。历史块在前、新输入在文末(回注头
/// 明示「直接回应文末的新输入」)。
fn compose_rebuild_context(history: &[crate::agent::memory::MessageRecord], goal: &str) -> String {
    const HEADER: &str = "[会话重建回注] 此前会话在引擎侧已失效(闲置回收或服务重启),以下为历史消息投影(含工具调用轮廓与结果摘要),供续接上下文;请勿复述,直接回应文末的新输入。";
    const MAX_MESSAGES: usize = 60;
    const MAX_PER_MESSAGE: usize = 4000;
    const MAX_TOTAL: usize = 24000;

    // 三角色入块;单行截断(按字符数,非字节,防切破多字节字符)
    let mut candidates: Vec<(String, usize)> = Vec::new();
    for m in history {
        let content = if m.content.chars().count() > MAX_PER_MESSAGE {
            m.content.chars().take(MAX_PER_MESSAGE).collect::<String>()
        } else {
            m.content.clone()
        };
        match m.role.as_str() {
            "user" => candidates.push((format!("[user] {content}"), content.chars().count())),
            "assistant" => {
                let outline = rebuild_assistant_outline(m);
                let line = format!("[assistant] {content}{outline}");
                let cost = content.chars().count() + outline.chars().count();
                candidates.push((line, cost));
            }
            "tool" => {
                let name = m.tool_name.clone().unwrap_or_else(|| "tool".to_string());
                let line = format!("[tool:{name}] {content}");
                candidates.push((line, content.chars().count()));
            }
            _ => {}
        }
    }

    // 自最新往回收集,总预算内(首行无条件保留=至少保 1 行;条数上限同步生效)
    let mut picked_rev: Vec<&(String, usize)> = Vec::new();
    let mut total = 0usize;
    for item in candidates.iter().rev() {
        if picked_rev.len() >= MAX_MESSAGES {
            break;
        }
        if !picked_rev.is_empty() && total + item.1 > MAX_TOTAL {
            break;
        }
        total += item.1;
        picked_rev.push(item);
    }

    let mut lines: Vec<String> = vec![HEADER.to_string(), "<recovered_history>".to_string()];
    for (line, _) in picked_rev.into_iter().rev() {
        lines.push(line.clone());
    }
    lines.push("</recovered_history>".to_string());
    lines.push(String::new());
    lines.push(goal.to_string());
    lines.join("\n")
}

// =============================================================================
// B21 PR-4 / G2:ATIF v1.8 轨迹导出(只读;journal × 审计链 × transcript 三源 join)
// =============================================================================

/// `GET /api/sessions/{id}/atif` 查询参数
#[derive(Debug, Deserialize)]
pub struct AtifQuery {
    /// agent 定义类型(工具 schema 与 memory namespace 解析用);缺省时按
    /// 会话索引回查,再缺省 general(与 transcript 端点同口径)
    #[serde(default)]
    pub agent_type: Option<String>,
}

/// `GET /api/sessions/{id}/atif` —— 三源 join 导出 ATIF v1.8 轨迹
///
/// 装配 [`crate::agent::atif::AtifSources`] 四源后调用纯函数导出器
/// [`crate::agent::atif::export`](crate::agent::atif::export);装配语义见
/// atif.rs 模块文档(三源 join 模型/确定性裁定),此处只做 IO 与错误映射:
///
/// - journal 骨架:`data/sessions/{sid}.jsonl`(与 runner 注入 journal_dir
///   同路径);缺失或为空 → 404 fail-visible,不产半截轨迹;
/// - 审计链全文:`EvoruleApiClient::replay`(fact.to_json() 列表;audit/export
///   端点只含哈希链元数据无内容,不可作内容源)不可达 → 502;
/// - transcript 消息投影:`session_index::load_transcript`(namespace 解析与
///   get_transcript 同口径),不可达 → 502;空投影合法(ATIF 全 Optional);
/// - 工具 schema:[`AgentRunner::openai_function_schemas_for`](crate::agent::AgentRunner::openai_function_schemas_for)
///   (与 LLM 请求单一实现)× agent 定义 tools 列表;定义缺失/空清单时
///   降级省略该字段(ATIF Optional)。
///
/// 端点纯只读:同 session 重导出逐字节一致(atif.rs 幂等保证,P3 验收 #7,
/// `extra.exported_at` 由 journal 尾事件 ts 派生而非当前时钟)。
async fn get_atif(
    State(state): State<AgentApiState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<AtifQuery>,
) -> Result<Json<crate::agent::atif::AtifTrajectory>, (StatusCode, String)> {
    // agent_type 解析:query 参数 > 索引回查 > general(与 get_transcript 同口径)
    let agent_type = q.agent_type.clone().or_else(|| {
        state
            .session_index()
            .list()
            .into_iter()
            .find(|e| e.session_id == session_id)
            .map(|e| e.agent_type)
    });
    let agent_type = agent_type.unwrap_or_else(|| "general".to_string());

    // 1. journal 骨架(路径与 runner 注入 journal_dir 同源)
    let journal_dir = state.workdir.join("data").join("sessions");
    let journal = crate::agent::journal::read_all(&crate::agent::journal::JournalWriter::path_for(
        &journal_dir,
        &session_id,
    ))
    .map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            format!("atif export: journal unavailable for session '{session_id}': {e}"),
        )
    })?;
    if journal.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("atif export: journal is empty for session '{session_id}'"),
        ));
    }

    // 2. 审计链全文(18080 权威面唯一全文通道)
    let audit_facts = state
        .evorule_client()
        .replay(&session_id)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("atif export: audit replay failed: {e}"),
            )
        })?;

    // 3. transcript 消息投影
    let namespace = resolve_memory_namespace(&state, &agent_type);
    let transcript =
        crate::api::session_index::load_transcript(state.evorule_client(), &session_id, &namespace)
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("atif export: transcript fetch failed: {e}"),
                )
            })?;

    // 4. 工具 schema(与 runner LLM 请求单一实现;定义缺失/空清单降级省略)
    let tool_definitions = state
        .definitions
        .load(&agent_type)
        .ok()
        .map(|def| AgentRunner::openai_function_schemas_for(&def.tools))
        .filter(|t| !t.is_empty())
        .map(serde_json::Value::Array);

    let sources = crate::agent::atif::AtifSources {
        session_id: &session_id,
        journal: &journal,
        transcript: &transcript,
        audit_facts: &audit_facts,
        tool_definitions,
        // 单会话端点不做父链路反查(父 journal 扫描属批量面);子轨迹
        // 带链路标注走 journal::scan_delegate_spawns 发现+离线导出
        parent_session_id: None,
        engine_chain_tail: None,
    };
    let trajectory = crate::agent::atif::export(sources)
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    Ok(Json(trajectory))
}

// =============================================================================
// 治理叠加:进化信号只读代理(工作台信号徽标数据源)
// =============================================================================

/// `GET /api/sessions/{id}/evolution-signals` —— 会话进化信号只读代理
///
/// 透传 [`EvoruleApiClient::get_evolution_signals`](crate::api::evorule_client::EvoruleApiClient::get_evolution_signals)
/// (既有只读聚合端点,服务端 fail-soft:空会话/不可读 → 200 + 空信号),
/// 供工作台信号徽标消费。展示层零写入、零新 Fact 类型;evorule 不可达时
/// 透传 502,由前端徽标 fail-soft 兜底(显示「—」不阻断)。
async fn get_evolution_signals(
    State(state): State<AgentApiState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sid: u64 = session_id.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "session id must be numeric".to_string(),
        )
    })?;
    let signals = state
        .evorule_client()
        .get_evolution_signals(sid, None)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(signals))
}

// =============================================================================
// 工作台配置(快照保留期)
// =============================================================================

/// `GET /api/workbench/config` —— 读取工作台配置(缺文件/损坏 → 缺省 3m)
async fn get_workbench_config(State(state): State<AgentApiState>) -> Json<serde_json::Value> {
    let cfg = state.workbench_config().load();
    Json(serde_json::json!({
        "retention": cfg.retention,
        "allowed": crate::api::snapshots::RetentionPolicy::ALL
            .iter()
            .map(|(label, _)| *label)
            .collect::<Vec<_>>(),
    }))
}

/// `PUT /api/workbench/config` 请求体
#[derive(Debug, serde::Deserialize)]
struct PutWorkbenchConfigBody {
    retention: String,
}

/// `PUT /api/workbench/config` —— 保存快照保留期(标签非法 400;原子写)
async fn put_workbench_config(
    State(state): State<AgentApiState>,
    axum::Json(body): axum::Json<PutWorkbenchConfigBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let policy = crate::api::snapshots::RetentionPolicy::from_label(&body.retention).ok_or((
        StatusCode::BAD_REQUEST,
        format!(
            "invalid retention: {} (allowed: {})",
            body.retention,
            crate::api::snapshots::RetentionPolicy::ALL
                .iter()
                .map(|(l, _)| *l)
                .collect::<Vec<_>>()
                .join("|")
        ),
    ))?;
    let cfg = crate::api::snapshots::WorkbenchConfig {
        retention: policy.label().to_string(),
    };
    state
        .workbench_config()
        .save(&cfg)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({ "retention": cfg.retention })))
}

// =============================================================================
// G14:记忆事件 HTTP 端点
// =============================================================================

/// 事件记忆 namespace 判定核心（读写同域,纯函数）
///
/// 与写侧完全同构（WS construct_runner / CLI from_definition）:
/// 定义声明优先;general 未启用记忆时同款注入 "general"（写侧
/// ws_handler 同口径例外）;其余按声明（含空串,与写侧传参一致）。
fn resolve_events_namespace(agent_type: &str, memory_type: &str, declared_ns: &str) -> String {
    if agent_type == "general" && (memory_type == "none" || memory_type.is_empty()) {
        return "general".to_string();
    }
    declared_ns.to_string()
}

/// 会话事件读面 namespace 解析（记忆事件账本读面修复:读侧此前硬编码
/// "default",与写侧定义声明 namespace 错位 → MemoryTab/replay 恒空）。
/// 会话索引查归属 agent → 定义取 memory.namespace;索引无记录/定义
/// 加载失败回落 "general"（[agents] default 即 general,桌面工作台
/// 主 agent 实际域）。
fn session_events_namespace(state: &AgentApiState, session_id: &str) -> String {
    let agent_type = state
        .session_index()
        .list()
        .into_iter()
        .find(|e| e.session_id == session_id)
        .map(|e| e.agent_type)
        .unwrap_or_else(|| "general".to_string());
    match state.definitions().load(&agent_type) {
        Ok(def) => {
            resolve_events_namespace(&agent_type, &def.memory.memory_type, &def.memory.namespace)
        }
        Err(_) => "general".to_string(),
    }
}

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
    // 记忆事件账本读面修复:namespace 与写侧同域解析（此前硬编码 "default" 错位恒空）
    let namespace = session_events_namespace(&state, &session_id);
    let mut store = MemoryEventStore::new(&namespace, client);
    store.set_session_id(&session_id);

    // best-effort 同步(HTTP 失败返回空列表)
    if let Err(e) = store.sync_from_evorule().await {
        tracing::warn!(
            session_id = %session_id,
            namespace = %namespace,
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

/// 审计回放本地兜底合成(纯函数核心,单测直测)
///
/// 输入 journal 全量行 + transcript tool 消息原文(按执行序),产出合成回放
/// 事件:ToolInvoked/ToolResult 按 call_id 配对,分发序对位原文,digest 复算
/// 防伪(不符不带原文,`digest_verified:false`)。
fn synth_replay_events(
    lines: &[crate::agent::journal::JournalLine],
    tool_outputs: &[String],
    session_id: &str,
) -> Vec<crate::agent::memory_event::event::MemoryEvent> {
    use crate::agent::journal::{evorule_digest, JournalEvent};
    use crate::agent::memory_event::event::{EventSource, EventType, MemoryEvent};

    // invoked 骨架:call_id → (tool 名, 分发序)
    let mut invoked_order: HashMap<String, (String, usize)> = HashMap::new();
    let mut events = Vec::new();
    for line in lines {
        match &line.event {
            JournalEvent::ToolInvoked { call_id, tool, .. } => {
                let ord = invoked_order.len();
                invoked_order.insert(call_id.clone(), (tool.clone(), ord));
            }
            JournalEvent::ToolResult {
                call_id,
                status,
                size_bytes,
                content_digest,
                execution_profile,
            } => {
                // 分发序对位原文 + digest 复算防伪(不符不带原文)
                let (output, verified) = match invoked_order.get(call_id) {
                    Some((_, ord)) => match tool_outputs.get(*ord) {
                        Some(text) => {
                            let digest = evorule_digest(text);
                            if digest == *content_digest {
                                (serde_json::Value::String(text.clone()), true)
                            } else {
                                (serde_json::Value::Null, false)
                            }
                        }
                        None => (serde_json::Value::Null, false),
                    },
                    None => (serde_json::Value::Null, false),
                };
                let event = MemoryEvent::new_root(
                    &format!("journal-{}-{call_id}", line.seq),
                    EventType::Custom("journal_tool_result".into()),
                    line.ts / 1000,
                    EventSource::ToolResult,
                )
                .with_content(serde_json::json!({
                    "call_id": call_id,
                    "tool": invoked_order.get(call_id).map(|(t, _)| t.clone()),
                    "status": status,
                    "size_bytes": size_bytes,
                    "content_digest": content_digest,
                    "execution_profile": execution_profile,
                    "output": output,
                    "digest_verified": verified,
                    "synthesized": true,
                }))
                .with_session(session_id)
                .with_tag("journal_synth");
                events.push(event);
            }
            _ => {}
        }
    }
    events
}

/// 审计回放本地兜底合成(IO 包装)
///
/// 引擎事件流为空(server 数据丢失/纯内存态重启)时,用本地 journal 事件
/// 骨架(journal 恒在本地 `data/sessions/{sid}.jsonl`,不依赖引擎)+
/// transcript 原文(经 evorule facts 持久投影,尽力取)合成回放事件。
/// 合成事件 `event_type: Custom("journal_tool_result")`,响应面带
/// `source: "local_journal_synth"` 标记,消费方可区分合成账与引擎账。
async fn synthesize_replay_from_journal(
    state: &AgentApiState,
    session_id: &str,
) -> Vec<crate::agent::memory_event::event::MemoryEvent> {
    use crate::agent::journal::JournalWriter;

    let dir = state.workdir().join("data").join("sessions");
    let path = JournalWriter::path_for(&dir, session_id);
    if !path.exists() {
        return Vec::new();
    }
    // 容错读:行级损坏跳过(审计合成面不因个别坏行整体拒答)
    let lines = match crate::agent::journal::read_all_tolerant(&path) {
        Ok(lines) => lines,
        Err(_) => return Vec::new(),
    };

    // transcript 原文(尽力取;tool 消息按执行序与 journal 分发序对位,
    // 与 ATIF 导出的 transcript 对位法同构)
    let tool_outputs: Vec<String> =
        crate::api::session_index::load_transcript(&state.evorule_client, session_id, "default")
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.role == "tool")
            .map(|m| m.content)
            .collect();

    synth_replay_events(&lines, &tool_outputs, session_id)
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
    // 记忆事件账本读面修复:namespace 与写侧同域解析（此前硬编码 "default" 错位恒空）
    let mut store = MemoryEventStore::new(&session_events_namespace(&state, &session_id), client);
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
        // 引擎事件流为空 → 本地 journal 兜底合成(事件骨架+
        // transcript 原文按分发序对齐,digest 复算防伪)。合成失败/无账
        // → 原空响应语义零变化
        let synthesized = synthesize_replay_from_journal(&state, &session_id).await;
        if !synthesized.is_empty() {
            let events_json: Vec<serde_json::Value> = synthesized
                .iter()
                .filter_map(|e| serde_json::to_value(e).ok())
                .collect();
            let count = events_json.len();
            return Ok(Json(serde_json::json!({
                "session_id": session_id,
                "count": count,
                "events": events_json,
                "source": "local_journal_synth",
                "message": "engine events empty; synthesized from local journal audit trail",
            })));
        }
        return Ok(Json(serde_json::json!({
            "session_id": session_id,
            "count": 0,
            "events": [],
            "message": "no events found",
        })));
    }

    let mut engine = ReplayEngine::new(store);

    // 可选注入 LLM(narrate=true 时)——统一解析产物,与 CLI 同一配置语义
    let do_narrate = params.narrate.unwrap_or(false);
    if do_narrate {
        let llm = crate::io_handlers::LlmHandler::from_config(&state.llm_config());
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
        // S5-b 起 switch_llm_model 会向 workdir/data/llm_state.json 落盘——
        // 测试态改用一次性 tempdir 作 workdir(有意泄漏,OS temp 兜底),
        // 避免测试残留写进仓根 runtime 面、被下次真 serve 启动误读。
        let tmp: &tempfile::TempDir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        AgentApiState::new_with_metrics(
            AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new("http://localhost:8080"),
            Arc::new(Metrics::new().unwrap()),
            tmp.path().to_path_buf(),
            Arc::new(WorkspaceApiClient::new("http://localhost:8080")),
            Arc::new(std::sync::RwLock::new(ToolHandler::new())),
        )
    }

    #[test]
    fn test_state_carries_resolved_llm_config() {
        // serve 接线:state 携带统一解析产物,请求路径 handler 构造
        // (from_config)与 CLI 直接消费 config.llm 输入相同——
        // 双路径一致性由同源保证,此处锁住 state 的携带与传递
        let injected = crate::config::LlmConfig {
            api_key: "resolved-key".to_string(),
            model: "resolved-model".to_string(),
            ..crate::config::LlmConfig::default()
        };
        let state = make_test_state().with_llm_config(injected.clone());
        assert_eq!(state.llm_config(), injected);
        // 默认态(未注入):api_key 为空(from_config 视为未配置,不误用占位符)
        let fresh = make_test_state();
        assert!(fresh.llm_config().api_key.is_empty());
    }

    #[test]
    fn test_llm_state_roundtrip_and_degrade() {
        // S5-b:持久化写/读往返 + 缺失/损坏/非对象/空值降级(fail-soft 不 panic)
        let tmp = tempfile::tempdir().unwrap();
        let wd = tmp.path();
        assert_eq!(super::read_llm_state_model(wd), None);
        super::write_llm_state_model(wd, "MiniMax-M3").unwrap();
        assert_eq!(
            super::read_llm_state_model(wd),
            Some("MiniMax-M3".to_string())
        );
        // 覆盖写 → 新值(tmp+rename 原子替换语义)
        super::write_llm_state_model(wd, "MiniMax-M2.5").unwrap();
        assert_eq!(
            super::read_llm_state_model(wd),
            Some("MiniMax-M2.5".to_string())
        );
        // 损坏件 → None 降级
        std::fs::write(wd.join("data").join("llm_state.json"), "not json{{").unwrap();
        assert_eq!(super::read_llm_state_model(wd), None);
        // 非对象 JSON → None
        std::fs::write(wd.join("data").join("llm_state.json"), "[]").unwrap();
        assert_eq!(super::read_llm_state_model(wd), None);
        // 空 model → None
        super::write_llm_state_model(wd, "").unwrap();
        assert_eq!(super::read_llm_state_model(wd), None);
    }

    #[test]
    fn test_llm_model_switch_persists_state() {
        // S5-b:切换成功即落盘,内存态与文件同步;重复切换覆盖更新
        let tmp = tempfile::tempdir().unwrap();
        let state = AgentApiState::new_with_metrics(
            AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new("http://localhost:8080"),
            Arc::new(Metrics::new().unwrap()),
            tmp.path().to_path_buf(),
            Arc::new(WorkspaceApiClient::new("http://localhost:8080")),
            Arc::new(std::sync::RwLock::new(ToolHandler::new())),
        )
        .with_llm_status(crate::config::LlmConfig::default().status_snapshot(None));
        state.switch_llm_model("MiniMax-M3");
        assert_eq!(state.current_llm_model(), "MiniMax-M3");
        assert_eq!(
            super::read_llm_state_model(tmp.path()),
            Some("MiniMax-M3".to_string())
        );
        state.switch_llm_model("MiniMax-M2.5");
        assert_eq!(state.current_llm_model(), "MiniMax-M2.5");
        assert_eq!(
            super::read_llm_state_model(tmp.path()),
            Some("MiniMax-M2.5".to_string())
        );
    }

    #[tokio::test]
    async fn test_atif_export_endpoint_joins_three_sources() {
        // G2/B21 PR-4:端点装配 journal × 审计链 × transcript 三源产出合法 ATIF。
        // journal 由 JournalWriter 真写;审计链由 mockito 提供 IoRequest/IoResponse
        // 全文(replay = fact JSON 数组);transcript 返回空投影(合法,全 Optional)。
        use crate::agent::journal::JournalWriter;

        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/atif-e2e/replay")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[
                    {"type":"IoRequest","id":100,"io_type":"call_service",
                     "params":{"service":"stock_price","args":{"ticker":"GOOGL"}}},
                    {"type":"IoResponse","request_id":100,
                     "result":{"price":185.35}}
                ]"#,
            )
            .create_async()
            .await;
        // transcript facts 前缀含 namespace(依赖 agent 配置),用前缀正则放宽;
        // 兼容 mockito 匹配面带/不带 query 串两种语义
        server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/api/sessions/atif-e2e/facts(\?.*)?$".to_string()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create_async()
            .await;

        // journal 事件序:turn_started(1) → llm_called(2) → tool_invoked(3,"t3")
        // → tool_result(4) → turn_ended(5)
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("data").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let writer = JournalWriter::open(&sessions, "atif-e2e").unwrap();
        let _guard = writer.begin_turn("查询股价").unwrap();
        writer
            .llm_called_react("test-model", Some(100), None, Some(1000), 3, "调用工具查价")
            .unwrap();
        let call_id = writer
            .tool_invoked(
                "stock_price",
                &serde_json::json!({"ticker": "GOOGL"}),
                Some(100),
            )
            .unwrap();
        writer
            .tool_result(&call_id, "ok", r#"{"price":185.35}"#, None)
            .unwrap();
        writer.end_turn("done", 1, 10).unwrap();

        let state = AgentApiState::new_with_metrics(
            AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new(&server.url()),
            Arc::new(Metrics::new().unwrap()),
            tmp.path().to_path_buf(),
            Arc::new(WorkspaceApiClient::new(&server.url())),
            Arc::new(std::sync::RwLock::new(ToolHandler::new())),
        );
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/atif-e2e/atif")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["schema_version"], "ATIF-v1.8");
        assert_eq!(body["session_id"], "atif-e2e");
        // transcript 为空 → 无 system 步:user 步(turn_started) + agent 步(react)
        let steps = body["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["source"], "user");
        assert_eq!(steps[1]["source"], "agent");
        // 三源 join:tool_call_id = journal 合成;arguments = 审计链 IoRequest;
        // observation content = 审计链 IoResponse
        assert_eq!(steps[1]["tool_calls"][0]["tool_call_id"], "t3");
        assert_eq!(steps[1]["tool_calls"][0]["arguments"]["ticker"], "GOOGL");
        assert_eq!(
            steps[1]["observation"]["results"][0]["source_call_id"],
            "t3"
        );
        assert_eq!(
            steps[1]["observation"]["results"][0]["content"],
            r#"{"price":185.35}"#
        );
        // tokens_est=1000(无真值)→ 7:3 拆分
        assert_eq!(body["final_metrics"]["total_prompt_tokens"], 700);
        assert_eq!(body["final_metrics"]["total_completion_tokens"], 300);
    }

    #[tokio::test]
    async fn test_atif_export_missing_journal_is_404() {
        // fail-visible:无 journal 的会话不产半截轨迹
        let state = make_test_state();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/no-such-session/atif")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// 本地兜底合成——call_id 配对 + 分发序对位 + digest 复算防伪
    #[test]
    fn test_synth_replay_events_pairs_and_digest_guard() {
        use crate::agent::journal::{evorule_digest, JournalEvent, JournalLine};
        let mk = |seq: u64, ts: u64, event: JournalEvent| JournalLine {
            seq,
            ts,
            event,
            hashes: None,
        };
        let content = r#"{"price":185.35}"#;
        let lines = vec![
            mk(
                1,
                1_000,
                JournalEvent::ToolInvoked {
                    call_id: "t1".into(),
                    tool: "http_get".into(),
                    args_digest: "a".into(),
                    evorule_request_id: None,
                },
            ),
            mk(
                2,
                2_000,
                JournalEvent::ToolResult {
                    call_id: "t1".into(),
                    status: "ok".into(),
                    size_bytes: content.len(),
                    content_digest: evorule_digest(content),
                    execution_profile: Some("sandbox-trusted".into()),
                },
            ),
            mk(
                3,
                3_000,
                JournalEvent::ToolInvoked {
                    call_id: "t2".into(),
                    tool: "file_read".into(),
                    args_digest: "b".into(),
                    evorule_request_id: None,
                },
            ),
            mk(
                4,
                4_000,
                JournalEvent::ToolResult {
                    call_id: "t2".into(),
                    status: "ok".into(),
                    size_bytes: 4,
                    content_digest: evorule_digest("real"),
                    execution_profile: None,
                },
            ),
        ];
        // transcript tool 消息按执行序两条:第 1 条与 t1 digest 对上;第 2 条对不上
        let tool_outputs = vec![content.to_string(), "tampered".to_string()];
        let events = super::synth_replay_events(&lines, &tool_outputs, "s1");
        assert_eq!(events.len(), 2);
        let e1 = &events[0];
        assert_eq!(e1.event_id, "journal-2-t1");
        assert_eq!(e1.content["tool"], "http_get");
        assert_eq!(e1.content["output"], content, "digest 对上=带原文");
        assert_eq!(e1.content["digest_verified"], true);
        assert_eq!(e1.content["execution_profile"], "sandbox-trusted");
        let e2 = &events[1];
        assert_eq!(e2.content["digest_verified"], false, "digest 不符不带原文");
        assert!(e2.content["output"].is_null(), "宁缺毋假");
    }

    /// 孤立 tool_result(无 invoked 配对)→ tool/output 双空,不误配
    #[test]
    fn test_synth_replay_events_orphan_result() {
        use crate::agent::journal::{JournalEvent, JournalLine};
        let lines = vec![JournalLine {
            seq: 1,
            ts: 1_000,
            event: JournalEvent::ToolResult {
                call_id: "t9".into(),
                status: "error".into(),
                size_bytes: 0,
                content_digest: "x".into(),
                execution_profile: None,
            },
            hashes: None,
        }];
        let events = super::synth_replay_events(&lines, &[], "s1");
        assert_eq!(events.len(), 1);
        assert!(events[0].content["tool"].is_null());
        assert!(events[0].content["output"].is_null());
        assert_eq!(events[0].content["synthesized"], true);
        assert_eq!(events[0].session_id, Some("s1".into()));
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
        let key_fixture = "test-key-abcd1234wxyz";
        let cfg = crate::config::LlmConfig {
            api_key: key_fixture.to_string(),
            ..crate::config::LlmConfig::default()
        };
        let snapshot = cfg.status_snapshot(Some("MINIMAX_API_KEY"));
        assert_eq!(snapshot.api_key.hint.as_deref(), Some("wxyz"));

        let state = make_test_state().with_llm_status(snapshot);
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
        assert!(
            !text.contains(key_fixture),
            "response body leaked the API key"
        );
        assert!(text.contains("wxyz"), "last-4 hint should be present");
    }

    // ---- S5:模型清单中继与切换端点 ----

    /// S5 测试辅助:起 mock 执行体(仅 /llm/models),返回 executor_url 形态地址
    async fn spawn_mock_executor(models: serde_json::Value) -> String {
        let app = axum::Router::new().route(
            "/llm/models",
            axum::routing::get(move || {
                let v = models.clone();
                async move { axum::Json(v) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/llm/execute")
    }

    #[test]
    fn test_executor_models_url_strips_execute_suffix() {
        assert_eq!(
            executor_models_url("http://127.0.0.1:9130/llm/execute"),
            "http://127.0.0.1:9130/llm/models"
        );
        assert_eq!(
            executor_models_url("http://127.0.0.1:9130/other"),
            "http://127.0.0.1:9130/other/llm/models"
        );
    }

    #[tokio::test]
    async fn test_llm_models_endpoint_relays_and_attaches_current() {
        let url = spawn_mock_executor(serde_json::json!({
            "ready": true,
            "models": ["MiniMax-M2.5", "MiniMax-M3"]
        }))
        .await;
        let cfg = crate::config::LlmConfig {
            executor_url: url,
            ..crate::config::LlmConfig::default()
        };
        let state = make_test_state().with_llm_config(cfg);
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/llm-models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ready"], serde_json::json!(true));
        assert_eq!(
            v["models"],
            serde_json::json!(["MiniMax-M2.5", "MiniMax-M3"])
        );
        // current 取自共享配置的出厂模型缺省值
        assert_eq!(v["current"], serde_json::json!("MiniMax-M2.5"));
    }

    #[tokio::test]
    async fn test_llm_models_endpoint_relay_offline_is_503() {
        // 127.0.0.1:1 = 必拒端口(不依赖本机 9130 是否有活执行体)
        let cfg = crate::config::LlmConfig {
            executor_url: "http://127.0.0.1:1/llm/execute".to_string(),
            ..crate::config::LlmConfig::default()
        };
        let state = make_test_state().with_llm_config(cfg);
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/llm-models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["error"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[tokio::test]
    async fn test_llm_model_switch_success_updates_config_and_status() {
        let url = spawn_mock_executor(serde_json::json!({
            "ready": true,
            "models": ["MiniMax-M2.5", "MiniMax-M3"]
        }))
        .await;
        let cfg = crate::config::LlmConfig {
            executor_url: url,
            ..crate::config::LlmConfig::default()
        };
        let state = make_test_state().with_llm_config(cfg);
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/llm-model")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model": "MiniMax-M3"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["status"]["model"], serde_json::json!("MiniMax-M3"));
    }

    #[tokio::test]
    async fn test_llm_model_switch_propagates_to_handler_construction() {
        // 切换后 state.llm_config() 即新值(handler 每请求现取现构=运行期生效),
        // 且脱敏快照 model 字段联动(状态端点可见)
        let url = spawn_mock_executor(serde_json::json!({
            "ready": true,
            "models": ["MiniMax-M2.5", "MiniMax-M3"]
        }))
        .await;
        let cfg = crate::config::LlmConfig {
            executor_url: url,
            ..crate::config::LlmConfig::default()
        };
        let state = make_test_state().with_llm_config(cfg);
        let app = router(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/llm-model")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model": "MiniMax-M3"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        assert_eq!(state.llm_config().model, "MiniMax-M3");
        assert_eq!(state.llm_status().model, "MiniMax-M3");
        // 切换不越界改其他字段(纯 model 态)
        assert!(state.llm_config().api_key.is_empty());
    }

    #[tokio::test]
    async fn test_llm_model_switch_rejects_non_allowlisted() {
        let url = spawn_mock_executor(serde_json::json!({
            "ready": true,
            "models": ["MiniMax-M2.5", "MiniMax-M3"]
        }))
        .await;
        let cfg = crate::config::LlmConfig {
            executor_url: url,
            ..crate::config::LlmConfig::default()
        };
        let state = make_test_state().with_llm_config(cfg);
        let app = router(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/llm-model")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model": "unlisted-model"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // 拒切后配置态不变
        assert_eq!(state.llm_config().model, "MiniMax-M2.5");
    }

    #[tokio::test]
    async fn test_llm_model_switch_rejects_when_executor_offline() {
        let cfg = crate::config::LlmConfig {
            executor_url: "http://127.0.0.1:1/llm/execute".to_string(),
            ..crate::config::LlmConfig::default()
        };
        let state = make_test_state()
            .with_llm_config(cfg)
            // 注入与配置同源的基线快照(unconfigured 快照 model 为空串,不适用)
            .with_llm_status(crate::config::LlmConfig::default().status_snapshot(None));
        let app = router(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/llm-model")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model": "MiniMax-M3"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // 不盲切:执行体不可达=拒切(503),配置态保持
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(state.llm_config().model, "MiniMax-M2.5");
        assert_eq!(state.llm_status().model, "MiniMax-M2.5");
    }

    #[tokio::test]
    async fn test_llm_model_switch_rejects_empty_body() {
        let state = make_test_state();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/llm-model")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model": "  "#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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
    async fn test_git_endpoints_require_auth() {
        // G16:git REST 面受统一鉴权保护——auth enabled 且无 token → 401
        let state = make_test_state();
        let auth_config = crate::api::auth::AuthConfig::new(vec!["secret".to_string()], true);
        let app = router_with_auth(state, auth_config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/git/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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
            container: None,
            execution_profile: None,
            tools: None,
            workspace: None,
            execution: None,
            resume_session_id: None,
            resume_mode: None,
            step_timeout_secs: None,
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

    // ===== consume_to_done 测试 =====

    type MockStream = std::pin::Pin<Box<dyn Stream<Item = Result<AgentEvent, AgentError>> + Send>>;

    #[tokio::test]
    async fn test_consume_to_done_aggregates_done() {
        // 正常链:Step → ToolCall → ToolResult → Done(权威结果原样聚合)
        let events: Vec<Result<AgentEvent, AgentError>> = vec![
            Ok(AgentEvent::Step { step: 1 }),
            Ok(AgentEvent::ToolCall {
                name: "file_read".to_string(),
                args: serde_json::json!({"path": "README.md"}),
            }),
            Ok(AgentEvent::ToolResult {
                name: "file_read".to_string(),
                result: serde_json::json!("file content"),
            }),
            Ok(AgentEvent::Done(AgentResult::success(
                "final answer".to_string(),
                3,
                1234,
                vec!["file_read".to_string()],
            ))),
        ];
        let (result, session_id) =
            consume_to_done(Box::pin(futures_util::stream::iter(events)) as MockStream).await;
        assert!(result.success);
        assert_eq!(result.content, "final answer");
        assert_eq!(result.steps, 3);
        assert_eq!(result.duration_ms, 1234);
        assert!(result.error.is_none());
        assert!(session_id.is_none(), "无 SessionCreated 时不应有会话 ID");
    }

    #[tokio::test]
    async fn test_consume_to_done_err_item_is_failure() {
        // Item 层 Err:组装失败结果(不 panic 不静默)
        let events: Vec<Result<AgentEvent, AgentError>> =
            vec![Err(AgentError::Internal("boom".to_string()))];
        let (result, _) =
            consume_to_done(Box::pin(futures_util::stream::iter(events)) as MockStream).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("boom"));
    }

    #[tokio::test]
    async fn test_consume_to_done_stream_without_done() {
        // 流意外结束且无 Done(防御性):返回失败结果
        let events: Vec<Result<AgentEvent, AgentError>> = vec![Ok(AgentEvent::Step { step: 1 })];
        let (result, _) =
            consume_to_done(Box::pin(futures_util::stream::iter(events)) as MockStream).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("without Done"));
    }

    #[tokio::test]
    async fn test_consume_to_done_intermediate_error_then_done() {
        // 中间 Error 事件不提前返回,其后 Done 承载权威判定
        let events: Vec<Result<AgentEvent, AgentError>> = vec![
            Ok(AgentEvent::Error(AgentError::EvoruleError(
                "rewindable".to_string(),
            ))),
            Ok(AgentEvent::Done(AgentResult::error(
                "final error".to_string(),
                2,
                500,
            ))),
        ];
        let (result, _) =
            consume_to_done(Box::pin(futures_util::stream::iter(events)) as MockStream).await;
        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("final error"));
    }

    #[tokio::test]
    async fn test_consume_to_done_captures_session_created() {
        // SessionCreated 事件携带的会话 ID 被捕获并随结果返回
        let events: Vec<Result<AgentEvent, AgentError>> = vec![
            Ok(AgentEvent::SessionCreated {
                session_id: "sess-run-1".to_string(),
                memory_enabled: false,
                constitution: None,
            }),
            Ok(AgentEvent::Done(AgentResult::success(
                "ok".to_string(),
                1,
                100,
                vec![],
            ))),
        ];
        let (result, session_id) =
            consume_to_done(Box::pin(futures_util::stream::iter(events)) as MockStream).await;
        assert!(result.success);
        assert_eq!(session_id.as_deref(), Some("sess-run-1"));
    }

    // ===== /version 端点测试 =====

    #[tokio::test]
    async fn test_version_endpoint() {
        let state = make_test_state();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json.get("version")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty()),
            "version 字段必须非空"
        );
        assert!(
            json.get("exe_mtime_epoch").is_some(),
            "exe_mtime_epoch 字段必须在"
        );
        assert!(json.get("workdir").is_some(), "workdir 字段必须在");
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
            session_id: Some("sess-1".to_string()),
            rebuilt_from_session: None,
            plan_stats: None,
            run_session_id: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("success"));
        assert!(json.contains("session_id"), "会话建立时应携带 session_id");
        // None 时字段整体省略(旧消费者零影响)
        let bare = AgentRunResponse {
            session_id: None,
            ..resp
        };
        let bare_json = serde_json::to_string(&bare).unwrap();
        assert!(!bare_json.contains("session_id"));
        assert!(
            !bare_json.contains("plan_stats"),
            "react/缺省不透出 plan_stats"
        );
        // B17:rebuilt_from_session None 不序列化(旧消费者零影响)
        assert!(!bare_json.contains("rebuilt_from_session"));
        assert!(json.contains("hello"));
        assert!(json.contains("3"));
    }

    #[test]
    fn test_agent_run_response_plan_stats_serialize() {
        // plan_execute 成功响应:plan_stats 七项透出 + session_id=marks_session
        let resp = AgentRunResponse {
            success: true,
            content: "digest".to_string(),
            steps: 2,
            duration_ms: 5000,
            error: None,
            session_id: Some("marks-1".to_string()),
            rebuilt_from_session: None,
            plan_stats: Some(PlanStatsResponse {
                plan_versions: 1,
                replans: 0,
                nodes_executed: 2,
                wall_ms: 4800,
                repeated_nodes: 0,
                tokens_used: 1024,
                replan_tokens: 0,
            }),
            run_session_id: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["plan_stats"]["plan_versions"], 1);
        assert_eq!(v["plan_stats"]["replans"], 0);
        assert_eq!(v["plan_stats"]["nodes_executed"], 2);
        assert_eq!(v["plan_stats"]["wall_ms"], 4800);
        assert_eq!(v["plan_stats"]["repeated_nodes"], 0);
        assert_eq!(v["plan_stats"]["tokens_used"], 1024);
        assert_eq!(v["plan_stats"]["replan_tokens"], 0);
        assert_eq!(v["session_id"], "marks-1");
    }

    #[test]
    fn test_execution_mode_request_deserialization() {
        // execution 扩展字段反序列化面:显式 mode 可读出,缺省 = None(react);
        // 分流行为(未知 mode=400/plan_execute 挂 driver)由 e2e_plan_execute 场景 F 覆盖
        let raw = r#"{"agent_type":"general","goal":"g","execution":{"mode":"plan_execute"}}"#;
        let req: AgentRunRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.execution.unwrap().mode, "plan_execute");
        // None/缺省 = react
        let bare: AgentRunRequest =
            serde_json::from_str(r#"{"agent_type":"general","goal":"g"}"#).unwrap();
        assert!(bare.execution.is_none());
    }

    // ===== G4 SSE 测试 =====
    // axum 的 Event 没有公开 getter,测试通过 format!("{:?}", ev) 检查 event 名 + data 内容

    #[test]
    fn test_agent_event_to_sse_session_created() {
        let ev = agent_event_to_sse(Ok(AgentEvent::SessionCreated {
            session_id: "s-123".to_string(),
            memory_enabled: false,
            constitution: Some("pass@2".to_string()),
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
    fn test_agent_event_to_sse_llm_thinking_delta() {
        // 注:Event 的 Debug 输出 buffer 为 Bytes 字节串形式,非 ASCII 以 \xNN
        // 转义——断言文本须用 ASCII(中文值透传正确性由 serde_json 序列化保证,
        // ws 侧同型测试已用中文覆盖)
        let ev = agent_event_to_sse(Ok(AgentEvent::LlmThinkingDelta {
            text: "thinking...".to_string(),
        }))
        .unwrap();
        let s = format!("{:?}", ev);
        assert!(s.contains("llm_thinking_delta"));
        assert!(s.contains("thinking..."));
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
            container: None,
            execution_profile: None,
            tools: None,
            workspace: None,
            execution: None,
            resume_session_id: None,
            resume_mode: None,
            step_timeout_secs: None,
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

    // ===== B17 持久会话:恢复语义校验 + 重建回退合成 =====

    /// B17 请求构造助手(react 面,general 定义测试环境可加载)
    fn b17_run_request(
        goal: &str,
        resume_mode: Option<&str>,
        sid: Option<&str>,
    ) -> AgentRunRequest {
        AgentRunRequest {
            agent_type: "general".to_string(),
            goal: goal.to_string(),
            max_steps: None,
            temperature: None,
            model: None,
            container: None,
            execution_profile: None,
            tools: None,
            workspace: None,
            execution: None,
            resume_session_id: sid.map(|s| s.to_string()),
            resume_mode: resume_mode.map(|m| m.to_string()),
            step_timeout_secs: None,
        }
    }

    async fn b17_post_run(
        app: axum::Router,
        body: &AgentRunRequest,
    ) -> http::Response<axum::body::Body> {
        app.oneshot(
            Request::builder()
                .method(http::Method::POST)
                .uri("/agents/general/run")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_run_rejects_unknown_resume_mode() {
        // J-B17-5a:resume_mode 非法值 400(fail-visible,先于 runner 构造)
        let app = router(make_test_state());
        let response = b17_post_run(app, &b17_run_request("g", Some("bogus"), Some("s1"))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_run_rejects_resume_mode_without_session() {
        // J-B17-5b:resume_mode 在场但缺 resume_session_id 400
        let app = router(make_test_state());
        let response = b17_post_run(app, &b17_run_request("g", Some("continue"), None)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_run_rejects_continue_with_empty_goal() {
        // J-B17-5c:continue 空 goal 400(新输入是续聊的硬前提)
        let app = router(make_test_state());
        let response =
            b17_post_run(app, &b17_run_request("   ", Some("continue"), Some("s1"))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    fn b17_msg(role: &str, content: &str) -> crate::agent::memory::MessageRecord {
        crate::agent::memory::MessageRecord {
            idx: 0,
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: None,
            tool_name: None,
            timestamp: 0,
            reasoning: None,
            fact_id: None,
        }
    }

    fn b17_tool(name: &str, content: &str) -> crate::agent::memory::MessageRecord {
        crate::agent::memory::MessageRecord {
            tool_name: Some(name.to_string()),
            ..b17_msg("tool", content)
        }
    }

    #[test]
    fn test_compose_rebuild_context_filters_roles_and_keeps_order() {
        // J-B17-6:system 剔除 + tool 以名称+摘要入块 + 旧→新序 + goal 文末
        let history = vec![
            b17_msg("system", "sys prompt"),
            b17_msg("user", "u1"),
            b17_msg("assistant", "a1"),
            b17_tool("file_read", "tool out"),
            b17_msg("user", "u2"),
        ];
        let out = compose_rebuild_context(&history, "new goal");
        assert!(out.starts_with("[会话重建回注]"));
        assert!(out.contains("<recovered_history>"));
        assert!(out.contains("</recovered_history>"));
        assert!(!out.contains("sys prompt"));
        assert!(out.contains("[tool:file_read] tool out"));
        let u1 = out.find("[user] u1").unwrap();
        let a1 = out.find("[assistant] a1").unwrap();
        let t1 = out.find("[tool:file_read] tool out").unwrap();
        let u2 = out.find("[user] u2").unwrap();
        assert!(u1 < a1 && a1 < t1 && t1 < u2, "历史按时间正序回注");
        assert!(out.ends_with("new goal"), "新输入在文末");
    }

    #[test]
    fn test_compose_rebuild_context_assistant_outline() {
        // assistant 带 tool_calls → 调用轮廓行(工具名三源兼容)
        let mut a = b17_msg("assistant", "");
        a.tool_calls = Some(serde_json::json!([
            {"tool_name": "file_read", "args": {"path": "a"}},
            {"name": "shell_exec", "args": {"command": "ls"}}
        ]));
        let out = compose_rebuild_context(&[a], "goal");
        assert!(out.contains("[assistant] (随后调用 file_read、shell_exec)"));
    }

    #[test]
    fn test_compose_rebuild_context_truncates_per_message() {
        // J-B17-6:单行 4000 字符截断(按字符数,防切破多字节)
        let long = "x".repeat(8000);
        let out = compose_rebuild_context(&[b17_msg("user", &long)], "goal");
        assert!(out.contains(&"x".repeat(4000)));
        assert!(!out.contains(&"x".repeat(4001)));
    }

    #[test]
    fn test_compose_rebuild_context_respects_total_budget_newest_first() {
        // J-B17-6:总预算 24000 字符,自最新往回收集(100 条 × 500 字符 → 恰收 48 行)
        let history: Vec<_> = (0..100)
            .map(|i| b17_msg("user", &format!("m{i:02}{}", "y".repeat(497))))
            .collect();
        let out = compose_rebuild_context(&history, "g");
        let picked = out.matches("[user] m").count();
        assert_eq!(picked, 48, "24000/500=48 行入预算");
        assert!(out.contains("[user] m99"), "最新一条必在");
        assert!(out.contains("[user] m52"), "预算内最旧一条");
        assert!(!out.contains("[user] m51"), "预算外旧条被裁");
    }

    #[test]
    fn test_compose_rebuild_context_empty_history_keeps_goal() {
        // 无历史(快照亦空)时块退化但合成仍合法(空历史在调用方已被 400 拦截,
        // 此处锁纯函数自身行为)
        let out = compose_rebuild_context(&[], "goal");
        assert!(out.contains("<recovered_history>\n</recovered_history>"));
        assert!(out.ends_with("goal"));
    }

    #[test]
    fn test_transcript_pagination_cursor_and_limit() {
        let mk = |i: usize| crate::agent::memory::MessageRecord {
            idx: i,
            role: "user".to_string(),
            content: format!("m{i}"),
            tool_calls: None,
            tool_name: None,
            reasoning: None,
            timestamp: 0,
            fact_id: None,
        };
        let idxs = |v: &[crate::agent::memory::MessageRecord]| -> Vec<usize> {
            v.iter().map(|m| m.idx).collect()
        };
        // 无参 → 全量,has_more=false
        let mut all: Vec<_> = (0..10).map(mk).collect();
        assert!(!apply_transcript_pagination(&mut all, None, None));
        assert_eq!(idxs(&all), (0..10).collect::<Vec<_>>());
        // 仅 limit → 自最新端截取,has_more=true
        let mut paged: Vec<_> = (0..10).map(mk).collect();
        assert!(apply_transcript_pagination(&mut paged, None, Some(3)));
        assert_eq!(idxs(&paged), vec![7, 8, 9]);
        // 游标+limit → 更早侧最新段
        let mut cur: Vec<_> = (0..10).map(mk).collect();
        assert!(apply_transcript_pagination(&mut cur, Some(7), Some(3)));
        assert_eq!(idxs(&cur), vec![4, 5, 6]);
        // 游标后不足 limit → 全给,has_more=false
        let mut tail: Vec<_> = (0..10).map(mk).collect();
        assert!(!apply_transcript_pagination(&mut tail, Some(2), Some(5)));
        assert_eq!(idxs(&tail), vec![0, 1]);
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

    // ===== 记忆事件账本读面:namespace 同域解析（纯函数核心） =====

    #[test]
    fn test_resolve_events_namespace_matches_write_side() {
        // 桌面捆绑 general 定义(persistent + namespace "general")——写侧实际域
        assert_eq!(
            resolve_events_namespace("general", "persistent", "general"),
            "general"
        );
        // general 未启用记忆:写侧 construct_runner 同款例外注入 "general"
        assert_eq!(resolve_events_namespace("general", "none", ""), "general");
        assert_eq!(resolve_events_namespace("general", "", ""), "general");
        // 非 general 自定义声明:按声明(读写同构)
        assert_eq!(
            resolve_events_namespace("researcher", "persistent", "researcher_ns"),
            "researcher_ns"
        );
        // general 显式声明自定义 ns:声明优先,例外注入不覆盖声明
        assert_eq!(
            resolve_events_namespace("general", "persistent", "custom_ns"),
            "custom_ns"
        );
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

    // ===== 治理叠加:进化信号只读代理 =====

    #[tokio::test]
    async fn test_evolution_signals_non_numeric_session_returns_400() {
        // 会话 id 非数字 → 400(不触 evorule 客户端)
        let state = make_test_state();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/not-a-number/evolution-signals")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_evolution_signals_proxies_evorule_payload() {
        // evorule-server 正常返回 → 透传聚合载荷(total_violations/signals/queue)
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/42/evolution-signals")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"session_id":42,"total_violations":3,"signals":[],"queue":{"pending_normal":0,"pending_meta_promotion":0}}"#,
            )
            .create_async()
            .await;

        let state = AgentApiState::new(
            crate::agent::AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new(&server.url()),
        );
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/42/evolution-signals")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total_violations"], 3);
        assert!(json["signals"].is_array());
    }
}
