// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent runner -- ReAct loop execution core (event-driven framework)
//!
//! Full Fact loop flow:
//! AgentRunner submits Command -> POST /api/sessions/{id}/command -> evorule produces IoRequest ->
//! SSE pushes io_request event -> AgentRunner executes external call -> POST /api/sessions/{id}/io_response ->
//! evorule produces IoResponse + StateTransition -> SSE pushes stable event -> AgentRunner returns result

use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use futures_core::Stream;
use futures_util::StreamExt;
use serde_json::Value;
use tracing::{debug, info, warn};

use tokio_util::sync::CancellationToken;

use crate::agent::approval::{
    parse_approval_request, ApprovalCallback, ApprovalDecision, ApprovalRequest,
};
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

/// TODO: doc
pub const DEFAULT_MAX_DELEGATE_DEPTH: usize = 3;

/// R2-T04 链体积告警阈值（收官遗留 B3）：单会话 facts_log 审计链长达到该值
/// 时 warn 告警（观测口径，不拦截）。量级参照：单节点 agent 会话典型链长
/// 数十至数百条；10,000 条 = 超长会话（多轮重试/长循环）的异常增长信号。
const CHAIN_SIZE_WARN_ENTRIES: u64 = 10_000;

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
    /// M5-a:生效能力边界声明(serve/CLI 层注入;None = 调用方未注入,
    /// 会话无边界段与边界事实——缺省定义行为同 v1.0)
    pub capability_boundary: Option<crate::agent::definition::CapabilityBoundary>,
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
            capability_boundary: None,
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
    match result.strip_suffix("```") {
        Some(stripped) => stripped.trim().to_string(),
        None => result.to_string(),
    }
}

/// 本地工具执行产物(流式本地 ReAct 循环与 call_service IoRequest 分支共用)
///
/// stream! 宏的 yield 无法跨越函数边界,审批事件数据由调用方取出后自行 yield。
struct ToolExecOutcome {
    final_result: Value,
    /// 审批留痕(本轮发生过审批时内嵌进 ToolResult.result,不扩 Fact 枚举)
    approval_record: Option<Value>,
    /// 发生审批时的 (请求, 决定),供调用方补发 ApprovalRequired/ApprovalResult
    approval_flow: Option<(ApprovalRequest, ApprovalDecision)>,
}

/// 工具执行两阶段拆分的阶段一产物(流式路径专用)
///
/// 背景(治理叠加实测暴露):原一气呵成实现里,ApprovalRequired 事件在
/// `request_approval` 返回后才 yield —— 帧到达前端时 60s 审批窗口已过
/// (超时自动拒绝先行),HTTP 审批通道在流式路径上结构性不可用。拆为:
/// 阶段一 [`AgentRunner::execute_tool_stage`] 执行并解析 proposal(不决策);
/// 调用方(stream! 生成器)此时 yield ApprovalRequired,再进阶段二
/// [`AgentRunner::resolve_approval`] 等待决定并按需重执行。
enum ToolExecStage {
    /// 无需审批(含 G13 缓存命中),执行已完成
    Done(ToolExecOutcome),
    /// 工具返回 needs_approval proposal,待调用方通知用户后进入阶段二
    Pending(ApprovalRequest),
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
        /// 该 agent 是否启用记忆配置(memory_config 存在即 true)
        memory_enabled: bool,
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
        /// 提案 ID(审批留痕主键,/approve 回传校验用)
        proposal_id: String,
    },
    /// G8:审批结果
    ApprovalResult {
        /// 工具名称
        tool_name: String,
        /// 是否批准
        approved: bool,
        /// 决策者标识(用户名 / unverified / cli-user / auto)
        approver: String,
        /// 是否系统自动拒绝(超时 / 通道关闭)
        auto_rejected: bool,
    },
}

// ----- M5-c:工具意图裁决(规范字段生产=机制层,处置=规则层,兜底=机制层)-----

/// M5-c:工具意图信号指令形态(纯函数)
///
/// 中性判据:`set meta_tool.pending_target_scope = <scope>`。机制层只生产
/// 规范字段(target_scope 解析结果),「拦不拦」的处置完全由规则层
/// 00_constraint_collab_acceptance 的 enforce 裁决——被拦时引擎丢弃指令
/// (version 不推进),放行时内建 set 落状态(version+1)。
pub fn intent_signal(scope: &str) -> Value {
    serde_json::json!({
        "type": "set",
        "params": {
            "attr": "meta_tool.pending_target_scope",
            "operation": "set",
            "value": scope
        }
    })
}

/// M5-c:解析 file 类工具调用的目标范围(纯函数,宪法 §七「规范字段生产」)
///
/// file_read/file_write 的 path 参数对照能力边界 sandbox_root 判
/// in_sandbox/out_of_sandbox;非 file 工具/无边界声明/无 path 参数
/// → None(不提交意图,零开销路径)。纯字符串/路径运算不触 fs——
/// symlink 逃逸等精确判定仍由机制层 handler 内联检查兜底(双层分工:
/// 意图快筛供规则层裁决,handler 精判为最终防线)。
pub fn resolve_target_scope(
    tool_name: &str,
    args: &Value,
    boundary: Option<&crate::agent::definition::CapabilityBoundary>,
) -> Option<&'static str> {
    let boundary = boundary?;
    if tool_name != "file_read" && tool_name != "file_write" {
        return None;
    }
    let raw = args.get("path").and_then(|v| v.as_str())?;
    let p = std::path::Path::new(raw);
    if p.is_absolute() {
        return Some("out_of_sandbox");
    }
    for component in p.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Some("out_of_sandbox");
        }
    }
    let joined = boundary.sandbox_root.join(p);
    if joined.starts_with(&boundary.sandbox_root) {
        Some("in_sandbox")
    } else {
        Some("out_of_sandbox")
    }
}

/// M5-c:意图裁决感知参数——提交后轮询会话 version 的窗口
///
/// command 端点=异步队列语义(HTTP success 不代表未被 enforce 拦截),
/// 拦截的引擎语义=丢弃指令不推进 version;ReAct 循环串行提交无竞态。
/// 引擎处理为毫秒级,20×50ms=1s 窗口上限远大于正常裁决时延。
const INTENT_VERDICT_POLLS: usize = 20;
const INTENT_VERDICT_INTERVAL_MS: u64 = 50;

async fn session_version(client: &EvoruleApiClient, session_id: &str) -> Result<u64, String> {
    let state = client
        .get_state(session_id)
        .await
        .map_err(|e| e.to_string())?;
    state
        .get("version")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("session {} state missing version", session_id))
}

