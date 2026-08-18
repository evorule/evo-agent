// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent runner -- ReAct loop execution core (event-driven framework)
//!
//! Full Fact loop flow:
//! AgentRunner submits Command -> POST /api/sessions/{id}/command -> evorule produces IoRequest ->
//! SSE pushes io_request event -> AgentRunner executes external call -> POST /api/sessions/{id}/io_response ->
//! evorule produces IoResponse + StateTransition -> SSE pushes stable event -> AgentRunner returns result

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use evorule_tcb::JsonValue;
use futures_core::Stream;
use futures_util::StreamExt;
use serde_json::Value;
use tracing::{info, warn};

use tokio_util::sync::CancellationToken;

use crate::agent::approval::{parse_approval_request, ApprovalCallback};
use crate::agent::callback::CallbackChain;
use crate::agent::context_window::{ContextWindowManager, TrimStrategy};
use crate::agent::definition::{AgentDefinition, OutputFormat};
use crate::agent::delegate::DelegateContext;
use crate::agent::memory::{MemoryManager, MessagePersistMode, MessageRecord};
use crate::agent::memory_event::extraction::{EventExtractor, ExtractionConfig};
use crate::agent::memory_event::MemoryEventStore;
use crate::agent::output_validator::OutputValidator;
use crate::agent::sediment;
use crate::agent::summarizer::ContextSummarizer;
use crate::agent::translator::{LlmResponse, Message};
use crate::api::api_core::ApiError;
use crate::api::evorule_client::EvoruleApiClient;
use crate::api::metrics::{SessionActiveGuard, SharedMetrics};
use crate::io_handler::IoHandler;
use crate::io_handlers::{LlmHandler, StreamChunk, ToolHandler};
use crate::json_convert::serde_to_tcb;

/// TODO: doc
pub const DEFAULT_MAX_DELEGATE_DEPTH: usize = 3;

#[derive(Debug, Clone)]
/// TODO: doc
pub struct AgentConfig {
    /// TODO: doc
    pub agent_type: String,
    /// TODO: doc
    pub system_prompt: String,
    /// TODO: doc
    pub model: String,
    /// TODO: doc
    pub temperature: f32,
    /// TODO: doc
    pub max_steps: usize,
    /// TODO: doc
    pub step_timeout: Duration,
    /// TODO: doc
    pub tool_names: Vec<String>,
    /// TODO: doc
    pub llm_retry_count: usize,
    /// G11:结构化输出格式(None = 不校验)
    pub output_format: Option<OutputFormat>,
    /// G13:单轮内并行工具调用上限(1 = 串行,>1 = active 工具并行)
    ///
    /// candidate 工具(需审批)始终串行,不受此参数影响。
    pub max_parallel_tools: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent_type: "default".to_string(),
            system_prompt: "You are a helpful assistant".to_string(),
            model: "gpt-4o-mini".to_string(),
            temperature: 0.7,
            max_steps: 10,
            step_timeout: Duration::from_secs(60),
            tool_names: Vec::new(),
            llm_retry_count: 3,
            output_format: None,
            max_parallel_tools: 1,
        }
    }
}

#[derive(Debug, Clone)]
/// TODO: doc
pub enum AgentError {
    /// TODO: doc
    LlmError(String),
    /// TODO: doc
    ToolError(String),
    /// TODO: doc
    Timeout(String),
    /// TODO: doc
    MaxStepsExceeded(usize),
    /// TODO: doc
    DelegateError(String),
    /// TODO: doc
    MemoryError(String),
    /// TODO: doc
    Internal(String),
    /// TODO: doc
    EvoruleError(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::LlmError(e) => write!(f, "LLM error: {}", e),
            AgentError::ToolError(e) => write!(f, "Tool error: {}", e),
            AgentError::Timeout(e) => write!(f, "Timeout: {}", e),
            AgentError::MaxStepsExceeded(s) => write!(f, "Max steps exceeded: {}", s),
            AgentError::DelegateError(e) => write!(f, "Delegate error: {}", e),
            AgentError::MemoryError(e) => write!(f, "Memory error: {}", e),
            AgentError::Internal(e) => write!(f, "Internal error: {}", e),
            AgentError::EvoruleError(e) => write!(f, "Evorule error: {}", e),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<ApiError> for AgentError {
    fn from(e: ApiError) -> Self {
        AgentError::EvoruleError(e.to_string())
    }
}

impl From<crate::agent::memory::MemoryError> for AgentError {
    fn from(e: crate::agent::memory::MemoryError) -> Self {
        AgentError::MemoryError(e.to_string())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
/// TODO: doc
pub struct AgentResult {
    /// TODO: doc
    pub success: bool,
    /// TODO: doc
    pub content: String,
    /// TODO: doc
    pub steps: usize,
    /// TODO: doc
    pub duration_ms: u64,
    /// TODO: doc
    pub tool_calls: Vec<String>,
    /// TODO: doc
    pub error: Option<String>,
    /// 是否为用户取消(区别于执行失败)
    pub cancelled: bool,
}

impl AgentResult {
    /// TODO: doc
    pub fn success(
        content: String,
        steps: usize,
        duration_ms: u64,
        tool_calls: Vec<String>,
    ) -> Self {
        Self {
            success: true,
            content,
            steps,
            duration_ms,
            tool_calls,
            error: None,
            cancelled: false,
        }
    }

    /// TODO: doc
    pub fn error(error: String, steps: usize, duration_ms: u64) -> Self {
        Self {
            success: false,
            content: String::new(),
            steps,
            duration_ms,
            tool_calls: Vec::new(),
            error: Some(error),
            cancelled: false,
        }
    }

    /// 构造取消结果(success=false, cancelled=true)
    pub fn cancelled(error: String, steps: usize, duration_ms: u64) -> Self {
        Self {
            success: false,
            content: String::new(),
            steps,
            duration_ms,
            tool_calls: Vec::new(),
            error: Some(error),
            cancelled: true,
        }
    }
}

/// Fallback: 从 LLM 文本内容中尝试解析 JSON 格式的 tool call。
///
/// 当 LLM 未使用 function calling 协议(即 tool_calls 为 None/空),
/// 但在文本中输出了形如 `{"name":"file_read","parameters":{...}}` 的 JSON 时,
/// 尝试提取为 ToolCall。
///
/// 支持以下形态:
/// 1. 单个 JSON 对象: `{"name":"file_read","parameters":{...}}`
/// 2. JSON 数组: `[{"name":"...",...}, {"name":"...",...}]`
/// 3. 连续多行 JSON: 每行一个 JSON 对象(LLM 自然倾向)
/// 4. 带 markdown 代码块包裹: ```` ```json\n{...}\n``` ````
///
/// 仅在 content 包含合法 JSON 且有 name/tool_name 字段时触发,
/// 避免误解析普通用户文本中的 JSON。
fn try_parse_tool_call_from_text(content: &str) -> Option<Vec<crate::agent::translator::ToolCall>> {
    let trimmed = content.trim();

    // 先尝试整体解析为单个 JSON 值(覆盖形态 1 和 2)
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(calls) = parse_tool_calls_from_json(&json) {
            return Some(calls);
        }
    }

    // 形态 4: 去除 markdown 代码块包裹后重试
    let cleaned = strip_markdown_codeblock(trimmed);
    if cleaned != trimmed {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&cleaned) {
            if let Some(calls) = parse_tool_calls_from_json(&json) {
                return Some(calls);
            }
        }
    }

    // 形态 3: 逐行解析连续多 JSON 行
    let lines: Vec<&str> = cleaned
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    if lines.len() > 1 {
        let mut calls = Vec::new();
        for line in &lines {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(parsed) = parse_tool_calls_from_json(&json) {
                    calls.extend(parsed);
                }
            }
        }
        if !calls.is_empty() {
            return Some(calls);
        }
    }

    None
}

/// 从 JSON Value 中提取 tool calls(单个对象或数组)
fn parse_tool_calls_from_json(
    json: &serde_json::Value,
) -> Option<Vec<crate::agent::translator::ToolCall>> {
    // 单个对象: {"name": "...", "parameters": {...}}
    if let Some(name) = json.get("name").and_then(|v| v.as_str()) {
        let args = json
            .get("parameters")
            .or_else(|| json.get("args"))
            .cloned()
            .unwrap_or(Value::Null);
        return Some(vec![crate::agent::translator::ToolCall {
            name: name.to_string(),
            arguments: args,
        }]);
    }

    // 单个对象: {"tool_name": "...", "args": {...}}
    if let Some(name) = json.get("tool_name").and_then(|v| v.as_str()) {
        let args = json
            .get("args")
            .or_else(|| json.get("parameters"))
            .cloned()
            .unwrap_or(Value::Null);
        return Some(vec![crate::agent::translator::ToolCall {
            name: name.to_string(),
            arguments: args,
        }]);
    }

    // 数组: [{"name": "...", ...}, ...]
    if let Some(arr) = json.as_array() {
        let mut calls = Vec::new();
        for item in arr {
            if let Some(name) = item
                .get("name")
                .or_else(|| item.get("tool_name"))
                .and_then(|v| v.as_str())
            {
                let args = item
                    .get("parameters")
                    .or_else(|| item.get("args"))
                    .cloned()
                    .unwrap_or(Value::Null);
                calls.push(crate::agent::translator::ToolCall {
                    name: name.to_string(),
                    arguments: args,
                });
            }
        }
        if !calls.is_empty() {
            return Some(calls);
        }
    }

    None
}

/// 去除 markdown 代码块包裹(```json ... ``` 或 ``` ... ```)
fn strip_markdown_codeblock(s: &str) -> String {
    let trimmed = s.trim();
    if !trimmed.starts_with("```") {
        return s.to_string();
    }
    // 去掉第一行(```json 或 ```)
    let after_first_line = match trimmed.find('\n') {
        Some(pos) => &trimmed[pos + 1..],
        None => return s.to_string(),
    };
    // 去掉结尾的 ```
    let result = after_first_line.trim_end();
    if result.ends_with("```") {
        result[..result.len() - 3].trim().to_string()
    } else {
        result.to_string()
    }
}

/// G4:Agent 执行过程中的事件流
///
/// 由 `run_streaming` 产出,调用方按需消费:
/// - 前端 SSE 网关:把每个 event 转 SSE 帧
/// - CLI:打印 Step / Delta / ToolCall,缓存 Done
/// - 审计观察者:全量记录到日志
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 会话已创建
    SessionCreated {
        /// evorule session ID
        session_id: String,
    },
    /// 进入第 N 步
    Step {
        /// 当前步数(从 1 开始)
        step: usize,
    },
    /// LLM 输出增量(来自 G1 的 `StreamChunk::Delta`)
    LlmDelta {
        /// 本次帧的增量文本
        text: String,
    },
    /// LLM 调用了工具
    ToolCall {
        /// 工具名称
        name: String,
        /// 工具参数
        args: serde_json::Value,
    },
    /// 工具执行完成
    ToolResult {
        /// 工具名称
        name: String,
        /// 工具返回结果
        result: serde_json::Value,
    },
    /// LLM 输出完成(本轮)
    LlmDone {
        /// 完整内容(聚合自所有 delta)
        content: String,
        /// 结束原因(stop / end_turn / tool_calls 等)
        finish_reason: Option<String>,
    },
    /// 整个任务完成
    Done(AgentResult),
    /// 错误(可能可恢复,看 caller 决定是否中断)
    Error(AgentError),
    /// 中间状态信息(如 auto_rewind 触发)
    Info(String),
    /// G8:工具需要用户审批(candidate 工具返回 needs_approval)
    ApprovalRequired {
        /// 工具名称
        tool_name: String,
        /// 命令描述
        command: String,
        /// 风险说明
        risk: String,
        /// 替代方案
        alternative: String,
    },
    /// G8:审批结果
    ApprovalResult {
        /// 工具名称
        tool_name: String,
        /// 是否批准
        approved: bool,
    },
}