/// M5-c:提交信号指令并按会话 version 判别规则层裁决结果
///
/// 通用感知原语(M5-c 三条 enforce 的统一感知面):command 端点=异步队列
/// 语义(HTTP success 不代表未被 enforce 拦截),拦截的引擎语义=丢弃指令
/// 不推进 version;ReAct 循环串行提交无竞态。
/// 返回 `Ok(true)`=放行(version 推进);`Ok(false)`=被 enforce 拦截
/// (轮询窗口内 version 未变);`Err`=感知通道故障(裁决不可信,fail-fast
/// 由调用方上抛——与 M5-b 信号提交失败同族的硬义务语义)。
pub async fn submit_signal_and_await_verdict(
    client: &EvoruleApiClient,
    session_id: &str,
    command: &Value,
) -> Result<bool, String> {
    let before = session_version(client, session_id).await?;
    client
        .submit_command(session_id, command)
        .await
        .map_err(|e| e.to_string())?;
    for _ in 0..INTENT_VERDICT_POLLS {
        tokio::time::sleep(std::time::Duration::from_millis(INTENT_VERDICT_INTERVAL_MS)).await;
        if session_version(client, session_id).await? > before {
            return Ok(true);
        }
    }
    Ok(false)
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
    /// key = `"{tool_name}:{serde(args)}"`,value = 工具返回的 Value。
    /// 每次 `call_external` 开始时清空(新一轮 LLM 调用,旧缓存失效)。
    parallel_tool_cache: Arc<std::sync::Mutex<std::collections::HashMap<String, Value>>>,
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
    /// plan-execute tokens 埋点累加器（纲领 §8 Phase 2 交付物 7，None = 不埋点）
    ///
    /// 由 [`DelegateContext::with_token_counter`] 注入并随每个子 runner 共享
    /// 同一 `Arc`：每次 LLM `IoRequest` 处理完，从 io_response result 的
    /// `token_usage.total_tokens`（llm_handler 已解析 provider usage）累加。
    /// 外层驱动据此维护 `BudgetCounters.tokens_used` 与 replan 重复执行
    /// token 埋点（D-02 判定数据源）。仅供观测，不改变任何控制流。
    token_counter: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// 独立裁决会话通道(file 类工具意图裁决)
    ///
    /// 主会话 call_external 在途时引擎命令串行评估使「主会话提交+轮询」
    /// 恒超时(假拦根因);裁决改走独立 evorule 会话(每会话独立反应器,
    /// 不受主会话 io 在途影响)。tokio Mutex:G13 并行工具路径可并发进入
    /// `execute_tool_call`,且裁决全程含 await。
    adjudicator: tokio::sync::Mutex<crate::agent::adjudicator::AdjudicationChannel>,
}

impl AgentRunner {
    /// TODO: doc
    pub fn new(config: AgentConfig, evorule_client: EvoruleApiClient) -> Self {
        // 裁决通道与 runner 同源装配(单一事实源——CLI/driver/serve
        // 三入口统一,与 M5-a 边界接线同款);agent_type 预取供 initial_content
        let agent_type = config.agent_type.clone();
        let adjudicator_client = evorule_client.clone();
        Self {
            config,
            evorule_client,
            llm_handler: LlmHandler::with_defaults(),
            tool_handler: ToolHandler::new(),
            memory: None,
            delegate_context: None,
            session_id: None,
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
            token_counter: None,
            adjudicator: tokio::sync::Mutex::new(
                crate::agent::adjudicator::AdjudicationChannel::new(
                    adjudicator_client,
                    &agent_type,
                ),
            ),
        }
    }

    /// 注入 tokens 埋点累加器（plan-execute 外层驱动经 [`DelegateContext`] 共享）
    pub fn with_token_counter(
        mut self,
        counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        self.token_counter = Some(counter);
        self
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
            // B5：stable 事实由摘要管道产出 → 域段记摘要模型（缺省回退主模型）
            llm_model_id: def
                .memory
                .summary_model
                .clone()
                .unwrap_or_else(|| def.model.clone()),
        };
        // 用户决策 3 + G10：summary_model 单独配置,同时构造 ContextSummarizer
        if let Some(sm) = def.memory.summary_model {
            runner = runner.with_summary_model(&sm);
            // G10:clone 主 LlmHandler 给摘要器(共享 API key/配置,独立调用)
            // summary_model 指定后,摘要调用使用该模型;否则 fallback 到主 model
            // P2-V3 结构性修复(2026-08-27)：必挂审计链执行器,
            // 摘要类 LLM 调用经 sidecar 会话入审计链,影子调用归零
            let audited = crate::agent::audited_llm::AuditedLlm::new(
                runner.evorule_client.clone(),
                runner.llm_handler.clone(),
            );
            let summarizer =
                ContextSummarizer::new(runner.llm_handler.clone(), Some(sm)).with_auditor(audited);
            runner = runner.with_summarizer(summarizer);
        }
        // G2:自动构造 ContextWindowManager(默认 8192 token,reserve 1/4)
        // R11：默认值必须可见，不得静默——未显式设置时记忆区预算
        // = 8192 × 25% = 2,048 token，约 60-80 条即饱和并开始裁剪（实测）。
        let max_tokens = match def.context_window_tokens {
            Some(t) => t,
            None => {
                warn!(
                    "context_window_tokens 未显式设置,使用默认 8192(记忆区预算 = 8192 × 25% = 2048 token,约 60-80 条即饱和;生产部署建议显式声明)"
                );
                8192
            }
        };
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