/// TODO: doc
pub struct AgentRunner {
    config: AgentConfig,
    evorule_client: EvoruleApiClient,
    llm_handler: LlmHandler,
    tool_handler: ToolHandler,
    memory: Option<MemoryManager>,
    delegate_context: Option<DelegateContext>,
    session_id: Option<String>,
    join_cluster_id: Option<u64>,
    /// 消息持久化模式（用户决策 2：可选开关）
    ///
    /// 控制 messages 何时写入 evorule payload。默认 `EveryMessage`。
    /// 由 `AgentDefinition.memory.message_persist` 配置解析而来。
    message_persist_mode: MessagePersistMode,
    /// 待刷写的消息缓冲区（用于 `EveryN` / `PerReactRound` 模式）
    ///
    /// 元组 `(idx, Message)` 中 idx 是消息在 `messages` 数组中的索引，
    /// 也是 evorule payload path 的最后一段（`messages.{idx}`）。
    pending_messages: Vec<(usize, Message)>,
    /// 摘要模型名称（用户决策 3：单独配置 summary_model）
    ///
    /// P4 阶段用于记忆压缩。P0+P1 阶段仅存储不使用。
    summary_model: Option<String>,
    /// G2:上下文窗口管理器
    ///
    /// `Some` 时,`handle_call_external` 在发送给 LLM 前裁剪 `messages`,
    /// 保留 system + 最近若干轮(含 tool_call/tool_result 原子对)。
    /// `None` 时不裁剪(向后兼容旧行为)。
    context_window: Option<ContextWindowManager>,
    /// G10:上下文摘要器(记忆压缩)
    ///
    /// `Some` 时,裁剪掉的消息会送给摘要 LLM 生成摘要,替换 `[earlier N messages trimmed]` 提示。
    /// `None` 时(未配置 summary_model),裁剪后只保留原 hint(向后兼容旧行为)。
    /// 由 `from_definition` 在 `def.memory.summary_model` 配置时自动构造。
    summarizer: Option<ContextSummarizer>,
    /// G6:取消令牌(外部可触发 cancel())
    ///
    /// `run()` / `run_streaming()` 在 SSE event 边界与 LLM 调用处用 `select!`
    /// 监听 `cancelled()`,触发后优雅清理(提交 error io_response、flush 消息)。
    cancel_token: CancellationToken,
    /// G11:结构化输出校验器(None = 不校验)
    ///
    /// 由 `from_definition` 从 `def.output_format` 构造。
    /// `handle_call_external` 中:注入格式指令到 system prompt + 校验 LLM 输出。
    output_validator: Option<OutputValidator>,
    /// G11:当前步骤的格式校验重试计数(超过 max 时降级为接受原输出)
    output_format_retries: usize,
    /// G8:工具审批回调(None = 默认拒绝 candidate 工具,安全优先)
    ///
    /// 由 `from_definition` 不自动构造(需要外部运行环境决定 CLI/HTTP 模式)。
    /// CLI 模式:`with_approval_callback(Arc::new(CliApproval { auto_approve: ... }))`
    /// HTTP 模式:`with_approval_callback(Arc::new(HttpApproval::new(pending)))`
    approval_callback: Option<Arc<dyn ApprovalCallback>>,
    /// G13:并行工具结果缓存
    ///
    /// 当 `max_parallel_tools > 1` 时,`call_external` 分支会并行执行所有 tool_calls,
    /// active 工具的结果(非 proposal)存入此缓存;后续 `call_service` IoRequest
    /// 命中缓存则直接返回(不重复执行),未命中(candidate proposal)则正常走审批。
    ///
    /// key = `"{tool_name}:{serde(args)}"`,value = 工具返回的 JsonValue。
    /// 每次 `call_external` 开始时清空(新一轮 LLM 调用,旧缓存失效)。
    parallel_tool_cache: Arc<std::sync::Mutex<std::collections::HashMap<String, JsonValue>>>,
    /// G17:Prometheus 指标(None = 不插桩,如 CLI 模式)
    ///
    /// 由 `with_metrics()` 注入。serve 模式下从 `AgentApiState.metrics` clone。
    /// session/step/LLM/工具关键路径在 `Some` 时记录指标,`None` 时全部 no-op。
    metrics: Option<SharedMetrics>,
    /// G18:结构化事件回调链
    ///
    /// 由 `with_event_callback()` 追加。在 `run_streaming()` 的每个事件 yield 点
    /// 被 `dispatch()` 调用(同步 await + 1s 超时 + panic 保护)。
    /// 用 `Arc` 包装以便在 `with_event_callback` 中 `make_mut` 追加回调。
    event_callbacks: Arc<CallbackChain>,
    /// G14:032 MemoryEvent 存储(None = 不启用结构化记忆事件)
    ///
    /// 由 `with_memory_event_store()` 注入,或由 `from_definition` 在
    /// `def.memory` 配置启用时自动构造。持有后,对话结束后可触发
    /// `EventExtractor` 提取结构化事件,`ReplayEngine` 可回放因果链。
    memory_event_store: Option<MemoryEventStore>,
    /// C1:事件提取器(None = memory 未启用)
    ///
    /// 由 `from_definition` 在 `def.memory.memory_type != "none"` 时自动构造。
    /// 会话结束时 `sediment_session()` 借用此提取器做事件提取(C1 阶段占位)。
    extractor: Option<EventExtractor>,
    /// C1:沉淀配置
    ///
    /// 由 `from_definition` 从 `def.memory` 自动构造。控制会话沉淀行为
    /// (命名空间、事件提取开关、摘要上限等)。
    sediment_config: sediment::SedimentConfig,
    /// C3:总上下文窗口 token 数（由 `def.context_window_tokens` 构造，默认 8192）
    ///
    /// 与 `memory_budget_ratio` 一起构造 `ContextBudget`，控制记忆区占比。
    max_context_tokens: usize,
    /// C3:记忆区占窗口比例（由 `def.memory.memory_budget_ratio` 构造，默认 0.25）
    memory_budget_ratio: f32,
}

impl AgentRunner {
    /// TODO: doc
    pub fn new(config: AgentConfig, evorule_client: EvoruleApiClient) -> Self {
        Self {
            config,
            evorule_client,
            llm_handler: LlmHandler::with_defaults(),
            tool_handler: ToolHandler::new(),
            memory: None,
            delegate_context: None,
            session_id: None,
            join_cluster_id: None,
            message_persist_mode: MessagePersistMode::default(),
            pending_messages: Vec::new(),
            summary_model: None,
            context_window: None,
            summarizer: None,
            cancel_token: CancellationToken::new(),
            output_validator: None,
            output_format_retries: 0,
            approval_callback: None,
            parallel_tool_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            metrics: None,
            event_callbacks: Arc::new(CallbackChain::new()),
            memory_event_store: None,
            extractor: None,
            sediment_config: sediment::SedimentConfig::default(),
            max_context_tokens: 8192,
            memory_budget_ratio: 0.25,
        }
    }

    /// 当前 agent 类型(读访问 — 5 原则:**透明**)
    pub fn agent_type(&self) -> &str {
        &self.config.agent_type
    }

    /// 当前 agent 配置的只读快照(读访问 — **透明**)
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// G6:外部触发取消
    ///
    /// 触发后,`run()` / `run_streaming()` 会在下一个 SSE event 边界或 LLM chunk 边界
    /// 返回 `error: "cancelled by user"`(并优雅清理:提交 error io_response、flush 消息)。
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// G6:是否已被取消
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