    /// M5-a:注入生效能力边界声明(serve/CLI 层按定义或启动配置合成后传入;
    /// 注入后 run/run_streaming 会话建立时追加系统级边界段 + 随
    /// create_session initial_content 进会话事实)
    pub fn with_capability_boundary(
        mut self,
        boundary: crate::agent::definition::CapabilityBoundary,
    ) -> Self {
        self.config.capability_boundary = Some(boundary);
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
        let drained: Vec<(usize, Message)> = std::mem::take(&mut self.pending_messages);
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

        // B3: 召回前按节流间隔校验 cache 与真相源漂移（server wins 对齐）
        if let Some(mem) = self.memory.as_mut() {
            let drift = mem.verify_cache_if_due().await;
            if drift > 0 {
                if let Some(m) = &self.metrics {
                    m.inc_memory_cache_drift(drift as u64);
                }
            }
        }

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
        let mut system_prompt = match self.memory.as_ref() {
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
        // M5-a:系统级边界段注入(会话建立稳定位置;首要读者 = LLM 自知)
        if let Some(b) = &self.config.capability_boundary {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&b.awareness_segment());
        }

        // M5-a:边界声明经 create_session initial_content 既有载体进会话事实
        let boundary_json = self
            .config
            .capability_boundary
            .as_ref()
            .map(|b| b.to_json());
        let session_id = self
            .evorule_client
            .create_session(boundary_json.as_ref())
            .await?;
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

        // 注意:必须先订阅 SSE 事件,再提交命令。
        // tokio broadcast 通道只接收订阅之后发出的消息,不重放历史。
        // 如果先 submit_command 再 subscribe,会错过 io_request 事件,导致 ReAct 循环无法启动。
        let mut event_stream = self.evorule_client.subscribe_events(&session_id).await?;

        let command =
            self.build_call_external_command(&system_prompt, goal, self.openai_tools_payload());
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
                        r = self.handle_io_request(&session_id, &event.payload, &mut messages, &mut tool_calls) => match r {
                            Ok(r) => r,
                            // 处理失败(60s 超时/LLM 错误/工具错误/内部错误)也必须
                            // 回写 error io_response —— 否则 server 侧 io_request 永久挂起、
                            // 链实不一致(幽灵在途请求)。与取消分支/流式错误分支/audited_llm
                            // 「失败也回写再返回」同一契约。回写后照旧上抛终止本轮。
                            Err(e) => {
                                if let Some(rid) = event.payload.get("id").and_then(|v| v.as_u64()) {
                                    let err_str = e.to_string();
                                    let _ = self.evorule_client
                                        .submit_io_response(
                                            &session_id,
                                            rid,
                                            &serde_json::json!({"error": &err_str}),
                                            Some(err_str.as_str()),
                                        )
                                        .await;
                                }
                                let _ = self.flush_messages(&session_id).await;
                                return Err(e);
                            }
                        },
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

                    // plan-execute tokens 埋点（纲领 §8 Phase 2 交付物 7）：llm_handler
                    // 把 provider usage 解析为 result.token_usage；此处只累加不干预。
                    if let Some(counter) = &self.token_counter {
                        if let Some(total) = result
                            .get("token_usage")
                            .and_then(|t| t.get("total_tokens"))
                            .and_then(|v| v.as_u64())
                        {
                            counter.fetch_add(total, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
                "Stable" => {
                    let duration = start_time.elapsed().as_millis() as u64;
                    // 确保所有缓冲的消息都写入 evorule（EveryN/PerReactRound 模式）
                    self.flush_messages(&session_id).await?;
                    // C1:会话沉淀（best-effort，摘要+稳定事实→共享空间）
                    let _ = self.sediment_session(&session_id, &messages).await;
                    // R2-T04 链体积观测（B3）：会话收尾时 best-effort 查审计链长告警
                    self.check_chain_size(&session_id).await;
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
                    // 与流式路径(last_llm_content fallback)对称——payload 读空时
                    // 回捞本轮最近一次 LLM 输出(非流式单发场景=本会话唯一 LLM 响应)。
                    let content = if content.is_empty() {
                        messages
                            .iter()
                            .rev()
                            .find_map(|m| match m {
                                Message::Assistant { content, .. } => {
                                    if content.is_empty() {
                                        None
                                    } else {
                                        Some(content.clone())
                                    }
                                }
                                _ => None,
                            })
                            .unwrap_or_default()
                    } else {
                        content
                    };
                    if content.is_empty() {
                        // 空产出观测补位(不改判 success——合法空响应不误伤)。
                        // 三联指纹(tokens=0+亚秒+空 content)曾掩盖 LLM 失败假绿,
                        // 此处保证链上观测可见。
                        warn!(%session_id, step_count, "Stable with empty content: possible LLM empty response (tokens_used side-channel in io_response)");
                    }

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
                "Violation" => {
                    // D-01 拍板结论（纲领 §9.5.4 修订版 + D-01 契约分析 §6.2）：enforce
                    // 命中直接失败上抛——不 auto_rewind、不重试；workflow 层凭固定
                    // 前缀 `enforce violation:` 判别后终止、不 replan（§9.5.1 选项 B）。
                    // 不 bump 会话版本——与 reactor 侧「指令已丢弃、状态未变」一致。
                    let rule_index = event.payload.get("rule_index").and_then(|v| v.as_u64());
                    let reason = event
                        .payload
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no reason)");
                    warn!(
                        %session_id,
                        rule_index,
                        %reason,
                        "enforce 拦截：违规指令被拒绝执行（D-01：一票否决，不重试）"
                    );
                    let _ = self.flush_messages(&session_id).await;
                    let _ = self.sediment_session(&session_id, &messages).await;
                    let duration = start_time.elapsed().as_millis() as u64;
                    return Ok(AgentResult::error(
                        format!("enforce violation: rule_index={rule_index:?}, reason={reason}"),
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
        // D-01 二次保险（B2）：断流可能吞掉 Violation 帧，查 evolution-signals
        // 兜底归因 enforce 命中；查询不可用时降级返回原错误（不掩盖不阻塞）。
        let closed_error = self
            .detect_enforce_after_stream_close(&session_id, "Event stream closed")
            .await;
        Ok(AgentResult::error(closed_error, step_count, duration))
    }

    /// 组装随 LLM 请求下发的工具 OpenAI function schema。
    ///
    /// 数据源 = 静态工具 spec 目录(`default_tool_specs`,delegate 若已注册则追加其 spec)
    /// 与 `tool_handler` 实际注册执行器求交;agent 配置的 `tools` 列表已在
    /// `from_definition` 校验过 ⊆ 注册集,故以注册集为准即可覆盖配置意图。
    ///
    /// 形状遵循 OpenAI function calling 标准 JSON Schema:
    /// `{"type":"function","function":{"name","description","parameters":{type:object,properties,required}}}`。
    /// 未知名(自定义注册、服务代理等无静态 spec)从服务消费桥注册表透出
    /// description/parameters,无声明时降级为最小 schema 并记 debug 日志。
    ///
    /// 返回 `None` = 请求不携带 tools 键(空集/无工具场景,向后兼容)。
    fn openai_tools_payload(&self) -> Option<Vec<Value>> {
        let registered = self.tool_handler.tool_names();
        let mut specs = crate::builtin_tools::default_tool_specs();
        // 规则工具静态 spec 并入:rule_tools 的 23 个工具若不在此处,会走 dynamic
        // 分支产出空参数 schema,LLM 无从得知 workspace_id 等必填参数(实测盲传
        // 导致 rule_list 失败)。spec 与执行器同源于 rule_tool_specs()。
        specs.extend(crate::rule_tools::rule_tool_specs());
        if self.tool_handler.has_tool("delegate") {
            specs.push(crate::builtin_tools::delegate_tool::delegate_tool_spec());
        }

        let mut tools = Vec::new();
        for name in &registered {
            let Some(spec) = specs.iter().find(|s| &s.name == name) else {
                // 动态注册的工具(如 server 插件服务代理)无静态 spec:描述与参数
                // 契约从服务消费桥的注册表透出(对账清单 description/parameters,
                // 插件包 plugin.json 声明);参数契约无声明时降级空 object(向后兼容)。
                let description =
                    crate::service_tools::service_description(name).unwrap_or_default();
                let parameters = crate::service_tools::service_parameters(name)
                    .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));
                debug!(tool = %name, "registered tool has no static spec; emitting dynamic schema");
                tools.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": parameters,
                    }
                }));
                continue;
            };
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for p in &spec.parameters {
                properties.insert(
                    p.name.clone(),
                    serde_json::json!({ "type": p.r#type, "description": p.description }),
                );
                if p.required {
                    required.push(p.name.clone());
                }
            }
            let mut parameters = serde_json::json!({ "type": "object", "properties": properties });
            if !required.is_empty() {
                parameters["required"] = serde_json::json!(required);
            }
            tools.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": spec.name,
                    "description": spec.description,
                    "parameters": parameters,
                }
            }));
        }

        if tools.is_empty() {
            None
        } else {
            Some(tools)
        }
    }

    /// LLM 请求的 tools 来源:server 中继优先,缺省回 runner 本地 schema
    ///
    /// 原设计依赖 server 的 io_request 规则以 `tools?` 键把指令中的 tools 中继回
    /// IoRequest.params;但 server 侧 evorule-tcb 0.6.1 的规则解释器尚不支持该
    /// 可选中继语法,IoRequest.params 实测可能为空。tools 是 runner 的自有状态
    /// (from_definition 已校验 ⊆ 注册集),本地兜底保证 serve 链路的 LLM 请求
    /// 始终携带工具契约 —— 缺了它模型只能凭训练先验盲猜或输出供应商原生 XML,
    /// 两侧解析器均无法消费。
    fn resolve_llm_tools(&self, params: &Value) -> Option<Value> {
        if let Some(tools) = params.get("tools") {
            let non_empty = tools.as_array().map(|a| !a.is_empty()).unwrap_or(false);
            if non_empty {
                return Some(tools.clone());
            }
        }
        self.openai_tools_payload().map(Value::Array)
    }

    fn build_call_external_command(
        &self,
        system_prompt: &str,
        goal: &str,
        tools: Option<Vec<Value>>,
    ) -> Value {
        // core_eval v0.3.1 合约:call_external 指令仅使用 messages(LLM 消息历史数组)
        // 与可选 tools;prompt/system/goal/tool_names 不再是指令参数。
        // core_eval 的 io_request 规则引用 __exec__.instruction.params.messages,
        // 缺失会导致 "path resolution failed"。
        // tools(OpenAI function schema)让 LLM 知晓可用工具的真实名字与
        // 参数形状,否则模型只能凭训练先验盲猜(如 read_file≠file_read)或输出
        // 供应商原生格式(<minimax:tool_call> XML),两侧解析器均无法消费。
        let mut params = serde_json::json!({
            "model": self.config.model,
            "temperature": self.config.temperature,
            "messages": [
                { "role": "system", "content": system_prompt },
                { "role": "user", "content": goal },
            ],
        });
        if let Some(tools) = tools {
            params["tools"] = Value::Array(tools);
        }
        // 链上补记实际生效的生成参数(与 llm_handler 兜底逻辑同源):
        // max_tokens 未在 agent 定义/配置层提供 → None 走 handler 兜底 4096,stream 恒 true
        let (effective_temperature, effective_max_tokens, effective_stream) =
            crate::io_handlers::llm_handler::effective_generation_params(
                Some(self.config.temperature as f64),
                None,
            );
        params["effective_params"] = serde_json::json!({
            "temperature": effective_temperature,
            "max_tokens": effective_max_tokens,
            "stream": effective_stream,
        });
        serde_json::json!({
            "type": "call_external",
            "params": params,
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
        let tcb_messages = serde_messages.clone();

        let mut call_params = serde_json::Map::new();
        call_params.insert("model".to_string(), Value::from(model.to_string()));
        call_params.insert(
            "temperature".to_string(),
            Value::from(temperature.to_string()),
        );
        call_params.insert("messages".to_string(), tcb_messages);

        // 转发 constitution 中继的 tools(OpenAI function schema)。
        // 引擎 io_request 规则以 `tools?` 键中继指令的 tools;缺了它 LLM 只能凭
        // 训练先验盲猜工具名(如 read_file≠file_read)或输出供应商原生 XML,
        // 两侧解析器均无法消费 —— 与 build_call_external_command 的注入同源。
        // server 中继缺省时回 runner 本地 schema(见 resolve_llm_tools)。
        if let Some(tools) = self.resolve_llm_tools(params) {
            call_params.insert("tools".to_string(), tools);
        }

        // G17:LLM 调用计时 + 指标(observe_llm_call 在 ? 之前记录,确保 error 也被统计)
        let llm_start = std::time::Instant::now();
        let llm_result = self
            .execute_external("call_external", &Value::Object(call_params))
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

        // core_eval v0.3.1:merge 规则引用 __exec__.payload.llm_response.messages
        // 作为下一轮 call_external 的消息历史,缺失会导致 "path resolution failed"。
        // 返回完整消息历史(含本轮 assistant 回复),保持 TCB 侧历史连续。
        let result_messages = serde_json::to_value(messages)
            .map_err(|e| AgentError::Internal(format!("serialize result messages: {}", e)))?;

        Ok(serde_json::json!({
            "content": final_content,
            "tool_calls": effective_tool_calls,
            "is_finished": llm_response.is_finished(),
            // plan-execute tokens 埋点数据源（纲领 §8 Phase 2 交付物 7）：
            // provider usage 透传进 io_response result，runner 累加点据此计数
            // （此前在此处被丢弃，埋点恒 0——E2E 场景 A 实测暴露后修复）
            "token_usage": llm_response.token_usage.clone(),
            "messages": result_messages,
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
        // 审批留痕:仅本轮发生过审批时为 Some(内嵌进 io_response.result)
        let (tool_result, approval_record) = if let Some(cached) =
            self.check_parallel_cache(tool_name, &args)
        {
            info!(%session_id, tool = %tool_name, "G13: call_service cache hit, skipping re-execution");
            (cached, None)
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

        let mut result = serde_json::json!({
            "tool_name": tool_name,
            "result": tool_result.to_string(),
        });
        if let Some(record) = approval_record {
            result["approval"] = record;
        }
        Ok(result)
    }

    /// G8:执行工具调用(不含审批逻辑,纯执行)
    ///
    /// 从 `handle_call_service` 和流式路径的审批重调用共用。
    /// 第一次调用不带 `approved` flag → 工具可能返回 `needs_approval` proposal。
    /// 第二次调用(审批通过后)带 `approved:true` → 工具直接执行。
    async fn execute_tool_call(&self, tool_name: &str, args: &Value) -> Result<Value, AgentError> {
        // M5-c:工具意图裁决(双层防线的外层)——file 类调用先把规范字段
        // target_scope 随意图指令进链,由协作验收规则 enforce 裁决:
        // 被拦(version 未推进)则不执行工具,向 LLM 返回治理拦截结果;
        // 放行则继续执行,机制层 handler 内联沙箱检查保留为最终防线。
        // 裁决改走独立裁决会话(AdjudicationChannel)——主会话
        // call_external 在途时引擎串行评估使主会话内轮询恒超时(假拦根因),
        // 独立会话裁决不受 io 在途影响(原型 PV2 实测 73ms)。原
        // `if let Some(session_id)` 守卫删除:首轮/续轮统一走裁决通道,
        // 伴生缺陷(新建分支漏设 session_id 致首轮跳过裁决)自然消解。
        if let Some(scope) =
            resolve_target_scope(tool_name, args, self.config.capability_boundary.as_ref())
        {
            let allowed = self
                .adjudicator
                .lock()
                .await
                .await_verdict(&intent_signal(scope), self.session_id.as_deref())
                .await
                .map_err(AgentError::Internal)?;
            if !allowed {
                warn!(
                    main_session = ?self.session_id, tool = %tool_name, scope = %scope,
                    "tool intent blocked by governance rule (collab acceptance, adjudication channel)"
                );
                let raw_path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let boundary_root = self
                    .config
                    .capability_boundary
                    .as_ref()
                    .map(|b| b.sandbox_root.display().to_string())
                    .unwrap_or_default();
                return Ok(serde_json::json!({
                    "status": "blocked_by_governance_rule",
                    "tool": tool_name,
                    "target_scope": scope,
                    "reason": format!(
                        "target '{}' is outside the sandbox boundary '{}'; \
                         the collaboration acceptance rule rejected this tool intent \
                         (see session audit Violation for rule attribution)",
                        raw_path, boundary_root
                    ),
                }));
            }
        }
        let args_tcb = args.clone();
        let mut call_params = serde_json::Map::new();
        call_params.insert("tool_name".to_string(), Value::from(tool_name.to_string()));
        call_params.insert("args".to_string(), args_tcb);
        // G17:工具调用计时 + 指标(单一插桩点,覆盖 run() / run_streaming() / G13 并行路径)
        let tool_start = std::time::Instant::now();
        let result = self
            .execute_external("call_service", &Value::Object(call_params))
            .await;
        let tool_duration = tool_start.elapsed();
        let tool_ok = result.is_ok();
        if let Some(m) = &self.metrics {
            m.observe_tool_call(tool_name, tool_duration, tool_ok);
        }
        result
    }

    /// 阶段一(流式路径):执行工具并解析 needs_approval proposal,不做决策
    ///
    /// 供 stream! 生成器在 yield ApprovalRequired **之前**调用 —— 帧必须在
    /// 60s 审批窗口开启后、超时前到达前端,否则 HTTP 审批结构性不可用。
    /// 无审批(含缓存命中)时返回 [`ToolExecStage::Done`],一步到位。
    async fn execute_tool_stage(
        &self,
        session_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> Result<ToolExecStage, AgentError> {
        // G13:并行缓存命中(如果 call_external 已并行执行过此 active 工具,
        // 直接返回缓存结果,跳过重复执行 + 审批;candidate 工具不缓存)
        if let Some(cached) = self.check_parallel_cache(tool_name, args) {
            info!(%session_id, tool = %tool_name, "G13: cache hit, skipping re-execution");
            return Ok(ToolExecStage::Done(ToolExecOutcome {
                final_result: cached,
                approval_record: None,
                approval_flow: None,
            }));
        }

        // G8:第一次调用(不带 approved flag)→ 可能返回 needs_approval proposal
        let tool_result = self.execute_tool_call(tool_name, args).await?;
        let result_str = tool_result.to_string();
        match parse_approval_request(session_id, tool_name, args, &result_str) {
            None => Ok(ToolExecStage::Done(ToolExecOutcome {
                final_result: tool_result,
                approval_record: None,
                approval_flow: None,
            })),
            Some(req) => Ok(ToolExecStage::Pending(req)),
        }
    }

    /// 阶段二(流式路径):等待审批决定并按需重执行(阶段一返回 Pending 后调用)
    ///
    /// 语义与原一气呵成实现完全一致:决定 → 审批留痕 record → 拒绝返回
    /// status:rejected / 批准带 approved:true 重新调用(不递归检查 proposal)。
    async fn resolve_approval(
        &self,
        session_id: &str,
        tool_name: &str,
        args: &Value,
        approval_req: ApprovalRequest,
    ) -> Result<ToolExecOutcome, AgentError> {
        // 审批决策(无 callback = 默认拒绝,安全优先)
        let decision = if let Some(cb) = &self.approval_callback {
            cb.request_approval(&approval_req).await
        } else {
            ApprovalDecision {
                approved: false,
                approver: "auto".to_string(),
                verified: true,
                reason: "denied by policy".to_string(),
                auto_rejected: false,
            }
        };

        // 审批留痕 record(decided_at 用 epoch 秒)
        let decided_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let decision_label = if decision.approved {
            "approved"
        } else if decision.auto_rejected {
            "auto_rejected"
        } else {
            "rejected"
        };
        let record = serde_json::json!({
            "proposal_id": approval_req.proposal_id,
            "tool": tool_name,
            "decision": decision_label,
            "approver": decision.approver,
            "verified": decision.verified,
            "decided_at": decided_at,
            "reason": decision.reason,
        });

        let final_result = if !decision.approved {
            warn!(%session_id, tool = %tool_name, "G8: tool call rejected");
            Value::from(r#"{"status":"rejected","message":"User denied approval"}"#)
        } else {
            // 批准 → 带 approved:true 重新调用(不递归检查 proposal)
            info!(%session_id, tool = %tool_name, "G8: tool call approved, re-executing with approved=true");
            let mut approved_args = args.clone();
            if let Some(obj) = approved_args.as_object_mut() {
                obj.insert("approved".to_string(), Value::Bool(true));
            } else {
                approved_args = serde_json::json!({"original_args": args, "approved": true});
            }
            self.execute_tool_call(tool_name, &approved_args).await?
        };

        Ok(ToolExecOutcome {
            final_result,
            approval_record: Some(record),
            approval_flow: Some((approval_req, decision)),
        })
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
    fn parallel_cache_get(&self, key: &str) -> Option<Value> {
        self.parallel_tool_cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(key).cloned())
    }

    /// G13:写入工具结果到缓存
    fn parallel_cache_put(&self, key: String, value: Value) {
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
    /// 返回 `Value`(工具结果,可能是 proposal)。
    async fn execute_single_tool(&self, tc: &crate::agent::translator::ToolCall) -> Value {
        let args_tcb = tc.arguments.clone();
        match self.tool_handler.execute_by_name(&tc.name, &args_tcb).await {
            Ok(result) => result,
            Err(e) => {
                // 工具执行失败:返回 error JSON(不中断其他并行工具)
                let mut map = serde_json::Map::new();
                map.insert("status".to_string(), Value::from("error"));
                map.insert("error".to_string(), Value::from(e));
                Value::Object(map)
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
    ) -> Vec<(String, Value, Value)> {
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
    /// 命中则返回缓存的 Value(不重复执行),未命中则返回 None。
    /// 仅当 `max_parallel_tools > 1` 时启用缓存查询。
    fn check_parallel_cache(&self, tool_name: &str, args: &Value) -> Option<Value> {
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
    ///
    /// 返回 `(工具最终结果, 审批留痕 record)`:record 仅在本轮发生过审批时
    /// 为 `Some`(内嵌进 io_response.result,不扩 Fact 枚举)。
    async fn maybe_handle_approval(
        &self,
        session_id: &str,
        tool_name: &str,
        args: &Value,
        tool_result: Value,
    ) -> Result<(Value, Option<Value>), AgentError> {
        let result_str = tool_result.to_string();
        let approval_req = match parse_approval_request(session_id, tool_name, args, &result_str) {
            Some(req) => req,
            None => return Ok((tool_result, None)), // 不是 proposal,直接返回
        };

        info!(%session_id, tool = tool_name, "G8: tool requires approval");

        // 问用户(无 callback = 默认拒绝,安全优先)
        let decision = if let Some(cb) = &self.approval_callback {
            cb.request_approval(&approval_req).await
        } else {
            ApprovalDecision {
                approved: false,
                approver: "auto".to_string(),
                verified: true,
                reason: "denied by policy".to_string(),
                auto_rejected: false,
            }
        };

        // 构造审批留痕 record(decided_at 用 epoch 秒)
        let decided_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let decision_label = if decision.approved {
            "approved"
        } else if decision.auto_rejected {
            "auto_rejected"
        } else {
            "rejected"
        };
        let approval_record = serde_json::json!({
            "proposal_id": approval_req.proposal_id,
            "tool": tool_name,
            "decision": decision_label,
            "approver": decision.approver,
            "verified": decision.verified,
            "decided_at": decided_at,
            "reason": decision.reason,
        });

        if !decision.approved {
            warn!(%session_id, tool = tool_name, "G8: tool call rejected");
            return Ok((
                Value::from(r#"{"status":"rejected","message":"User denied approval"}"#),
                Some(approval_record),
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
        let final_result = self.execute_tool_call(tool_name, &approved_args).await?;
        Ok((final_result, Some(approval_record)))
    }

    async fn execute_external(&self, io_type: &str, params: &Value) -> Result<Value, AgentError> {
        tokio::time::timeout(self.config.step_timeout, async {
            let mut instr = serde_json::Map::new();
            instr.insert("type".to_string(), Value::from(io_type.to_string()));
            instr.insert("params".to_string(), params.clone());

            let tcb_instr = Value::Object(instr);
            self.execute_io_request(&tcb_instr).await
        })
        .await
        .map_err(|_| AgentError::Timeout(format!("{} timeout", io_type)))?
    }

    async fn execute_io_request(&self, request: &Value) -> Result<Value, AgentError> {
        match request.get("type").and_then(|v| v.as_str()) {
            Some("call_external") => {
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                self.execute_llm_request(&params).await
            }
            Some("call_service") => {
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                self.execute_tool_request(&params).await
            }
            Some(t) => Err(AgentError::Internal(format!("unsupported io type: {}", t))),
            None => Err(AgentError::Internal("missing io type".to_string())),
        }
    }

    async fn execute_llm_request(&self, params: &Value) -> Result<Value, AgentError> {
        // Actually invoke LlmHandler (uses reqwest to call real HTTP API)
        // - Read MINIMAX_API_KEY / DEEPSEEK_API_KEY / OPENAI_API_KEY env vars
        // - Supports messages / tools / temperature / max_tokens
        // - Returns full OpenAI-compatible JSON response
        self.llm_handler
            .execute(params)
            .await
            .map_err(AgentError::LlmError)
    }

    async fn execute_tool_request(&self, params: &Value) -> Result<Value, AgentError> {
        // Real call through ToolHandler (60s timeout, tool_not_found detection).
        // Replaces the previous stub that returned a hardcoded "Simulated tool result".
        self.tool_handler
            .execute(params)
            .await
            .map_err(AgentError::ToolError)
    }

    async fn auto_recall(&self, session_id: &str) -> Result<Vec<u64>, AgentError> {
        // B5/D3：限定本 namespace（旧实现 `shared.` 跨 namespace 越权读）
        let prefix = format!("shared.{}.", self.sediment_config.namespace);
        let shared_facts = self.evorule_client.get_shared_facts(Some(&prefix)).await?;

        // R01（E19/S6 写回面）：与 recall_context 同一去重语义——
        // 同 path 只保留最新版本，墓碑（最新版为 null）不进写回内容，
        // 否则已删除的旧值经 auto_recall 写回记忆而复活。
        let latest = crate::agent::memory::latest_entries_by_path(shared_facts);

        if latest.is_empty() {
            info!(%session_id, "No shared facts to recall");
            return Ok(Vec::new());
        }

        let mut recalled_ids = Vec::new();
        let mut recalled_content = String::new();

        for (fact, _) in &latest {
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

    /// D-01 二次保险 + 降级兜底（收官遗留 B2；契约档 §6.1 降级口径）
    ///
    /// SSE 断流可能吞掉 `Violation` 帧（违规表现为「静默成功后流关闭」）。流
    /// 关闭时 best-effort 查 evolution-signals 检测 enforce 命中：
    /// - `total_violations > 0` → 返回 `enforce violation: ...` 固定前缀错误，
    ///   workflow 层凭前缀判别终止且不 replan（§9.5.1-B）；归因取链上最新违规
    ///   信号（`last_version` 最大者——signals 按 count 排序非时间序）。
    /// - 兜底查询不可用（网络断/会话被 TTL 收割/server 不可达）→ **降级**：
    ///   warn 留痕 + 返回携带本地上下文的原流关闭错误——不掩盖、不阻塞、不重试。
    async fn detect_enforce_after_stream_close(
        &self,
        session_id: &str,
        base_error: &str,
    ) -> String {
        let sid = match session_id.parse::<u64>() {
            Ok(v) => v,
            Err(_) => {
                warn!(%session_id, "enforce 兜底查询跳过：session id 非 u64");
                return base_error.to_string();
            }
        };
        match self
            .evorule_client
            .get_evolution_signals(sid, Some(8))
            .await
        {
            Ok(signals) if signals["total_violations"].as_u64().unwrap_or(0) > 0 => {
                let latest = signals["signals"].as_array().and_then(|arr| {
                    arr.iter()
                        .max_by_key(|s| s["last_version"].as_u64().unwrap_or(0))
                });
                let rule_ref = latest
                    .and_then(|s| s["rule_ref"].as_str())
                    .unwrap_or("(unknown rule)");
                let reason = latest
                    .and_then(|s| s["reason_summary"].as_str())
                    .unwrap_or("(no reason)");
                warn!(
                    %session_id,
                    rule_ref,
                    %reason,
                    "SSE 流关闭后经 evolution-signals 检测到 enforce 命中（D-01 二次保险）"
                );
                format!(
                    "enforce violation: rule_ref={rule_ref}, reason={reason} (detected via evolution-signals after stream close)"
                )
            }
            Ok(_) => base_error.to_string(),
            Err(e) => {
                warn!(
                    %session_id,
                    error = %e,
                    "enforce 兜底查询不可用，降级返回流关闭错误（本地信息附带）"
                );
                format!("{base_error} (steps context only; enforce-fallback unavailable: {e})")
            }
        }
    }

    /// R2-T04 链体积监控（收官遗留 B3）：长会话 facts_log 体积增长观测告警。
    ///
    /// 只读观测——best-effort 查审计报告 `entry_count`（BLAKE3 审计链长，链
    /// 体积的权威只读投影），达到 [`CHAIN_SIZE_WARN_ENTRIES`] 时 warn 告警；
    /// 查询失败静默降级。监控不干预执行：不写链、不拦截、不改变控制流
    /// （零红线风险，观测面与审计链解耦）。
    async fn check_chain_size(&self, session_id: &str) {
        match self.evorule_client.get_audit_report(session_id).await {
            Ok(report) => {
                let entries = report["entry_count"].as_u64().unwrap_or(0);
                if entries >= CHAIN_SIZE_WARN_ENTRIES {
                    warn!(
                        %session_id,
                        chain_entries = entries,
                        threshold = CHAIN_SIZE_WARN_ENTRIES,
                        "facts_log 链体积告警：会话链长达到阈值（R2-T04 观测）"
                    );
                } else {
                    debug!(
                        %session_id,
                        chain_entries = entries,
                        "facts_log 链体积观测（R2-T04）"
                    );
                }
            }
            Err(e) => {
                debug!(
                    %session_id,
                    error = %e,
                    "链体积观测查询失败（不干预执行）"
                );
            }
        }
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
            // B3: 召回前按节流间隔校验 cache 与真相源漂移（server wins 对齐）
            if let Some(mem) = runner.memory.as_mut() {
                let drift = mem.verify_cache_if_due().await;
                if drift > 0 {
                    if let Some(m) = &runner.metrics {
                        m.inc_memory_cache_drift(drift as u64);
                    }
                }
            }

            // C2: 召回顺序修复 —— recall 在 build_system_prompt 之前
            let recall = match runner.memory.as_ref() {
                Some(mem) => mem.recall_context(
                    &goal,
                    runner.sediment_config.max_session_summaries,
                    runner.sediment_config.max_injected_events,
                ).await,
                None => crate::agent::memory::RecallContext::default(),
            };
            let mut system_prompt = match runner.memory.as_ref() {
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
            // M5-a:系统级边界段注入(与 run() 同口径)
            if let Some(b) = &runner.config.capability_boundary {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&b.awareness_segment());
            }

            // 2. session:新建 或 复用(G15:continuation)
            let session_id = if let Some(id) = existing_session_id.clone() {
                // G15:continuation — 复用已有 session,不创建新 session
                // (session_active 守卫在下方统一创建,避免双重计数)
                runner.session_id = Some(id.clone());
                info!(%id, "G15: continuing existing session");
                id
            } else {
                // 新建 session(原 run_streaming 逻辑)
                // M5-a:边界声明经 initial_content 既有载体进会话事实
                let boundary_json = runner.config.capability_boundary.as_ref().map(|b| b.to_json());
                match runner.evorule_client.create_session(boundary_json.as_ref()).await {
                    Ok(id) => {
                        // 伴生缺陷修复:新建分支回填 runner.session_id
                        // (裁决通道已不依赖它,但审计一致性/messages 持久化
                        // 等消费方需要;与 continuation 分支对齐)
                        runner.session_id = Some(id.clone());
                        id
                    }
                    Err(e) => {
                        yield Err(AgentError::EvoruleError(e.to_string()));
                        return;
                    }
                }
            };

            // G17:session 活跃度守卫(新建 / 复用均持有,stream! 块结束时 dec)
            let _session_guard = SessionActiveGuard::new(runner.metrics.clone());

            if existing_session_id.is_none() {
                // 新建 session 才计 sessions_total + yield SessionCreated + auto_recall
                if let Some(m) = &runner.metrics {
                    m.inc_sessions_total();
                }
                yield Ok(AgentEvent::SessionCreated {
                    session_id: session_id.clone(),
                    // memory_config 存在(from_definition 已装 MemoryManager)即视为启用
                    memory_enabled: runner.memory.is_some(),
                });

                // 3. auto_recall(best-effort,不阻塞流)
                let _ = runner.auto_recall(&session_id).await;
            }

            // 5. 订阅 SSE(必须在 submit_command 之前,否则错过 io_request)
            let mut event_stream = match runner.evorule_client.subscribe_events(&session_id).await {
                Ok(s) => s,
                Err(e) => {
                    yield Err(AgentError::EvoruleError(e.to_string()));
                    return;
                }
            };

            // 6. 提交 call_external 命令(携带工具 OpenAI schema)
            let command =
                runner.build_call_external_command(&system_prompt, &goal, runner.openai_tools_payload());
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
                                // ===== 本地 ReAct 循环(v0.5.0 后多轮编排回归应用层) =====
                                // server 的 collect/merge 元指令已随 v0.5.0 退役,call_external
                                // 指令的 io_response 提交后即 Stable,不会再有下一轮。工具结果
                                // 回喂 LLM 由本循环负责:LLM 返回 tool_calls → 本地执行(审批/
                                // 缓存经 execute_tool_stage + resolve_approval) → tool 消息追加 → 再调
                                // LLM;直到产出最终 content 才提交 io_response(中间态不提交,
                                // server 无感知,无 IoRequest 响应超时风险)。回喂轮计入
                                // step_count 受 max_steps 限流,防失控循环。
                                let react_model = params
                                    .get("model")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| runner.config.model.clone());
                                let react_temperature = params
                                    .get("temperature")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(runner.config.temperature as f64);
                                let mut react_round: u32 = 0;
                                'react: loop {
                                    // 首轮 LLM 调用已随 IoRequest 到达计过 step(L2508),回喂轮补计
                                    if react_round > 0 {
                                        step_count += 1;
                                        if step_count > runner.config.max_steps {
                                            let err = AgentError::Internal(format!(
                                                "达到最大步数上限({}),工具结果回喂终止",
                                                runner.config.max_steps
                                            ));
                                            if let Some(rid) = request_id {
                                                let err_str = err.to_string();
                                                let _ = runner.evorule_client
                                                    .submit_io_response(&session_id, rid, &serde_json::json!({"error": &err_str}), Some(err_str.as_str()))
                                                    .await;
                                            }
                                            yield Ok(AgentEvent::Error(err.clone()));
                                            let duration = start_time.elapsed().as_millis() as u64;
                                            let _ = runner.flush_messages(&session_id).await;
                                            yield Ok(AgentEvent::Done(AgentResult::error(
                                                err.to_string(), step_count, duration,
                                            )));
                                            return;
                                        }
                                        info!(
                                            %session_id,
                                            round = react_round,
                                            "本地 ReAct 回喂:工具结果已入列,发起下一轮 LLM 调用"
                                        );
                                    }
                                    react_round += 1;
                                    let model: &str = react_model.as_str();
                                    let temperature = react_temperature;

                                // G1:流式调用 LLM
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
                                let tcb_messages = serde_messages.clone();

                                let mut call_params = serde_json::Map::new();
                                call_params.insert("model".to_string(), Value::from(model.to_string()));
                                call_params.insert("temperature".to_string(), Value::from(temperature.to_string()));
                                call_params.insert("messages".to_string(), tcb_messages);
                                // 转发 constitution 中继的 tools(同 handle_call_external 非流式路径);
                                // server 中继缺省时回 runner 本地 schema(见 resolve_llm_tools)
                                if let Some(tools) = runner.resolve_llm_tools(&params) {
                                    call_params.insert("tools".to_string(), tools);
                                }
                                let call_params_json = Value::Object(call_params);

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
                                            // plan-execute tokens 埋点（流式路径等效累加点，
                                            // 对齐非流式 run() IoRequest 臂）：
                                            // delegate 改走流式运行，埋点随 token_counter 继续生效
                                            // （流式中间态不提交 io_response，无非流式的 result 侧通道）
                                            if let Some(counter) = &runner.token_counter {
                                                if let Some(usage) = &resp.token_usage {
                                                    counter.fetch_add(
                                                        usage.total_tokens as u64,
                                                        std::sync::atomic::Ordering::Relaxed,
                                                    );
                                                }
                                            }
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
                                    // 持久化失败也不留悬挂在途 io_request(回写后终止)
                                    if let Some(rid) = request_id {
                                        let err_str = e.to_string();
                                        let _ = runner.evorule_client
                                            .submit_io_response(&session_id, rid, &serde_json::json!({"error": &err_str}), Some(err_str.as_str()))
                                            .await;
                                    }
                                    yield Err(e);
                                    return;
                                }

                                // ===== ReAct 分叉:有 tool_calls → 本地执行回喂;无 → 提交收尾 =====
                                let has_tool_calls = full_tool_calls
                                    .as_ref()
                                    .map(|tcs| !tcs.is_empty())
                                    .unwrap_or(false);
                                if has_tool_calls {
                                    // 有 tool_calls:本地执行每个工具(审批/缓存经 helper),
                                    // tool 消息入列后 continue 'react 发起回喂轮
                                    let tcs = full_tool_calls.unwrap();
                                    for tc in &tcs {
                                        yield Ok(AgentEvent::ToolCall {
                                            name: tc.name.clone(),
                                            args: tc.arguments.clone(),
                                        });
                                        // 本地执行(含 G8 审批流 + G13 缓存命中)。
                                        // 工具执行 Err(参数错/后端 404 等)不终止回合:错误
                                        // 作为 tool 消息回喂,LLM 可重试/换路/放弃 —— 实测
                                        // 硬终止会让一次 knowledge_search 404 毁掉整个草稿回合
                                        // (两阶段:Pending 时先 yield ApprovalRequired 再等
                                        // 决策 —— 帧必须赶在 60s 审批窗口内到达前端)
                                        let outcome_res = match runner
                                            .execute_tool_stage(&session_id, &tc.name, &tc.arguments)
                                            .await
                                        {
                                            Err(e) => Err(e),
                                            Ok(ToolExecStage::Done(o)) => Ok(o),
                                            Ok(ToolExecStage::Pending(req)) => {
                                                yield Ok(AgentEvent::ApprovalRequired {
                                                    tool_name: tc.name.clone(),
                                                    command: req.command.clone(),
                                                    risk: req.risk.clone(),
                                                    alternative: req.alternative.clone(),
                                                    proposal_id: req.proposal_id.clone(),
                                                });
                                                let res = runner
                                                    .resolve_approval(&session_id, &tc.name, &tc.arguments, req)
                                                    .await;
                                                if let Ok(o) = &res {
                                                    if let Some((_, decision)) = &o.approval_flow {
                                                        yield Ok(AgentEvent::ApprovalResult {
                                                            tool_name: tc.name.clone(),
                                                            approved: decision.approved,
                                                            approver: decision.approver.clone(),
                                                            auto_rejected: decision.auto_rejected,
                                                        });
                                                    }
                                                }
                                                res
                                            }
                                        };
                                        let outcome = match outcome_res {
                                            Ok(o) => o,
                                            Err(e) => {
                                                warn!(
                                                    %session_id,
                                                    tool = %tc.name,
                                                    error = %e,
                                                    "本地 ReAct:工具执行失败,错误作为 tool 消息回喂"
                                                );
                                                tool_calls.push(tc.name.clone());
                                                let err_tool_msg = Message::Tool {
                                                    content: serde_json::json!({
                                                        "error": e.to_string(),
                                                        "tool_name": tc.name,
                                                    })
                                                    .to_string(),
                                                    tool_name: tc.name.clone(),
                                                };
                                                messages.push(err_tool_msg.clone());
                                                if let Err(pe) = runner
                                                    .persist_message(&session_id, messages.len() - 1, err_tool_msg)
                                                    .await
                                                {
                                                    // 持久化失败也不留悬挂在途 io_request(回写后终止)
                                                    if let Some(rid) = request_id {
                                                        let pe_str = pe.to_string();
                                                        let _ = runner.evorule_client
                                                            .submit_io_response(&session_id, rid, &serde_json::json!({"error": &pe_str}), Some(pe_str.as_str()))
                                                            .await;
                                                    }
                                                    yield Err(pe);
                                                    return;
                                                }
                                                yield Ok(AgentEvent::ToolResult {
                                                    name: tc.name.clone(),
                                                    result: serde_json::json!({
                                                        "tool_name": tc.name,
                                                        "result": serde_json::json!({"error": e.to_string()}).to_string(),
                                                    }),
                                                });
                                                continue;
                                            }
                                        };
                                        // 审批事件已在上面的两阶段流程中即时 yield
                                        // (ApprovalRequired 先于决策、ApprovalResult 随决定)
                                        // 记录 tool_calls(回合级汇总,Done/审计消费)+ tool 消息持久化
                                        // (回喂轮 LLM 需要它;tool_call_id 配对由 LlmHandler
                                        // 按 tool_name FIFO 匹配最近 assistant)
                                        tool_calls.push(tc.name.clone());
                                        let tool_idx = messages.len();
                                        let tool_msg = Message::Tool {
                                            content: outcome.final_result.to_string(),
                                            tool_name: tc.name.clone(),
                                        };
                                        messages.push(tool_msg.clone());
                                        if let Err(e) = runner.persist_message(&session_id, tool_idx, tool_msg).await {
                                            // 持久化失败也不留悬挂在途 io_request(回写后终止)
                                            if let Some(rid) = request_id {
                                                let err_str = e.to_string();
                                                let _ = runner.evorule_client
                                                    .submit_io_response(&session_id, rid, &serde_json::json!({"error": &err_str}), Some(err_str.as_str()))
                                                    .await;
                                            }
                                            yield Err(e);
                                            return;
                                        }
                                        // ToolResult 事件(审批留痕内嵌)
                                        let mut result_value = serde_json::json!({
                                            "tool_name": tc.name,
                                            "result": outcome.final_result.to_string(),
                                        });
                                        if let Some(record) = outcome.approval_record {
                                            result_value["approval"] = record;
                                        }
                                        yield Ok(AgentEvent::ToolResult {
                                            name: tc.name.clone(),
                                            result: result_value,
                                        });
                                    }
                                    // 所有 tool 结果已入列 messages,回喂轮 LLM 将看到它们
                                    continue 'react;
                                }

                                // 无 tool_calls:产出最终 content,提交 io_response 收尾
                                if let Some(rid) = request_id {
                                    let tool_calls_json: serde_json::Value = serde_json::Value::Null;
                                    let is_finished = matches!(finish_reason.as_deref(), Some("stop") | Some("end_turn"));
                                    let resp = serde_json::json!({
                                        "content": full_content,
                                        "tool_calls": tool_calls_json,
                                        "is_finished": is_finished,
                                        // core_eval v0.3.1:merge 规则引用 llm_response.messages
                                        "messages": serde_json::to_value(&messages)
                                            .unwrap_or(serde_json::Value::Null),
                                    });
                                    if let Err(e) = runner.evorule_client
                                        .submit_io_response(&session_id, rid, &resp, None)
                                        .await
                                    {
                                        yield Err(AgentError::EvoruleError(e.to_string()));
                                        return;
                                    }
                                }
                                break 'react;
                                } // end 'react loop

                                // (工具执行由本地 ReAct 循环驱动,不再依赖 server 的 collect/merge)
                            }
                            "call_service" => {
                                let tool_name = params.get("tool_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let args = params.get("args").cloned().unwrap_or(Value::Null);
                                yield Ok(AgentEvent::ToolCall { name: tool_name.clone(), args: args.clone() });

                                // 审批+执行抽到两阶段 helper(与本地 ReAct 循环共用);
                                // 事件仍在此处 yield(stream! 宏限制)。Pending 时先
                                // yield ApprovalRequired 再等决策(帧须在 60s 窗口内
                                // 到达前端)。非流式路径(run)仍走 handle_call_service
                                // (内部 maybe_handle_approval,不产事件)
                                let outcome_res = match runner
                                    .execute_tool_stage(&session_id, &tool_name, &args)
                                    .await
                                {
                                    Err(e) => Err(e),
                                    Ok(ToolExecStage::Done(o)) => Ok(o),
                                    Ok(ToolExecStage::Pending(req)) => {
                                        yield Ok(AgentEvent::ApprovalRequired {
                                            tool_name: tool_name.clone(),
                                            command: req.command.clone(),
                                            risk: req.risk.clone(),
                                            alternative: req.alternative.clone(),
                                            proposal_id: req.proposal_id.clone(),
                                        });
                                        let res = runner
                                            .resolve_approval(&session_id, &tool_name, &args, req)
                                            .await;
                                        if let Ok(o) = &res {
                                            if let Some((_, decision)) = &o.approval_flow {
                                                yield Ok(AgentEvent::ApprovalResult {
                                                    tool_name: tool_name.clone(),
                                                    approved: decision.approved,
                                                    approver: decision.approver.clone(),
                                                    auto_rejected: decision.auto_rejected,
                                                });
                                            }
                                        }
                                        res
                                    }
                                };
                                let outcome = match outcome_res {
                                    Ok(o) => o,
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
                                // 审批事件已在两阶段流程中即时 yield(见 Pending 分支)
                                let final_result = outcome.final_result;

                                // 3. 记录 tool_calls + 持久化 tool 消息(同 handle_call_service)
                                tool_calls.push(tool_name.clone());
                                let tool_idx = messages.len();
                                let tool_msg = Message::Tool {
                                    content: final_result.to_string(),
                                    tool_name: tool_name.clone(),
                                };
                                messages.push(tool_msg.clone());
                                if let Err(e) = runner.persist_message(&session_id, tool_idx, tool_msg).await {
                                    // 持久化失败也不留悬挂在途 io_request(回写后终止)
                                    if let Some(rid) = request_id {
                                        let err_str = e.to_string();
                                        let _ = runner.evorule_client
                                            .submit_io_response(&session_id, rid, &serde_json::json!({"error": &err_str}), Some(err_str.as_str()))
                                            .await;
                                    }
                                    yield Err(e);
                                    return;
                                }

                                // 4. yield ToolResult + 提交 io_response(格式同 handle_call_service)
                                // 本轮发生过审批时,把审批留痕内嵌进 result(不扩 Fact 枚举)
                                let mut result_value = serde_json::json!({
                                    "tool_name": tool_name,
                                    "result": final_result.to_string(),
                                });
                                if let Some(record) = outcome.approval_record {
                                    result_value["approval"] = record;
                                }
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
                        // R2-T04 链体积观测（B3）：会话收尾时 best-effort 查审计链长告警
                        runner.check_chain_size(&session_id).await;
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
                    "Violation" => {
                        // D-01（契约档 §6.2，流式消费面补齐）：enforce 命中直接失败
                        // 上抛——不 rewind、不重试；与 workflow 链路 Violation 分支
                        // 同语义，凭 `enforce violation:` 前缀供上层判别终止不 replan。
                        let rule_index = event.payload.get("rule_index").and_then(|v| v.as_u64());
                        let reason = event
                            .payload
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(no reason)");
                        warn!(%session_id, rule_index, %reason, "enforce 拦截（流式路径）：违规指令被拒绝执行");
                        let duration = start_time.elapsed().as_millis() as u64;
                        let _ = runner.flush_messages(&session_id).await;
                        let _ = runner.sediment_session(&session_id, &messages).await;
                        yield Ok(AgentEvent::Done(AgentResult::error(
                            format!("enforce violation: rule_index={rule_index:?}, reason={reason}"),
                            step_count, duration,
                        )));
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
            // D-01 二次保险（B2）：断流可能吞掉 Violation 帧，查 evolution-signals
            // 兜底归因 enforce 命中；查询不可用时降级返回原错误（不掩盖不阻塞）。
            let closed_error = runner
                .detect_enforce_after_stream_close(&session_id, "Event stream closed")
                .await;
            yield Ok(AgentEvent::Done(AgentResult::error(
                closed_error, step_count, duration,
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
    args: &Value,
    delegate_context: &DelegateContext,
) -> Value {
    let mut merged = args.clone();
    if let Value::Object(map) = &mut merged {
        map.insert(
            "delegate_depth".to_string(),
            Value::from(delegate_context.current_depth as i64),
        );
        map.insert(
            "parent_agent".to_string(),
            Value::from(delegate_context.parent_agent_type.clone()),
        );
    }
    merged
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod runner_tests;