    /// G6:获取取消令牌的引用(用于在 `select!` 中监听,或 clone 后存入 SessionStore)
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }

    /// 从 `AgentDefinition` + 预组装的组件构造一个**完整可跑**的 AgentRunner
    ///
    /// 责任:
    /// 1. 把 `AgentDefinition` 字段映射到 `AgentConfig`(已经由 `to_agent_config()` 做好)
    /// 2. 校验 `def.tools` 全部已在 `tool_handler` 注册(早失败,**可控**)
    /// 3. 根据 `def.memory.memory_type` 决定是否装 MemoryManager
    /// 4. 把所有部件组装成 AgentRunner
    ///
    /// # 参数
    /// - `def`:AgentDefinition(从 `agent.json` 加载)
    /// - `client`:evorule HTTP 客户端
    /// - `tool_handler`:**已经组装好**的 ToolHandler(用 `builtin_tools::default_safe_toolkit(workdir)` 或自己组)
    /// - `llm_handler`:可选 LLM handler,None 时从 env 自动读
    ///
    /// # 错误
    /// - `def.tools` 里有名字在 `tool_handler` 中**没注册**:返回 `AgentError::Internal`
    /// - `def.memory.memory_type == "persistent"` 但 evorule 拉取失败:返回 `AgentError::MemoryError`
    ///
    /// # 示例
    /// ```ignore
    /// use evo_agent::builtin_tools::default_safe_toolkit;
    /// use evo_agent::config::Config;
    /// use evo_agent::agent::definition::AgentDefinitionManager;
    ///
    /// let config = Config::load(Path::new("."))?;
    /// let def_mgr = AgentDefinitionManager::new(config.agents.dir);
    /// let def = def_mgr.load("general")?;
    ///
    /// let client = EvoruleApiClient::new(&config.evorule.base_url);
    /// let tool_handler = default_safe_toolkit(Path::new("."));
    ///
    /// let mut runner = AgentRunner::from_definition(def, client, tool_handler, None).await?;
    /// let result = runner.run("hello world").await?;
    /// ```
    pub async fn from_definition(
        def: AgentDefinition,
        client: EvoruleApiClient,
        tool_handler: ToolHandler,
        llm_handler: Option<LlmHandler>,
    ) -> Result<Self, AgentError> {
        // 1. 配置
        let config = def.to_agent_config();

        // 2. 校验:def.tools 全部已在 tool_handler 注册
        // (早失败:用户能在跑之前就发现配错,而不是跑一半才挂)
        for tool_name in &config.tool_names {
            if !tool_handler.has_tool(tool_name) {
                return Err(AgentError::Internal(format!(
                    "agent '{}' requires tool '{}' but it is NOT registered in tool_handler; \
                     check your agent.json 'tools' list vs the tool_handler you passed",
                    def.agent_type, tool_name
                )));
            }
        }

        // 3. Memory(按 spec 决定要不要)
        let memory = if def.memory.memory_type == "none" || def.memory.memory_type.is_empty() {
            None
        } else {
            // 0.1.0 简化:只支持 "persistent" 模式("none" 已处理)
            // 其他 memory_type 未来加
            if def.memory.memory_type != "persistent" {
                return Err(AgentError::Internal(format!(
                    "agent '{}' has unsupported memory.type '{}'; \
                     0.1.0 supports 'none' and 'persistent'",
                    def.agent_type, def.memory.memory_type
                )));
            }
            let mut mem = MemoryManager::new(&def.memory.namespace, client.clone());
            // 用户决策 5：TTL 配置传递给 MemoryManager
            if let Some(ttl) = def.memory.ttl_secs {
                mem = mem.with_ttl_secs(ttl);
            }
            // 同步:从 evorule 把 namespace 下的 facts 拉下来
            mem.sync_from_evorule().await?;
            Some(mem)
        };

        // 用户决策 2：解析 message_persist 配置为 MessagePersistMode
        let persist_mode = def
            .memory
            .message_persist
            .to_mode()
            .map_err(AgentError::Internal)?;

        // 4. LLM handler(默认从 env 读)
        let llm = llm_handler.unwrap_or_else(LlmHandler::with_defaults);

        // 5. 组装
        let mut runner = Self::new(config, client)
            .with_llm_handler(llm)
            .with_tool_handler(tool_handler)
            .with_message_persist_mode(persist_mode);
        if let Some(mem) = memory {
            runner = runner.with_memory(mem);
        }
        // G14:自动构造 MemoryEventStore(与 MemoryManager 共享 namespace + client)
        // 启用条件:memory_type != "none"(与 MemoryManager 一致)
        // session_id 在 runner.run() 创建/加载 session 后通过 set_session_id 设置
        if def.memory.memory_type != "none" {
            let event_store =
                MemoryEventStore::new(&def.memory.namespace, runner.evorule_client.clone());
            runner = runner.with_memory_event_store(event_store);
        }
        // C1:自动构造 EventExtractor(memory 启用时)
        // extractor 在 sediment_session() 中被借用做事件提取(C1 阶段占位)
        if def.memory.memory_type != "none" {
            let config = ExtractionConfig {
                extraction_model: def.memory.extraction_model.clone(),
                ..Default::default()
            };
            let extractor = EventExtractor::new(runner.llm_handler.clone(), config);
            runner.extractor = Some(extractor);
        }
        // C1:沉淀配置(总是构造,sediment_session 在 memory 为 None 时是 no-op)
        // C3/C4:从 MemoryConfig 读取 max_session_summaries/max_injected_events/
        //        summary_rollup_threshold/enable_event_extraction
        runner.sediment_config = sediment::SedimentConfig {
            namespace: def.memory.namespace.clone(),
            enable_event_extraction: def.memory.memory_type != "none"
                && def.memory.enable_event_extraction,
            max_session_summaries: def.memory.max_session_summaries,
            max_injected_events: def.memory.max_injected_events,
            summary_rollup_threshold: def.memory.summary_rollup_threshold,
        };
        // 用户决策 3 + G10：summary_model 单独配置,同时构造 ContextSummarizer
        if let Some(sm) = def.memory.summary_model {
            runner = runner.with_summary_model(&sm);
            // G10:clone 主 LlmHandler 给摘要器(共享 API key/配置,独立调用)
            // summary_model 指定后,摘要调用使用该模型;否则 fallback 到主 model
            let summarizer = ContextSummarizer::new(runner.llm_handler.clone(), Some(sm));
            runner = runner.with_summarizer(summarizer);
        }
        // G2:自动构造 ContextWindowManager(默认 8192 token,reserve 1/4)
        let max_tokens = def.context_window_tokens.unwrap_or(8192);
        let reserve = max_tokens / 4;
        let ctx_mgr = ContextWindowManager::with_approx_counter(
            max_tokens,
            reserve,
            TrimStrategy::KeepSystemKeepLast,
        );
        runner = runner.with_context_window(ctx_mgr);
        // C3:记录总窗口 token 与记忆区占比,供 run() 构造 ContextBudget
        runner.max_context_tokens = max_tokens;
        runner.memory_budget_ratio = def.memory.memory_budget_ratio;

        // G11:从 def.output_format 构造 OutputValidator
        // schema 编译失败时早失败(可控),不让用户跑一半才发现配错
        if let Some(fmt) = &def.output_format {
            match OutputValidator::from_output_format(fmt) {
                Ok(validator) => {
                    runner = runner.with_output_validator(validator);
                }
                Err(e) => {
                    return Err(AgentError::Internal(format!(
                        "agent '{}' has invalid output_format schema: {}",
                        def.agent_type, e
                    )));
                }
            }
        }

        Ok(runner)
    }

    /// Replace default LLM Handler (used to actually wire up specific provider)
    pub fn with_llm_handler(mut self, llm_handler: LlmHandler) -> Self {
        self.llm_handler = llm_handler;
        self
    }

    /// TODO: doc
    pub fn with_tool_handler(mut self, tool_handler: ToolHandler) -> Self {
        self.tool_handler = tool_handler;
        self
    }

    /// TODO: doc
    pub fn with_memory(mut self, memory: MemoryManager) -> Self {
        self.memory = Some(memory);
        self
    }

    /// G14:注入 032 MemoryEvent 存储
    ///
    /// 启用后,runner 持有 `MemoryEventStore`,可用于:
    /// - 对话结束后触发 `EventExtractor` 提取结构化事件
    /// - `ReplayEngine` 回放因果链("则灵"生活回放核心)
    ///
    /// 通常由 `from_definition` 在 `def.memory` 配置启用时自动构造,
    /// 也可手动注入(如测试或自定义配置)。
    pub fn with_memory_event_store(mut self, store: MemoryEventStore) -> Self {
        self.memory_event_store = Some(store);
        self
    }

    /// G14:获取 MemoryEvent 存储(只读引用)
    ///
    /// 返回 `Some(&MemoryEventStore)` 时表示结构化记忆已启用。
    /// CLI `replay` 子命令和 HTTP `/replay` 端点通过此方法访问事件数据。
    pub fn memory_event_store(&self) -> Option<&MemoryEventStore> {
        self.memory_event_store.as_ref()
    }

    /// G14:获取 MemoryEvent 存储(可变引用)
    ///
    /// `EventExtractor` 写入事件时需要可变访问。
    pub fn memory_event_store_mut(&mut self) -> Option<&mut MemoryEventStore> {
        self.memory_event_store.as_mut()
    }

    /// TODO: doc
    pub fn with_delegate_context(mut self, ctx: DelegateContext) -> Self {
        // G9:同时注册 `delegate` 工具,让 LLM 能在 ReAct 循环中调用子 agent。
        // 工具是否对 LLM 可见还取决于 agent.json 的 `tools` 列表是否包含 "delegate";
        // 这里只提供**执行能力**(LLM 发出 delegate tool_call 时能解析执行)。
        let delegate_tool = Arc::new(crate::builtin_tools::delegate_tool::DelegateTool::new(
            ctx.clone(),
        ));
        self.tool_handler.register_tool("delegate", delegate_tool);
        self.delegate_context = Some(ctx);
        self
    }

    /// TODO: doc
    pub fn with_join_cluster(mut self, cluster_id: &str) -> Self {
        self.join_cluster_id = cluster_id.parse().ok();
        self
    }

    /// 设置消息持久化模式（用户决策 2：可选开关）
    ///
    /// 由 `from_definition` 自动从 `agent.json` 解析，通常不需要手动调用。
    pub fn with_message_persist_mode(mut self, mode: MessagePersistMode) -> Self {
        self.message_persist_mode = mode;
        self
    }

    /// 设置摘要模型（用户决策 3：单独配置 summary_model）
    ///
    /// P4 阶段用于记忆压缩。P0+P1 阶段仅存储不使用。
    pub fn with_summary_model(mut self, model: &str) -> Self {
        self.summary_model = Some(model.to_string());
        self
    }

    /// G2:设置上下文窗口管理器
    ///
    /// 通常由 `from_definition` 根据 `def.context_window_tokens` 自动构造,
    /// 测试或自定义场景可手动注入。
    pub fn with_context_window(mut self, mgr: ContextWindowManager) -> Self {
        self.context_window = Some(mgr);
        self
    }

    /// G10:设置上下文摘要器(记忆压缩)
    ///
    /// 通常由 `from_definition` 根据 `def.memory.summary_model` 自动构造。
    /// 设置后,`handle_call_external` 中裁剪掉的消息会被送给摘要 LLM 生成摘要,
    /// 替换 `[earlier N messages trimmed]` 占位提示。
    ///
    /// 测试或自定义场景可手动注入(如自定义阈值/提示)。
    pub fn with_summarizer(mut self, summarizer: ContextSummarizer) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    /// G11:设置结构化输出校验器
    ///
    /// 通常由 `from_definition` 根据 `def.output_format` 自动构造,
    /// 测试或自定义场景可手动注入。
    ///
    /// 设置后,`handle_call_external` 会:
    /// 1. 把格式指令注入到 system prompt
    /// 2. 清理 LLM 输出(去掉 markdown 代码块标记)
    /// 3. 用 JSON Schema 校验输出
    /// 4. 校验失败时注入校正消息并让 LLM 重试(最多 `validator.max_retries()` 次)
    pub fn with_output_validator(mut self, validator: OutputValidator) -> Self {
        self.output_validator = Some(validator);
        self
    }

    /// G8:设置工具审批回调
    ///
    /// - `Some(callback)`:candidate 工具返回 `needs_approval` 时,通过 callback 问用户
    /// - `None`(默认):candidate 工具被自动拒绝(安全优先)
    ///
    /// CLI 模式用 `CliApproval`,HTTP 模式用 `HttpApproval`,测试用 `AutoApprove`/`DenyAll`。
    pub fn with_approval_callback(mut self, callback: Arc<dyn ApprovalCallback>) -> Self {
        self.approval_callback = Some(callback);
        self
    }

    /// G17:设置 Prometheus 指标(用于 session/step/LLM/工具关键路径插桩)
    ///
    /// 通常由 serve 模式的 API handler 从 `AgentApiState.metrics` clone 注入。
    /// CLI 模式不设置(默认 `None`),所有插桩点为 no-op。
    pub fn with_metrics(mut self, metrics: SharedMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// G18:追加一个事件回调
    ///
    /// 回调在 `run_streaming()` 的每个事件 yield 点被调用(同步 await + 1s 超时 + panic 保护)。
    /// 可多次调用以注册多个回调,调用顺序 = 注册顺序。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use evo_agent::agent::AgentRunner;
    /// # use evo_agent::agent::callback::LoggingCallback;
    /// # // ... construct runner ...
    /// # fn wrap(runner: AgentRunner) -> AgentRunner {
    /// runner.with_event_callback(Arc::new(LoggingCallback::new()))
    /// # }
    /// ```
    pub fn with_event_callback(
        mut self,
        cb: Arc<dyn crate::agent::callback::EventCallback>,
    ) -> Self {
        Arc::make_mut(&mut self.event_callbacks).push(cb);
        self
    }

    /// 持久化单条消息（按 `message_persist_mode` 决定立即写或缓冲）
    ///
    /// 在 `messages.push(...)` 之后调用。行为：
    /// - `EveryMessage`: 立即调用 `memory.append_message()` 写入 evorule
    /// - `EveryN(n)`: 加入 `pending_messages` 缓冲，达到 n 条时自动 flush
    /// - `PerReactRound`: 加入缓冲，等 IoRequest 处理前由 `flush_messages` 刷写
    /// - `Disabled`: 不做任何事
    ///
    /// 如果 `memory` 为 `None`（agent.json 配置 `memory.type = "none"`），
    /// 此方法是 no-op。
    async fn persist_message(
        &mut self,
        session_id: &str,
        idx: usize,
        message: Message,
    ) -> Result<(), AgentError> {
        if self.message_persist_mode.is_disabled() || self.memory.is_none() {
            return Ok(());
        }
        match self.message_persist_mode {
            MessagePersistMode::EveryMessage => {
                if let Some(memory) = self.memory.as_mut() {
                    memory.append_message(session_id, idx, &message).await?;
                }
                Ok(())
            }
            MessagePersistMode::EveryN(n) => {
                self.pending_messages.push((idx, message));
                if self.pending_messages.len() >= n {
                    self.flush_messages(session_id).await?;
                }
                Ok(())
            }
            MessagePersistMode::PerReactRound => {
                self.pending_messages.push((idx, message));
                Ok(())
            }
            MessagePersistMode::Disabled => Ok(()),
        }
    }

    /// 刷写所有缓冲的消息到 evorule payload
    ///
    /// 在以下场景被调用：
    /// - `EveryN` 模式达到阈值时（`persist_message` 内部触发）
    /// - `PerReactRound` 模式下 IoRequest 处理前（`run()` 显式调用）
    /// - `run()` 结束前（确保所有缓冲消息都写入）
    ///
    /// 如果没有缓冲消息或 `memory` 为 `None`，此方法是 no-op。
    async fn flush_messages(&mut self, session_id: &str) -> Result<(), AgentError> {
        if self.pending_messages.is_empty() || self.memory.is_none() {
            return Ok(());
        }
        let drained: Vec<(usize, Message)> = self.pending_messages.drain(..).collect();
        if let Some(memory) = self.memory.as_mut() {
            // &Vec<T> 自动 coercion 为 &[T]
            memory.append_messages_batch(session_id, &drained).await?;
        }
        Ok(())
    }

    /// C1:会话沉淀入口（best-effort）
    ///
    /// 在 `run()` / `run_streaming_inner()` 的 Stable 和 Error 分支返回前调用，
    /// 把整段对话的摘要和稳定事实写入共享空间。
    ///
    /// # Best-effort 语义
    ///
    /// - `memory` 为 `None` 时直接返回（no-op）
    /// - `summarizer` 为 `None` 时跳过摘要生成（仅 memory 启用但未配 summary_model）
    /// - LLM 调用 / 写入失败时记 `tracing::warn!`，不阻断会话返回
    ///
    /// # 借用说明
    ///
    /// `memory` / `summarizer` / `extractor` 是 `AgentRunner` 的不同字段，
    /// Rust 允许同时借用不同字段（disjoint borrows），不会冲突。
    async fn sediment_session(
        &mut self,
        session_id: &str,
        messages: &[Message],
    ) -> Result<(), AgentError> {
        if let Some(memory) = self.memory.as_mut() {
            let mut deps = sediment::SedimentDeps {
                memory,
                summarizer: self.summarizer.as_ref(),
                extractor: self.extractor.as_mut(),
            };
            let _ =
                sediment::sediment(&mut deps, &self.sediment_config, session_id, messages).await;
        }
        Ok(())
    }

    /// TODO: doc
    pub async fn run(&mut self, goal: &str) -> Result<AgentResult, AgentError> {
        let start_time = std::time::Instant::now();

        // C2: 召回顺序修复 —— recall 在 build_system_prompt 之前
        let recall = match self.memory.as_ref() {
            Some(mem) => {
                mem.recall_context(
                    goal,
                    self.sediment_config.max_session_summaries,
                    self.sediment_config.max_injected_events,
                )
                .await
            }
            None => crate::agent::memory::RecallContext::default(),
        };
        let system_prompt = match self.memory.as_ref() {
            Some(mem) => mem.build_system_prompt_with_recall(
                &self.config.system_prompt,
                &recall,
                &crate::agent::memory::ContextBudget::new(
                    self.max_context_tokens,
                    self.memory_budget_ratio,
                ),
            ),
            None => self.config.system_prompt.clone(),
        };

        let session_id = self.evorule_client.create_session(None).await?;
        self.session_id = Some(session_id.clone());
        info!(%session_id, "Created evorule session");

        // G14:同步 session_id 到 MemoryEventStore(若已注入)
        if let Some(store) = self.memory_event_store.as_mut() {
            store.set_session_id(&session_id);
            // best-effort 从 evorule 同步已有事件(HTTP 失败不阻塞)
            let _ = store.sync_from_evorule().await;
        }

        // G17:session 指标 — sessions_total + sessions_active(RAII guard 保证所有返回路径 dec)
        if let Some(m) = &self.metrics {
            m.inc_sessions_total();
        }
        let _session_guard = SessionActiveGuard::new(self.metrics.clone());

        let _recalled_fact_ids = self.auto_recall(&session_id).await?;

        if let Some(cluster_id) = self.join_cluster_id {
            self.evorule_client
                .join_cluster(&session_id, Some(cluster_id))
                .await?;
            info!(%session_id, cluster_id, "Joined cluster");
        }

        // 注意:必须先订阅 SSE 事件,再提交命令。
        // tokio broadcast 通道只接收订阅之后发出的消息,不重放历史。
        // 如果先 submit_command 再 subscribe,会错过 io_request 事件,导致 ReAct 循环无法启动。
        let mut event_stream = self.evorule_client.subscribe_events(&session_id).await?;

        let command = self.build_call_external_command(&system_prompt, goal);
        self.evorule_client
            .submit_command(&session_id, &command)
            .await?;
        info!(%session_id, "Submitted call_external command");
        let mut step_count = 0;
        let mut tool_calls: Vec<String> = Vec::new();
        let mut messages: Vec<Message> = Vec::new();

        if !system_prompt.is_empty() {
            messages.push(Message::System {
                content: system_prompt.clone(),
            });
            // P0: 持久化 system 消息（idx = 0）
            self.persist_message(
                &session_id,
                0,
                Message::System {
                    content: system_prompt,
                },
            )
            .await?;
        }
        let user_idx = messages.len();
        messages.push(Message::User {
            content: goal.to_string(),
        });
        // P0: 持久化 user 消息
        self.persist_message(
            &session_id,
            user_idx,
            Message::User {
                content: goal.to_string(),
            },
        )
        .await?;

        info!(%session_id, "Starting SSE event loop");
        while let Some(event) = event_stream.next().await {
            // G6:取消检查(event 边界 — 即使 LLM 调用已返回,也在此处响应取消)
            if self.cancel_token.is_cancelled() {
                info!(%session_id, "Cancellation requested at event boundary, cleaning up");
                let _ = self.flush_messages(&session_id).await;
                let duration = start_time.elapsed().as_millis() as u64;
                return Ok(AgentResult::cancelled(
                    "cancelled by user".to_string(),
                    step_count,
                    duration,
                ));
            }
            info!(%session_id, event_type = %event.event_type, "Received event");
            match event.event_type.as_str() {
                "IoRequest" => {
                    step_count += 1;
                    // G17:步数指标
                    if let Some(m) = &self.metrics {
                        m.inc_steps();
                    }
                    if step_count > self.config.max_steps {
                        let duration = start_time.elapsed().as_millis() as u64;
                        return Ok(AgentResult::error(
                            format!("Max steps exceeded: {}", self.config.max_steps),
                            step_count,
                            duration,
                        ));
                    }

                    // PerReactRound 模式：IoRequest 处理前刷写上一轮缓冲的消息
                    if matches!(self.message_persist_mode, MessagePersistMode::PerReactRound) {
                        self.flush_messages(&session_id).await?;
                    }

                    info!(%session_id, step = step_count, "Received IoRequest event");
                    // G6:clone token 避免 &mut self(handle_io_request) 与 &self(cancel_token) 借用冲突
                    let cancel_token = self.cancel_token.clone();
                    let result = tokio::select! {
                        r = self.handle_io_request(&session_id, &event.payload, &mut messages, &mut tool_calls) => r?,
                        _ = cancel_token.cancelled() => {
                            info!(%session_id, "Cancelled during io_request, cleaning up");
                            // 提交 error io_response 防止 evorule 卡死等 IoResponse
                            if let Some(rid) = event.payload.get("id").and_then(|v| v.as_u64()) {
                                let _ = self.evorule_client
                                    .submit_io_response(
                                        &session_id,
                                        rid,
                                        &serde_json::json!({"error": "cancelled"}),
                                        Some("cancelled"),
                                    )
                                    .await;
                            }
                            let _ = self.flush_messages(&session_id).await;
                            let duration = start_time.elapsed().as_millis() as u64;
                            return Ok(AgentResult::error(
                                "cancelled by user".to_string(),
                                step_count,
                                duration,
                            ));
                        }
                    };

                    if let Some(request_id) = event.payload.get("id").and_then(|v| v.as_u64()) {
                        self.evorule_client
                            .submit_io_response(&session_id, request_id, &result, None)
                            .await?;
                        info!(%session_id, request_id, "Submitted io_response");
                    }
                }
                "Stable" => {
                    let duration = start_time.elapsed().as_millis() as u64;
                    // 确保所有缓冲的消息都写入 evorule（EveryN/PerReactRound 模式）
                    self.flush_messages(&session_id).await?;
                    // C1:会话沉淀（best-effort，摘要+稳定事实→共享空间）
                    let _ = self.sediment_session(&session_id, &messages).await;
                    let state = self.evorule_client.get_state(&session_id).await?;
                    // payload 结构取决于 evorule 规则如何存储 io_response 结果。
                    // 默认规则将 call_external 的 io_response result 存储在
                    // payload.llm_response.content,因此优先读该路径;
                    // 若规则将结果直接放在 payload.content / payload.result,
                    // 或 payload 本身是字符串,则依次 fallback。
                    let content = state["payload"]["llm_response"]["content"]
                        .as_str()
                        .or_else(|| state["payload"]["content"].as_str())
                        .or_else(|| state["payload"]["result"].as_str())
                        .or_else(|| state["payload"].as_str())
                        .unwrap_or_default()
                        .to_string();

                    info!(%session_id, content_len = content.len(), "Received Stable event, execution complete");
                    return Ok(AgentResult::success(
                        content, step_count, duration, tool_calls,
                    ));
                }
                "StateTransition" => {
                    info!(%session_id, "State transition occurred");
                }
                "Error" => {
                    let error_msg = event
                        .payload
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    let duration = start_time.elapsed().as_millis() as u64;

                    if let Ok(rewind_result) = self.auto_rewind(&session_id).await {
                        info!(%session_id, "Auto-rewind successful, retrying from version {}", rewind_result);
                        continue;
                    }

                    // 错误返回前尝试刷写缓冲消息（best-effort，忽略 flush 错误）
                    let _ = self.flush_messages(&session_id).await;
                    // C1:会话沉淀（best-effort，即使出错也尝试沉淀已收集的对话）
                    let _ = self.sediment_session(&session_id, &messages).await;
                    return Ok(AgentResult::error(
                        error_msg.to_string(),
                        step_count,
                        duration,
                    ));
                }
                _ => {
                    info!(%session_id, event_type = %event.event_type, "Unknown event type");
                }
            }
        }

        let duration = start_time.elapsed().as_millis() as u64;
        info!(%session_id, step_count, duration_ms = duration, "SSE event loop ended (stream closed)");
        // 流关闭前也尝试刷写
        let _ = self.flush_messages(&session_id).await;
        Ok(AgentResult::error(
            "Event stream closed".to_string(),
            step_count,
            duration,
        ))
    }

    fn build_call_external_command(&self, system_prompt: &str, goal: &str) -> Value {
        serde_json::json!({
            "type": "call_external",
            "params": {
                "model": self.config.model,
                "temperature": self.config.temperature,
                "system_prompt": system_prompt,
                "goal": goal,
                "tool_names": self.config.tool_names,
            }
        })
    }

    async fn handle_io_request(
        &mut self,
        session_id: &str,
        payload: &Value,
        messages: &mut Vec<Message>,
        tool_calls: &mut Vec<String>,
    ) -> Result<Value, AgentError> {
        let io_type = payload
            .get("io_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Internal("missing io_type in IoRequest".to_string()))?;

        let params = payload.get("params").cloned().unwrap_or(Value::Null);

        match io_type {
            "call_external" => {
                self.handle_call_external(session_id, &params, messages)
                    .await
            }
            "call_service" => {
                self.handle_call_service(session_id, &params, messages, tool_calls)
                    .await
            }
            _ => Err(AgentError::Internal(format!(
                "unsupported io_type: {}",
                io_type
            ))),
        }
    }

    async fn handle_call_external(
        &mut self,
        session_id: &str,
        params: &Value,
        messages: &mut Vec<Message>,
    ) -> Result<Value, AgentError> {
        let model = params
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.config.model);
        let temperature = params
            .get("temperature")
            .and_then(|v| v.as_f64())
            .unwrap_or(self.config.temperature as f64);

        // G2+G10:发给 LLM 前裁剪 messages(不修改原 messages,只影响本次请求)
        // 被裁剪的消息仍保留在 `messages` 中(进审计链/replay),不影响 evorule payload
        // G10:如果有 summarizer,裁剪掉的消息会送给摘要 LLM 生成摘要,替换 hint
        let mut messages_to_send = if let Some(ctx) = &self.context_window {
            let mut trim_result = ctx.trim_detailed(messages);
            if !trim_result.dropped.is_empty() {
                info!(
                    dropped = trim_result.dropped.len(),
                    "trimmed history messages to fit context window"
                );
            }
            // G10:记忆压缩 — 如果有 summarizer,用摘要替换 [earlier N messages trimmed] 提示
            if let Some(summarizer) = &self.summarizer {
                if !trim_result.dropped.is_empty() {
                    match summarizer.summarize_dropped(&trim_result.dropped).await {
                        Ok(summary) if !summary.is_empty() => {
                            // 在 trim_result.messages 中找到 hint 消息并替换为摘要
                            for msg in &mut trim_result.messages {
                                if let Message::System { content } = msg {
                                    if content.starts_with("[earlier") {
                                        *content = summary.clone();
                                        break;
                                    }
                                }
                            }
                            info!(
                                %session_id,
                                dropped = trim_result.dropped.len(),
                                "G10: generated summary for dropped messages"
                            );
                        }
                        Ok(_) => {
                            tracing::debug!(
                                %session_id,
                                "G10: summary skipped (below threshold or empty)"
                            );
                        }
                        Err(e) => {
                            warn!(
                                %session_id,
                                error = %e,
                                "G10: summary generation failed, keeping original hint"
                            );
                        }
                    }
                }
            }
            trim_result.messages
        } else {
            messages.clone()
        };

        // G11:注入格式指令到 system prompt(只影响本次请求的 messages_to_send,不改原 messages)
        if let Some(validator) = &self.output_validator {
            let instruction = validator.instruction();
            if !instruction.is_empty() {
                for msg in &mut messages_to_send {
                    if let Message::System { content } = msg {
                        content.push_str(instruction);
                        break;
                    }
                }
            }
        }

        let serde_messages = serde_json::to_value(&messages_to_send)
            .map_err(|e| AgentError::Internal(format!("serialize messages: {}", e)))?;
        let tcb_messages = serde_to_tcb(&serde_messages);

        let mut call_params = BTreeMap::new();
        call_params.insert("model".to_string(), JsonValue::string(model.to_string()));
        call_params.insert(
            "temperature".to_string(),
            JsonValue::string(temperature.to_string()),
        );
        call_params.insert("messages".to_string(), tcb_messages);

        // G17:LLM 调用计时 + 指标(observe_llm_call 在 ? 之前记录,确保 error 也被统计)
        let llm_start = std::time::Instant::now();
        let llm_result = self
            .execute_external("call_external", &JsonValue::Object(call_params))
            .await;
        let llm_duration = llm_start.elapsed();
        let llm_ok = llm_result.is_ok();
        if let Some(m) = &self.metrics {
            m.observe_llm_call(model, llm_duration, llm_ok);
        }
        let llm_result = llm_result?;

        let llm_response: LlmResponse = serde_json::from_str(&llm_result.to_string())
            .map_err(|e| AgentError::Internal(format!("parse LLM response: {}", e)))?;

        // G11:结构化输出校验
        // 先提取校验结果(owned data),释放 &self.output_validator 的借用,
        // 才能后续 &mut self.output_format_retries / persist_message
        let validation_outcome = if let Some(validator) = &self.output_validator {
            let cleaned = validator.clean_output(&llm_response.content);
            let max_retries = validator.max_retries();
            let result = validator.validate(&cleaned);
            Some((cleaned, result, max_retries))
        } else {
            None
        };

        let final_content = match validation_outcome {
            // 无校验器:原样返回
            None => llm_response.content.clone(),
            // 校验通过:重置重试计数,使用 cleaned 内容
            Some((cleaned, Ok(()), _)) => {
                self.output_format_retries = 0;
                cleaned
            }
            // 校验失败
            Some((cleaned, Err(err_msg), max_retries)) => {
                if self.output_format_retries < max_retries {
                    // 重试:推送原始 assistant 消息 + 校正消息,返回 is_finished: false
                    // 让 reactor 继续循环,LLM 在下一轮看到校正消息后修正输出
                    self.output_format_retries += 1;
                    let retry_count = self.output_format_retries;

                    // 推送原始(未清洗)assistant 消息到审计链
                    let assistant_idx = messages.len();
                    let assistant_msg = Message::Assistant {
                        content: llm_response.content.clone(),
                        tool_calls: llm_response.tool_calls.clone(),
                    };
                    messages.push(assistant_msg.clone());
                    self.persist_message(session_id, assistant_idx, assistant_msg)
                        .await?;

                    // 推送 System 校正消息
                    let correction_idx = messages.len();
                    let correction_msg = Message::System {
                        content: format!(
                            "你的上一次输出不符合要求的格式。校验错误:\n{}\n\n\
                             请重新输出,严格符合 JSON Schema 要求,不要包含 markdown 代码块标记。",
                            err_msg
                        ),
                    };
                    messages.push(correction_msg.clone());
                    self.persist_message(session_id, correction_idx, correction_msg)
                        .await?;

                    info!(
                        %session_id,
                        retry = retry_count,
                        max_retries,
                        "G11: output validation failed, requesting LLM retry"
                    );

                    return Ok(serde_json::json!({
                        "content": llm_response.content,
                        "tool_calls": llm_response.tool_calls,
                        "is_finished": false,
                        "validation_error": err_msg,
                        "retry": retry_count,
                    }));
                } else {
                    // 重试次数耗尽:降级接受 cleaned 输出(避免死循环)
                    info!(
                        %session_id,
                        max_retries,
                        "G11: output validation failed, max retries exhausted, accepting degraded output"
                    );
                    self.output_format_retries = 0;
                    cleaned
                }
            }
        };

        // Fallback: LLM 未走 function calling 协议时,尝试从文本内容中解析 JSON tool call
        let mut final_content = final_content;
        let mut effective_tool_calls = llm_response.tool_calls.clone();
        if effective_tool_calls.is_none()
            || effective_tool_calls
                .as_ref()
                .map(|t| t.is_empty())
                .unwrap_or(true)
        {
            if let Some(parsed) = try_parse_tool_call_from_text(&final_content) {
                info!(
                    %session_id,
                    count = parsed.len(),
                    "Fallback: parsed tool call from LLM text content (non-streaming)"
                );
                effective_tool_calls = Some(parsed);
                final_content = String::new();
            }
        }

        let assistant_idx = messages.len();
        let assistant_msg = Message::Assistant {
            content: final_content.clone(),
            tool_calls: effective_tool_calls.clone(),
        };
        messages.push(assistant_msg.clone());
        // P0: 持久化 assistant 消息
        self.persist_message(session_id, assistant_idx, assistant_msg)
            .await?;

        // G13:并行预执行工具(max_parallel_tools > 1 且有多个 tool_calls 时)
        // 结果存入 parallel_tool_cache,后续 call_service IoRequest 命中缓存秒回
        // candidate 工具(返回 proposal)不缓存,留给 call_service 走审批
        if self.config.max_parallel_tools > 1 {
            if let Some(tcs) = &effective_tool_calls {
                if tcs.len() > 1 {
                    self.parallel_cache_clear();
                    let _results = self.execute_tools_parallel(session_id, tcs).await;
                    info!(
                        %session_id,
                        count = tcs.len(),
                        "G13: parallel tool pre-execution completed (results cached)"
                    );
                }
            }
        }

        Ok(serde_json::json!({
            "content": final_content,
            "tool_calls": effective_tool_calls,
            "is_finished": llm_response.is_finished(),
        }))
    }

    async fn handle_call_service(
        &mut self,
        session_id: &str,
        params: &Value,
        messages: &mut Vec<Message>,
        tool_calls: &mut Vec<String>,
    ) -> Result<Value, AgentError> {
        let tool_name = params
            .get("tool_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AgentError::Internal("missing tool_name in call_service params".to_string())
            })?;

        let args = params.get("args").cloned().unwrap_or(Value::Null);

        // G13:检查并行缓存(如果 call_external 已并行执行过此 active 工具,直接返回缓存结果,跳过重复执行 + 审批)
        // candidate 工具(proposal)不会被缓存,所以缓存命中的一定是 active 工具,无需审批
        let tool_result = if let Some(cached) = self.check_parallel_cache(tool_name, &args) {
            info!(%session_id, tool = %tool_name, "G13: call_service cache hit, skipping re-execution");
            cached
        } else {
            // 缓存未命中:走正常的 execute_tool_call + 审批流程
            // 第一次调用(不带 approved flag)
            let tool_result = self.execute_tool_call(tool_name, &args).await?;
            // G8:检查是否需要审批,如果需要则走审批流程(可能重新调用 with approved:true)
            self.maybe_handle_approval(session_id, tool_name, &args, tool_result)
                .await?
        };

        tool_calls.push(tool_name.to_string());
        let tool_idx = messages.len();
        let tool_msg = Message::Tool {
            content: tool_result.to_string(),
            tool_name: tool_name.to_string(),
        };
        messages.push(tool_msg.clone());
        // P0: 持久化 tool 消息
        self.persist_message(session_id, tool_idx, tool_msg).await?;

        Ok(serde_json::json!({
            "tool_name": tool_name,
            "result": tool_result.to_string(),
        }))
    }

    /// G8:执行工具调用(不含审批逻辑,纯执行)
    ///
    /// 从 `handle_call_service` 和流式路径的审批重调用共用。
    /// 第一次调用不带 `approved` flag → 工具可能返回 `needs_approval` proposal。
    /// 第二次调用(审批通过后)带 `approved:true` → 工具直接执行。
    async fn execute_tool_call(
        &self,
        tool_name: &str,
        args: &Value,
    ) -> Result<JsonValue, AgentError> {
        let args_tcb = serde_to_tcb(args);
        let mut call_params = BTreeMap::new();
        call_params.insert(
            "tool_name".to_string(),
            JsonValue::string(tool_name.to_string()),
        );
        call_params.insert("args".to_string(), args_tcb);
        // G17:工具调用计时 + 指标(单一插桩点,覆盖 run() / run_streaming() / G13 并行路径)
        let tool_start = std::time::Instant::now();
        let result = self
            .execute_external("call_service", &JsonValue::Object(call_params))
            .await;
        let tool_duration = tool_start.elapsed();
        let tool_ok = result.is_ok();
        if let Some(m) = &self.metrics {
            m.observe_tool_call(tool_name, tool_duration, tool_ok);
        }
        result
    }

    // ===== G13:并行工具调用 =====

    /// G13:构造并行缓存 key
    ///
    /// key = `"{tool_name}:{serde_json(args)}"`,用于唯一标识一次工具调用。
    fn parallel_cache_key(tool_name: &str, args: &Value) -> String {
        format!(
            "{}:{}",
            tool_name,
            serde_json::to_string(args).unwrap_or_default()
        )
    }

    /// G13:从缓存读取工具结果(命中则返回克隆)
    fn parallel_cache_get(&self, key: &str) -> Option<JsonValue> {
        self.parallel_tool_cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(key).cloned())
    }

    /// G13:写入工具结果到缓存
    fn parallel_cache_put(&self, key: String, value: JsonValue) {
        if let Ok(mut cache) = self.parallel_tool_cache.lock() {
            cache.insert(key, value);
        }
    }

    /// G13:清空缓存(每次 call_external 开始时调用)
    fn parallel_cache_clear(&self) {
        if let Ok(mut cache) = self.parallel_tool_cache.lock() {
            cache.clear();
        }
    }

    /// G13:直接本地执行单个工具(不经 evorule IoRequest)
    ///
    /// 调 `tool_handler.execute_by_name`,用于并行批量执行。
    /// 返回 `JsonValue`(工具结果,可能是 proposal)。
    async fn execute_single_tool(&self, tc: &crate::agent::translator::ToolCall) -> JsonValue {
        let args_tcb = serde_to_tcb(&tc.arguments);
        match self.tool_handler.execute_by_name(&tc.name, &args_tcb).await {
            Ok(result) => result,
            Err(e) => {
                // 工具执行失败:返回 error JSON(不中断其他并行工具)
                let mut map = std::collections::BTreeMap::new();
                map.insert("status".to_string(), JsonValue::string("error"));
                map.insert("error".to_string(), JsonValue::string(e));
                JsonValue::object(map)
            }
        }
    }

    /// G13:并行执行多个 tool_calls
    ///
    /// - 所有工具并行执行(`futures::future::join_all`)
    /// - active 工具(返回非 proposal)的结果存入缓存,后续 call_service 命中缓存秒回
    /// - candidate 工具(返回 proposal)的结果**不**缓存,留给 call_service 走审批
    ///
    /// 返回 `Vec<(tool_name, args_clone, result)>`,按原始 tool_calls 顺序。
    async fn execute_tools_parallel(
        &self,
        session_id: &str,
        tool_calls: &[crate::agent::translator::ToolCall],
    ) -> Vec<(String, Value, JsonValue)> {
        info!(
            %session_id,
            count = tool_calls.len(),
            max_parallel = self.config.max_parallel_tools,
            "G13: executing tool calls in parallel"
        );

        // 构造 futures:每个 tool_call 一个 execute_single_tool
        let futures: Vec<_> = tool_calls
            .iter()
            .map(|tc| async move {
                let name = tc.name.clone();
                let args = tc.arguments.clone();
                let result = self.execute_single_tool(tc).await;
                (name, args, result)
            })
            .collect();

        // 并行执行(join_all 保证顺序与输入一致)
        let results = futures_util::future::join_all(futures).await;

        // 缓存 active 工具结果(非 proposal)
        for (name, args, result) in &results {
            let result_str = result.to_string();
            // 检查是否是 proposal(candidate 工具)
            let is_proposal = parse_approval_request(session_id, name, args, &result_str).is_some();
            if !is_proposal {
                let key = Self::parallel_cache_key(name, args);
                self.parallel_cache_put(key, result.clone());
            } else {
                info!(%session_id, tool = %name, "G13: tool returned proposal, not caching (will go through call_service approval)");
            }
        }

        results
    }

    /// G13:检查 call_service 是否命中并行缓存
    ///
    /// 命中则返回缓存的 JsonValue(不重复执行),未命中则返回 None。
    /// 仅当 `max_parallel_tools > 1` 时启用缓存查询。
    fn check_parallel_cache(&self, tool_name: &str, args: &Value) -> Option<JsonValue> {
        if self.config.max_parallel_tools <= 1 {
            return None; // 串行模式,不走缓存
        }
        let key = Self::parallel_cache_key(tool_name, args);
        self.parallel_cache_get(&key)
    }

    /// G8:检查 tool_result 是否是 proposal,如果是则走审批流程
    ///
    /// 流程:
    /// 1. 解析 tool_result,如果不是 `needs_approval` → 直接返回原结果
    /// 2. 通过 `approval_callback` 问用户(无 callback = 默认拒绝)
    /// 3. 用户批准 → 带 `approved:true` 重新调用工具
    /// 4. 用户拒绝 → 返回 `{"status":"rejected"}`
    ///
    /// **不递归**:重调用的结果不再检查 proposal(避免无限循环)。
    async fn maybe_handle_approval(
        &self,
        session_id: &str,
        tool_name: &str,
        args: &Value,
        tool_result: JsonValue,
    ) -> Result<JsonValue, AgentError> {
        let result_str = tool_result.to_string();
        let approval_req = match parse_approval_request(session_id, tool_name, args, &result_str) {
            Some(req) => req,
            None => return Ok(tool_result), // 不是 proposal,直接返回
        };

        info!(%session_id, tool = tool_name, "G8: tool requires approval");

        // 问用户(无 callback = 默认拒绝,安全优先)
        let approved = if let Some(cb) = &self.approval_callback {
            cb.request_approval(&approval_req).await
        } else {
            false
        };

        if !approved {
            warn!(%session_id, tool = tool_name, "G8: tool call rejected");
            return Ok(JsonValue::string(
                r#"{"status":"rejected","message":"User denied approval"}"#,
            ));
        }

        // 用户批准 → 带 approved:true 重新调用
        info!(%session_id, tool = tool_name, "G8: tool call approved, re-executing with approved=true");
        let mut approved_args = args.clone();
        if let Some(obj) = approved_args.as_object_mut() {
            obj.insert("approved".to_string(), Value::Bool(true));
        } else {
            // args 不是 object,包装一下
            approved_args = serde_json::json!({"original_args": args, "approved": true});
        }
        self.execute_tool_call(tool_name, &approved_args).await
    }

    async fn execute_external(
        &self,
        io_type: &str,
        params: &JsonValue,
    ) -> Result<JsonValue, AgentError> {
        tokio::time::timeout(self.config.step_timeout, async {
            let mut instr = BTreeMap::new();
            instr.insert("type".to_string(), JsonValue::string(io_type.to_string()));
            instr.insert("params".to_string(), params.clone());

            let tcb_instr = JsonValue::Object(instr);
            self.execute_io_request(&tcb_instr).await
        })
        .await
        .map_err(|_| AgentError::Timeout(format!("{} timeout", io_type)))?
    }

    async fn execute_io_request(&self, request: &JsonValue) -> Result<JsonValue, AgentError> {
        match request.get("type").and_then(|v| v.as_str()) {
            Some("call_external") => {
                let params = request.get("params").cloned().unwrap_or(JsonValue::Null);
                self.execute_llm_request(&params).await
            }
            Some("call_service") => {
                let params = request.get("params").cloned().unwrap_or(JsonValue::Null);
                self.execute_tool_request(&params).await
            }
            Some(t) => Err(AgentError::Internal(format!("unsupported io type: {}", t))),
            None => Err(AgentError::Internal("missing io type".to_string())),
        }
    }

    async fn execute_llm_request(&self, params: &JsonValue) -> Result<JsonValue, AgentError> {
        // Actually invoke LlmHandler (uses reqwest to call real HTTP API)
        // - Read MINIMAX_API_KEY / DEEPSEEK_API_KEY / OPENAI_API_KEY env vars
        // - Supports messages / tools / temperature / max_tokens
        // - Returns full OpenAI-compatible JSON response
        self.llm_handler
            .execute(params)
            .await
            .map_err(AgentError::LlmError)
    }

    async fn execute_tool_request(&self, params: &JsonValue) -> Result<JsonValue, AgentError> {
        // Real call through ToolHandler (60s timeout, tool_not_found detection).
        // Replaces the previous stub that returned a hardcoded "Simulated tool result".
        self.tool_handler
            .execute(params)
            .await
            .map_err(AgentError::ToolError)
    }

    async fn auto_recall(&self, session_id: &str) -> Result<Vec<u64>, AgentError> {
        let shared_facts = self
            .evorule_client
            .get_shared_facts(Some("shared."))
            .await?;

        if shared_facts.is_empty() {
            info!(%session_id, "No shared facts to recall");
            return Ok(Vec::new());
        }

        let mut recalled_ids = Vec::new();
        let mut recalled_content = String::new();

        for fact in &shared_facts {
            recalled_ids.push(fact.fact_id);
            recalled_content.push_str(&format!(
                "[Fact {}] {}: {}\n",
                fact.fact_id,
                fact.path,
                serde_json::to_string(&fact.value).unwrap_or_default()
            ));
        }

        info!(%session_id, fact_count = recalled_ids.len(), "Auto-recalled shared facts");

        if let Some(memory) = self.memory.as_ref() {
            let mut mem = memory.clone();
            mem.set("auto_recall_context", &recalled_content).await?;
        }

        self.evorule_client
            .record_used_at_startup(session_id, &recalled_ids)
            .await?;
        info!(%session_id, "Recorded used_at_startup");

        Ok(recalled_ids)
    }

    async fn auto_rewind(&self, session_id: &str) -> Result<u64, AgentError> {
        let history = self.evorule_client.get_facts(session_id, None).await?;

        if history.len() < 2 {
            return Err(AgentError::Internal(
                "Not enough history to rewind".to_string(),
            ));
        }

        let target_version = history[history.len() - 2].version;
        info!(%session_id, target_version, "Attempting auto-rewind");

        let rewind_result = self
            .evorule_client
            .rewind(session_id, target_version)
            .await?;
        let rewind_version = rewind_result["version"].as_u64().unwrap_or(target_version);

        info!(%session_id, rewind_version, "Auto-rewind completed");
        Ok(rewind_version)
    }

    /// G15:从 evorule payload 加载历史消息(continuation 模式用)
    ///
    /// 路径:`payload["__memory__"][namespace]["session_{session_id}"]["messages"]`
    /// 每条消息是 `MessageRecord` JSON 对象,key 为消息索引("0", "1", ...)。
    ///
    /// 返回按 `idx` 排序的 `Vec<Message>`。加载失败返回 `Err`(调用方降级处理)。
    ///
    /// 如果 `memory` 为 `None`(agent 未配置记忆),直接返回空 Vec。
    async fn load_messages_from_payload(
        runner: &AgentRunner,
        session_id: &str,
    ) -> Result<Vec<Message>, AgentError> {
        let namespace = match runner.memory.as_ref() {
            Some(m) => m.namespace().to_string(),
            None => return Ok(Vec::new()),
        };

        let state = runner.evorule_client.get_state(session_id).await?;
        let messages_node = &state["payload"]["__memory__"][&namespace]
            [&format!("session_{}", session_id)]["messages"];

        if messages_node.is_null() {
            return Ok(Vec::new());
        }

        let messages_obj = messages_node
            .as_object()
            .ok_or_else(|| AgentError::Internal("messages node is not an object".to_string()))?;

        let mut records: Vec<MessageRecord> = Vec::new();
        for (_key, value) in messages_obj.iter() {
            match serde_json::from_value::<MessageRecord>(value.clone()) {
                Ok(rec) => records.push(rec),
                Err(e) => {
                    warn!(%session_id, error = %e, "G15: skipping unparseable message record");
                }
            }
        }

        // 按 idx 排序,确保消息顺序正确
        records.sort_by_key(|r| r.idx);

        let messages: Vec<Message> = records
            .into_iter()
            .filter_map(|rec| rec_to_message(&rec))
            .collect();

        Ok(messages)
    }

    /// TODO: doc
    pub async fn join_cluster(&mut self, cluster_id: &str) -> Result<(), AgentError> {
        let cid: u64 = cluster_id.parse().map_err(|_| {
            AgentError::Internal(format!("invalid cluster_id (expected u64): {}", cluster_id))
        })?;
        if let Some(session_id) = &self.session_id {
            self.evorule_client
                .join_cluster(session_id, Some(cid))
                .await?;
            self.join_cluster_id = Some(cid);
            info!(%session_id, cluster_id, "Joined cluster");
        }
        Ok(())
    }

    /// TODO: doc
    pub async fn leave_cluster(&mut self) -> Result<(), AgentError> {
        if let Some(session_id) = &self.session_id {
            self.evorule_client.leave_cluster(session_id).await?;
            self.join_cluster_id = None;
            info!(%session_id, "Left cluster");
        }
        Ok(())
    }

    /// TODO: doc
    pub async fn get_cluster_status(&self) -> Result<Value, AgentError> {
        if let Some(session_id) = &self.session_id {
            self.evorule_client
                .get_cluster_status(session_id)
                .await
                .map_err(|e| e.into())
        } else {
            Err(AgentError::Internal("No active session".to_string()))
        }
    }

    /// TODO: doc
    pub async fn replay_session(&self, session_id: &str) -> Result<Vec<Value>, AgentError> {
        self.evorule_client
            .replay(session_id)
            .await
            .map_err(|e| e.into())
    }

    /// TODO: doc
    pub async fn diff_session(
        &self,
        session_id: &str,
        version_a: u64,
        version_b: u64,
    ) -> Result<Value, AgentError> {
        self.evorule_client
            .diff(session_id, version_a, version_b)
            .await
            .map_err(|e| e.into())
    }

    /// TODO: doc
    pub async fn compare_strategies(
        &self,
        session_a: &str,
        session_b: &str,
    ) -> Result<Value, AgentError> {
        let history_a = self.evorule_client.get_facts(session_a, None).await?;
        let history_b = self.evorule_client.get_facts(session_b, None).await?;

        let version_a = history_a.last().map(|f| f.version).unwrap_or(0);
        let version_b = history_b.last().map(|f| f.version).unwrap_or(0);

        let diff_a = self.evorule_client.diff(session_a, 0, version_a).await?;
        let diff_b = self.evorule_client.diff(session_b, 0, version_b).await?;

        let replay_a = self.evorule_client.replay(session_a).await?;
        let replay_b = self.evorule_client.replay(session_b).await?;

        Ok(serde_json::json!({
            "session_a": {
                "id": session_a,
                "final_version": version_a,
                "history_length": history_a.len(),
                "diff": diff_a,
                "replay": replay_a,
            },
            "session_b": {
                "id": session_b,
                "final_version": version_b,
                "history_length": history_b.len(),
                "diff": diff_b,
                "replay": replay_b,
            },
        }))
    }

    /// G4:流式运行 Agent
    ///
    /// 与 `run()` 相同的 ReAct 循环,但 LLM 调用使用 `execute_stream`(G1),
    /// 逐 token 产出 `AgentEvent::LlmDelta`,使调用方能实时显示 LLM 输出。
    ///
    /// 事件流顺序(典型):
    /// ```text
    /// SessionCreated → Step → LlmDelta* → LlmDone → (ToolCall → ToolResult)* → ... → Done
    /// ```
    ///
    /// G18:每个 `Ok(AgentEvent)` 在 yield 前被 `event_callbacks.dispatch()` 调用
    /// (同步 await + 1s 超时 + panic 保护)。`Err` 事件不触发回调。
    ///
    /// 错误处理:
    /// - LLM 流中断:提交 error io_response(防止 evorule 卡死),yield Error + Done
    /// - evorule Error 事件:尝试 auto_rewind,yield Info;失败则 yield Done(error)
    /// - max_steps 超限:yield Error + Done
    ///
    /// 边界:
    /// - 不支持 delegate_context(子 agent 委托时改用 `run()`)
    /// - 消息持久化在流式下仍走 `persist_message`,`PerReactRound` 模式适配流式
    ///   (在 IoRequest 处理前批量 flush,而非每 delta 后 flush)
    pub fn run_streaming(
        self,
        goal: String,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<AgentEvent, AgentError>> + Send>> {
        // G18:提取回调链(Arc clone),在 inner stream 之外包装 dispatch
        let callbacks = self.event_callbacks.clone();
        let inner = self.run_streaming_inner(goal, None);
        Box::pin(async_stream::stream! {
            let mut inner = inner;
            while let Some(result) = inner.next().await {
                // G18:Ok 事件在 yield 前分发给所有回调(超时/panic 不中断)
                if let Ok(ref event) = result {
                    callbacks.dispatch(event).await;
                }
                yield result;
            }
        })
    }

    /// G15:在已有 session 上追加一轮对话(REPL / WebSocket 复用)
    ///
    /// 与 `run_streaming` 的区别:不创建新 evorule session,而是向已有 session
    /// 提交新 command,复用 payload + 审计链。历史消息从 evorule payload 加载
    /// (best-effort,加载失败则降级为仅 system + 新 user)。
    ///
    /// 事件流顺序(典型):
    /// ```text
    /// Step → LlmDelta* → LlmDone → (ToolCall → ToolResult)* → ... → Done
    /// ```
    /// 注意:不产出 `SessionCreated`(调用方已知 session_id)。
    ///
    /// G18:每个 `Ok(AgentEvent)` 在 yield 前被 `event_callbacks.dispatch()` 调用。
    pub fn run_continuation(
        self,
        session_id: String,
        user_input: String,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<AgentEvent, AgentError>> + Send>> {
        // G18:提取回调链(Arc clone),在 inner stream 之外包装 dispatch
        let callbacks = self.event_callbacks.clone();
        let inner = self.run_streaming_inner(user_input, Some(session_id));
        Box::pin(async_stream::stream! {
            let mut inner = inner;
            while let Some(result) = inner.next().await {
                if let Ok(ref event) = result {
                    callbacks.dispatch(event).await;
                }
                yield result;
            }
        })
    }

    /// G4:流式运行 Agent(内部实现,不含 G18 回调分发)
    ///
    /// 由 `run_streaming()` / `run_continuation()` 包装调用。直接调用此方法不会触发事件回调。
    ///
    /// G15:`existing_session_id = Some(id)` 时复用已有 session(continuation 模式),
    /// 跳过 create_session / auto_recall / join_cluster / SessionCreated 事件,
    /// 并从 evorule payload 加载历史消息。
    fn run_streaming_inner(
        self,
        goal: String,
        existing_session_id: Option<String>,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<AgentEvent, AgentError>> + Send>> {
        Box::pin(stream! {
            let mut runner = self;
            let start_time = std::time::Instant::now();

            // 1. 构造 system_prompt(同 run())
            // C2: 召回顺序修复 —— recall 在 build_system_prompt 之前
            let recall = match runner.memory.as_ref() {
                Some(mem) => mem.recall_context(
                    &goal,
                    runner.sediment_config.max_session_summaries,
                    runner.sediment_config.max_injected_events,
                ).await,
                None => crate::agent::memory::RecallContext::default(),
            };
            let system_prompt = match runner.memory.as_ref() {
                Some(mem) => mem.build_system_prompt_with_recall(
                    &runner.config.system_prompt,
                    &recall,
                    &crate::agent::memory::ContextBudget::new(
                        runner.max_context_tokens,
                        runner.memory_budget_ratio,
                    ),
                ),
                None => runner.config.system_prompt.clone(),
            };

            // 2. session:新建 或 复用(G15:continuation)
            let session_id = if let Some(id) = existing_session_id.clone() {
                // G15:continuation — 复用已有 session,不创建新 session
                // (session_active 守卫在下方统一创建,避免双重计数)
                runner.session_id = Some(id.clone());
                info!(%id, "G15: continuing existing session");
                id
            } else {
                // 新建 session(原 run_streaming 逻辑)
                match runner.evorule_client.create_session(None).await {
                    Ok(id) => id,
                    Err(e) => {
                        yield Err(AgentError::EvoruleError(e.to_string()));
                        return;
                    }
                }
            };

            // G17:session 活跃度守卫(新建 / 复用均持有,stream! 块结束时 dec)
            let _session_guard = SessionActiveGuard::new(runner.metrics.clone());

            if existing_session_id.is_none() {
                // 新建 session 才计 sessions_total + yield SessionCreated + auto_recall + join_cluster
                if let Some(m) = &runner.metrics {
                    m.inc_sessions_total();
                }
                yield Ok(AgentEvent::SessionCreated { session_id: session_id.clone() });

                // 3. auto_recall(best-effort,不阻塞流)
                let _ = runner.auto_recall(&session_id).await;

                // 4. join cluster(如果配置了)
                if let Some(cluster_id) = runner.join_cluster_id {
                    let _ = runner.evorule_client.join_cluster(&session_id, Some(cluster_id)).await;
                }
            }

            // 5. 订阅 SSE(必须在 submit_command 之前,否则错过 io_request)
            let mut event_stream = match runner.evorule_client.subscribe_events(&session_id).await {
                Ok(s) => s,
                Err(e) => {
                    yield Err(AgentError::EvoruleError(e.to_string()));
                    return;
                }
            };

            // 6. 提交 call_external 命令
            let command = runner.build_call_external_command(&system_prompt, &goal);
            if let Err(e) = runner.evorule_client.submit_command(&session_id, &command).await {
                yield Err(AgentError::EvoruleError(e.to_string()));
                return;
            }

            // 7. 初始化消息历史
            let mut messages: Vec<Message> = Vec::new();
            let mut step_count = 0;
            let mut tool_calls: Vec<String> = Vec::new();

            if existing_session_id.is_some() {
                // G15:continuation — 从 evorule payload 加载历史消息(best-effort)
                match Self::load_messages_from_payload(&runner, &session_id).await {
                    Ok(loaded) if !loaded.is_empty() => {
                        info!(%session_id, loaded_count = loaded.len(), "G15: loaded historical messages");
                        messages = loaded;
                    }
                    Ok(_) => {
                        info!(%session_id, "G15: no historical messages found, starting fresh");
                    }
                    Err(e) => {
                        warn!(%session_id, error = %e, "G15: failed to load historical messages, starting fresh");
                    }
                }
            }

            if messages.is_empty() {
                // 新 session 或历史加载失败 — 用 system_prompt 初始化
                if !system_prompt.is_empty() {
                    messages.push(Message::System { content: system_prompt.clone() });
                    if let Err(e) = runner
                        .persist_message(&session_id, 0, Message::System { content: system_prompt.clone() })
                        .await
                    {
                        yield Err(e);
                        return;
                    }
                }
            }
            let user_idx = messages.len();
            messages.push(Message::User { content: goal.clone() });
            if let Err(e) = runner
                .persist_message(&session_id, user_idx, Message::User { content: goal.clone() })
                .await
            {
                yield Err(e);
                return;
            }

            // 8. SSE 事件循环(同 run(),但 call_external 分支用 execute_stream)
            // G6:用 select! 监听取消,使等待 event 时也能即时响应
            let cancel_token = runner.cancel_token.clone();
            let mut last_llm_content = String::new(); // 追踪最近一次 LLM 输出(Stable 时 fallback)
            loop {
                let event = tokio::select! {
                    ev = event_stream.next() => match ev {
                        Some(e) => e,
                        None => break,
                    },
                    _ = cancel_token.cancelled() => {
                        info!("Cancellation requested during streaming, cleaning up");
                        let _ = runner.flush_messages(&session_id).await;
                        let duration = start_time.elapsed().as_millis() as u64;
                        yield Ok(AgentEvent::Error(AgentError::Internal(
                            "cancelled by user".to_string(),
                        )));
                        yield Ok(AgentEvent::Done(AgentResult::cancelled(
                            "cancelled by user".to_string(),
                            step_count,
                            duration,
                        )));
                        return;
                    }
                };
                match event.event_type.as_str() {
                    "IoRequest" => {
                        step_count += 1;
                        // G17:步数指标
                        if let Some(m) = &runner.metrics {
                            m.inc_steps();
                        }
                        if step_count > runner.config.max_steps {
                            yield Ok(AgentEvent::Error(AgentError::MaxStepsExceeded(
                                runner.config.max_steps,
                            )));
                            let duration = start_time.elapsed().as_millis() as u64;
                            let _ = runner.flush_messages(&session_id).await;
                            yield Ok(AgentEvent::Done(AgentResult::error(
                                format!("Max steps exceeded: {}", runner.config.max_steps),
                                step_count,
                                duration,
                            )));
                            return;
                        }
                        yield Ok(AgentEvent::Step { step: step_count });

                        // PerReactRound:处理前刷写上一轮缓冲的消息
                        if matches!(runner.message_persist_mode, MessagePersistMode::PerReactRound) {
                            let _ = runner.flush_messages(&session_id).await;
                        }

                        let io_type = event.payload.get("io_type").and_then(|v| v.as_str()).unwrap_or("");
                        let params = event.payload.get("params").cloned().unwrap_or(Value::Null);
                        let request_id = event.payload.get("id").and_then(|v| v.as_u64());

                        match io_type {
                            "call_external" => {
                                // G1:流式调用 LLM
                                let model = params.get("model").and_then(|v| v.as_str()).unwrap_or(&runner.config.model);
                                let temperature = params.get("temperature").and_then(|v| v.as_f64())
                                    .unwrap_or(runner.config.temperature as f64);

                                // G2+G10:裁剪 messages(同 handle_call_external)
                                // G10:如果有 summarizer,裁剪掉的消息生成摘要替换 hint
                                let messages_to_send = if let Some(ctx) = &runner.context_window {
                                    let mut trim_result = ctx.trim_detailed(&messages);
                                    if !trim_result.dropped.is_empty() {
                                        info!(
                                            dropped = trim_result.dropped.len(),
                                            "trimmed history messages to fit context window"
                                        );
                                    }
                                    // G10:记忆压缩
                                    if let Some(summarizer) = &runner.summarizer {
                                        if !trim_result.dropped.is_empty() {
                                            match summarizer.summarize_dropped(&trim_result.dropped).await {
                                                Ok(summary) if !summary.is_empty() => {
                                                    for msg in &mut trim_result.messages {
                                                        if let Message::System { content } = msg {
                                                            if content.starts_with("[earlier") {
                                                                *content = summary.clone();
                                                                break;
                                                            }
                                                        }
                                                    }
                                                    info!(
                                                        %session_id,
                                                        dropped = trim_result.dropped.len(),
                                                        "G10: generated summary for dropped messages"
                                                    );
                                                }
                                                Ok(_) => {
                                                    tracing::debug!(
                                                        %session_id,
                                                        "G10: summary skipped (below threshold or empty)"
                                                    );
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        %session_id,
                                                        error = %e,
                                                        "G10: summary generation failed, keeping original hint"
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    trim_result.messages
                                } else {
                                    messages.clone()
                                };

                                let serde_messages = match serde_json::to_value(&messages_to_send) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        let err = AgentError::Internal(format!("serialize messages: {}", e));
                                        yield Ok(AgentEvent::Error(err.clone()));
                                        let duration = start_time.elapsed().as_millis() as u64;
                                        yield Ok(AgentEvent::Done(AgentResult::error(
                                            err.to_string(), step_count, duration,
                                        )));
                                        return;
                                    }
                                };
                                let tcb_messages = serde_to_tcb(&serde_messages);

                                let mut call_params = BTreeMap::new();
                                call_params.insert("model".to_string(), JsonValue::string(model.to_string()));
                                call_params.insert("temperature".to_string(), JsonValue::string(temperature.to_string()));
                                call_params.insert("messages".to_string(), tcb_messages);
                                let call_params_json = JsonValue::Object(call_params);

                                // G1:启动流式 LLM 调用
                                // G17:LLM 流式调用计时(在 loop 前后记录,Err 分支单独记录)
                                let llm_start = std::time::Instant::now();
                                let mut llm_stream = runner.llm_handler.execute_stream(&call_params_json);
                                let mut full_content = String::new();
                                let mut full_tool_calls: Option<Vec<crate::agent::translator::ToolCall>> = None;
                                let mut finish_reason: Option<String> = None;

                                // G6:LLM 流式输出期间也监听取消(token-by-token 响应)
                                loop {
                                    let chunk = tokio::select! {
                                        c = llm_stream.next() => match c {
                                            Some(c) => c,
                                            None => break,
                                        },
                                        _ = cancel_token.cancelled() => {
                                            info!("Cancelled during LLM streaming, cleaning up");
                                            if let Some(rid) = request_id {
                                                let _ = runner.evorule_client
                                                    .submit_io_response(
                                                        &session_id, rid,
                                                        &serde_json::json!({"content": "", "error": "cancelled"}),
                                                        Some("cancelled"),
                                                    )
                                                    .await;
                                            }
                                            let _ = runner.flush_messages(&session_id).await;
                                            let duration = start_time.elapsed().as_millis() as u64;
                                            yield Ok(AgentEvent::Error(AgentError::Internal(
                                                "cancelled by user".to_string(),
                                            )));
                                            yield Ok(AgentEvent::Done(AgentResult::cancelled(
                                                "cancelled by user".to_string(),
                                                step_count,
                                                duration,
                                            )));
                                            return;
                                        }
                                    };
                                    match chunk {
                                        Ok(StreamChunk::Delta(text)) => {
                                            full_content.push_str(&text);
                                            yield Ok(AgentEvent::LlmDelta { text });
                                        }
                                        Ok(StreamChunk::ToolCallDelta { .. }) => {
                                            // 聚合在 execute_stream 内部完成,不 yield 半截 JSON
                                        }
                                        Ok(StreamChunk::Done(resp)) => {
                                            full_content = resp.content.clone();
                                            full_tool_calls = resp.tool_calls.clone();
                                            finish_reason = resp.finish_reason.clone();
                                            last_llm_content = full_content.clone();
                                            // Fallback: LLM 未走 function calling 协议时,
                                            // 尝试从文本内容中解析 JSON tool call
                                            if full_tool_calls.is_none() || full_tool_calls.as_ref().map(|t| t.is_empty()).unwrap_or(true) {
                                                if let Some(parsed) = try_parse_tool_call_from_text(&full_content) {
                                                    info!(
                                                        %session_id,
                                                        count = parsed.len(),
                                                        "Fallback: parsed tool call from LLM text content"
                                                    );
                                                    full_tool_calls = Some(parsed);
                                                    // 文本内容已被解析为 tool call,清空 content 避免重复展示
                                                    full_content = String::new();
                                                    last_llm_content = String::new();
                                                }
                                            }
                                            yield Ok(AgentEvent::LlmDone {
                                                content: full_content.clone(),
                                                finish_reason: resp.finish_reason.clone(),
                                            });
                                        }
                                        Ok(StreamChunk::Warn(msg)) => {
                                            yield Ok(AgentEvent::Info(msg));
                                        }
                                        Err(e) => {
                                            // G17:记录 LLM 流式调用失败指标
                                            if let Some(m) = &runner.metrics {
                                                m.observe_llm_call(model, llm_start.elapsed(), false);
                                            }
                                            // 提交 error io_response 防止 evorule 卡死
                                            if let Some(rid) = request_id {
                                                let err_resp = serde_json::json!({"content": "", "error": &e});
                                                let _ = runner.evorule_client
                                                    .submit_io_response(&session_id, rid, &err_resp, Some(e.as_str()))
                                                    .await;
                                            }
                                            let err = AgentError::LlmError(e);
                                            yield Ok(AgentEvent::Error(err.clone()));
                                            let duration = start_time.elapsed().as_millis() as u64;
                                            let _ = runner.flush_messages(&session_id).await;
                                            yield Ok(AgentEvent::Done(AgentResult::error(
                                                err.to_string(), step_count, duration,
                                            )));
                                            return;
                                        }
                                    }
                                }

                                // G17:记录 LLM 流式调用成功指标(正常完成)
                                if let Some(m) = &runner.metrics {
                                    m.observe_llm_call(model, llm_start.elapsed(), true);
                                }

                                // G13:并行预执行工具(max_parallel_tools > 1 且有多个 tool_calls 时)
                                // 结果存入 parallel_tool_cache,后续 call_service IoRequest 命中缓存秒回
                                // candidate 工具(返回 proposal)不缓存,留给 call_service 走审批
                                if runner.config.max_parallel_tools > 1 {
                                    if let Some(tcs) = &full_tool_calls {
                                        if tcs.len() > 1 {
                                            runner.parallel_cache_clear();
                                            let _results = runner.execute_tools_parallel(&session_id, tcs).await;
                                            info!(
                                                %session_id,
                                                count = tcs.len(),
                                                "G13: parallel tool pre-execution completed (results cached)"
                                            );
                                        }
                                    }
                                }

                                // 持久化 assistant 消息(同 handle_call_external)
                                let assistant_idx = messages.len();
                                let assistant_msg = Message::Assistant {
                                    content: full_content.clone(),
                                    tool_calls: full_tool_calls.clone(),
                                };
                                messages.push(assistant_msg.clone());
                                if let Err(e) = runner.persist_message(&session_id, assistant_idx, assistant_msg).await {
                                    yield Err(e);
                                    return;
                                }

                                // yield ToolCall 事件(如果有)
                                if let Some(tcs) = &full_tool_calls {
                                    for tc in tcs {
                                        yield Ok(AgentEvent::ToolCall {
                                            name: tc.name.clone(),
                                            args: tc.arguments.clone(),
                                        });
                                    }
                                }

                                // 提交 io_response(同 handle_call_external)
                                if let Some(rid) = request_id {
                                    let tool_calls_json: serde_json::Value = match &full_tool_calls {
                                        Some(tcs) => serde_json::to_value(tcs).unwrap_or(serde_json::Value::Null),
                                        None => serde_json::Value::Null,
                                    };
                                    let is_finished = matches!(finish_reason.as_deref(), Some("stop") | Some("end_turn"));
                                    let resp = serde_json::json!({
                                        "content": full_content,
                                        "tool_calls": tool_calls_json,
                                        "is_finished": is_finished,
                                    });
                                    if let Err(e) = runner.evorule_client
                                        .submit_io_response(&session_id, rid, &resp, None)
                                        .await
                                    {
                                        yield Err(AgentError::EvoruleError(e.to_string()));
                                        return;
                                    }
                                }

                                // (工具执行由 evorule rules 驱动,evorule 会发出 call_service IoRequest)
                            }
                            "call_service" => {
                                let tool_name = params.get("tool_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let args = params.get("args").cloned().unwrap_or(Value::Null);
                                yield Ok(AgentEvent::ToolCall { name: tool_name.clone(), args: args.clone() });

                                // G13:检查并行缓存(如果 call_external 已并行执行过此 active 工具,直接返回缓存结果,跳过重复执行 + 审批)
                                // candidate 工具(proposal)不会被缓存,所以缓存命中的一定是 active 工具,无需审批
                                let final_result = if let Some(cached) = runner.check_parallel_cache(&tool_name, &args) {
                                    info!(%session_id, tool = %tool_name, "G13: call_service cache hit, skipping re-execution");
                                    cached
                                } else {
                                    // G8:流式路径内联审批逻辑(不调 handle_call_service,以便 yield 审批事件)
                                    // 非流式路径(run)仍用 handle_call_service(内部调 maybe_handle_approval,不产事件)
                                    // 1. 第一次调用(不带 approved flag)→ 可能返回 needs_approval proposal
                                    let tool_result = match runner.execute_tool_call(&tool_name, &args).await {
                                        Ok(r) => r,
                                        Err(e) => {
                                            if let Some(rid) = request_id {
                                                let err_str = e.to_string();
                                                let err_resp = serde_json::json!({"error": &err_str});
                                                let _ = runner.evorule_client
                                                    .submit_io_response(&session_id, rid, &err_resp, Some(err_str.as_str()))
                                                    .await;
                                            }
                                            yield Ok(AgentEvent::Error(e.clone()));
                                            let duration = start_time.elapsed().as_millis() as u64;
                                            let _ = runner.flush_messages(&session_id).await;
                                            yield Ok(AgentEvent::Done(AgentResult::error(
                                                e.to_string(), step_count, duration,
                                            )));
                                            return;
                                        }
                                    };

                                    // 2. 检查是否是审批 proposal,如果是则走审批流程(yield 事件)
                                    let result_str = tool_result.to_string();
                                    match parse_approval_request(
                                        &session_id, &tool_name, &args, &result_str,
                                    ) {
                                        None => tool_result, // 不是 proposal,直接用第一次结果
                                        Some(approval_req) => {
                                            // 是 proposal → yield ApprovalRequired 给前端
                                            yield Ok(AgentEvent::ApprovalRequired {
                                                tool_name: tool_name.clone(),
                                                command: approval_req.command.clone(),
                                                risk: approval_req.risk.clone(),
                                                alternative: approval_req.alternative.clone(),
                                            });

                                            // 问用户(无 callback = 默认拒绝,安全优先)
                                            // borrow runner.approval_callback 仅在此 block 内,yield 已在 borrow 之前
                                            let approved = if let Some(cb) = &runner.approval_callback {
                                                cb.request_approval(&approval_req).await
                                            } else {
                                                false
                                            };

                                            yield Ok(AgentEvent::ApprovalResult {
                                                tool_name: tool_name.clone(),
                                                approved,
                                            });

                                            if !approved {
                                                warn!(%session_id, tool = %tool_name, "G8: streaming tool call rejected");
                                                JsonValue::string(
                                                    r#"{"status":"rejected","message":"User denied approval"}"#,
                                                )
                                            } else {
                                                // 批准 → 带 approved:true 重新调用(不递归检查 proposal)
                                                info!(%session_id, tool = %tool_name, "G8: streaming tool call approved, re-executing with approved=true");
                                                let mut approved_args = args.clone();
                                                if let Some(obj) = approved_args.as_object_mut() {
                                                    obj.insert("approved".to_string(), Value::Bool(true));
                                                } else {
                                                    approved_args = serde_json::json!({"original_args": args, "approved": true});
                                                }
                                                match runner.execute_tool_call(&tool_name, &approved_args).await {
                                                    Ok(r) => r,
                                                    Err(e) => {
                                                        if let Some(rid) = request_id {
                                                            let err_str = e.to_string();
                                                            let err_resp = serde_json::json!({"error": &err_str});
                                                            let _ = runner.evorule_client
                                                                .submit_io_response(&session_id, rid, &err_resp, Some(err_str.as_str()))
                                                                .await;
                                                        }
                                                        yield Ok(AgentEvent::Error(e.clone()));
                                                        let duration = start_time.elapsed().as_millis() as u64;
                                                        let _ = runner.flush_messages(&session_id).await;
                                                        yield Ok(AgentEvent::Done(AgentResult::error(
                                                            e.to_string(), step_count, duration,
                                                        )));
                                                        return;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                };

                                // 3. 记录 tool_calls + 持久化 tool 消息(同 handle_call_service)
                                tool_calls.push(tool_name.clone());
                                let tool_idx = messages.len();
                                let tool_msg = Message::Tool {
                                    content: final_result.to_string(),
                                    tool_name: tool_name.clone(),
                                };
                                messages.push(tool_msg.clone());
                                if let Err(e) = runner.persist_message(&session_id, tool_idx, tool_msg).await {
                                    yield Err(e);
                                    return;
                                }

                                // 4. yield ToolResult + 提交 io_response(格式同 handle_call_service)
                                let result_value = serde_json::json!({
                                    "tool_name": tool_name,
                                    "result": final_result.to_string(),
                                });
                                yield Ok(AgentEvent::ToolResult {
                                    name: tool_name.clone(),
                                    result: result_value.clone(),
                                });

                                if let Some(rid) = request_id {
                                    if let Err(e) = runner.evorule_client
                                        .submit_io_response(&session_id, rid, &result_value, None)
                                        .await
                                    {
                                        yield Err(AgentError::EvoruleError(e.to_string()));
                                        return;
                                    }
                                }
                            }
                            _ => {
                                // 未知 io_type:提交错误 io_response 防止卡死
                                if let Some(rid) = request_id {
                                    let _ = runner.evorule_client
                                        .submit_io_response(&session_id, rid, &serde_json::json!({"error": "unsupported io_type"}), Some("unsupported io_type"))
                                        .await;
                                }
                                yield Ok(AgentEvent::Info(format!("Unknown io_type: {}", io_type)));
                            }
                        }
                    }
                    "Stable" => {
                        let duration = start_time.elapsed().as_millis() as u64;
                        let _ = runner.flush_messages(&session_id).await;
                        // C1:会话沉淀（best-effort，摘要+稳定事实→共享空间）
                        let _ = runner.sediment_session(&session_id, &messages).await;
                        let state = match runner.evorule_client.get_state(&session_id).await {
                            Ok(s) => s,
                            Err(e) => {
                                yield Err(AgentError::EvoruleError(e.to_string()));
                                return;
                            }
                        };
                        let content = state["payload"]["llm_response"]["content"]
                            .as_str()
                            .or_else(|| state["payload"]["content"].as_str())
                            .or_else(|| state["payload"]["result"].as_str())
                            .or_else(|| state["payload"].as_str())
                            .unwrap_or_default()
                            .to_string();
                        // Fallback: evorule payload 无 content 时,使用最近一次 LLM 输出
                        let content = if content.is_empty() && !last_llm_content.is_empty() {
                            last_llm_content.clone()
                        } else {
                            content
                        };
                        yield Ok(AgentEvent::Done(AgentResult::success(
                            content, step_count, duration, tool_calls,
                        )));
                        return;
                    }
                    "StateTransition" => {
                        // 状态转换,继续循环
                    }
                    "Error" => {
                        let msg = event.payload.get("message").and_then(|v| v.as_str()).unwrap_or("unknown error");
                        // 尝试 auto_rewind
                        if let Ok(rewind_version) = runner.auto_rewind(&session_id).await {
                            yield Ok(AgentEvent::Info(format!("Auto-rewind to version {}", rewind_version)));
                            continue;
                        }
                        let duration = start_time.elapsed().as_millis() as u64;
                        let _ = runner.flush_messages(&session_id).await;
                        // C1:会话沉淀（best-effort，即使出错也尝试沉淀已收集的对话）
                        let _ = runner.sediment_session(&session_id, &messages).await;
                        yield Ok(AgentEvent::Done(AgentResult::error(msg.to_string(), step_count, duration)));
                        return;
                    }
                    _ => {
                        // 未知事件,继续循环
                    }
                }
            }

            // 事件流关闭
            let duration = start_time.elapsed().as_millis() as u64;
            let _ = runner.flush_messages(&session_id).await;
            yield Ok(AgentEvent::Done(AgentResult::error(
                "Event stream closed".to_string(), step_count, duration,
            )));
        })
    }
}

/// G15:将 `MessageRecord` 转换回 `Message`(continuation 历史加载用)
///
/// 返回 `None` 的情况:
/// - 未知 role(非 system/user/assistant/tool)
/// - assistant 的 tool_calls 反序列化失败(降级为无 tool_calls)
/// - tool 消息缺少 tool_name
fn rec_to_message(rec: &MessageRecord) -> Option<Message> {
    match rec.role.as_str() {
        "system" => Some(Message::System {
            content: rec.content.clone(),
        }),
        "user" => Some(Message::User {
            content: rec.content.clone(),
        }),
        "assistant" => {
            let tool_calls = rec.tool_calls.as_ref().and_then(|v| {
                serde_json::from_value::<Vec<crate::agent::translator::ToolCall>>(v.clone()).ok()
            });
            Some(Message::Assistant {
                content: rec.content.clone(),
                tool_calls,
            })
        }
        "tool" => rec.tool_name.clone().map(|tool_name| Message::Tool {
            content: rec.content.clone(),
            tool_name,
        }),
        other => {
            warn!(role = %other, idx = rec.idx, "G15: unknown message role, skipping");
            None
        }
    }
}

/// TODO: doc
pub fn merge_delegate_tool(
    _tool_name: &str,
    args: &JsonValue,
    delegate_context: &DelegateContext,
) -> JsonValue {
    let mut merged = args.clone();
    if let JsonValue::Object(map) = &mut merged {
        map.insert(
            "delegate_depth".to_string(),
            JsonValue::integer(delegate_context.current_depth as i64),
        );
        map.insert(
            "parent_agent".to_string(),
            JsonValue::string(delegate_context.parent_agent_type.clone()),
        );
    }
    merged
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod runner_tests;
