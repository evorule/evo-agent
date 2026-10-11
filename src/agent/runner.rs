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
    parse_approval_request, strip_approved_flag, ApprovalCallback, ApprovalDecision,
    ApprovalRequest,
};
use crate::agent::callback::CallbackChain;
use crate::agent::context_window::{ContextWindowManager, TrimStrategy};
use crate::agent::journal::REWIND_BUDGET_LIMIT;

/// F-302:组装策略版本——组装行为(分层方式/预算口径/记忆注入)的协议级版本标识。
/// 版本变更=组装行为变更=历史重建需切版本（RL-B3 落地）。
/// v2:组装路径由配方数据驱动(AssemblyExecutor 取代硬编码分层),配方三元落账链上。
/// v3:S2b_knowledge 治理知识契约槽位加入(定义声明 knowledge_datasets 驱动,
/// 组装期实时拉取渲染;新增槽位来源=执行器升级=major)。
pub const ASSEMBLY_PROTOCOL_VERSION: &str = "assembly-v3";

use crate::agent::acceptance::{apply_acceptance_gate, GateOutcome};
use crate::agent::definition::{AgentDefinition, OutputFormat};
use crate::agent::delegate::DelegateContext;
use crate::agent::memory::{MemoryManager, MessagePersistMode, MessageRecord};
use crate::agent::memory_event::extraction::{EventExtractor, ExtractionConfig};
use crate::agent::memory_event::MemoryEventStore;
use crate::agent::output_validator::OutputValidator;
use crate::agent::sediment;
use crate::agent::stagnation::{
    StagnationVerdict, STAGNATION_EXHAUSTED_MARK, STAGNATION_WARNING_MARK,
};
use crate::agent::summarizer::{ContextSummarizer, SummarizeOutcome};
use crate::agent::translator::{LlmResponse, Message};
use crate::api::api_core::ApiError;
use crate::api::evorule_client::EvoruleApiClient;
use crate::api::metrics::{SessionActiveGuard, SharedMetrics};
use crate::io_handler::IoHandler;
use crate::io_handlers::tool_handler::ToolFunction;
use crate::io_handlers::{LlmHandler, StreamChunk, ToolHandler};

/// TODO: doc
pub const DEFAULT_MAX_DELEGATE_DEPTH: usize = 3;

/// R2-T04 链体积告警阈值（收官遗留 B3）：单会话 facts_log 审计链长达到该值
/// 时 warn 告警（观测口径，不拦截）。量级参照：单节点 agent 会话典型链长
/// 数十至数百条；10,000 条 = 超长会话（多轮重试/长循环）的异常增长信号。
const CHAIN_SIZE_WARN_ENTRIES: u64 = 10_000;

/// 输出门禁（server 侧 io_guard）拒绝收尾后的纠偏重试上限（G11 同款 max_retries=2）
const IO_GUARD_MAX_RETRIES: u32 = 2;

/// 输出门禁纠偏回喂文本:server enforce 模式拒绝收尾时追加为 user 消息,
/// LLM 下轮要么先调用相应工具获取真实结果,要么如实说明未执行该动作
const IO_GUARD_CORRECTION_PROMPT: &str = "系统输出门禁反馈：上一条收尾输出中包含未实际执行的动作描述（如声称已执行命令、已写入文件或已提交代码，但本会话未调用对应工具）。请修正后重新收尾：要么先调用相应工具获取真实结果，要么如实说明当前未执行该动作、仅作说明性描述。";

/// tool_result 回喂 LLM 前的默认字符上限已配方化
/// (AssemblyRecipe::default() budget.tool_result_max_chars = 48000,约 12k
/// tokens,ASCII 口径)
///
/// 单个超大工具输出(整页网页、长日志等)会挤占上下文预算,连带把任务锚点
/// 从尾部保留区挤出。截断仅作用于回喂 LLM 的 messages 入列值;审计链
/// persist_message / ToolResult 事件保留原始全文(事实记录不动,与 trim
/// 不改写原 messages 的哲学一致)。生效值见 `AgentRunner::tool_result_max_chars`。
/// 截断过长的 tool_result:保留头尾各半,中间插入截断标注;未超限原样归还
///
/// 元层先行批:上限由配方 budget.tool_result_max_chars 声明(默认 48000 =
/// 原 TOOL_RESULT_MAX_CHARS 常量,等价迁移)
fn truncate_tool_result(raw: String, max_chars: usize) -> String {
    // 字节长度快路径(字符数 ≤ 字节数,未超字节限必然未超字符限)
    if raw.len() <= max_chars {
        return raw;
    }
    let total_chars = raw.chars().count();
    if total_chars <= max_chars {
        return raw;
    }
    let truncated = total_chars - max_chars;
    let marker = format!("\n...[truncated {} chars]...\n", truncated);
    let keep = max_chars.saturating_sub(marker.chars().count());
    let head = keep / 2;
    let tail = keep - head;
    let head_str: String = raw.chars().take(head).collect();
    let tail_str: String = raw.chars().skip(total_chars - tail).collect();
    format!("{}{}{}", head_str, marker, tail_str)
}

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
    /// 元层先行批:组装配方(None = 内置默认配方 = 现状行为;from_definition
    /// 会把 memory_budget_ratio 合入默认配方的 S3 槽位,保持既有配置语义)
    pub assembly: Option<crate::agent::assembly::AssemblyRecipe>,
    /// B2:skills 生效清单(None = 无技能,manifest 槽位静默跳过 = 现状行为;
    /// serve/CLI 层 wire_skills 从 def.skills 解析注入)
    pub skills: Option<Vec<crate::agent::definition::SkillManifestEntry>>,
    /// F-101:身份资产段(None = S1 槽仅基底块 = 现状行为;声明后 S1 槽内
    /// 拼接序 = 基底块→身份段)
    pub identity_segment: Option<String>,
    /// F-101 北极星锚(None = 不注入;声明后 S1 槽内拼接序 = 基底块→
    /// 身份段→北极星锚——目标对焦供锚面)
    pub north_star: Option<String>,
    /// 交接底座包(None = 不注入;声明后 S3 槽内渲染 "## Handoff Base"
    /// 结构化块——确定性底座,与滚动摘要语义面分层配对)
    pub handoff: Option<crate::agent::definition::HandoffPackage>,
    /// 治理门禁段(S2 槽位内容物;serve 三路径构造期算好传入,CLI=None;
    /// L2 约束前馈/进化信号感知/规范入口索引 合并段,v2 序=权威紧跟 S1)
    pub governance_segment: Option<String>,
    /// 治理知识数据集声明(声明面数据,definition 直拷;None/空 = 不注入 =
    /// 既有定义零影响。拉取渲染在 runner 组装期做——build_knowledge_segment
    /// fail-soft,产物进 S2b_knowledge 独立槽位,任何 runner 路径同口径)
    pub knowledge_datasets: Option<Vec<String>>,
    /// I2 词表声明(数据化;None=机制内建 v2 双语表——context_inspector)
    pub i2_lexicon: Option<crate::agent::context_inspector::I2Lexicon>,
    /// 全局时限预算秒(H3 看门狗;None=不启用——既有定义零影响)
    pub wall_clock_budget_secs: Option<u64>,
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
            assembly: None,
            skills: None,
            identity_segment: None,
            north_star: None,
            handoff: None,
            governance_segment: None,
            knowledge_datasets: None,
            i2_lexicon: None,
            wall_clock_budget_secs: None,
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
        /// 宪法审查结论(过审凭据 "pass@<规则集版本>";违反定义在加载期
        /// fail-fast 不会到达会话建立,故运行期恒为过审值;None=旧版兼容)
        constitution: Option<String>,
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
    if tool_name != "file_read" && tool_name != "file_write" {
        return None;
    }
    let raw = args.get("path").and_then(|v| v.as_str())?;
    path_scope(raw, boundary)
}

/// P2:path 参数越界快筛(resolve_target_scope/resolve_tool_intent 共用)
///
/// 判据与 M5-c resolve_target_scope 逐字同源:绝对路径/含 `..` 组件/join 后
/// 越出 sandbox_root 即 out_of_sandbox;boundary 未声明 → None(不判越界,
/// handler 内联检查保留为最终防线)。纯字符串/路径运算不触 fs。
fn path_scope(
    raw: &str,
    boundary: Option<&crate::agent::definition::CapabilityBoundary>,
) -> Option<&'static str> {
    let boundary = boundary?;
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

// ----- P2 治理级工具事前意图裁决(裁决泛化) -----

/// P2:治理级工具分级表(裁决面单一事实源——manifest 派生)
///
/// 一表化(工具面统一架构 §3.1):`adjudication_class != Standard` 即 P2
/// 裁决通道——Sentineled(file_create/file_move/file_delete,正本 enforce
/// 已在场)+ Sensitive(git_stage/git_commit + 规则治理写族 19)= 24 工具,
/// 与原 GOVERNANCE_ADJUDICATION_TOOLS 手工表派生等价(快照测试锁:
/// tool_manifest::tests::test_p2_adjudicated_set_matches_governance_snapshot;
/// 行为防漂移守卫:下方 p2_adjudication_table_matches_design)。
/// 划定口径(设计档 §3.1,2026-09-28 用户批 D1=A 全量 candidate 族):
/// file 破坏族+git 写族+rule_* 治理写族+publish 写面+bundle 面;纯读面
/// (file_read/file_list/search_files/grep_files/git_status/git_diff/
/// git_log/publish_list/publish_queue_get)排除;shell_exec 高频不上裁决
/// (P1 事后 shell_guard 已覆盖其治理语义);file_read/file_write 维持
/// M5-c 既有 R1 通道([`resolve_target_scope`])不变。
///
/// 历史:原手工 const 表(GOVERNANCE_ADJUDICATION_TOOLS 24 项)已由
/// manifest 静态表取代——手工表与注册面两张皮的漂移从机制上消灭。
///
/// D6 终态(2026-10-06):本函数保持**纯静态查询**语义(P2 管道判定已迁
/// [`crate::agent::tool_manifest::is_p2_adjudicated_runtime`]——静态优先
/// 防降级 ∪ 动态按 runtime manifest);快照守卫测试的消费面不变。
pub fn is_governance_adjudication_tool(tool_name: &str) -> bool {
    crate::agent::tool_manifest::lookup_static(tool_name)
        .map(|m| m.is_p2_adjudicated())
        .unwrap_or(false)
}

/// P2:解析治理级工具调用的意图规范字段(纯函数,宪法 §七「规范字段生产」)
///
/// P2 派生判定 = [`crate::agent::tool_manifest::is_p2_adjudicated_runtime`]
/// (D6 终态:静态优先防降级 ∪ 动态按 runtime manifest)——`runtime` 传管道
/// 阶段①查得的 manifest(生产消费点 pipeline P2 段);`None` 退化为纯静态
/// 语义(既有测试与静态查询面)。分级表未命中 → None(零开销路径)。file 族
/// (file_create/file_move/file_delete)附加 target_scope([`path_scope`])
/// 快筛;file_move 对 path+target_dir 双字段判定,任一越界即
/// out_of_sandbox,其余字段全 None 时无 scope 字段);其余治理工具无 scope
/// 字段(拦截条件由规则种子自行定义,首批=放行留痕)。args 经脱敏+截断
/// (与 P1 轨迹同纪律:SENSITIVE_KEYS redact+体积上限)。
pub fn resolve_tool_intent(
    tool_name: &str,
    args: &Value,
    boundary: Option<&crate::agent::definition::CapabilityBoundary>,
    runtime: Option<&crate::agent::tool_manifest::ToolManifest>,
) -> Option<Value> {
    if !crate::agent::tool_manifest::is_p2_adjudicated_runtime(tool_name, runtime) {
        return None;
    }
    let mut intent = serde_json::json!({ "tool_name": tool_name });
    let path_fields: &[&str] = match tool_name {
        // file_write：D1 兑现——升 P2 哨兵后意图信号经 resolve_tool_intent
        // 采集（与 M5-c R1 的 pending_target_scope 通道并存互不干扰）
        "file_create" | "file_delete" | "file_write" => &["path"],
        "file_move" => &["path", "target_dir"],
        _ => &[],
    };
    let scopes: Vec<Option<&'static str>> = path_fields
        .iter()
        .filter_map(|k| args.get(*k).and_then(|v| v.as_str()))
        .map(|raw| path_scope(raw, boundary))
        .collect();
    if scopes.contains(&Some("out_of_sandbox")) {
        intent["target_scope"] = Value::from("out_of_sandbox");
    } else if let Some(s) = scopes.into_iter().flatten().next() {
        intent["target_scope"] = Value::from(s);
    }
    intent["args"] = crate::agent::tool_trace::sanitize_args(args);
    Some(intent)
}

/// P2:治理级工具意图信号指令形态(纯函数)
///
/// 中性判据:`set meta_tool.pending_tool_intent = <tool_intent.v1 契约 value>`。
/// value 经 [`crate::agent::tool_intent::ToolIntentV1`] 从解析输出派生:
/// tool_name/target_scope/args 透传(与历史形态前缀兼容——规则层 enforce
/// 匹配键不变),增补 args_digest(规范化摘要)/session_ref(主会话审计关联,
/// 由管道调用方上下文补齐)/schema_ver(契约版本号)。与 M5-c 的
/// `pending_target_scope`(R1 通道)并存互不干扰;宪法 set 规则纯
/// 透传(rules_dir enforce 对裁决会话 set 指令可达——先占裁决只卡 call_external),
/// 被拦=引擎丢弃指令不推进 version,放行=内建 set 落状态。
pub fn tool_intent_signal(intent: &Value) -> Value {
    let contract = crate::agent::tool_intent::ToolIntentV1::from_resolved(intent);
    serde_json::json!({
        "type": "set",
        "params": {
            "attr": "meta_tool.pending_tool_intent",
            "operation": "set",
            "value": contract
        }
    })
}

/// M5-c:意图裁决感知参数——提交后轮询会话 version 的窗口
///
/// command 端点=异步队列语义(HTTP success 不代表未被 enforce 拦截),
/// 拦截的引擎语义=丢弃指令不推进 version;ReAct 循环串行提交无竞态。
/// 引擎处理为毫秒级,20×50ms=1s 窗口上限远大于正常裁决时延。
const INTENT_VERDICT_POLLS: usize = 20;
const INTENT_VERDICT_INTERVAL_MS: u64 = 50;

/// 查询会话当前服务端 Fact 版本(evorule-server state.version 权威口径)。
/// 调用方:每轮 Done 回执权威校正(ws_handler);runner 内部 M5-c 裁决判别。
pub(crate) async fn session_version(
    client: &EvoruleApiClient,
    session_id: &str,
) -> Result<u64, String> {
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

/// H1(自主交接设计):窗口余量信号行构造
///
/// 使用率>70% 才出信号,以下静默(固定每轮注入=固定 token 开销+注意力税,
/// v1.2 优化口径,Q-SZ10 同源);纯聚焦信号不强制动作——护栏面(链深度/
/// 预算硬顶)不在本行。预算/窗口为零(未配置窗口管理)不出信号。
fn context_signal_line(used: usize, budget: usize, window: usize, step: usize) -> Option<String> {
    if budget == 0 || window == 0 {
        return None;
    }
    let pct = (used * 100) / budget;
    if pct <= 70 {
        return None;
    }
    Some(format!(
        "\n\n[context] 窗口使用 {pct}%（输入预算 {}k/{}k），回合 {step}",
        budget / 1000,
        window / 1000,
    ))
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
    /// R3-b/G-7:已落链的格式指令(去重——同指令不重复落链)
    landed_format_instruction: std::sync::Mutex<Option<String>>,
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
    /// A2-2(F-611 写件):memory_propose 会话锚(None=写件未注册)
    ///
    /// 注册期构造空锚并注入 MemoryProposer;两 run 路径 create_session 后
    /// 绑定——Shared 域写=写当前会话 payload,运行期才可绑定(与
    /// MemoryManager/MemoryEventStore 的 session_id 同步同型)。
    propose_anchor: Option<std::sync::Arc<std::sync::RwLock<Option<String>>>>,
    /// C3:总上下文窗口 token 数（由 `def.context_window_tokens` 构造，默认 8192）
    ///
    /// 作为记忆区预算基准传入 `AssemblyExecutor::assemble`（配方的
    /// `base: total_window` 声明所指的总窗口）。
    max_context_tokens: usize,
    /// 元层先行批:组装执行器(配方驱动的单一组装出口;run/流式两组装点共用)
    assembly: crate::agent::assembly::AssemblyExecutor,
    /// 工具输出回喂字符上限(配方 budget.tool_result_max_chars;默认 48000)
    tool_result_max_chars: usize,
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
    /// P1:工具调用轨迹采集器(会话内累积,io_response 收尾后随
    /// tool_trace 指令批量提交进引擎审计链;std Mutex:临界区无 await)
    tool_traces: std::sync::Arc<std::sync::Mutex<crate::agent::tool_trace::ToolTraceCollector>>,
    /// B21 PR-1:journal 目录(serve 注入 `<workdir>/data/sessions`;None=不启用
    /// ——CLI/子代理路径零改动)。run_streaming_inner 创建会话时在此目录建
    /// `{session_id}.jsonl` 事件流(会话唯一真相源,见 crate::agent::journal)
    journal_dir: Option<std::path::PathBuf>,
    /// B21 D3:主动压缩策略(serve 面从 `longSession.compaction.*` 工作台设置
    /// 构造注入;None=不启用——CLI 路径与未注入面行为零变化,被动 trim 独任)
    compaction_policy: Option<CompactionPolicy>,
    /// 验收判据自检命令(可选;长程/TB 模式)。task_done 提交前
    /// runner 强制执行,exit 0=通过放行,非 0=门禁拒绝(不存在 done 退出路径)。
    /// None=不拦截(非长程运行零影响)。由 from_definition 从 def 穿线。
    acceptance_command: Option<String>,
    /// 进展停滞检测器(F2 空转克星)。跨轮持续观察
    /// (工具名,参数,结果)三元组,连续重复→警告→按 H2 阻塞收尾
    stagnation: crate::agent::stagnation::StagnationDetector,
    /// 管道入口类标记(delegate 统一装配批;默认 React 主路径)
    ///
    /// CallerContext.entry 的 runner 侧单一来源:execute_tool_call_gated /
    /// run_pipeline 据此落账——delegate 子代理的工具调用与主路径在账面可分
    /// (子代理聚焦决策落账的观测面)。ParallelPreflight 并行实例在调用点
    /// 显式传入,不经此字段。
    pipeline_entry: crate::agent::pipeline::PipelineEntry,
    /// 装配面聚焦(delegate 统一装配批;delegate 子代理装配路径置 true)
    ///
    /// true:主路径聚焦快照=注册面本身(装配产物)——LLM 契约面(messages
    /// 侧 tools payload,同为注册面)与管道②聚焦允许面同源(设计档 §4.4:
    /// 子代理工具面=manifest 过滤后的聚焦快照,装配即过滤),静态表不再自动
    /// 进入允许面。false:注册面 ∪ 静态表(主路径行为等价口径不变,B2 断言
    /// 测试盯守)。
    assembly_scope_focus: bool,
    /// 语义精判会话内缓存(两级通路第二级;键=候选六元组 digest,值=裁决
    /// 结论——每候选每会话至多一次 sidecar 调用,重复候选零成本复用)
    i2_verdict_cache: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, crate::agent::context_inspector::I2Verdict>,
        >,
    >,
    /// 语义精判开关(true=默认:候选触发 sidecar 裁决;false=回退纯字面级,
    /// 逐字节兼容旧行为)
    semantic_i2_enabled: bool,
    /// 双通道笔记强制回喂 R-2 触发闩(停滞 Warning/Exhausted、工具错误、
    /// 审批拒绝置位;下一轮 recall 消费——failure 笔记回喂进 S3,消费后复位)
    pending_note_feed: bool,
    /// 写前置查询会话级路径去重(同路径重复写不再重复建议;R-4)
    advised_paths: std::sync::Mutex<std::collections::HashSet<String>>,
    /// 摘要保真对照(规格修正批交付物 B):当前会话 journal 写者(流式路径
    /// 注入;CLI run 纯路径无 journal=只 warn 不落账)。G10 摘要替换时
    /// 自动对照落 summary_fidelity_scan 事件。
    active_journal: Option<std::sync::Arc<crate::agent::journal::JournalWriter>>,
    /// 查账工具族（PR-11a）：会话 journal 上下文共享槽——wire_accounting
    /// 重绑的查账工具实例经此感知当前会话（journal_dir 未启用=槽保持
    /// None，query_journal 如实报错）
    accounting_journal:
        std::sync::Arc<std::sync::RwLock<Option<crate::builtin_tools::accounting::JournalCtx>>>,
    /// 自主交接 PR-H3:会话链共享运行态(None=根会话尚未发起过 spawn;首个
    /// spawn 接线点惰性创建,经组件快照沿链传递同一 Arc——链 token 预算/
    /// 停链标记/spawn 同签名观察全链共享)
    chain: Option<std::sync::Arc<crate::agent::session_spawn_tool::ChainRuntimeState>>,
    /// 自主交接 PR-H3:链熔断观察窗(仅派生子会话经组件快照携带;轮守卫
    /// 创建时 move 注入 TurnEndGuard,根会话恒 None 不观察)
    chain_watch: Option<crate::agent::session_spawn_tool::ChainWatch>,
}

/// 管道阶段⑦执行器：runner 的 call_service 通路
///
/// step_timeout 在 execute_external 内包裹（现状语义保持——管道不另设超时，
/// TOOL_TIMEOUT 60s 在 ToolHandler 内不变）；call_params 形态与原
/// execute_tool_call 逐字一致（{"tool_name","args"}）。
impl crate::agent::pipeline::PipelineExecutor for AgentRunner {
    fn execute_tool(
        &self,
        tool_name: &str,
        args: &Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, AgentError>> + Send + '_>>
    {
        let mut call_params = serde_json::Map::new();
        call_params.insert("tool_name".to_string(), Value::from(tool_name.to_string()));
        call_params.insert("args".to_string(), args.clone());
        Box::pin(async move {
            self.execute_external("call_service", &Value::Object(call_params))
                .await
        })
    }
}

/// 悬挂工具处置动作(L1 分类路由)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DanglingAction {
    /// 幂等读类:经管道重执行,回喂真实结果(无损恢复)
    Reexecute,
    /// 幂等写/非幂等:不盲重执行,回喂崩溃观察(LLM 自行决定重试)
    CrashObservation,
}

/// 悬挂处置报告(恢复标记 rebuilt 清单的数据源)
#[derive(Debug, Clone, Copy, Default)]
struct DanglingRepairReport {
    dangling: usize,
    reexecuted: usize,
    observed: usize,
}

/// 会话流式入口的恢复模式(react 面恢复语义的载体)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryMode {
    /// 全新会话(既有 run_streaming 语义)
    Fresh,
    /// 多轮续跑(既有 run_continuation 语义;要求新 user 输入)
    Continuation,
    /// 崩溃恢复(resume_crashed;无新输入,悬挂工具处置后续完当前 turn)
    CrashResume,
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
            landed_format_instruction: std::sync::Mutex::new(None),
            output_format_retries: 0,
            approval_callback: None,
            parallel_tool_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            metrics: None,
            event_callbacks: Arc::new(CallbackChain::new()),
            memory_event_store: None,
            extractor: None,
            sediment_config: sediment::SedimentConfig::default(),
            propose_anchor: None,
            max_context_tokens: 8192,
            assembly: crate::agent::assembly::AssemblyExecutor::default_executor(),
            tool_result_max_chars: 48_000,
            token_counter: None,
            adjudicator: tokio::sync::Mutex::new(
                crate::agent::adjudicator::AdjudicationChannel::new(
                    adjudicator_client,
                    &agent_type,
                ),
            ),
            tool_traces: Arc::new(std::sync::Mutex::new(
                crate::agent::tool_trace::ToolTraceCollector::default(),
            )),
            journal_dir: None,
            compaction_policy: None,
            acceptance_command: None,
            stagnation: crate::agent::stagnation::StagnationDetector::new(),
            pipeline_entry: crate::agent::pipeline::PipelineEntry::React,
            assembly_scope_focus: false,
            i2_verdict_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            semantic_i2_enabled: true,
            pending_note_feed: false,
            advised_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
            active_journal: None,
            accounting_journal: std::sync::Arc::new(std::sync::RwLock::new(None)),
            chain: None,
            chain_watch: None,
        }
    }

    /// 执行桥后端上下文注入轨迹采集器:run 请求携带容器名(docker-exec 后端)时,
    /// shell_exec 轨迹 danger_hits 仅保留违禁域旗标(合规红线双后端
    /// enforce 维持),host 视角 program/rm 旗标分流至 `program_hits` 留链备裁
    /// (分流≠删检,容器内命令策略由规则面随动);不注入=宿主后端成形零变化
    pub fn with_trace_exec_backend(self, container: &str) -> Self {
        if let Ok(mut tt) = self.tool_traces.lock() {
            tt.set_exec_backend(Some(container));
        }
        self
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

    /// 当前 evorule 会话 id(读访问;`create_session` 发生在 run/run_streaming
    /// 内部,委托方在子 runner 流终止后经此读取子会话 id)
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
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
        mut tool_handler: ToolHandler,
        llm_handler: Option<LlmHandler>,
    ) -> Result<Self, AgentError> {
        // 1. 配置
        // F-201:配方绑定审查(R1/R2)。R3 哨兵检查不在此重复——serve 面 M1/L2/
        // 进化信号机制注入先于本构造修改 system_prompt,注入段合法含机制分区,
        // R3 由文件入口 load_from_dir 门卫 4 单独把关(批次 D E2E 实测修订;
        // 违反=拒建,错误明示规则名)
        def.validate_assembly_binding()
            .map_err(|e| AgentError::Internal(e.to_string()))?;
        let config = def.to_agent_config();

        // B2:skills 声明接线(read_skill 注册 + manifest 槽位源;声明时刻=
        // 人工把关,路径由系统解析,LLM 无法用 read_skill 读任意文件)。
        // C 形态后 CLI 直启路径仍为纯声明面(不扫目录——serve 会话创建路径
        // 才做两源合并,见 skill_api::merged_manifest_for_session)
        let declared_skills =
            crate::api::serve_tools::resolve_declared_skills(&def).map_err(AgentError::Internal)?;
        let resolved_skills =
            crate::api::serve_tools::wire_skills(&mut tool_handler, declared_skills)
                .map_err(AgentError::Internal)?;

        // 2. 校验:def.tools 全部已在 tool_handler 注册
        // (早失败:用户能在跑之前就发现配错,而不是跑一半才挂)
        // 例外:自省记忆工具在记忆装配后才注册(阶段 3 F-611),此处按暴露
        // 条件预放行;装配后仍未注册=配置矛盾,由注册步显式报错(fail-visible)
        let introspect_allowed = crate::agent::memory_tool::exposed_tools_from_definition(&def);
        for tool_name in &config.tool_names {
            if !tool_handler.has_tool(tool_name) && !introspect_allowed.contains(tool_name) {
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

        // step_timeout ≥ LLM 最坏预算断言（预算失配会让 step_timeout 先炸,
        // 掩盖 LLM 端点慢的真因——错误归因指向编排层）。
        // 默认 warn（既有配置面广,60~300s 常见）;EVORULE_STEP_BUDGET_ENFORCE=1
        // 时硬 fail（生产/竞赛口径,防 step_timeout 先炸掩盖 LLM 慢的误归因）。
        // 预算公式唯一真相源=LlmHandler::step_budget_mismatch（同实例实算）。
        if let Err(msg) = llm.step_budget_mismatch(config.step_timeout.as_secs()) {
            if std::env::var("EVORULE_STEP_BUDGET_ENFORCE").as_deref() == Ok("1") {
                return Err(AgentError::Internal(msg));
            }
            tracing::warn!(msg = %msg, "step budget mismatch (warn; enforce via EVORULE_STEP_BUDGET_ENFORCE=1)");
        }

        // 5. 组装
        let mut runner = Self::new(config, client)
            .with_llm_handler(llm)
            .with_tool_handler(tool_handler)
            .with_message_persist_mode(persist_mode)
            .with_skills(resolved_skills);
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
            let mut config = ExtractionConfig {
                extraction_model: def.memory.extraction_model.clone(),
                ..Default::default()
            };
            // 触发词配置接线——配置面宣称「可被 agent.json 覆盖」自此成立
            // （None=沿用内置默认表,既有 agent 配置零影响）
            if let Some(kws) = &def.memory.extraction_keywords {
                config.keywords = kws.clone();
            }
            if let Some(phrases) = &def.memory.extraction_explicit_phrases {
                config.explicit_phrases = phrases.clone();
            }
            let extractor = EventExtractor::new(runner.llm_handler.clone(), config);
            runner.extractor = Some(extractor);
        }
        // 阶段 1(F-618):LexStore 检索缓存(definition 配 lex_store 路径时启用;
        // open 失败 warn 降级全量路径,I14)
        // 阶段 2(F-610):MemoryRecipe 策略规则集(内嵌 JSON;解析失败 warn 降级
        // 词法 legacy,零影响)
        if let Some(rj) = &def.memory.recipe {
            match serde_json::from_value::<crate::agent::recipe::MemoryRecipe>(rj.clone()) {
                Ok(recipe) => {
                    let rollup = recipe.lifecycle.rollup_threshold;
                    let journal_digest = recipe.sources.journal_digest;
                    // 修复:failure_drafts 此前误接 journal_digest（复制粘贴错,
                    // 草稿面从未被 Recipe 独立声明控制过）
                    let failure_drafts = recipe.sources.failure_drafts;
                    let material_harvest = recipe.sources.materials;
                    let recipe_for_mem = recipe.clone();
                    if let Some(mem) = runner.memory.as_mut() {
                        mem.set_recipe(recipe_for_mem);
                    }
                    // Recipe.rollup_threshold 覆盖同名 def 配置（策略数据化）
                    runner.sediment_config.summary_rollup_threshold = rollup;
                    // Recipe.sources 三源穿线（跨源注册规格策略面；
                    // 缺省关=既有 agent 零影响）
                    runner.sediment_config.enable_journal_digest = journal_digest;
                    runner.sediment_config.enable_failure_drafts = failure_drafts;
                    runner.sediment_config.enable_material_harvest = material_harvest;
                    // 战役 B：任务域事件触发词穿线（Recipe sources.task_event_keywords
                    // → extractor config.task_keywords；缺省空=既有 agent 零影响。
                    // extract_from_conversation 内部复检 detect_trigger，
                    // 故必须在 extractor config 层注入而非仅外层放行）
                    if !recipe.sources.task_event_keywords.is_empty() {
                        if let Some(extractor) = runner.extractor.as_mut() {
                            extractor.set_task_keywords(recipe.sources.task_event_keywords.clone());
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "MemoryRecipe 解析失败——召回走词法 legacy 路径");
                }
            }
        }
        if let Some(db) = &def.memory.lex_store {
            match crate::agent::lexstore::LexStore::open(std::path::Path::new(db)) {
                Ok(store) => {
                    if let Some(mem) = runner.memory.as_mut() {
                        mem.set_lex_store(std::sync::Arc::new(store));
                    }
                }
                Err(e) => {
                    tracing::warn!(db = %db, error = %e, "LexStore open failed——召回走全量拉取降级路径");
                }
            }
        }
        // 阶段 3(F-611)读件注册前移除——A2-2 起注册挪至 sediment_config 赋值
        // 之后(写件 MemoryProposer 的 namespace 取自 sediment_config)
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
            // 知识候选提取开关（默认 true；提取走 auditor sidecar，
            // 无审计通路时 sediment 内部自动跳过）
            enable_knowledge_extraction: def.memory.enable_knowledge_extraction,
            min_messages_for_extraction: 4,
            // 跨源批 C:journal 摘要投影(缺省关;Recipe sources 穿线于上)
            enable_journal_digest: false,
            // 阶段 5 F-613:知识候选巩固(缺省开,跟随最小版先例)
            enable_consolidation: true,
            // 阶段 5 NB-2:双通道笔记事件驱动草稿(缺省关;Recipe sources 穿线于上)
            enable_failure_drafts: false,
            enable_material_harvest: false,
            // S-5 收尾批+运营接线批:知识候选自动出口(dataset 配置即启用,
            // 缺省 None=off;SedimentConfig 内闸不动=双保险;门限缺省 0.7)
            enable_knowledge_propose: def.memory.knowledge_propose_dataset.is_some(),
            knowledge_propose_dataset: def.memory.knowledge_propose_dataset.clone(),
            knowledge_propose_min_confidence: def
                .memory
                .knowledge_propose_min_confidence
                .unwrap_or(0.7),
        };
        // 阶段 3(F-611)+A2-2:自省记忆工具注册(声明面已在 step 2 按暴露条件
        // 预放行;此处声明了而条件不满足=配置矛盾,早失败)。置于 sediment_config
        // 赋值之后:写件(MemoryProposer)的 namespace 取自 sediment_config
        runner.register_memory_introspection_tools()?;
        // 判据自检回路——acceptance_command 从 definition 穿线
        // (task_done 提交前 runner 强制执行验收命令,判据不过不存在 done 退出路径)
        runner.acceptance_command = def.acceptance_command.clone();
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
        // 元层先行批:组装执行器构造。definition 未声明 assembly 时用内置
        // 默认配方,并把 memory_budget_ratio 合入 S3 槽位(保持既有配置语义:
        // 默认 0.25 与配方默认等价,显式值透传);已声明配方的 ratio 以配方
        // 为唯一权威(单一真相源,memory.memory_budget_ratio 不再生效)。
        let recipe = def.assembly.clone().unwrap_or_else(|| {
            let mut r = crate::agent::assembly::AssemblyRecipe::default();
            for slot in &mut r.slots {
                if slot.id == "S3_memory" {
                    if let Some(b) = slot.budget.as_mut() {
                        b.ratio = def.memory.memory_budget_ratio;
                    }
                }
            }
            r
        });
        let executor = crate::agent::assembly::AssemblyExecutor::new(recipe);
        runner.assembly = executor;
        // G2/R11/C3:上下文窗口接线(默认 8192,reserve 1/4;显式声明=单一
        // 事实源)——PR-H4 验收批抽取为 wire_definition_context_window 单一
        // 实现,WS 流式构造路径(construct_runner)复用同源逻辑
        runner.wire_definition_context_window(def.context_window_tokens);
        runner.tool_result_max_chars = runner.assembly.tool_result_max_chars();

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
        self.tool_handler.register_static("delegate", delegate_tool);
        self.delegate_context = Some(ctx);
        self
    }

    /// delegate 子代理装配路径标记（delegate() 构建子 runner 时接线）
    ///
    /// 管道账面入口类记 [`crate::agent::pipeline::PipelineEntry::Delegate`]——
    /// 子代理工具调用与主路径（React）在账面 caller.entry 可分（子代理聚焦
    /// 决策落账的观测面）。
    pub(crate) fn with_delegate_pipeline_entry(mut self) -> Self {
        self.pipeline_entry = crate::agent::pipeline::PipelineEntry::Delegate;
        self
    }

    /// 委托子会话锚落账:drain spawn 账逐条落 journal 事件（delegate 工具
    /// 结果写账前调用,事件序 tool_invoked → delegate_spawned → tool_result;
    /// 无账/无记录均静默跳过——无 journal 时同样 drain 防跨调用残留）
    fn flush_delegate_spawns(
        &self,
        journal: Option<&std::sync::Arc<crate::agent::journal::JournalWriter>>,
    ) {
        let Some(ctx) = &self.delegate_context else {
            return;
        };
        let records = ctx.drain_spawn_records();
        if records.is_empty() {
            return;
        }
        let Some(j) = journal else {
            return;
        };
        for r in records {
            if let Err(e) =
                j.delegate_spawned(&r.child_session_id, &r.agent_type, r.depth, &r.task_digest)
            {
                warn!(error = %e, "delegate_spawned journal failed");
            }
        }
    }

    /// 召回配额注入（delegate 子代理记忆声明下放用:与主装配同源取
    /// definition.memory 召回配额;同 crate 内部装配面,非公开构造 API）
    pub(crate) fn with_recall_quotas(mut self, max_summaries: usize, max_events: usize) -> Self {
        self.sediment_config.max_session_summaries = max_summaries;
        self.sediment_config.max_injected_events = max_events;
        self
    }

    /// 语义精判开关（I2 两级通路第二级;true=默认开——候选触发 sidecar
    /// 裁决+会话内缓存;false=回退纯字面级,逐字节兼容旧行为）
    pub fn with_semantic_i2(mut self, enabled: bool) -> Self {
        self.semantic_i2_enabled = enabled;
        self
    }

    /// Recipe 热重载（LM-2 可热重载属性;运行体重解析+指纹审计）:
    /// 重解析→memory.set_recipe→sediment 门控随新 Recipe 刷新;新指纹随
    /// 下一轮 effective_params 落链（回放可锚定）。
    pub fn reload_memory_recipe(&mut self, recipe_json: &str) -> Result<(String, String), String> {
        let (version, hash) = self
            .memory
            .as_mut()
            .ok_or_else(|| "memory not enabled".to_string())?
            .reload_recipe(recipe_json)?;
        // sediment 门控随新 Recipe 刷新(三源穿线与 from_definition 同口径)
        if let Some(mem) = self.memory.as_ref() {
            if let Some(recipe) = mem.current_recipe() {
                self.sediment_config.enable_journal_digest = recipe.sources.journal_digest;
                self.sediment_config.enable_failure_drafts = recipe.sources.failure_drafts;
                self.sediment_config.enable_material_harvest = recipe.sources.materials;
                self.sediment_config.summary_rollup_threshold = recipe.lifecycle.rollup_threshold;
            }
        }
        info!(version = %version, fingerprint = %hash, "memory recipe reloaded on runner");
        Ok((version, hash))
    }

    /// Recipe 资产提案（LM-2 治理层接入:经治理写通路把当前 Recipe 作为
    /// KnowledgeEntry 入 evorule-rule 数据集——可版本/可审批/可包交换;
    /// offline/无 Recipe/无 dataset 如实报错,不静默）
    pub async fn propose_memory_recipe_asset(
        &self,
        dataset_id: &str,
        session_id: &str,
    ) -> Result<Value, String> {
        let payload = self
            .memory
            .as_ref()
            .and_then(|m| m.recipe_asset_payload())
            .ok_or_else(|| "memory recipe not configured".to_string())?;
        let (version, fingerprint) = self
            .memory
            .as_ref()
            .and_then(|m| m.recipe_fingerprint())
            .ok_or_else(|| "memory recipe fingerprint unavailable".to_string())?;
        let cause =
            format!("memory recipe asset proposal; version={version}; fingerprint={fingerprint}");
        let receipt = self
            .evorule_client
            .propose_knowledge_entry(dataset_id, &payload, &cause, Some(session_id))
            .await
            .map_err(|e| format!("recipe asset proposal failed: {e}"))?;
        info!(version = %version, %fingerprint, "memory recipe asset proposed");
        Ok(receipt)
    }

    /// 写前置查询（Q2 第四触发点 R-4）:写族意图→目标路径历史 advisory。
    /// fail-soft 静默（拉取失败/无记忆面/无匹配/同路径已建议→None,写入
    /// 不受影响——可用性优先于回喂,与 R-1 fail-visible 取向相反是设计使然）
    pub(crate) async fn write_intent_advisory(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Option<String> {
        let path = crate::agent::memory::extract_write_path(tool_name, args)?;
        {
            let mut seen = self.advised_paths.lock().unwrap_or_else(|p| p.into_inner());
            if !seen.insert(path.clone()) {
                return None; // 同路径本会话已建议过（降噪）
            }
        }
        let mem = self.memory.as_ref()?;
        let catalog = mem.fetch_advisory_catalog().await.ok()?;
        let lines = crate::agent::memory::format_write_advisory(&path, &catalog, 3);
        if lines.is_empty() {
            return None;
        }
        info!(tool = %tool_name, %path, hits = lines.len(), "write-intent advisory attached");
        Some(format!(
            "⚠ 写入目标 {path} 的历史记录（写前置查询回喂）:
{}",
            lines.join(
                "
"
            )
        ))
    }

    /// 装配面聚焦（delegate 子代理装配路径）
    ///
    /// 主路径聚焦快照=注册面本身（装配产物），不再并静态表——子代理聚焦
    /// 快照=manifest 过滤后的装配面，LLM 契约面（messages 侧 tools payload，
    /// 同为注册面）与管道②聚焦允许面同源（设计档 §4.4 装配即过滤）；未装配
    /// 的静态表工具②聚焦即拒（union 提权面在装配期收口，不再依赖⑦执行器
    /// 报错兜底）。主路径不启用（行为等价口径不变，B2 断言测试盯守）。
    pub(crate) fn with_assembly_scope_focus(mut self) -> Self {
        self.assembly_scope_focus = true;
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

    /// G2/R11/C3:按定义接线上下文窗口(PR-H4 验收批抽取为单一实现)
    ///
    /// `from_definition` 与 WS 流式构造路径(ws_handler::construct_runner,
    /// 走 AgentRunner::new 不经 from_definition)共用的唯一接线点:
    /// 显式声明 `def.context_window_tokens` = 单一事实源;未显式设置时
    /// warn 可见 + 8192 兜底(R11 默认值不得静默——记忆区预算
    /// = 8192 × 25% = 2,048 token,约 60-80 条即饱和并开始裁剪(实测))。
    /// 响应预留:配方 budget.reserve_for_response_pct 声明(默认 25%,
    /// 整数算术与现状 max_tokens/4 逐值等价)。
    ///
    /// 调用前置:`self.assembly` 已就位(reserve pct 取自配方执行器)。
    /// session_spawn 链预算基数(CHAIN_BUDGET_WINDOW_MULT ×
    /// max_context_tokens)与记忆区预算基准随之同源归真。
    pub(crate) fn wire_definition_context_window(&mut self, context_window_tokens: Option<usize>) {
        let max_tokens = match context_window_tokens {
            Some(t) => t,
            None => {
                warn!(
                    "context_window_tokens 未显式设置,使用默认 8192(记忆区预算 = 8192 × 25% = 2048 token,约 60-80 条即饱和;生产部署建议显式声明)"
                );
                8192
            }
        };
        let reserve = max_tokens * self.assembly.reserve_for_response_pct() as usize / 100;
        let ctx_mgr = ContextWindowManager::with_approx_counter(
            max_tokens,
            reserve,
            TrimStrategy::KeepSystemKeepLast,
        );
        self.context_window = Some(ctx_mgr);
        // C3:记录总窗口 token,供组装执行器作记忆区预算基准
        self.max_context_tokens = max_tokens;
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

    /// B21 PR-1:注入 journal 目录(serve 模式;启用会话事件流落盘)。
    /// 注入后 run_streaming_inner 每会话创建 `data/sessions/{sid}.jsonl`
    /// 唯一真相源;不注入(默认)行为零变化。
    pub fn with_journal_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.journal_dir = Some(dir);
        self
    }

    /// B21 D3:注入主动压缩策略(serve 面;`longSession.compaction.*` 设置映射,
    /// 见 [`CompactionPolicy::from_settings`]。不注入=None=不启用,行为零变化)
    pub fn with_compaction_policy(mut self, policy: CompactionPolicy) -> Self {
        self.compaction_policy = Some(policy);
        self
    }

    /// 查账工具接线（工具面统一架构 PR-11a）：以本 runner 会话态重绑
    /// handler 内已注册的查账工具实例（进程级 toolkit 中的共享占位实例 →
    /// per-runner 实例，与 delegate 定义级注册同型）。
    ///
    /// - `workdir` = read_back 沙箱根（与装配 toolkit 的 workdir 同源）；
    /// - 轨迹句柄 = 本 runner 的 collector（Arc clone，只读快照消费）；
    /// - journal 上下文 = 共享槽（set_session_id 时经 sync 写入）。
    ///
    /// handler 未注册查账工具（自定义 toolkit）时逐名跳过，零强加。
    pub fn wire_accounting(mut self, workdir: &std::path::Path) -> Self {
        use crate::builtin_tools::accounting::{
            AccountingDeps, DiffRunsTool, QueryJournalTool, QueryTraceTool, ReadBackTool,
        };
        let deps = AccountingDeps::wired(
            workdir,
            self.tool_traces.clone(),
            self.accounting_journal.clone(),
        );
        for name in ["query_journal", "query_trace", "read_back", "diff_runs"] {
            if !self.tool_handler.has_tool(name) {
                continue;
            }
            let func: Arc<dyn ToolFunction> = match name {
                "query_journal" => Arc::new(QueryJournalTool::new(deps.clone())),
                "query_trace" => Arc::new(QueryTraceTool::new(deps.clone())),
                "read_back" => Arc::new(ReadBackTool::new(deps.clone())),
                _ => Arc::new(DiffRunsTool::new(deps.clone())),
            };
            self.tool_handler.register_static(name, func);
        }
        self
    }

    /// journal 上下文同步到查账工具（session_id 落定点调用；journal_dir
    /// 未启用时槽保持 None——CLI 直跑路径 query_journal 如实报错）
    fn sync_accounting_journal(&self) {
        if let (Some(dir), Some(sid)) = (&self.journal_dir, self.session_id.as_ref()) {
            if let Ok(mut slot) = self.accounting_journal.write() {
                *slot = Some(crate::builtin_tools::accounting::JournalCtx {
                    dir: dir.clone(),
                    session_id: sid.clone(),
                });
            }
        }
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

    /// B2:注入 skills 生效清单(manifest 槽位源;None = 无技能,槽位静默跳过)。
    /// read_skill 工具的注册在 serve_tools::wire_skills(handler 面),两者
    /// 必须同源同批——清单来自同一次 wire_skills 返回值。
    pub fn with_skills(
        mut self,
        skills: Option<Vec<crate::agent::definition::SkillManifestEntry>>,
    ) -> Self {
        self.config.skills = skills;
        self
    }

    /// 治理门禁段注入(serve 三路径构造期传入;None=CLI 纯基底,S2 槽位
    /// 缺席合法)。段内容=serve_tools::build_governance_segment 纯构造产物。
    pub fn with_governance_segment(mut self, segment: Option<String>) -> Self {
        self.config.governance_segment = segment;
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
    /// R3：摘要生成 + PayloadUpdate 落链 + hint 替换（非流式/流式两路径共用）。
    ///
    /// - `Generated(meta)` → PayloadUpdate 至 `__memory__.{ns}.session_{sid}.rolling_summary`
    ///   （best-effort：失败留痕不阻塞主流程；run/rebuild 分叉由 context-doctor 标记）
    /// - hint 替换语义与既有实现一致（[earlier 前缀消息替换为摘要）
    async fn handle_summary_outcome(
        &self,
        session_id: &str,
        summarizer: &ContextSummarizer,
        dropped: &[Message],
        trim_messages: &mut [Message],
        goal: &str,
        purpose: &str,
    ) -> Result<(), AgentError> {
        let outcome = summarizer
            .summarize_dropped_with_purpose(dropped, goal, purpose)
            .await
            .map_err(AgentError::Internal)?;
        // 摘要保真对照(规格修正批交付物 B):被裁剪消息确定性锚点 vs 摘要
        // 文本,每次压缩自动对照(I5 升级)。journal 在位才落账(流式路径);
        // ratio<0.5 warn(fail-visible);空摘要=跳过判定但事件照落。
        let summary_text = outcome.formatted();
        let scan = crate::agent::summary_fidelity::scan(
            dropped,
            summary_text.as_deref().unwrap_or(""),
            summary_text.is_none(),
        );
        if scan.ratio < 0.5 && !scan.summary_empty {
            warn!(
                session_id = %session_id,
                ratio = scan.ratio,
                anchors = scan.anchors_n,
                hit = scan.hit_n,
                "summary fidelity below 0.5 - key info may be lost from trimmed history (fail-visible)"
            );
        }
        if let Some(j) = self.active_journal.as_deref() {
            let _ = j.summary_fidelity_scan(
                session_id,
                scan.trimmed_n,
                scan.anchors_n,
                scan.hit_n,
                scan.ratio,
                scan.summary_empty,
            );
        }
        let Some(formatted) = summary_text else {
            tracing::debug!(%session_id, "R3: summary skipped (below threshold or empty)");
            return Ok(());
        };
        if let SummarizeOutcome::Generated(meta) = &outcome {
            if let Some(memory) = &self.memory {
                let entry = serde_json::json!({
                    "gen": meta.gen,
                    "frozen_len_before": meta.frozen_len_before,
                    "frozen_len_after": meta.frozen_len_after,
                    "strategy_fingerprint": meta.strategy_fingerprint,
                    "parent_gen": meta.parent_gen,
                    "summary_text": meta.summary_text,
                });
                match memory.save_rolling_summary(session_id, &entry).await {
                    Ok(()) => info!(
                        %session_id,
                        gen = meta.gen,
                        frozen = meta.frozen_len_after,
                        "R3: rolling summary landed (PayloadUpdate)"
                    ),
                    Err(e) => warn!(
                        %session_id,
                        gen = meta.gen,
                        error = %e,
                        "R3: rolling summary landing failed (best-effort)——run/rebuild divergence possible, context-doctor flags"
                    ),
                }
            }
        }
        for msg in trim_messages {
            if let Message::System { content } = msg {
                if content.starts_with("[earlier") {
                    *content = formatted;
                    break;
                }
            }
        }
        Ok(())
    }

    /// B21 D3 主动 compaction:阈值驱动的窗口压力管理(被动 trim 的前置层)。
    ///
    /// 触发:本轮 LLM 调用前,上下文用量(count)≥ 窗口×thresholdPct 时执行。
    /// 动作(设计 D3):①近摘要区 = system 前缀 + 最近
    /// [`COMPACTION_KEEP_ROUNDS`] 轮之外的全部消息;②近摘要区滚动摘要
    /// (purpose=compaction,经 audited_llm 留痕,复用保真对照+记忆落链);
    /// ③区内大块工具结果原文以 `[cleared: 工具名]` 引用替代(按原文长度
    /// 从大到小,条数受 maxClearToolResults 约束);④摘要块回注区前 +
    /// compaction_performed 落 journal。
    ///
    /// 返回 `Some(压缩后序列)` 交被动 trim 继续兜底(trim 仍是最后防线);
    /// `None` = 未启用/未触发/无 summarizer/无窗口/近摘要区空——调用方按
    /// 原消息走被动 trim。可恢复原则:被清原文完整留存 journal(tool_result
    /// 直写),消息层仅留结构化引用;口径注记:设计档「[cleared: call_id]」
    /// 的 call_id 在消息层不存在(Message::Tool 无 call_id 标识),以工具名
    /// 引用替代——call_id 级对账在 journal 侧(t{seq} 合成键)成立。
    async fn active_compaction(
        &self,
        session_id: &str,
        messages: &[Message],
        goal: &str,
        journal: Option<&std::sync::Arc<crate::agent::journal::JournalWriter>>,
    ) -> Option<Vec<Message>> {
        let policy = self.compaction_policy.as_ref()?;
        if !policy.enabled {
            return None;
        }
        let ctx = self.context_window.as_ref()?;
        let summarizer = self.summarizer.as_ref()?;
        let threshold = ctx
            .window_tokens()
            .saturating_mul(policy.threshold_pct as usize)
            / 100;
        if threshold == 0 || ctx.count(messages) < threshold {
            return None;
        }
        let (system_end, split) = compaction_region_bounds(messages, COMPACTION_KEEP_ROUNDS)?;
        if split <= system_end {
            return None;
        }
        let region = &messages[system_end..split];
        // ③工具结果引用替代(近摘要区内;保留尾段不动=近期工作记忆全量保真)
        let (cleared_region, cleared_n) = clear_tool_results(region, policy.max_clear_tool_results);
        // ④摘要块占位回注区前(handle_summary_outcome 把首条 [earlier 开头的
        // System 消息替换为摘要文本;摘要未产(Empty/失败)时占位保留=结构化
        // 提示,工具结果清除的降压不受影响)
        let mut body: Vec<Message> = vec![Message::System {
            content: format!(
                "[earlier {} messages compacted due to context window pressure]",
                region.len()
            ),
        }];
        body.extend(cleared_region);
        // ②滚动摘要(purpose=compaction;保真对照/记忆落链在 helper 内)
        if let Err(e) = self
            .handle_summary_outcome(
                session_id,
                summarizer,
                region,
                &mut body,
                goal,
                "compaction",
            )
            .await
        {
            warn!(
                %session_id,
                error = %e,
                "D3: compaction summary failed, keeping placeholder hint"
            );
        }
        let summary_generated = matches!(
            &body[0],
            Message::System { content } if content.starts_with("[earlier conversation summary]")
        );
        let mut compacted: Vec<Message> = messages[..system_end].to_vec();
        compacted.extend(body);
        compacted.extend_from_slice(&messages[split..]);
        // F-902:主动压缩事件落 journal(观测面;before/after=全量消息字符数,
        // 与被动 trim 落账同口径)
        if let Some(j) = journal {
            let before: usize = messages.iter().map(|m| m.content().len()).sum();
            let after: usize = compacted.iter().map(|m| m.content().len()).sum();
            if let Err(e) = j.compaction_performed(before, after, summary_generated) {
                warn!(%session_id, error = %e, "compaction_performed journal failed (active)");
            }
        }
        info!(
            %session_id,
            region = region.len(),
            cleared = cleared_n,
            summary_generated,
            "D3: active compaction performed (threshold-driven)"
        );
        Some(compacted)
    }

    /// 会话终态标记（PayloadUpdate，append-only 不改既有事实）。
    /// 早期失败窗口的失败也留痕——不留「有始无终」孤儿会话。
    /// 标记提交自身失败时再留一层 warn（两层失败可见，不静默）。
    fn mark_session_terminal(&self, session_id: &str, stage: &str, reason: &str) {
        warn!(%session_id, stage, reason, "startup failure - session terminal marker pending");
        let session_id = session_id.to_owned();
        let marker = serde_json::json!({
            "session_terminal": {
                "state": "error",
                "stage": stage,
                "reason": reason,
                "at_epoch_ms": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            }
        });
        let client = self.evorule_client.clone();
        tokio::spawn(async move {
            if let Err(land_err) = client
                .update_payload(&session_id, "__meta__.session_terminal", &marker)
                .await
            {
                warn!(%session_id, error = %land_err, "终态标记提交也失败（会话彻底孤儿，人工介入）");
            }
        });
    }

    /// 会话即时终止面：轮错误收尾路径 best-effort 中断 server 侧
    /// 反应器，不留「agent 已终、反应器空转到 TTL 收割」的悬挂会话（恢复模式
    /// 断点续做场景的即时终止面）。fail-soft：中断失败仅 warn
    /// 留痕，不阻塞错误上抛。
    async fn interrupt_evorule_session_best_effort(&self, session_id: &str) {
        if let Err(e) = self.evorule_client.interrupt_session(session_id).await {
            warn!(
                %session_id,
                error = %e,
                "interrupt_session (error finalize) failed; server session left to TTL reap"
            );
        }
    }

    /// 会话健康声明：IO 契约协商+建会话后拉取一次语义不变量
    /// 自检计数（三性面），违规非零即 warn 留痕——只声明不执法。fail-soft：
    /// 404/连不通（旧 server 无端点）仅 warn 通过，不阻塞会话。
    async fn log_session_invariants_best_effort(&self, session_id: &str) {
        match self.evorule_client.get_session_invariants(session_id).await {
            Ok(v) => {
                let violations = v
                    .get("structural_invariant_violations")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                if violations == 0 {
                    info!(%session_id, "session invariants clean (health declaration)");
                } else {
                    warn!(
                        %session_id,
                        violations,
                        "session invariants violations detected (health declaration, advisory only)"
                    );
                }
            }
            Err(e) => {
                warn!(
                    %session_id,
                    error = %e,
                    "session invariants fetch failed (old server without endpoint?); health declaration skipped"
                );
            }
        }
    }

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
    /// 空闲巩固入口（sleep-time 触发变体转正,账本记忆设计档 §六）:会话间隙外的
    /// 空闲窗口可由调度器(运维/CLI/后续产品化)调用——仅重跑巩固阶段
    /// (跨会话候选确定性聚类→sidecar 合并提议→Consolidated 落账),
    /// 门控仍随 Recipe(consolidation 缺省开;无审计通路内部自动跳过)。
    /// 返回本次巩固候选数(离线/不可用=如实 Err)。
    pub async fn idle_consolidation(&mut self, session_id: &str) -> Result<usize, String> {
        let memory = self.memory.as_mut().ok_or("memory not enabled")?;
        memory.flush_usage(session_id).await;
        let mut deps = sediment::SedimentDeps {
            memory,
            summarizer: self.summarizer.as_ref(),
            extractor: self.extractor.as_mut(),
            event_store: self.memory_event_store.as_mut(),
            auditor: self.summarizer.as_ref().and_then(|s| s.auditor()),
            journal_lines: Vec::new(),
        };
        let mut result = sediment::SedimentResult::default();
        sediment::consolidate_knowledge_candidates(
            &mut deps,
            &self.sediment_config,
            session_id,
            &mut result,
        )
        .await;
        Ok(result.knowledge_consolidated.len())
    }

    async fn sediment_session(
        &mut self,
        session_id: &str,
        messages: &[Message],
        journal: Option<&crate::agent::journal::JournalWriter>,
    ) -> Result<(), AgentError> {
        if let Some(memory) = self.memory.as_mut() {
            // F-616:usage 增量批量回写(sediment 前刷,批量端点一次 HTTP)
            memory.flush_usage(session_id).await;
            // L1 修复(断点5-时序):apply_lifecycle_transitions 移至 sediment 之后。
            // 旧序:lifecycle→sediment——本会话新捕获的 Captured 事件必然缺席
            // 本次生命周期检查(sediment 还没跑),单次会话模式下永远等不到
            // 「下一次」检查。新序:sediment 先捕获→lifecycle 紧随检查,事件
            // 出生即受检;已水合的历史事件同批受检(断点5-水合配合)。
            let recipe = memory.recipe.clone().unwrap_or_default();
            let mut deps = sediment::SedimentDeps {
                memory,
                summarizer: self.summarizer.as_ref(),
                extractor: self.extractor.as_mut(),
                event_store: self.memory_event_store.as_mut(),
                // A2-1：审计执行器自 summarizer 复用（同一 sidecar 通路）
                auditor: self.summarizer.as_ref().and_then(|s| s.auditor()),
                // 跨源批 C:journal 全量行预读(读取失败=空集如实降级;
                // sources.journal_digest 门控在 sediment 内判定)
                journal_lines: journal
                    .map(|j| j.read_lines().unwrap_or_default())
                    .unwrap_or_default(),
            };
            // sediment 四项结果落 journal——此前被 `let _ =` 丢弃，
            // 沉淀成功与否无对账依据（受信通道持久化信号闭环的最后半程）
            let result =
                sediment::sediment(&mut deps, &self.sediment_config, session_id, messages).await;
            if let Some(j) = journal {
                if let Err(e) = j.sediment_performed(
                    result.summary_written,
                    result.stable_facts.clone(),
                    result.stable_facts_cache_only.clone(),
                    result.events.len(),
                    result.rollup_done,
                    result.knowledge_candidates.len(),
                    result.flushed_events,
                ) {
                    warn!(%session_id, error = %e, "sediment_performed journal failed");
                }

                // 冷迁（F-617 冷热分层,Recipe storage.cold_tier 门控缺省关）:
                // 生命周期迁移产物(Archived/Tombstoned/Decayed)事务移入 lex-cold.db,
                // 计数入账(cold_moved 事件);cache 镜像同步逐出
                if recipe.storage.cold_tier {
                    if let Some(mem) = self.memory.as_mut() {
                        match mem.move_cold_tier(&recipe).await {
                            Ok(n) if n > 0 => {
                                if let Some(j) = journal {
                                    if let Err(e) = j.cold_moved(n as u64) {
                                        warn!(%session_id, error = %e, "cold_moved journal failed");
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                warn!(%session_id, error = %e, "cold tier move failed (best-effort)");
                            }
                        }
                    }
                }
            }
            if !result.stable_facts_cache_only.is_empty() {
                tracing::warn!(
                    session_id = %session_id,
                    cache_only = result.stable_facts_cache_only.len(),
                    persisted = result.stable_facts.len(),
                    "sediment: 部分 stable 事实仅本地 cache（持久化失败），由 B3 对账补偿"
                );
            }
            // L1 修复(断点5-时序,后半):sediment 完成后执行生命周期检查——
            // 本会话新捕获的 Captured 事件与已水合的历史事件在此同批受检。
            // (冷迁仍在其后,保持 F-617 原序)
            {
                let mem = self.memory.as_mut().unwrap();
                let recipe = mem.recipe.clone().unwrap_or_default();
                mem.apply_lifecycle_transitions(session_id, &recipe).await;
            }
        }
        Ok(())
    }

    /// P1:会话收尾把工具调用轨迹随 tool_trace 指令批量提交进引擎审计链
    ///
    /// 宪法 core_eval v0.5.0 tool_trace 规则将 value 按指令给定 attr set 入
    /// payload(attr=meta_tool.tool_traces.<seq>),随 StateTransition 事实落链;
    /// 规则面可对 instruction_type=tool_trace 精确求值(shell_exec 黑名单类
    /// enforce 在约束门事前检测,违规轨迹以 Violation 留痕)。
    ///
    /// 时序:io_response 提交后调用——若 io 仍在途,submit_command 由引擎
    /// 串行语义排队,IoResponse 收敛后按序评估(先收敛 call_external 转换
    /// 再评估轨迹),顺序确定性由引擎保证。fail-soft:单条失败仅计数+warn,
    /// 绝不阻断会话收尾(与 L2 前馈注入同纪律)。
    async fn submit_tool_traces(&self, session_id: &str) {
        let entries = match self.tool_traces.lock() {
            Ok(mut tt) => tt.drain(),
            Err(_) => return,
        };
        if entries.is_empty() {
            return;
        }
        let total = entries.len();
        for entry in &entries {
            let seq = entry.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
            let cmd = serde_json::json!({
                "type": "tool_trace",
                "params": {
                    "attr": format!("meta_tool.tool_traces.{}", seq),
                    "value": entry,
                }
            });
            if let Err(e) = self.evorule_client.submit_command(session_id, &cmd).await {
                if let Ok(mut tt) = self.tool_traces.lock() {
                    tt.record_submit_failure();
                }
                warn!(
                    %session_id, seq, error = %e,
                    "tool_trace submit failed; audit chain gap (fail-soft, counted)"
                );
            }
        }
        info!(%session_id, total, "tool_trace batch submitted to engine audit chain");
    }

    /// 非流式主循环:执行 ReAct 推理至目标完成,返回最终结果
    ///
    /// 与 [`Self::run_streaming`] 共享 G17 工具插桩与治理链路;本路径在
    /// 全部终止边界(取消/超步/错误/Violation/Stable/流关闭)提交工具轨迹。
    ///
    /// 引擎自 v0.5.0 起仅单发桥接(call_external 的 io_response 消费后即
    /// Stable,不再发起下一轮),多轮编排由应用层负责——本方法**没有**本地
    /// 工具结果回喂循环,LLM 首轮返回 tool_calls 的任务会在 Stable 终止
    /// (此时按编排断裂指纹显式判错,不再假绿)。生产多轮工具任务一律走
    /// [`Self::run_streaming`](本地回喂循环);本方法仅保留给 mock 引擎
    /// 测试面与单轮纯文本场景。
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

        // C2: 召回顺序修复 —— recall 在组装之前
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
        // 元层先行批:组装执行器单一出口(run/流式两组装点收敛为同一段代码,
        // 双路径一致性由代码结构保证;槽位序/预算比例/分隔符由配方声明)
        let boundary_segment = self
            .config
            .capability_boundary
            .as_ref()
            .map(|b| b.awareness_segment());
        // 治理知识契约段(S2b 槽位内容物;定义声明 knowledge_datasets 驱动,
        // runner 内实时拉取渲染——声明即生效,任何 runner 路径同口径。
        // fail-soft:数据集拉取失败 warn 跳过,绝不阻断会话)
        let knowledge_segment = match self.config.knowledge_datasets.as_deref() {
            Some(datasets) if !datasets.is_empty() => {
                crate::api::serve_tools::build_knowledge_segment(&self.evorule_client, datasets)
                    .await
            }
            _ => None,
        };
        let system_prompt = self
            .assembly
            .assemble(
                &self.config.system_prompt,
                self.config.identity_segment.as_deref(),
                self.config.north_star.as_deref(),
                self.memory.as_ref(),
                &recall,
                self.max_context_tokens,
                boundary_segment.as_deref(),
                self.config.skills.as_deref(),
                self.config.handoff.as_ref(),
                self.config.governance_segment.as_deref(),
                knowledge_segment.as_deref(),
            )
            .map_err(AgentError::Internal)?;

        // 建会话前 IO 形状契约协商——404/连不通=旧 server warn
        // 通过;端点在但版本不匹配=hard fail(未验证升级行为宁停不错)。
        // client clone 进闭包达 'static(共享 reqwest 连接池,无额外开销)。
        let io_client = self.evorule_client.clone();
        if let Err(e) = crate::api::io_contract::negotiate_io_contract(move || {
            Box::pin(async move { io_client.fetch_io_contract().await })
        })
        .await
        {
            return Err(AgentError::EvoruleError(format!(
                "io-contract negotiation failed: {e}"
            )));
        }
        // M5-a:边界声明经 create_session initial_content 既有载体进会话事实
        let boundary_json = self
            .config
            .capability_boundary
            .as_ref()
            .map(|b| b.to_json());
        let session_id = self
            .evorule_client
            .create_session(boundary_json.as_ref(), Some("llm"))
            .await?;
        self.session_id = Some(session_id.clone());
        self.sync_accounting_journal();
        info!(%session_id, "Created evorule session");
        // B2:健康声明拉取一次（协商+建会话后，fail-soft）
        self.log_session_invariants_best_effort(&session_id).await;

        // G14:同步 session_id 到 MemoryEventStore(若已注入)
        if let Some(store) = self.memory_event_store.as_mut() {
            store.set_session_id(&session_id);
            // best-effort 从 evorule 同步已有事件(HTTP 失败不阻塞)
            if let Err(e) = store.sync_from_evorule().await {
                tracing::warn!(session_id = %session_id, error = %e, "sync_from_evorule failed; memory event store may be incomplete");
            }
        }
        // 修复(2026-09-29 实测):此前注释承诺"session_id 通过 set_session_id 设置"
        // 但从未调用 → sediment 写 Shared 域全部 SessionNotSet。
        // 与 MemoryEventStore 对齐,会话建立后同步到 MemoryManager。
        if let Some(mem) = self.memory.as_mut() {
            mem.set_session_id(&session_id);
        }
        // A2-2:memory_propose 会话锚绑定(注册期空锚,运行期才可绑定)
        self.bind_propose_anchor(&session_id);
        // 双通道笔记批:note_write 会话期注册(payload 写,session_id 在手)
        self.register_session_scoped_memory_tools(&session_id);
        // 自主交接批:handover 双工具+session_spawn 会话期重绑(占位→wired,
        // session_id 在手;run 纯路径无 journal,传 None——语义事件缺席如实
        // 降级,调用镜像 tool_invoked/tool_result 仍在)
        self.register_session_scoped_handover_tools(&session_id, None);
        // 跨源批 D:技能双层注册同步(声明面真账镜像+正文本地索引;
        // Recipe sources.skills_index 门控,缺省关=no-op;best-effort
        // 不阻塞会话)
        if self.config.skills.is_some() {
            if let Some(mem) = self.memory.as_mut() {
                let empty_manifest = Vec::new();
                let manifest = self.config.skills.as_ref().unwrap_or(&empty_manifest);
                let stats = crate::agent::skills_mirror::sync_skills_mirror(mem, manifest).await;
                if !stats.skipped {
                    info!(
                        written = stats.metadata_written,
                        tombstoned = stats.tombstoned,
                        sections = stats.body_sections,
                        degraded = stats.degraded,
                        "skills mirror synced"
                    );
                }
            }
        }

        // G17:session 指标 — sessions_total + sessions_active(RAII guard 保证所有返回路径 dec)
        if let Some(m) = &self.metrics {
            m.inc_sessions_total();
        }
        let _session_guard = SessionActiveGuard::new(self.metrics.clone());

        // 早期失败窗口收口——create_session 成功后的失败也留终态标记，
        // 不留「有始无终」孤儿会话（与「失败也回写 io_response」契约同族）
        let _recalled_fact_ids = match self.auto_recall(&session_id).await {
            Ok(v) => v,
            Err(e) => {
                self.mark_session_terminal(&session_id, "startup", &e.to_string());
                return Err(e);
            }
        };

        // 注意:必须先订阅 SSE 事件,再提交命令。
        // tokio broadcast 通道只接收订阅之后发出的消息,不重放历史。
        // 如果先 submit_command 再 subscribe,会错过 io_request 事件,导致 ReAct 循环无法启动。
        let mut event_stream = match self.evorule_client.subscribe_events(&session_id).await {
            Ok(es) => es,
            Err(e) => {
                let err = AgentError::from(e);
                self.mark_session_terminal(&session_id, "startup", &err.to_string());
                return Err(err);
            }
        };

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
        // 连续 Error→auto_rewind→continue 回退预算(有界封顶)。
        // rewind 后 continue 不耗 step_count——引擎/网络持续 Error 时无界
        // 重试=不可终止回退循环。预算熔断后走 Error 收尾路径(flush/sediment/
        // tool_traces/error 结果),fail-visible 不静默。重置语义:任意非 Error
        // 事件(正常推进)即清零——只惩罚"连续"失败,不惩罚间歇错误。
        let mut rewind_budget = RewindBudget::new(REWIND_BUDGET_LIMIT);
        while let Some(event) = event_stream.next().await {
            // G6:取消检查(event 边界 — 即使 LLM 调用已返回,也在此处响应取消)
            if self.cancel_token.is_cancelled() {
                info!(%session_id, "Cancellation requested at event boundary, cleaning up");
                if let Err(e) = self.flush_messages(&session_id).await {
                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                }
                // 取消路径补 sediment——代谢出口堵洞。已发生
                // 的对话/事件/usage 不随取消流失(best-effort,与 Error 路径同款)
                if let Err(e) = self.sediment_session(&session_id, &messages, None).await {
                    tracing::warn!(session_id = %session_id, error = %e, "sediment_session failed");
                }
                self.submit_tool_traces(&session_id).await;
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
                        self.submit_tool_traces(&session_id).await;
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
                        r = self.handle_io_request(&session_id, &event.payload, &mut messages, &mut tool_calls, goal, step_count) => match r {
                            Ok(r) => r,
                            // 处理失败(60s 超时/LLM 错误/工具错误/内部错误)也必须
                            // 回写 error io_response —— 否则 server 侧 io_request 永久挂起、
                            // 链实不一致(幽灵在途请求)。与取消分支/流式错误分支/audited_llm
                            // 「失败也回写再返回」同一契约。回写后照旧上抛终止本轮。
                            Err(e) => {
                                if let Some(rid) = event.payload.get("id").and_then(|v| v.as_u64()) {
                                    let err_str = e.to_string();
                                    if let Err(ie) = self.evorule_client
                                        .submit_io_response(
                                            &session_id,
                                            rid,
                                            &serde_json::json!({"error": &err_str}),
                                            Some(err_str.as_str()),
                                        )
                                        .await
                                    {
                                        tracing::warn!(session_id = %session_id, request_id = rid, error = %ie, "submit_io_response (error) failed; io_request may hang on engine side");
                                    }
                                }
                                if let Err(e) = self.flush_messages(&session_id).await {
                                tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                            }
                                self.submit_tool_traces(&session_id).await;
                                // B1:轮错误收尾 best-effort 中断 server 反应器（fail-soft，
                                // 不留悬挂会话空转到 TTL 收割）
                                self.interrupt_evorule_session_best_effort(&session_id).await;
                                return Err(e);
                            }
                        },
                        _ = cancel_token.cancelled() => {
                            info!(%session_id, "Cancelled during io_request, cleaning up");
                            // 提交 error io_response 防止 evorule 卡死等 IoResponse
                            if let Some(rid) = event.payload.get("id").and_then(|v| v.as_u64()) {
                                if let Err(e) = self.evorule_client
                                    .submit_io_response(
                                        &session_id,
                                        rid,
                                        &serde_json::json!({"error": "cancelled"}),
                                        Some("cancelled"),
                                    )
                                    .await
                                {
                                    tracing::warn!(session_id = %session_id, request_id = rid, error = %e, "submit_io_response (cancel) failed; io_request may hang on engine side");
                                }
                            }
                            if let Err(e) = self.flush_messages(&session_id).await {
                                tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                            }
                            self.submit_tool_traces(&session_id).await;
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
                    if let Err(e) = self.sediment_session(&session_id, &messages, None).await {
                        tracing::warn!(session_id = %session_id, error = %e, "sediment_session failed");
                    }
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
                        // 假绿改判(fail-fast):content 空且末条 Assistant 携带
                        // tool_calls = LLM 意图调用工具但多轮编排未继续(引擎
                        // v0.5.0 起仅单发桥接,本方法无本地回喂循环)——此时返回
                        // success 即静默假绿,改判 error 并指向流式回喂路径。
                        // 纯 LLM 真返空(末条 assistant 无 tool_calls)仍按
                        // success 空产出放行,不误伤。
                        let orchestration_broken = matches!(
                            messages.last(),
                            Some(Message::Assistant {
                                tool_calls: Some(tcs),
                                ..
                            }) if !tcs.is_empty()
                        );
                        if orchestration_broken {
                            warn!(
                                %session_id, step_count,
                                "Stable with empty content while LLM requested tool calls: \
                                 multi-round orchestration did not continue (single-shot bridge), \
                                 failing fast"
                            );
                            self.submit_tool_traces(&session_id).await;
                            return Ok(AgentResult::error(
                                "agent requested tool calls but no further round executed \
                                 (engine single-shot bridge); use the streaming run path, \
                                 which feeds tool results back to the LLM"
                                    .to_string(),
                                step_count,
                                duration,
                            ));
                        }
                        // 空产出观测补位(不改判 success——合法空响应不误伤)。
                        // 三联指纹(tokens=0+亚秒+空 content)曾掩盖 LLM 失败假绿,
                        // 此处保证链上观测可见。
                        warn!(%session_id, step_count, "Stable with empty content: possible LLM empty response (tokens_used side-channel in io_response)");
                    }

                    info!(%session_id, content_len = content.len(), "Received Stable event, execution complete");
                    self.submit_tool_traces(&session_id).await;
                    return Ok(AgentResult::success(
                        content, step_count, duration, tool_calls,
                    ));
                }
                "StateTransition" => {
                    info!(%session_id, "State transition occurred");
                    rewind_budget.reset(REWIND_BUDGET_LIMIT); // H1:正常推进即回满(只罚连续失败)
                }
                "Error" => {
                    let error_msg = event
                        .payload
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    let duration = start_time.elapsed().as_millis() as u64;

                    // H1:回退预算执法——连续 Error 超预算即熔断(rewind 不计步的
                    // 无界循环封顶;耗尽后落 Error 收尾,与 rewind 本身失败同路)
                    if let Ok(rewind_result) = self.auto_rewind(&session_id).await {
                        if rewind_budget.consume() {
                            info!(%session_id, remaining = rewind_budget.remaining,
                                "Auto-rewind successful, retrying from version {}", rewind_result);
                            continue;
                        }
                        warn!(%session_id, remaining = 0,
                            "H1 rewind budget exhausted: 连续 Error 回退达上限(32),熔断为 fail-visible 错误结果");
                    }

                    // 错误返回前尝试刷写缓冲消息（best-effort，忽略 flush 错误）
                    if let Err(e) = self.flush_messages(&session_id).await {
                        tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                    }
                    // C1:会话沉淀（best-effort，即使出错也尝试沉淀已收集的对话）
                    if let Err(e) = self.sediment_session(&session_id, &messages, None).await {
                        tracing::warn!(session_id = %session_id, error = %e, "sediment_session failed");
                    }
                    self.submit_tool_traces(&session_id).await;
                    // B1:Error 熔断收尾 best-effort 中断 server 反应器（fail-soft）
                    self.interrupt_evorule_session_best_effort(&session_id)
                        .await;
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
                    if let Err(e) = self.flush_messages(&session_id).await {
                        tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                    }
                    if let Err(e) = self.sediment_session(&session_id, &messages, None).await {
                        tracing::warn!(session_id = %session_id, error = %e, "sediment_session failed");
                    }
                    self.submit_tool_traces(&session_id).await;
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
        if let Err(e) = self.flush_messages(&session_id).await {
            tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
        }
        // 断流路径补 sediment——网络/引擎断流不吞已捕获的
        // 会话经验(与 Stable 正常收尾同款 best-effort)
        if let Err(e) = self.sediment_session(&session_id, &messages, None).await {
            tracing::warn!(session_id = %session_id, error = %e, "sediment_session failed");
        }
        self.submit_tool_traces(&session_id).await;
        // D-01 二次保险（B2）：断流可能吞掉 Violation 帧，查 evolution-signals
        // 兜底归因 enforce 命中；查询不可用时降级返回原错误（不掩盖不阻塞）。
        let closed_error = self
            .detect_enforce_after_stream_close(&session_id, "Event stream closed")
            .await;
        Ok(AgentResult::error(closed_error, step_count, duration))
    }

    /// 阶段 3(F-611)+A2-2+双通道笔记:记忆工具注册——读两件(search/get)
    /// 装配期注册;写两件(propose/note_write)中 propose 装配期注册、
    /// note_write 会话期注册(payload 通道,session_id 在手)。
    ///
    /// 暴露面=策略(按读写拆分,A2-2 §3.3):`MemoryRecipe.tools.expose` 白名单
    /// 声明;读件另要求 LexStore 在位(检索缓存是读面数据前提),写件
    /// (memory_propose/note_write)不检索、声明即可。协作件全部与
    /// MemoryManager 共享(usage 计数/审计器同源,不产生第二策略面)。
    /// `tools` 配置声明了自省工具而暴露条件不满足=配置矛盾,早失败(可控);
    /// 未声明而条件满足=照常注册(注册即随 openai_tools_payload 下发,
    /// 与既有工具语义一致)。
    fn register_memory_introspection_tools(&mut self) -> Result<(), AgentError> {
        let declared: Vec<String> = self
            .config
            .tool_names
            .iter()
            .filter(|n| crate::agent::memory_tool::is_registered_memory_tool(n))
            .cloned()
            .collect();
        let (exposed_all, intro) = match self.memory.as_ref() {
            Some(mem) => (mem.exposed_introspection_tools(), mem.memory_introspector()),
            None => (Vec::new(), None),
        };
        // note_write 不在装配期注册(会话期,payload 通道);只校验声明一致性
        let note_declared = declared
            .iter()
            .any(|n| n == crate::agent::memory_tool::NOTE_WRITE_TOOL);
        let note_exposed = exposed_all
            .iter()
            .any(|n| n == crate::agent::memory_tool::NOTE_WRITE_TOOL);
        if note_declared && self.memory.is_none() {
            return Err(AgentError::Internal(
                "agent config lists note_write but memory is disabled (requires memory.type=persistent)"
                    .to_string(),
            ));
        }
        if note_declared && !note_exposed {
            return Err(AgentError::Internal(
                "agent config lists note_write but memory.recipe tools.expose does not declare it (policy carrier is the recipe)"
                    .to_string(),
            ));
        }
        let exposed: Vec<String> = exposed_all
            .iter()
            .filter(|n| crate::agent::memory_tool::is_introspection_tool(n))
            .cloned()
            .collect();
        let missing: Vec<&String> = declared
            .iter()
            .filter(|n| *n != crate::agent::memory_tool::NOTE_WRITE_TOOL && !exposed.contains(n))
            .collect();
        if !missing.is_empty() {
            return Err(AgentError::Internal(format!(
                "agent config lists memory introspection tool(s) {declared:?} but exposure conditions are not met (requires memory.type=persistent and memory.recipe with tools.expose declaring them; read tools additionally require memory.lex_store configured); unmet: {missing:?}"
            )));
        }
        if exposed.is_empty() {
            return Ok(());
        }
        let intro = intro.map(std::sync::Arc::new);
        // A2-2:写件协作件惰性构造(与读件 Intro 相互独立——不依赖 LexStore);
        // 会话锚注册期为空,两 run 路径 create_session 后 bind_propose_anchor
        let mut proposer: Option<std::sync::Arc<crate::agent::memory_tool::MemoryProposer>> = None;
        for name in &exposed {
            let exec: Option<std::sync::Arc<dyn ToolFunction>> = match name.as_str() {
                crate::agent::memory_tool::MEMORY_SEARCH_TOOL => intro.as_ref().map(|i| {
                    std::sync::Arc::new(crate::agent::memory_tool::MemorySearchTool::new(
                        std::sync::Arc::clone(i),
                    )) as std::sync::Arc<dyn ToolFunction>
                }),
                crate::agent::memory_tool::MEMORY_GET_TOOL => intro.as_ref().map(|i| {
                    std::sync::Arc::new(crate::agent::memory_tool::MemoryGetTool::new(
                        std::sync::Arc::clone(i),
                    )) as std::sync::Arc<dyn ToolFunction>
                }),
                crate::agent::memory_tool::MEMORY_PROPOSE_TOOL => {
                    let p = proposer.get_or_insert_with(|| {
                        let anchor = std::sync::Arc::new(std::sync::RwLock::new(None));
                        let inner =
                            std::sync::Arc::new(crate::agent::memory_tool::MemoryProposer::new(
                                self.sediment_config.namespace.clone(),
                                self.evorule_client.clone(),
                                std::sync::Arc::clone(&anchor),
                            ));
                        self.propose_anchor = Some(anchor);
                        inner
                    });
                    Some(
                        std::sync::Arc::new(crate::agent::memory_tool::MemoryProposeTool::new(
                            std::sync::Arc::clone(p),
                        )) as std::sync::Arc<dyn ToolFunction>,
                    )
                }
                // note_write 走会话期注册(register_session_scoped_memory_tools)
                _ => None,
            };
            // 暴露拆分后读件无 Intro 不可能(exposed 已按 lex_store 过滤);防御 continue
            let Some(exec) = exec else {
                continue;
            };
            self.tool_handler.register_static(name, exec);
            info!(tool = %name, "memory introspection tool registered");
        }
        Ok(())
    }

    /// 检索质量观测批(K-11 观测级):从 RecallContext 构建命中集条目
    /// ("层@序:key",排序位=分位)。journal 在位才落(流式路径);
    /// ground truth 判据列二期,先积累数据。
    fn build_recall_set_hits(recall: &crate::agent::memory::RecallContext) -> Vec<String> {
        let mut hits = Vec::new();
        for (i, r) in recall.stable.iter().enumerate() {
            hits.push(format!("stable@{}:{}", i + 1, r.key));
        }
        for (i, r) in recall.summaries.iter().enumerate() {
            hits.push(format!("summaries@{}:{}", i + 1, r.key));
        }
        for (i, r) in recall.events.iter().enumerate() {
            hits.push(format!("events@{}:{}", i + 1, r.key));
        }
        hits
    }

    /// A2-2:绑定 memory_propose 会话锚(两 run 路径 create_session 后调用;
    /// 写件未注册时 no-op)。Shared 域写=写当前会话 payload,运行期才可绑定。
    fn bind_propose_anchor(&self, session_id: &str) {
        if let Some(anchor) = &self.propose_anchor {
            *anchor.write().unwrap_or_else(|p| p.into_inner()) = Some(session_id.to_string());
            info!(%session_id, "memory_propose session anchor bound");
        }
    }

    /// 双通道笔记批:会话期注册 note_write(payload 通道写,session_id 在手
    /// 后才可注册;G15 续跑幂等——已注册即跳过)。门控=Recipe.tools.expose
    /// 声明;best-effort 条件不满足=静默跳过(装配期已做一致性校验)。
    fn register_session_scoped_memory_tools(&mut self, session_id: &str) {
        let (note_declared, link_declared, forget_declared, namespace, client, link_relations) =
            match self.memory.as_ref() {
                Some(mem) => {
                    let exposed = mem.exposed_introspection_tools();
                    let note_declared = exposed
                        .iter()
                        .any(|n| n == crate::agent::memory_tool::NOTE_WRITE_TOOL);
                    let link_declared = exposed
                        .iter()
                        .any(|n| n == crate::agent::memory_tool::MEMORY_LINK_TOOL);
                    let forget_declared = exposed
                        .iter()
                        .any(|n| n == crate::agent::memory_tool::MEMORY_FORGET_TOOL);
                    (
                        note_declared,
                        link_declared,
                        forget_declared,
                        mem.namespace().to_string(),
                        mem.evorule_client.clone(),
                        mem.link_relations(),
                    )
                }
                None => return,
            };
        if note_declared
            && !self
                .tool_handler
                .has_tool(crate::agent::memory_tool::NOTE_WRITE_TOOL)
        {
            let exec: std::sync::Arc<dyn ToolFunction> =
                std::sync::Arc::new(crate::agent::memory_tool::MemoryNoteWriteTool::new(
                    namespace.clone(),
                    client.clone(),
                    session_id.to_string(),
                ));
            self.tool_handler
                .register_static(crate::agent::memory_tool::NOTE_WRITE_TOOL, exec);
        }
        if forget_declared
            && !self
                .tool_handler
                .has_tool(crate::agent::memory_tool::MEMORY_FORGET_TOOL)
        {
            let exec: std::sync::Arc<dyn ToolFunction> =
                std::sync::Arc::new(crate::agent::memory_tool::MemoryForgetTool::new(
                    namespace.clone(),
                    client.clone(),
                    session_id.to_string(),
                ));
            self.tool_handler
                .register_static(crate::agent::memory_tool::MEMORY_FORGET_TOOL, exec);
        }
        if link_declared
            && !self
                .tool_handler
                .has_tool(crate::agent::memory_tool::MEMORY_LINK_TOOL)
        {
            let exec: std::sync::Arc<dyn ToolFunction> =
                std::sync::Arc::new(crate::agent::memory_tool::MemoryLinkTool::new(
                    namespace.clone(),
                    client.clone(),
                    session_id.to_string(),
                    link_relations.clone(),
                ));
            self.tool_handler
                .register_static(crate::agent::memory_tool::MEMORY_LINK_TOOL, exec);
        }
        if note_declared || link_declared || forget_declared {
            info!(
                note = note_declared,
                link = link_declared,
                forget = forget_declared,
                "memory write tools registered (session-scoped)"
            );
        }
    }

    /// 自主交接批:会话期重绑 handover 双工具(启动期占位→wired;session_id
    /// 在手;查账工具族 wire_accounting 同构)。门控=has_tool(启动期注册+
    /// 开关 agentTools.handover 过滤后在场才重绑——开关关=占位未进面,LLM
    /// 契约同步缺席);G15 续跑幂等(重绑即覆盖注册)。memory 未启用时跳过
    /// (namespace 无权威源,占位保持 fail-visible 报错——不静默造 namespace)。
    /// PR-H3 起兼接 session_spawn 接线(链态惰性初始化+子 runner 工厂注入;
    /// journal 透传=停链 chain_halted 落账锚,None=无 journal 会话账面镜像
    /// 如实降级)。
    fn register_session_scoped_handover_tools(
        &mut self,
        session_id: &str,
        journal: Option<std::sync::Arc<crate::agent::journal::JournalWriter>>,
    ) {
        use crate::agent::handover_tool::{HandoverReadTool, HandoverWriteTool};
        let (namespace, client) = match self.memory.as_ref() {
            Some(mem) => (mem.namespace().to_string(), mem.evorule_client.clone()),
            None => return,
        };
        for name in [
            crate::agent::handover_tool::HANDOVER_WRITE_TOOL,
            crate::agent::handover_tool::HANDOVER_READ_TOOL,
        ] {
            if !self.tool_handler.has_tool(name) {
                continue;
            }
            let exec: std::sync::Arc<dyn ToolFunction> = match name {
                crate::agent::handover_tool::HANDOVER_WRITE_TOOL => {
                    std::sync::Arc::new(HandoverWriteTool::wired(
                        namespace.clone(),
                        client.clone(),
                        session_id.to_string(),
                    ))
                }
                _ => {
                    std::sync::Arc::new(HandoverReadTool::wired(namespace.clone(), client.clone()))
                }
            };
            self.tool_handler.register_static(name, exec);
        }
        // 自主开会话接线(自主交接设计 PR-H3):门控同 handover(启动期占位
        // 在场才接线)。链态惰性初始化(发起方会话首个接线点创建);链 token
        // 计量注入(既有 delegate 预算计数器在场时让位——链预算降级为仅计
        // 链上子会话消耗,如实降级)。链态生命周期=发起方请求内(跨请求的
        // 链续接由 server 侧 parent 链保深度权威,预算/熔断属请求内底线)。
        if self
            .tool_handler
            .has_tool(crate::agent::session_spawn_tool::SESSION_SPAWN_TOOL)
        {
            let chain = match self.chain.clone() {
                Some(c) => c,
                None => {
                    // K-06 口径:链预算=4×会话上下文窗口(常数族拍定值,随
                    // budget-report 数据校准后调)
                    let budget = (self.max_context_tokens as u64)
                        .saturating_mul(crate::agent::session_spawn_tool::CHAIN_BUDGET_WINDOW_MULT);
                    let c = std::sync::Arc::new(
                        crate::agent::session_spawn_tool::ChainRuntimeState::new(budget),
                    );
                    self.chain = Some(c.clone());
                    if self.token_counter.is_none() {
                        self.token_counter = Some(c.token_counter());
                    }
                    c
                }
            };
            let factory = self.spawn_factory(chain.clone());
            let exec: std::sync::Arc<dyn ToolFunction> =
                std::sync::Arc::new(crate::agent::session_spawn_tool::SessionSpawnTool::wired(
                    session_id.to_string(),
                    client.clone(),
                    chain,
                    journal,
                    factory,
                ));
            self.tool_handler
                .register_static(crate::agent::session_spawn_tool::SESSION_SPAWN_TOOL, exec);
        }
        info!(%session_id, "handover tools re-bound (session-scoped)");
    }

    /// 自主交接 PR-H3:子会话 runner 工厂(SessionSpawnTool 接线用)
    ///
    /// 闭包构造期一次性克隆可 Clone 组件(组件快照);不可克隆项按同源输入
    /// 重建:context_window=max_context_tokens+配方响应预留(与 from_definition
    /// 同构)、output_validator=config.output_format、adjudicator=同源 client
    /// 重挂;每会话可变态(消息缓冲/缓存/轨迹/停滞检测/取消令牌/格式指令
    /// 落链去重槽)全新。子 runner 预置:session_id=child、chain=继承同一
    /// Arc、chain_watch=新观察窗(仅子会话携带)、token_counter=链共享计量器
    /// (全链 LLM 消耗向同一累加器归集,链预算口径)。
    pub(crate) fn spawn_factory(
        &self,
        chain: std::sync::Arc<crate::agent::session_spawn_tool::ChainRuntimeState>,
    ) -> std::sync::Arc<dyn Fn(&str) -> AgentRunner + Send + Sync> {
        use crate::agent::session_spawn_tool::ChainWatch;
        let config = self.config.clone();
        let evorule_client = self.evorule_client.clone();
        let llm_handler = self.llm_handler.clone();
        let tool_handler = self.tool_handler.clone();
        let memory = self.memory.clone();
        let delegate_context = self.delegate_context.clone();
        let message_persist_mode = self.message_persist_mode.clone();
        let summary_model = self.summary_model.clone();
        let summarizer = self.summarizer.clone();
        let approval_callback = self.approval_callback.clone();
        let metrics = self.metrics.clone();
        let event_callbacks = self.event_callbacks.clone();
        let memory_event_store = self.memory_event_store.clone();
        let extractor = self.extractor.clone();
        let sediment_config = self.sediment_config.clone();
        let propose_anchor = self.propose_anchor.clone();
        let max_context_tokens = self.max_context_tokens;
        let assembly = self.assembly.clone();
        let tool_result_max_chars = self.tool_result_max_chars;
        let journal_dir = self.journal_dir.clone();
        let acceptance_command = self.acceptance_command.clone();
        let pipeline_entry = self.pipeline_entry;
        let assembly_scope_focus = self.assembly_scope_focus;
        let semantic_i2_enabled = self.semantic_i2_enabled;
        let reserve = max_context_tokens * self.assembly.reserve_for_response_pct() as usize / 100;
        let chain_counter = chain.token_counter();
        Arc::new(move |child_sid: &str| {
            let agent_type = config.agent_type.clone();
            let adjudicator_client = evorule_client.clone();
            let output_validator = config.output_format.as_ref().and_then(|fmt| {
                match OutputValidator::from_output_format(fmt) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "session_spawn: child output validator rebuild failed; validation disabled"
                        );
                        None
                    }
                }
            });
            let context_window = Some(ContextWindowManager::with_approx_counter(
                max_context_tokens,
                reserve,
                TrimStrategy::KeepSystemKeepLast,
            ));
            AgentRunner {
                config: config.clone(),
                evorule_client: evorule_client.clone(),
                llm_handler: llm_handler.clone(),
                tool_handler: tool_handler.clone(),
                memory: memory.clone(),
                delegate_context: delegate_context.clone(),
                session_id: Some(child_sid.to_string()),
                message_persist_mode: message_persist_mode.clone(),
                pending_messages: Vec::new(),
                pending_note_feed: false,
                advised_paths: std::sync::Mutex::new(std::collections::HashSet::new()),
                summary_model: summary_model.clone(),
                context_window,
                summarizer: summarizer.clone(),
                compaction_policy: None,
                cancel_token: CancellationToken::new(),
                output_validator,
                landed_format_instruction: std::sync::Mutex::new(None),
                output_format_retries: 0,
                approval_callback: approval_callback.clone(),
                parallel_tool_cache: Arc::new(std::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
                metrics: metrics.clone(),
                event_callbacks: event_callbacks.clone(),
                memory_event_store: memory_event_store.clone(),
                extractor: extractor.clone(),
                sediment_config: sediment_config.clone(),
                propose_anchor: propose_anchor.clone(),
                max_context_tokens,
                assembly: assembly.clone(),
                tool_result_max_chars,
                token_counter: Some(chain_counter.clone()),
                adjudicator: tokio::sync::Mutex::new(
                    crate::agent::adjudicator::AdjudicationChannel::new(
                        adjudicator_client,
                        &agent_type,
                    ),
                ),
                tool_traces: Arc::new(std::sync::Mutex::new(
                    crate::agent::tool_trace::ToolTraceCollector::default(),
                )),
                journal_dir: journal_dir.clone(),
                acceptance_command: acceptance_command.clone(),
                stagnation: crate::agent::stagnation::StagnationDetector::new(),
                pipeline_entry,
                assembly_scope_focus,
                i2_verdict_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                semantic_i2_enabled,
                active_journal: None,
                accounting_journal: std::sync::Arc::new(std::sync::RwLock::new(None)),
                chain: Some(chain.clone()),
                chain_watch: Some(ChainWatch::new(chain.clone())),
            }
        })
    }

    /// 组装随 LLM 请求下发的工具 OpenAI function schema。
    ///
    /// 数据源与 schema 形状见 [`Self::openai_function_schemas_for`];此处仅决定
    /// 名字清单来源(`tool_handler` 实际注册执行器;agent 配置的 `tools` 列表已在
    /// `from_definition` 校验过 ⊆ 注册集,故以注册集为准即可覆盖配置意图)与
    /// 空集语义(返回 `None` = 请求不携带 tools 键,向后兼容)。
    fn openai_tools_payload(&self) -> Option<Vec<Value>> {
        let registered = self.tool_handler.tool_names();
        let tools = Self::openai_function_schemas_for(&registered);
        if tools.is_empty() {
            None
        } else {
            Some(tools)
        }
    }

    /// 工具名清单 → OpenAI function calling schema 数组。
    ///
    /// runner LLM 请求([`Self::openai_tools_payload`])与 G2 atif 导出端点
    /// (`GET /api/sessions/{id}/atif` 的 tool_definitions)共用此**单一实现**,
    /// 防两处 schema 组装漂移。
    ///
    /// 数据源 = 静态工具 spec 目录(`default_tool_specs` + `rule_tool_specs` +
    /// delegate)与传入名字清单求交(delegate spec 常驻目录,仅在名字命中时
    /// 产出,与旧「注册才并入」语义等价);未知名(自定义注册、服务代理等无
    /// 静态 spec)从服务消费桥注册表透出 description/parameters,无声明时
    /// 降级为最小 schema 并记 debug 日志。
    ///
    /// 形状遵循 OpenAI function calling 标准 JSON Schema:
    /// `{"type":"function","function":{"name","description","parameters":{type:object,properties,required}}}`。
    pub(crate) fn openai_function_schemas_for(names: &[String]) -> Vec<Value> {
        let mut specs = crate::builtin_tools::default_tool_specs();
        // 规则工具静态 spec 并入:rule_tools 的 45 个工具若不在此处,会走 dynamic
        // 分支产出空参数 schema,LLM 无从得知 workspace_id 等必填参数(实测盲传
        // 导致 rule_list 失败)。spec 与执行器同源于 rule_tool_specs()。
        specs.extend(crate::rule_tools::rule_tool_specs());
        specs.push(crate::builtin_tools::delegate_tool::delegate_tool_spec());
        // 自省记忆工具静态 spec(阶段 3 F-611):仅名字命中注册集时产出
        // (见下方循环),静态并入目录无暴露副作用。
        specs.extend(crate::agent::memory_tool::memory_tool_specs());

        let mut tools = Vec::new();
        for name in names {
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
        tools
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
        // F-302:配方三元落账——协议版本(组装行为,码级)+配方版本(数据契约)+配方指纹
        // (内容寻址:同配方同 hash,改配方即变)。同协议下配方数据可独立演进(不改码),
        // 指纹保证回放/审计可锚定实际生效的配方内容。
        let recipe = self.assembly.recipe();
        let recipe_version = recipe.recipe_version.as_str();
        let recipe_json = serde_json::to_string(recipe).unwrap_or_default();
        let recipe_hash = format!("blake3:{}", blake3::hash(recipe_json.as_bytes()).to_hex());
        params["effective_params"] = serde_json::json!({
            "temperature": effective_temperature,
            "max_tokens": effective_max_tokens,
            "stream": effective_stream,
            "assembly_protocol_version": ASSEMBLY_PROTOCOL_VERSION,
            "assembly_recipe_version": recipe_version,
            "assembly_recipe_hash": recipe_hash,
        });
        // LM-2 资产化:MemoryRecipe 指纹随首轮落链(有 Recipe 时)——回放可
        // 锚定实际生效的记忆策略版本
        if let Some((m_version, m_hash)) = self.memory.as_ref().and_then(|m| m.recipe_fingerprint())
        {
            params["effective_params"]["memory_recipe_version"] = serde_json::json!(m_version);
            params["effective_params"]["memory_recipe_hash"] = serde_json::json!(m_hash);
        }
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
        goal: &str,
        step: usize,
    ) -> Result<Value, AgentError> {
        let io_type = payload
            .get("io_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Internal("missing io_type in IoRequest".to_string()))?;

        let params = payload.get("params").cloned().unwrap_or(Value::Null);

        match io_type {
            "call_external" => {
                self.handle_call_external(session_id, &params, messages, goal, step)
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
        goal: &str,
        step: usize,
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
                    // R3:摘要生成 + PayloadUpdate 落链 + hint 替换（helper 共用）
                    if let Err(e) = self
                        .handle_summary_outcome(
                            session_id,
                            summarizer,
                            &trim_result.dropped,
                            &mut trim_result.messages,
                            goal,
                            "summarize",
                        )
                        .await
                    {
                        warn!(%session_id, error = %e, "R3: summary handling failed, keeping original hint");
                    }
                }
            }
            trim_result.messages
        } else {
            messages.clone()
        };

        // H1(自主交接设计):窗口余量信号行——每轮请求前按当前发送集计数,
        // 使用率>70% 才注入(以下静默,纯聚焦信号不强制动作);口径与 G2 裁剪
        // /agent_api 暴露同源(零第二套计数),回合数=ReAct step。
        if let Some(ctx) = &self.context_window {
            let used = ctx.count(&messages_to_send);
            if let Some(line) = context_signal_line(used, ctx.budget(), ctx.window_tokens(), step) {
                for msg in &mut messages_to_send {
                    if let Message::System { content } = msg {
                        content.push_str(&line);
                        break;
                    }
                }
            }
        }

        // G11:注入格式指令到 system prompt(只影响本次请求的 messages_to_send,不改原 messages)
        // R3-b/G-7 收口:指令落链(影响输出必落链,RL-A2);同指令去重;失败 best-effort 留痕
        if let Some(validator) = &self.output_validator {
            let instruction = validator.instruction();
            if !instruction.is_empty() {
                for msg in &mut messages_to_send {
                    if let Message::System { content } = msg {
                        content.push_str(instruction);
                        break;
                    }
                }
                let needs_land = {
                    let landed = self
                        .landed_format_instruction
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    landed.as_deref() != Some(instruction)
                };
                if needs_land {
                    match self
                        .evorule_client
                        .update_payload(
                            session_id,
                            "__context__.format_instruction",
                            &serde_json::json!({ "format_instruction": instruction }),
                        )
                        .await
                    {
                        Ok(_fact_id) => {
                            let mut landed = self
                                .landed_format_instruction
                                .lock()
                                .unwrap_or_else(|p| p.into_inner());
                            *landed = Some(instruction.to_owned());
                            info!(%session_id, "R3: format instruction landed (G-7 closed)");
                        }
                        Err(e) => warn!(
                            %session_id,
                            error = %e,
                            "R3: format instruction landing failed (best-effort)——G-7 divergence, doctor flags"
                        ),
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
        // 预执行=管道并行实例(PR-3):产物=已过闸结果,存入 parallel_tool_cache,
        // 后续 call_service 命中走缓存收口路径(①-⑤⑧照常仅⑦免重执行)
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

        // 判据自检门禁——task_done/task_blocked 提交前过验收;
        // 拒绝时详情作为 tool_result 回喂 LLM(指令不提交引擎)
        let args = match apply_acceptance_gate(self.acceptance_command.as_deref(), &args).await {
            GateOutcome::Allow(a) => a,
            GateOutcome::Reject(detail) => {
                warn!(%session_id, %detail, "acceptance gate rejected instruction submission");
                return Ok(serde_json::json!({
                    "status": "rejected_by_acceptance_gate",
                    "detail": detail,
                    "guidance": "task_done 需判据自检通过(acceptance_passed=true);task_blocked 需非空 reason。请继续工作或按 fail-visible 诚实退出。"
                }));
            }
        };

        // G13:检查并行缓存(如果 call_external 已并行执行过此 active 工具,直接返回缓存结果,跳过重复执行 + 审批)
        // candidate 工具(proposal)不会被缓存,所以缓存命中的一定是 active 工具,无需审批
        // 审批留痕:仅本轮发生过审批时为 Some(内嵌进 io_response.result)
        // PR-3 缓存收口:命中值=预执行管道实例的已过闸产物——命中调用仍走
        // 完整管道(①-⑤⑧照常,仅⑦免重执行),意图/裁决/账面逐调用在场。
        let (tool_result, approval_record) = if let Some(cached) =
            self.check_parallel_cache(tool_name, &args)
        {
            info!(%session_id, tool = %tool_name, "G13: call_service cache hit, serving gated result (gates re-run, execution memoized)");
            self.execute_tool_call_gated(session_id, tool_name, &args, None, Some(cached))
                .await?
        } else {
            // 缓存未命中:统一走 gated 入口——candidate 由管道⑤评估臂先行
            // 拦截(PR-4 收编,暂停→决策端),遗留 proposal 形态由兼容臂承接。
            // 第一次调用(不带 approved flag):LLM 自带的 approved 旗标
            // 强制剥离,决策门唯一控制权归 runner。
            let first_args = strip_approved_flag(&args);
            self.execute_tool_call_gated(session_id, tool_name, &first_args, None, None)
                .await?
        };

        tool_calls.push(tool_name.to_string());
        // 停滞检测(F2 空转克星)——观察(工具,参数,结果)三元组;
        // Warning/Exhausted 标记随结果回喂 LLM(fail-visible,下一轮可见)
        let stagnation_verdict =
            self.stagnation
                .observe(tool_name, &args.to_string(), &tool_result.to_string());
        let tool_idx = messages.len();
        // 回喂 LLM 的入列值按上限截断;审计链持久化保留原始全文(事实记录)
        let raw_content = tool_result.to_string();
        let tool_msg = Message::Tool {
            content: truncate_tool_result(raw_content.clone(), self.tool_result_max_chars),
            tool_name: tool_name.to_string(),
        };
        messages.push(tool_msg);
        // P0: 持久化 tool 消息
        self.persist_message(
            session_id,
            tool_idx,
            Message::Tool {
                content: raw_content,
                tool_name: tool_name.to_string(),
            },
        )
        .await?;

        let mut result = serde_json::json!({
            "tool_name": tool_name,
            "result": tool_result.to_string(),
        });
        match stagnation_verdict {
            StagnationVerdict::Normal => {}
            StagnationVerdict::Warning { repeat_count } => {
                warn!(%session_id, tool = %tool_name, repeat_count, "stagnation warning (F2)");
                result["stagnation"] = serde_json::json!(STAGNATION_WARNING_MARK);
            }
            StagnationVerdict::Exhausted => {
                warn!(%session_id, tool = %tool_name, "stagnation EXHAUSTED (F2)——按 H2 阻塞收尾指引");
                result["stagnation"] = serde_json::json!(STAGNATION_EXHAUSTED_MARK);
            }
        }
        if let Some(record) = approval_record {
            result["approval"] = record;
        }
        Ok(result)
    }

    /// G8 执行入口的缓存收口形态(PR-3):`precomputed=Some` 为 G13 并行缓存
    /// 命中——缓存键=「已过门禁的证据」(预执行管道实例的落账结果),命中调用
    /// ①-⑤与⑧随本次调用照常(意图/裁决/账面逐调用在场,P0-3/A1 关闭判据),
    /// 仅⑦免重执行。审批重执行路径恒传 None(批准后的动作必须真实执行)。
    ///
    /// PR-4 ⑤收编:本函数为工具面唯一决策编排点——
    /// - 管道⑤暂停(Denial(ApprovalPending),收编型 candidate proposal)→
    ///   统一决策端([`Self::decide_and_reexecute`]);
    /// - ⑦结果解析命中 proposal(未收编工具自管协议的遗留形态,兼容臂)→
    ///   同一决策端;
    /// - 其余结局=translate 直传。
    ///
    /// 返回 `(工具最终结果, 审批留痕 record)`:record 仅在本轮发生过审批时
    /// 为 `Some`(内嵌进 io_response.result,不扩 Fact 枚举)。
    async fn execute_tool_call_gated(
        &self,
        session_id: &str,
        tool_name: &str,
        args: &Value,
        journal: Option<&crate::agent::journal::JournalWriter>,
        precomputed: Option<Value>,
    ) -> Result<(Value, Option<Value>), AgentError> {
        // 一管道(工具面统一架构 §3.2):本函数为管道的薄翻译层——
        // 八阶段(①查表②聚焦③意图④裁决⑤审批⑥沙箱位⑦执行⑧落账)由
        // ToolExecutionPipeline 执行;治理拦截两态 JSON 文案/裁决 fail-closed
        // 语义/轨迹与指标采集点经管道逐字保留(行为等价,runner_tests 基线
        // 不改一行即验收)。PR-3 起 G13 并行预执行同为管道并行实例
        // (ParallelPreflight 入口),两入口共用 run_pipeline 单点;delegate
        // 统一装配批起子代理路径入口类由 runner 标记决定(React 主路径 /
        // Delegate 子代理),账面 caller.entry 落账。
        let outcome = self
            .run_pipeline(
                tool_name,
                args,
                journal,
                precomputed,
                self.pipeline_entry,
                None,
            )
            .await;
        let ledger = outcome.ledger;
        match outcome.result {
            Ok(result) => {
                // 兼容臂:未收编工具自管协议遗留形态(⑦结果即 proposal JSON)
                // 仍走统一决策端——收编后内置 candidate 工具不再产生此形态
                // (评估臂在⑤先行拦截,call 直接执行)。
                let result_str = result.to_string();
                if let Some(req) = parse_approval_request(session_id, tool_name, args, &result_str)
                {
                    return self
                        .decide_and_reexecute(session_id, tool_name, args, req, journal)
                        .await;
                }
                Ok((result, None))
            }
            Err(crate::agent::pipeline::PipelineFailure::Denial(denial))
                if denial.stage == crate::agent::pipeline::DenialStage::ApprovalPending =>
            {
                // ⑤收编暂停:proposal JSON 由评估臂生成(payload 在案),
                // parse 取回 ApprovalRequest(proposal_id 三方一致)
                let payload = denial.llm_payload.unwrap_or_else(
                    || serde_json::json!({"status": "needs_approval", "tool": tool_name}),
                );
                let payload_str = payload.to_string();
                let req = parse_approval_request(session_id, tool_name, args, &payload_str)
                    .ok_or_else(|| {
                        AgentError::Internal(format!(
                            "approval pending payload is not a parsable proposal: {}",
                            denial.reason
                        ))
                    })?;
                self.decide_and_reexecute(session_id, tool_name, args, req, journal)
                    .await
            }
            other => Self::translate_pipeline_outcome(
                crate::agent::pipeline::PipelineOutcome {
                    result: other,
                    ledger,
                },
                tool_name,
            )
            .map(|v| (v, None)),
        }
    }

    /// 主路径聚焦快照(注册面 ∪ 静态表面;装配面聚焦时=注册面本身)
    ///
    /// PR-2 行为等价口径:②不产生新拒绝(裸 runner/CLI 直构造场景 handler
    /// 为空但工具名有效——静态表内,原实现直达裁决不查 handler,①查表必须
    /// 同样放行才是行为等价)。收窄为注册面严格子集随装配收口批次落地
    /// (B2 断言测试盯守);G13 并行预执行实例与本入口共用本构造。
    /// delegate 统一装配批:子代理路径(装配面聚焦标记)快照=注册面本身——
    /// LLM 契约面(messages 侧 tools payload)与②聚焦允许面同源。
    fn main_path_focus(&self) -> crate::agent::pipeline::FocusSnapshot {
        let registered = self.tool_handler.tool_names();
        if self.assembly_scope_focus {
            return crate::agent::pipeline::FocusSnapshot::from_names(registered);
        }
        crate::agent::pipeline::FocusSnapshot::from_names(
            registered.into_iter().chain(
                crate::agent::tool_manifest::static_manifests()
                    .into_iter()
                    .map(|m| m.name),
            ),
        )
    }

    /// 管道执行单点(两入口共用:React 主路径 / ParallelPreflight 并行实例)
    ///
    /// 聚焦快照/查表合并视图/依赖装配统一在此,入口差异仅 CallerContext.entry
    /// 与 precomputed(缓存收口面)。裁决通道为 runner 持有的 tokio Mutex——
    /// 并行实例在③④天然串行(设计档 §3.2 Mutex 语义保持)。
    /// PR-4 ⑤收编:proposal_of 按注册执行器实例求值(评估与执行同源);
    /// approval_preset 为预供给决策(ApprovalReexec 重执行入口消费)。
    async fn run_pipeline(
        &self,
        tool_name: &str,
        args: &Value,
        journal: Option<&crate::agent::journal::JournalWriter>,
        precomputed: Option<Value>,
        entry: crate::agent::pipeline::PipelineEntry,
        approval_preset: Option<crate::agent::approval::ApprovalDecision>,
    ) -> crate::agent::pipeline::PipelineOutcome {
        // 查表合并视图(一表两源):运行时注册条目优先(可覆盖静态同名),
        // 未注册回落静态表。
        let manifest_of = |name: &str| {
            self.tool_handler
                .manifest(name)
                .or_else(|| crate::agent::tool_manifest::lookup_static(name))
        };
        // ⑤评估单源:按 handler 注册条目的执行器实例求值(冒名注册以实际
        // 执行器为准——EchoTool 注册在 candidate 名下亦无协议)。
        let proposal_of =
            move |name: &str, args: &Value| self.tool_handler.evaluate_proposal_for(name, args);
        let focus = self.main_path_focus();
        let deps = crate::agent::pipeline::PipelineDeps {
            executor: self,
            adjudicator: &self.adjudicator,
            manifest_of: &manifest_of,
            proposal_of: Some(&proposal_of),
            journal: journal.map(|j| j as &(dyn crate::agent::pipeline::PolicyJudgedSink + Sync)),
            boundary: self.config.capability_boundary.as_ref(),
            traces: Some(&self.tool_traces),
            metrics: self.metrics.as_deref(),
            retry_backoff: std::time::Duration::from_secs(1),
        };
        let req = crate::agent::pipeline::PipelineRequest {
            tool_name,
            args,
            caller: crate::agent::pipeline::CallerContext {
                entry,
                session_id: self.session_id.clone(),
            },
            focus: &focus,
            precomputed,
            approval_preset,
        };
        crate::agent::pipeline::ToolExecutionPipeline
            .execute(req, deps)
            .await
    }

    /// 管道结局 → LLM 可见面翻译(两入口共用;错误语义逐臂与原实现等价)
    fn translate_pipeline_outcome(
        outcome: crate::agent::pipeline::PipelineOutcome,
        tool_name: &str,
    ) -> Result<Value, AgentError> {
        match outcome.result {
            Ok(value) => Ok(value),
            // 阶段⑦执行错误 = 原样透传(错误显式回喂,与原实现一致)
            Err(crate::agent::pipeline::PipelineFailure::Execution(e)) => Err(e),
            Err(crate::agent::pipeline::PipelineFailure::Denial(denial)) => match denial.stage {
                // ①查表拒绝 = 原 ToolHandler not-found 错误文本(行为等价);
                // ②聚焦/⑤审批拒绝 = 显式错误上抛
                crate::agent::pipeline::DenialStage::NoManifest
                | crate::agent::pipeline::DenialStage::OutOfFocus
                | crate::agent::pipeline::DenialStage::ApprovalDenied => {
                    Err(AgentError::ToolError(denial.reason))
                }
                // ⑤审批暂停 = 决策端（execute_tool_call_gated 内统一决策）的
                // 消化对象；漏到翻译层 = 编排缺失（fail-visible 显式内部错误）
                crate::agent::pipeline::DenialStage::ApprovalPending => {
                    Err(AgentError::Internal(format!(
                        "approval pending leaked to translation layer (no adjudication \
                         orchestrator consumed it): {}",
                        denial.reason
                    )))
                }
                // ③④通道故障/⑧账面失败 = 显式内部错误(fail-closed/fail-visible)
                crate::agent::pipeline::DenialStage::Channel
                | crate::agent::pipeline::DenialStage::Ledger => {
                    Err(AgentError::Internal(denial.reason))
                }
                // 治理拦截 = 原状 Ok(blocked JSON) 回喂 LLM(被拦调用也是真实
                // 执行史,作为工具结果进对话——与原实现一致)
                crate::agent::pipeline::DenialStage::Governance => {
                    Ok(denial.llm_payload.unwrap_or_else(|| {
                        serde_json::json!({
                            "status": "blocked_by_governance_rule",
                            "tool": tool_name,
                            "reason": denial.reason,
                        })
                    }))
                }
            },
        }
    }

    /// 自主交接批:handover_write 成功落 journal handover_written(结构化锚:
    /// path/id;写档动作镜像已在 tool_invoked/tool_result,本事件供跨会话
    /// 链对账)。非 handover_write/无 path 锚/无 journal = 不落(语义事件
    /// 仅成功形态在场,失败由调用镜像覆盖)。
    fn record_handover_written(
        journal: Option<&crate::agent::journal::JournalWriter>,
        session_id: &str,
        tool_name: &str,
        result: &Value,
    ) {
        if tool_name != crate::agent::handover_tool::HANDOVER_WRITE_TOOL {
            return;
        }
        let Some(j) = journal else {
            return;
        };
        let path = result.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            return;
        }
        let schema_ok = result
            .get("schema_ok")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Err(e) = j.handover_written(session_id, path, schema_ok) {
            tracing::warn!(error = %e, "handover_written journal append failed");
        }
    }

    /// 自主交接 PR-H3 批:session_spawn 成功落 journal session_spawned(镜像
    /// server 侧 parent 因果链;调用镜像已在 tool_invoked/tool_result,本事件
    /// 供链审计对账)。非 session_spawn/无 child 锚/无 journal = 不落(语义
    /// 事件仅成功形态在场,失败由调用镜像覆盖)。
    fn record_session_spawned(
        journal: Option<&crate::agent::journal::JournalWriter>,
        session_id: &str,
        tool_name: &str,
        result: &Value,
    ) {
        if tool_name != crate::agent::session_spawn_tool::SESSION_SPAWN_TOOL {
            return;
        }
        let Some(j) = journal else {
            return;
        };
        let child = result
            .get("child_session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if child.is_empty() {
            return;
        }
        let depth = result
            .get("chain_depth")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        if let Err(e) = j.session_spawned(session_id, child, depth) {
            tracing::warn!(error = %e, "session_spawned journal append failed");
        }
    }

    /// 阶段一(流式路径):执行工具并解析审批请求,不做决策
    ///
    /// 供 stream! 生成器在 yield ApprovalRequired **之前**调用 —— 帧必须在
    /// 60s 审批窗口开启后、超时前到达前端,否则 HTTP 审批结构性不可用。
    /// 无审批(含缓存命中)时返回 [`ToolExecStage::Done`],一步到位。
    ///
    /// PR-4 ⑤收编:本阶段直调管道原始结局(不经 gated 决策编排——决策必须
    /// 留给事件循环,ApprovalRequired 帧时序约束)。收编型 candidate 在⑤
    /// 暂停(ApprovalPending 携带 proposal JSON)=流式 Pending 语义;兼容型
    /// (遗留 proposal 形态)经⑦结果解析识别,两型统一转 Pending。
    async fn execute_tool_stage(
        &self,
        session_id: &str,
        tool_name: &str,
        args: &Value,
        journal: Option<&crate::agent::journal::JournalWriter>,
    ) -> Result<ToolExecStage, AgentError> {
        // G13:并行缓存命中(如果 call_external 已并行执行过此 active 工具,
        // 直接返回缓存结果,跳过重复执行 + 审批;candidate 工具不缓存)
        // PR-3 缓存收口:命中调用仍走完整管道(①-⑤⑧照常,仅⑦免重执行——
        // 缓存值=预执行管道实例的已过闸产物),意图/裁决/账面逐调用在场。
        if let Some(cached) = self.check_parallel_cache(tool_name, args) {
            info!(%session_id, tool = %tool_name, "G13: cache hit, serving gated result (gates re-run, execution memoized)");
            let (final_result, _record) = self
                .execute_tool_call_gated(session_id, tool_name, args, journal, Some(cached))
                .await?;
            Self::record_handover_written(journal, session_id, tool_name, &final_result);
            Self::record_session_spawned(journal, session_id, tool_name, &final_result);
            return Ok(ToolExecStage::Done(ToolExecOutcome {
                final_result,
                approval_record: None,
                approval_flow: None,
            }));
        }

        // G8:第一次调用(不带 approved flag)。LLM 自带 approved 旗标强制
        // 剥离(决策门唯一控制权归 runner)。
        let first_args = strip_approved_flag(args);
        let outcome = self
            .run_pipeline(
                tool_name,
                &first_args,
                journal,
                None,
                crate::agent::pipeline::PipelineEntry::React,
                None,
            )
            .await;
        let ledger = outcome.ledger;
        match outcome.result {
            Ok(tool_result) => {
                // 兼容臂:未收编工具自管协议遗留形态
                let result_str = tool_result.to_string();
                match parse_approval_request(session_id, tool_name, &first_args, &result_str) {
                    None => {
                        Self::record_handover_written(journal, session_id, tool_name, &tool_result);
                        Self::record_session_spawned(journal, session_id, tool_name, &tool_result);
                        Ok(ToolExecStage::Done(ToolExecOutcome {
                            final_result: tool_result,
                            approval_record: None,
                            approval_flow: None,
                        }))
                    }
                    Some(req) => Ok(ToolExecStage::Pending(req)),
                }
            }
            Err(crate::agent::pipeline::PipelineFailure::Denial(denial))
                if denial.stage == crate::agent::pipeline::DenialStage::ApprovalPending =>
            {
                // ⑤收编暂停:proposal JSON 由评估臂生成 → 流式 Pending
                let payload = denial.llm_payload.unwrap_or_else(
                    || serde_json::json!({"status": "needs_approval", "tool": tool_name}),
                );
                let payload_str = payload.to_string();
                let req = parse_approval_request(session_id, tool_name, &first_args, &payload_str)
                    .ok_or_else(|| {
                        AgentError::Internal(format!(
                            "approval pending payload is not a parsable proposal: {}",
                            denial.reason
                        ))
                    })?;
                Ok(ToolExecStage::Pending(req))
            }
            other => {
                let final_result = Self::translate_pipeline_outcome(
                    crate::agent::pipeline::PipelineOutcome {
                        result: other,
                        ledger,
                    },
                    tool_name,
                )?;
                Ok(ToolExecStage::Done(ToolExecOutcome {
                    final_result,
                    approval_record: None,
                    approval_flow: None,
                }))
            }
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
        journal: Option<&crate::agent::journal::JournalWriter>,
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
            // 批准 → 管道 ApprovalReexec 重执行(不递归检查 proposal):
            // 预供给决策由⑤消费(收编型直通⑦真实执行),args 注入 approved:true
            // 兼容未收编工具的自管检查
            info!(%session_id, tool = %tool_name, "G8: tool call approved, re-executing via pipeline (preset decision)");
            let mut approved_args = args.clone();
            if let Some(obj) = approved_args.as_object_mut() {
                obj.insert("approved".to_string(), Value::Bool(true));
            } else {
                approved_args = serde_json::json!({"original_args": args, "approved": true});
            }
            let outcome = self
                .run_pipeline(
                    tool_name,
                    &approved_args,
                    journal,
                    None,
                    crate::agent::pipeline::PipelineEntry::ApprovalReexec,
                    Some(decision.clone()),
                )
                .await;
            Self::translate_pipeline_outcome(outcome, tool_name)?
        };

        // PR-H4 验收修复:审批恢复路径的语义事件落账与常规路径同钩位——
        // 此前单侧落点,走 G8 审批的 session_spawn 在 journal 缺
        // session_spawned(server 因果链权威无损,账面镜像缺席;实测 844
        // 直通路径有、847 审批路径无即此根因)。handover_write 同理对称补挂。
        Self::record_handover_written(journal, session_id, tool_name, &final_result);
        Self::record_session_spawned(journal, session_id, tool_name, &final_result);

        // 人工审查开合:决策事件入审计链(tool_trace 条目附加 approval 子对象;
        // 批准=重执行条目,拒绝=proposal 首调条目;candidate 始终串行无交错)
        if let Ok(mut tt) = self.tool_traces.lock() {
            tt.attach_approval_to_last(record.clone());
        }

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

    /// G13:并行预执行单个工具=管道并行实例(PR-3 入口收口)
    ///
    /// 预执行与主路径共用 run_pipeline 单点(ParallelPreflight 入口)——八阶段
    /// 全部在场(意图/裁决/审批/落账逐调用发生),产物=已过闸结果,不再存在
    /// 零门禁直调 execute_by_name 的旁路窗口(修 P0-3/A1)。裁决阶段经共享
    /// tokio Mutex 天然串行(设计档 §3.2 Mutex 语义保持)。失败不中断其他
    /// 并行工具:执行错误/拒绝折叠为 error JSON(拒绝中治理拦截保留两态
    /// blocked 原文案,被拦调用也是真实执行史)。
    ///
    /// 观测由管道⑦⑧统一采集(G17 同点同规格),此处不再手工记账。
    async fn execute_single_tool(&self, tc: &crate::agent::translator::ToolCall) -> Value {
        let outcome = self
            .run_pipeline(
                &tc.name,
                &tc.arguments,
                None,
                None,
                crate::agent::pipeline::PipelineEntry::ParallelPreflight,
                None,
            )
            .await;
        match outcome.result {
            Ok(result) => result,
            Err(crate::agent::pipeline::PipelineFailure::Execution(e)) => {
                let mut map = serde_json::Map::new();
                map.insert("status".to_string(), Value::from("error"));
                map.insert("error".to_string(), Value::from(e.to_string()));
                Value::Object(map)
            }
            Err(crate::agent::pipeline::PipelineFailure::Denial(denial)) => {
                match denial.stage {
                    // 治理拦截保留两态 blocked JSON(与主路径文案逐字一致)
                    crate::agent::pipeline::DenialStage::Governance => {
                        denial.llm_payload.unwrap_or_else(|| {
                            serde_json::json!({
                                "status": "blocked_by_governance_rule",
                                "tool": tc.name,
                                "reason": denial.reason,
                            })
                        })
                    }
                    // ⑤审批暂停(PR-4 收编):candidate proposal 的预执行形态——
                    // 映射回 proposal JSON 原文,execute_tools_parallel 的
                    // parse 命中 → 不入缓存(G13 语义保持:candidate 不缓存,
                    // 审批留给 call_service/流式主路径)
                    crate::agent::pipeline::DenialStage::ApprovalPending => {
                        denial.llm_payload.unwrap_or_else(|| {
                            serde_json::json!({
                                "status": "needs_approval",
                                "tool": tc.name,
                            })
                        })
                    }
                    // 其余拒绝(①②⑤拒绝/③④通道/⑧账面)= error JSON 显式回喂,
                    // 不中断并行批次中其他工具
                    _ => {
                        let mut map = serde_json::Map::new();
                        map.insert("status".to_string(), Value::from("error"));
                        map.insert("error".to_string(), Value::from(denial.reason));
                        Value::Object(map)
                    }
                }
            }
        }
    }

    /// G13:并行执行多个 tool_calls
    ///
    /// - 所有工具=管道并行实例并行执行(`futures::future::join_all`,PR-3 起
    ///   无任何门禁豁免——治理级工具同样走完整管道,临时加固已随本体修复移除)
    /// - active 工具(返回非 proposal)的结果存入缓存,后续 call_service 命中
    ///   缓存走缓存收口路径(①-⑤⑧照常仅⑦免重执行)
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
            "G13: executing tool calls in parallel (pipeline preflight instances)"
        );

        // 构造 futures:每个 tool_call 一个管道并行实例
        let futures: Vec<_> = tool_calls
            .iter()
            .map(|tc| async move {
                let name = tc.name.clone();
                let args = tc.arguments.clone();
                let result = self.execute_single_tool(tc).await;
                (name, args, result)
            })
            .collect();

        // 并行执行(join_all 保证顺序与输入一致;裁决阶段经共享 Mutex 串行)
        let results = futures_util::future::join_all(futures).await;

        // 缓存 active 工具结果(非 proposal)——缓存键=「已过门禁的证据」
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

    /// G8:统一审批决策端(决策 → 留痕 → 拒绝回喂 / 批准重执行)
    ///
    /// 流程:
    /// 1. 通过 `approval_callback` 问用户(无 callback = 默认拒绝)
    /// 2. 用户批准 → 管道 ApprovalReexec 重执行(preset 消费)
    /// 3. 用户拒绝 → 返回 `{"status":"rejected"}`
    ///
    /// **不递归**:重调用的结果不再检查 proposal(避免无限循环)。
    ///
    /// PR-4 ⑤收编定位:统一决策端——管道⑤暂停(收编型 candidate)与兼容臂
    /// (未收编工具遗留 proposal 形态)共用本方法完成决策与重执行。批准重调
    /// 走管道 ApprovalReexec 入口+预供给决策(⑤消费批准结论→⑦真实执行),
    /// 同时保留 args 注入 approved:true(未收编工具的自管检查兼容形态)。
    ///
    /// 返回 `(工具最终结果, 审批留痕 record)`:record 仅在本轮发生过审批时
    /// 为 `Some`(内嵌进 io_response.result,不扩 Fact 枚举)。
    async fn decide_and_reexecute(
        &self,
        session_id: &str,
        tool_name: &str,
        args: &Value,
        approval_req: ApprovalRequest,
        journal: Option<&crate::agent::journal::JournalWriter>,
    ) -> Result<(Value, Option<Value>), AgentError> {
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
            // 人工审查开合:决策事件入审计链(拒绝不重执行,附加到载体条目——
            // 收编型=管道⑤已落的 approval_pending 条目;兼容型=proposal 首调
            // 执行条目)
            if let Ok(mut tt) = self.tool_traces.lock() {
                tt.attach_approval_to_last(approval_record.clone());
            }
            return Ok((
                Value::from(r#"{"status":"rejected","message":"User denied approval"}"#),
                Some(approval_record),
            ));
        }

        // 用户批准 → 管道 ApprovalReexec 重执行:预供给决策由⑤消费(收编型
        // 直通⑦真实执行),args 注入 approved:true 兼容未收编工具的自管检查
        info!(%session_id, tool = tool_name, "G8: tool call approved, re-executing via pipeline (preset decision)");
        let mut approved_args = args.clone();
        if let Some(obj) = approved_args.as_object_mut() {
            obj.insert("approved".to_string(), Value::Bool(true));
        } else {
            // args 不是 object,包装一下
            approved_args = serde_json::json!({"original_args": args, "approved": true});
        }
        let outcome = self
            .run_pipeline(
                tool_name,
                &approved_args,
                journal,
                None,
                crate::agent::pipeline::PipelineEntry::ApprovalReexec,
                Some(decision),
            )
            .await;
        // 人工审查开合:决策事件入审计链(附加到重执行条目)
        if let Ok(mut tt) = self.tool_traces.lock() {
            tt.attach_approval_to_last(approval_record.clone());
        }
        let final_result = Self::translate_pipeline_outcome(outcome, tool_name)?;
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

    async fn auto_recall(&mut self, session_id: &str) -> Result<Vec<u64>, AgentError> {
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

        if let Some(mem) = self.memory.as_mut() {
            // L1 修复(断点5-水合,CLI 路径):auto_recall 是 CLI 模式的召回主路径
            // (不走 recall_context)——events 分区事实在此注入水合暂存区,
            // 会话末 flush_usage/apply_lifecycle 吸收进 cache,跨会话晋升链
            // 在 CLI 模式闭合。fact_id 从 SharedFactEntry 带入(镜像匹配键)。
            {
                let mut hp = mem
                    .hydration_pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                for (fact, _) in &latest {
                    if fact.path.contains(".events.")
                        || fact.path.contains("shared.") && fact.path.contains(".events")
                    {
                        // path 形如 shared.{ns}.events.{event_id} → cache_key=shared::{ns}.events.{id}
                        // 与 path_to_cache_key 同构(Shared 域 strip 前缀)
                        let ns = &self.sediment_config.namespace;
                        if let Some(key) = fact
                            .path
                            .strip_prefix(&format!("shared.{ns}."))
                            .map(|k| format!("shared::{k}"))
                        {
                            if let Ok(mut r) = serde_json::from_value::<
                                crate::agent::memory::MemoryRecord,
                            >(fact.value.clone())
                            {
                                r.fact_id = Some(fact.fact_id);
                                hp.push((key, r));
                            }
                        }
                    }
                }
            }
            // 直写主体 cache——原实现 clone 后写,server 有写但主体
            // cache 永不含该条目(靠 B3 对账回填,对账前视图不一致)
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

    /// 悬挂工具处置计划(纯函数):定位历史尾部未配对的 assistant tool_calls。
    /// 判定:最后一个带非空 tool_calls 的 assistant 之后无任何 Tool 消息=
    /// 该批 calls 全部悬挂(按 L1 幂等分类路由);有 Tool 跟随=视作已配对
    /// (并行部分完成的边角留待后续精化)。返回 (工具名, 参数, 动作) 列表。
    fn plan_dangling_repair(
        messages: &[Message],
    ) -> Vec<(String, serde_json::Value, DanglingAction)> {
        let mut last_calls: Option<&Vec<crate::agent::translator::ToolCall>> = None;
        for m in messages.iter() {
            if let Message::Assistant {
                tool_calls: Some(calls),
                ..
            } = m
            {
                if !calls.is_empty() {
                    last_calls = Some(calls);
                }
            }
        }
        let Some(calls) = last_calls else {
            return Vec::new();
        };
        // 尾部配对检查:最后一个 tool_calls assistant 之后不能再有 Tool 消息
        let last_assistant_idx = messages
            .iter()
            .rposition(
                |m| matches!(m, Message::Assistant { tool_calls: Some(c), .. } if !c.is_empty()),
            )
            .unwrap_or(0);
        if messages[last_assistant_idx + 1..]
            .iter()
            .any(|m| matches!(m, Message::Tool { .. }))
        {
            return Vec::new();
        }
        calls
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    c.arguments.clone(),
                    match crate::agent::tool_retry::retry_class(&c.name) {
                        crate::agent::tool_retry::RetryClass::IdempotentRead => {
                            DanglingAction::Reexecute
                        }
                        _ => DanglingAction::CrashObservation,
                    },
                )
            })
            .collect()
    }

    /// 悬挂工具处置执行(恢复面):按 [`Self::plan_dangling_repair`] 的计划逐
    /// call 处置——幂等读经既有管道重执行(治理面全程在闸),结果与观察均作为
    /// Tool 消息回填历史并持久化(transcript 权威面同步),历史配对自此完整。
    async fn repair_dangling_tail(
        &mut self,
        session_id: &str,
        messages: &mut Vec<Message>,
        journal: Option<&crate::agent::journal::JournalWriter>,
    ) -> Result<DanglingRepairReport, String> {
        let plan = Self::plan_dangling_repair(messages);
        let mut report = DanglingRepairReport::default();
        for (name, arguments, action) in plan {
            report.dangling += 1;
            let content = match action {
                DanglingAction::Reexecute => {
                    match self
                        .execute_tool_call_gated(session_id, &name, &arguments, journal, None)
                        .await
                    {
                        Ok((result, _)) => {
                            report.reexecuted += 1;
                            truncate_tool_result(result.to_string(), self.tool_result_max_chars)
                        }
                        Err(e) => {
                            report.observed += 1;
                            serde_json::json!({
                                "status": "error",
                                "error": "process crashed before this tool executed; re-issue if needed",
                                "retry_error": e.to_string(),
                            })
                            .to_string()
                        }
                    }
                }
                DanglingAction::CrashObservation => {
                    report.observed += 1;
                    serde_json::json!({
                        "status": "error",
                        "error": "process crashed before this tool executed; re-issue if needed",
                    })
                    .to_string()
                }
            };
            let msg = Message::Tool {
                content,
                tool_name: name,
            };
            let idx = messages.len();
            messages.push(msg.clone());
            self.persist_message(session_id, idx, msg)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(report)
    }

    /// 挂起 io_request 对账(纯函数):history 事实数组中 IoRequest 与
    /// IoResponse 按 id 差集 = 未响应请求。多个挂起取 id 最大者(引擎串行
    /// 处理,同时至多一个挂起)。返回 None = 无挂起(崩溃落在轮界)。
    fn plan_pending_resolution(
        history: &serde_json::Value,
    ) -> Option<(u64, String, serde_json::Value)> {
        let facts: &[serde_json::Value] = match history {
            serde_json::Value::Array(a) => a,
            serde_json::Value::Object(o) => o.get("facts").and_then(|f| f.as_array())?,
            _ => return None,
        };
        let mut requested: Vec<(u64, String, serde_json::Value)> = Vec::new();
        let mut responded: Vec<u64> = Vec::new();
        for f in facts {
            let typ = f.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match typ {
                "IoRequest" => {
                    if let Some(id) = f.get("id").and_then(|v| v.as_u64()) {
                        requested.push((
                            id,
                            f.get("io_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            f.get("params").cloned().unwrap_or(serde_json::Value::Null),
                        ));
                    }
                }
                "IoResponse" => {
                    if let Some(rid) = f.get("request_id").and_then(|v| v.as_u64()) {
                        responded.push(rid);
                    }
                }
                _ => {}
            }
        }
        requested.reverse(); // 最新优先(引擎串行,至多一个挂起;防御性取最新)
        requested
            .into_iter()
            .find(|(id, _, _)| !responded.contains(id))
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
            .filter_map(|rec| rec_to_message(&rec, runner.tool_result_max_chars))
            .collect();

        // R3/G-3：回读滚动摘要种子——continuation/重启后缓存从账上恢复，
        // 不触发 LLM 重算（重建读账不重算；G-3 关闭）
        if let Some(summarizer) = runner.summarizer.as_ref() {
            if let Some((frozen, text, gen)) =
                crate::agent::memory::MemoryManager::rolling_summary_from_state(
                    &state, &namespace, session_id,
                )
            {
                summarizer.seed_cache(frozen, text, gen);
                info!(%session_id, gen, frozen, "R3: rolling summary seeded from payload");
            }
        }

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

    /// 崩溃恢复入口(react 面):无新输入——加载历史(权威面=evorule payload)
    /// → 悬挂工具处置(L1 分类路由:幂等读经管道重执行回喂真结果,写类回喂
    /// 崩溃观察)→ 发射恢复标记 → LLM 自然续完当前 turn。恢复路径的 journal
    /// 重开自动触发尾部悬挂检测补写(账面序列=崩溃标记→恢复标记)。
    pub fn resume_crashed(
        self,
        session_id: String,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<AgentEvent, AgentError>> + Send>> {
        let callbacks = self.event_callbacks.clone();
        let inner =
            self.run_streaming_inner(String::new(), Some(session_id), RecoveryMode::CrashResume);
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
        let inner = self.run_streaming_inner(goal, None, RecoveryMode::Fresh);
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
        let inner =
            self.run_streaming_inner(user_input, Some(session_id), RecoveryMode::Continuation);
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
        recovery: RecoveryMode,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<AgentEvent, AgentError>> + Send>> {
        Box::pin(stream! {
                    let mut runner = self;
                    let start_time = std::time::Instant::now();
                let wall_clock_budget_secs = runner.config.wall_clock_budget_secs;

                    // 1. 构造 system_prompt(与 run() 同源:组装执行器单一出口)
                    // B3: 召回前按节流间隔校验 cache 与真相源漂移（server wins 对齐）
                    if let Some(mem) = runner.memory.as_mut() {
                        let drift = mem.verify_cache_if_due().await;
                        if drift > 0 {
                            if let Some(m) = &runner.metrics {
                                m.inc_memory_cache_drift(drift as u64);
                            }
                        }
                    }

                    // C2: 召回顺序修复 —— recall 在组装之前
                    let mut recall = match runner.memory.as_ref() {
                        Some(mem) => mem.recall_context(
                            &goal,
                            runner.sediment_config.max_session_summaries,
                            runner.sediment_config.max_injected_events,
                        ).await,
                        None => crate::agent::memory::RecallContext::default(),
                    };
                    // 笔记强制回喂 R-1 消费点:上一轮 R-2 触发闩在位=failure 教训/
                    // 催写行注入本轮 S3(消费即复位;无记忆面时闩复位不回喂)
                    if runner.pending_note_feed {
                        runner.pending_note_feed = false;
                        if let Some(mem) = runner.memory.as_ref() {
                            recall.note_feed = mem.build_failure_feed(&goal, 3).await;
                        }
                    }
                    // 检索质量观测批(K-11 观测级)+ P2-1 LexStore 缓存三计数器:
                    // 此处只计算暂存,落账延迟到 turn_guard 建立之后——本块执行时
                    // journal 写者尚未绑定(runner.active_journal 在下方 B21 journal
                    // open 处才赋值)、session_id 亦未定,原就地落账两门控恒空,
                    // 观测事件在 serve 流永不落账(2026-10-07 agent 面活体验收发现,
                    // 587/829/839 journal 实证:turn_started 在场而两事件恒缺)。
                    let recall_hits = Self::build_recall_set_hits(&recall);
                    let lex_stats = runner
                        .memory
                        .as_ref()
                        .and_then(|mem| mem.lex_cache_stats());
                    // 元层先行批:组装执行器单一出口(run/流式两组装点收敛为同一段
                    // 代码,双路径一致性由代码结构保证;槽位序/预算比例/分隔符由配方声明)
                    let boundary_segment = runner
                        .config
                        .capability_boundary
                        .as_ref()
                        .map(|b| b.awareness_segment());
                    // 治理知识契约段(S2b 槽位内容物;同 run() 组装点口径——定义声明
                    // 驱动+runner 内拉取+fail-soft,双路径一致性由代码结构保证)
                    let knowledge_segment = match runner.config.knowledge_datasets.as_deref() {
                        Some(datasets) if !datasets.is_empty() => {
                            crate::api::serve_tools::build_knowledge_segment(
                                &runner.evorule_client,
                                datasets,
                            )
                            .await
                        }
                        _ => None,
                    };
                    let system_prompt = match runner.assembly.assemble(
                        &runner.config.system_prompt,
                        runner.config.identity_segment.as_deref(),
                        runner.config.north_star.as_deref(),
                        runner.memory.as_ref(),
                        &recall,
                        runner.max_context_tokens,
                        boundary_segment.as_deref(),
                        runner.config.skills.as_deref(),
                        runner.config.handoff.as_ref(),
                        runner.config.governance_segment.as_deref(),
                        knowledge_segment.as_deref(),
                    ) {
                        Ok(p) => p,
                        Err(e) => {
                            yield Err(AgentError::Internal(e));
                            return;
                        }
                    };

                    // 2. session:新建 或 复用(G15:continuation)
                    let session_id = if let Some(id) = existing_session_id.clone() {
                        // G15:continuation — 复用已有 session,不创建新 session
                        // (session_active 守卫在下方统一创建,避免双重计数)
                        runner.session_id = Some(id.clone());
                        runner.sync_accounting_journal();
                        info!(%id, "G15: continuing existing session");
                        id
                    } else {
                        // 建会话前 IO 形状契约协商——同 run() 路径口径:
                        // 404/连不通=旧 server warn 通过;版本不匹配=hard fail。
                        // client clone 进闭包达 'static(共享 reqwest 连接池)。
                        let io_client = runner.evorule_client.clone();
                        if let Err(e) = crate::api::io_contract::negotiate_io_contract(move || {
                            Box::pin(async move { io_client.fetch_io_contract().await })
                        })
                        .await
                        {
                            yield Err(AgentError::EvoruleError(format!(
                                "io-contract negotiation failed: {e}"
                            )));
                            return;
                        }
                        // 新建 session(原 run_streaming 逻辑)
                        // M5-a:边界声明经 initial_content 既有载体进会话事实
                        let boundary_json = runner.config.capability_boundary.as_ref().map(|b| b.to_json());
                        match runner
                            .evorule_client
                            .create_session(boundary_json.as_ref(), Some("llm"))
                            .await
                        {
                            Ok(id) => {
                                // 伴生缺陷修复:新建分支回填 runner.session_id
                                // (裁决通道已不依赖它,但审计一致性/messages 持久化
                                // 等消费方需要;与 continuation 分支对齐)
                                runner.session_id = Some(id.clone());
                                runner.sync_accounting_journal();
                                id
                            }
                            Err(e) => {
                                yield Err(AgentError::EvoruleError(e.to_string()));
                                return;
                            }
                        }
                    };

                    // B2:健康声明拉取一次（协商+建会话后，fail-soft；与 run() 同钩位）
                    runner.log_session_invariants_best_effort(&session_id).await;
                    // 修复(2026-09-29 实测):MemoryManager.session_id 同步(与 run() 对齐),
                    // sediment Shared 域写入依赖此绑定。
                    if let Some(mem) = runner.memory.as_mut() {
                        mem.set_session_id(&session_id);
                    }
                    // A2-2:memory_propose 会话锚绑定(与 run() 对齐;G15 continuation
                    // 复用会话分支同样绑定,保证锚与 session 事实一致)
                    runner.bind_propose_anchor(&session_id);
                    // 双通道笔记批:note_write 会话期注册(与 run() 同钩位)
                    runner.register_session_scoped_memory_tools(&session_id);
                    // 跨源批 D:技能双层注册同步(与 run() 同钩位;Recipe
                    // sources.skills_index 门控缺省关=no-op;best-effort 不阻塞会话)
                    if runner.config.skills.is_some() {
                        if let Some(mem) = runner.memory.as_mut() {
                            let empty_manifest = Vec::new();
                            let manifest = runner.config.skills.as_ref().unwrap_or(&empty_manifest);
                            let stats =
                                crate::agent::skills_mirror::sync_skills_mirror(mem, manifest).await;
                            if !stats.skipped {
                                info!(
                                    written = stats.metadata_written,
                                    tombstoned = stats.tombstoned,
                                    sections = stats.body_sections,
                                    degraded = stats.degraded,
                                    "skills mirror synced"
                                );
                            }
                        }
                    }

                    // B21 PR-1:journal 会话事件流(serve 注入 journal_dir 时启用)。
                    // 打开失败 fail-soft 降级为无 journal 会话(warn 留痕,不阻塞主流程
                    // ——与 metrics/tool_traces 同风格);读侧 seq 连续性校验 fail-visible。
                    let journal: Option<std::sync::Arc<crate::agent::journal::JournalWriter>> =
                        match &runner.journal_dir {
                            Some(dir) => match crate::agent::journal::JournalWriter::open(dir, &session_id)
                            {
                                Ok(w) => Some(std::sync::Arc::new(w)),
                                // 同会话已有活跃写者=继续运行只会交错损坏账面,
                                // 此特定错误 fail-fast 上浮(其余 IO 错误保持 fail-soft 降级)
                                Err(crate::agent::journal::JournalError::WriterActive(sid)) => {
                                    yield Err(AgentError::Internal(format!(
                                        "session '{sid}' already has an active journal writer (并发双写防护;等先前运行收尾后再续跑)"
                                    )));
                                    return;
                                }
                                Err(e) => {
                                    warn!(
                                        %session_id,
                                        error = %e,
                                        "B21: journal open failed, session runs without journal"
                                    );
                                    None
                                }
                            },
                            None => None,
                        };
                    // 摘要保真对照(交付物 B):journal 写者克隆挂 runner(摘要替换时落账)
                    runner.active_journal = journal.clone();
                    // 自主交接 PR-H2/H3:handover 双工具+session_spawn 会话期重绑
                    // (与 run() 同钩位补挂——此前流式路径漏挂,handover 工具在 serve
                    // 流式会话恒 unwired;journal 在手后透传,派生/停链语义事件可落账)
                    runner.register_session_scoped_handover_tools(&session_id, journal.clone());
                    // 崩溃恢复 server 侧预检:崩溃早于首轮落账的会话在 server 无状态
                    // 文档(get_state 404)——无进展可保,显式指引重发,而非裸 404。
                    // 预检在 journal 开立之后:崩溃标记已补写,拒绝恢复的会话以
                    // crashed 态自我排除出扫尾列表(账面诚实)
                    if recovery == RecoveryMode::CrashResume {
                        if let Err(e) = runner.evorule_client.get_state(&session_id).await {
                            yield Err(AgentError::Internal(format!(
                                "crash recovery: no server-side state for session '{session_id}'                          (crash before first round persist) - nothing to recover, start a                          fresh run: {e}"
                            )));
                            return;
                        }
                    }

                    // turn_started(轮顶;turn_seq 按 journal 内既有轮数递增,G15 续跑同文件续轮)。
                    // turn_guard 保证所有终止路径(优雅显式 end / 异常 drop 补写 aborted)轮界闭合。
                    let mut turn_guard = match &journal {
                        Some(j) => match j.begin_turn(&goal) {
                            Ok(g) => Some(g),
                            Err(e) => {
                                warn!(%session_id, error = %e, "B21: turn_started journal failed");
                                None
                            }
                        },
                        None => None,
                    };
                    // 自主交接 PR-H3:链熔断观察窗挂接(仅派生子会话携带 chain_watch
                    // ——组件快照沿链注入,根会话恒 None 不观察)。守卫按轮新建,
                    // ChainWatch 随守卫 move(窗口判定=journal turn_seq,窗口外轮
                    // 关闭;链态生命周期=发起方请求内的 v1 口径见接线段注释)
                    if let Some(g) = turn_guard.as_mut() {
                        if let Some(w) = runner.chain_watch.take() {
                            g.attach_chain_watch(w);
                        }
                    }

                    // K-11/P2-1 观测落账(延迟点;recall 块已暂存 recall_hits/lex_stats,
                    // 此处 journal 与 session_id 均已在位)。journal 序:turn_started →
                    // recall_set → lex_cache_stats。fail-soft 与其它 journal 写入同风格。
                    if let Some(j) = &journal {
                        let _ = j.recall_set(&session_id, recall_hits);
                        if let Some((hit, expired, fetch)) = lex_stats {
                            let _ = j.lex_cache_stats(&session_id, hit, expired, fetch);
                        }
                    }

                    // B-1:逐轮 wire 留痕(挂点=本轮 wire 组装完成+轮顶事件之后、首个 LLM
                    // 调用之前;F-903 重建演示以此为逐字节比对基准)。失败 fail-soft
                    // (warn 留痕,不阻塞主流程——与其它 journal 写入同风格)
                    if let Some(j) = &journal {
                        let round = turn_guard.as_ref().map(|g| g.turn_seq()).unwrap_or(0);
                        if let Err(e) = j.wire_rendered(round, &system_prompt) {
                            warn!(%session_id, error = %e, "wire_rendered journal failed");
                        }
                    }

                    // C-3/F-905 I2 检查器:组装后 system 分区间冲突扫描——两级通路:
                    // 第一级词法召回(确定性,零成本),第二级语义精判(sidecar 审计链
                    // 内裁决,候选触发+会话内缓存+短超时,失败→uncertain 兜底);
                    // verdict 为观测注释不进控制流。输出=报告落账(仅检出时),
                    // 不阻断会话。失败 fail-soft(与其它 journal 写入同风格)
                    if let Some(j) = &journal {
                        let round = turn_guard.as_ref().map(|g| g.turn_seq()).unwrap_or(0);
                        let mut tokens: Vec<String> = runner.config.tool_names.clone();
                        if let Some(skills) = &runner.config.skills {
                            tokens.extend(skills.iter().map(|s| s.name.clone()));
                        }
                        // I2 词表数据化:definition.i2_lexicon 声明覆盖,缺省内建 v2 双语表
                        let lexicon = runner
                            .config
                            .i2_lexicon
                            .clone()
                            .unwrap_or_default();
                        let mut conflicts = crate::agent::context_inspector::inspect_system_sections_with(
                            &system_prompt,
                            &tokens,
                            &lexicon,
                        );
                        if !conflicts.is_empty() && runner.semantic_i2_enabled {
                            // 第二级:sidecar 审计链内裁决(purpose=i2_semantic;每候选
                            // 每会话至多一次,缓存命中零调用)
                            let auditor = crate::agent::audited_llm::AuditedLlm::new(
                                runner.evorule_client.clone(),
                                runner.llm_handler.clone(),
                            )
                            .with_timeout_secs(
                                crate::agent::context_inspector::I2_SEMANTIC_TIMEOUT_SECS,
                            );
                            let model = runner.config.model.clone();
                            conflicts =
                                crate::agent::context_inspector::adjudicate_candidates(
                                    conflicts,
                                    &runner.i2_verdict_cache,
                                    &model,
                                    |rec| {
                                        let params = crate::agent::context_inspector::
                                            build_adjudication_params(&model, rec);
                                        let auditor = auditor.clone();
                                        async move {
                                            let resp =
                                                auditor.execute(crate::agent::context_inspector::
                                                    I2_SEMANTIC_PURPOSE, &params).await?;
                                            let content = resp
                                                .get("content")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or_default()
                                                .to_string();
                                            let tokens = resp
                                                .get("token_usage")
                                                .and_then(|v| {
                                                    serde_json::from_value::<
                                                        crate::agent::translator::TokenUsage,
                                                    >(v.clone())
                                                    .ok()
                                                })
                                                .map(|t| crate::agent::journal::TokenRecord {
                                                    prompt: t.prompt_tokens as u64,
                                                    completion: t.completion_tokens as u64,
                                                    total: t.total_tokens as u64,
                                                });
                                            Ok((content, tokens))
                                        }
                                    },
                                    journal.as_deref(),
                                )
                                .await;
                        }
                        if !conflicts.is_empty() {
                            if let Err(e) = j.i2_scan_report(round, conflicts) {
                                warn!(%session_id, error = %e, "i2_scan_report journal failed");
                            }
                        }
                    }

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
                            // 宪法审查过审凭据(加载期已把关,违反定义不会到达此处)
                            constitution: Some(
                                crate::agent::definition::AgentDefinition::constitution_pass_mark(),
                            ),
                        });

                        // 3. auto_recall(best-effort,不阻塞流)
                        if let Err(e) = runner.auto_recall(&session_id).await {
                            tracing::warn!(session_id = %session_id, error = %e, "auto_recall failed; session continues without recalled facts");
                        }
                    }

                    // 7(前移). 初始化消息历史(崩溃恢复的本地修复须先于订阅/
                    // 解析——修复后的 messages 是重放轮 LLM 请求的内容)
                    let mut report_recovery = DanglingRepairReport::default();
                    let mut messages: Vec<Message> = Vec::new();
                    let mut step_count = 0;
                    let mut tool_calls: Vec<String> = Vec::new();
                    // H1 护栏跨重启连续:崩溃恢复分支重放回填的回退预算余额
                    // (None=非恢复路径或无从回填,按满额起算)
                    let mut resume_rewind_remaining: Option<u32> = None;

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

                        if recovery == RecoveryMode::CrashResume {
                            // 崩溃恢复:历史缺失=无可恢复对象(显式失败);悬挂工具处置
                            // (L1 分类路由)+恢复标记发射(崩溃标记已随 journal 重开补写,
                            // 账面序列=崩溃标记→恢复标记)
                            if messages.is_empty() {
                                yield Err(AgentError::Internal(format!(
                                    "crash recovery: session '{session_id}' has no recoverable history"
                                )));
                                return;
                            }
                            match runner
                                .repair_dangling_tail(&session_id, &mut messages, journal.as_deref())
                                .await
                            {
                                Ok(report) => {
                                    report_recovery = report;
                                    info!(
                                        %session_id,
                                        dangling = report.dangling,
                                        reexecuted = report.reexecuted,
                                        observed = report.observed,
                                        "crash recovery: dangling tool tail repaired"
                                    );
                                    if let Some(j) = journal.as_ref() {
                                        let replay_seq = j
                                            .read_lines()
                                            .map(|ls| ls.last().map(|l| l.seq).unwrap_or(0))
                                            .unwrap_or(0);
                                        // H1 护栏跨重启连续:重放回填回退预算余额
                                        // (替换旧 runaway_counters:reset 语义——护栏
                                        // 计数不因进程边界清零,重启不可绕过护栏)
                                        let rewind_remaining = j.replay_rewind_budget_remaining();
                                        resume_rewind_remaining = Some(rewind_remaining);
                                        if let Err(e) = j.session_resumed(
                                            replay_seq,
                                            vec![
                                                format!("messages:{}", messages.len()),
                                                format!(
                                                    "dangling_tools:{} reexecuted:{} observed:{}",
                                                    report.dangling, report.reexecuted, report.observed
                                                ),
                                                "pending_approvals:lost".to_string(),
                                                format!("rewind_budget:remaining={rewind_remaining}"),
                                            ],
                                        ) {
                                            warn!(%session_id, error = %e, "crash recovery: resumed marker write failed");
                                        }
                                    }
                                }
                                Err(e) => {
                                    yield Err(AgentError::Internal(format!(
                                        "crash recovery: dangling tail repair failed: {e}"
                                    )));
                                    return;
                                }
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
                    // 崩溃恢复不注入新 user 输入:悬挂处置完毕后 LLM 自然续完当前 turn
                    if recovery != RecoveryMode::CrashResume {
                        let user_idx = messages.len();
                        messages.push(Message::User { content: goal.clone() });
                        if let Err(e) = runner
                            .persist_message(&session_id, user_idx, Message::User { content: goal.clone() })
                            .await
                        {
                            yield Err(e);
                            return;
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

                    // 挂起 io 对账+解析(崩溃恢复):订阅先行——解析触发的重放
                    // IoRequest 经已开订阅送达。解析结果=恢复汇总(error=None
                    // 保重放,server 重放缓存指令驱动续跑);无挂起(轮界崩溃)=
                    // 重振指令,走下方 submit_command。幂等读已在本地修复段
                    // 重执行,两本账(server 事实链/本地 transcript)记同一处置
                    let mut pending_resolved = false;
                    if recovery == RecoveryMode::CrashResume {
                        match runner.evorule_client.get_session_history(&session_id).await {
                            Ok(history) => match Self::plan_pending_resolution(&history) {
                                Some((rid, io_type, _params)) => {
                                    let summary = serde_json::json!({
                                        "success": true,
                                        "content": format!(
                                            "session recovered after crash: dangling tool call(s) resolved ({} re-executed, {} crash observations); turn resumes from repaired message state",
                                            report_recovery.reexecuted, report_recovery.observed
                                        ),
                                        "steps": 0,
                                        "duration_ms": 0,
                                        "tool_calls": [],
                                        "error": null,
                                        "cancelled": false,
                                    });
                                    if let Err(e) = runner
                                        .evorule_client
                                        .submit_io_response(&session_id, rid, &summary, None)
                                        .await
                                    {
                                        yield Err(AgentError::Internal(format!(
                                            "crash recovery: pending io_response submit failed (request {rid}, {io_type}): {e}"
                                        )));
                                        return;
                                    }
                                    pending_resolved = true;
                                    info!(%session_id, request_id = rid, io_type = %io_type, "crash recovery: pending io_request resolved, engine will replay instruction");
                                }
                                None => {
                                    info!(%session_id, "crash recovery: no pending io_request (turn-boundary crash), re-arming with fresh command");
                                }
                            },
                            Err(e) => {
                                yield Err(AgentError::Internal(format!(
                                    "crash recovery: pending io reconciliation failed: {e}"
                                )));
                                return;
                            }
                        }
                    }

                    // 6. 提交 call_external 命令。挂起已解析时跳过——server
                    // 重放缓存指令即驱动;挂起期间新命令只入队不执行(执行
                    // 门控 pending_io_count==0)
                    if !(recovery == RecoveryMode::CrashResume && pending_resolved) {
                        let command = runner
                            .build_call_external_command(&system_prompt, &goal, runner.openai_tools_payload());
                        if let Err(e) = runner.evorule_client.submit_command(&session_id, &command).await {
                            yield Err(AgentError::EvoruleError(e.to_string()));
                            return;
                        }
                    }

                    // 8. SSE 事件循环(同 run(),但 call_external 分支用 execute_stream)
                    // G6:用 select! 监听取消,使等待 event 时也能即时响应
                    let cancel_token = runner.cancel_token.clone();
                    let mut last_llm_content = String::new(); // 追踪最近一次 LLM 输出(Stable 时 fallback)
                    // 连续 Error→auto_rewind→continue 回退预算(非流式
                    // run() 同款镜像)——rewind 不计步的无界回退循环在此封顶。重置语义:
                    // 任意非 Error 事件(正常推进)即清零,只惩罚连续失败。
                    // 崩溃恢复时按 journal 末次余额回填(H1 护栏跨重启连续)。
                    let mut rewind_budget = match resume_rewind_remaining {
                        Some(r) => RewindBudget::restored(r),
                        None => RewindBudget::new(REWIND_BUDGET_LIMIT),
                    };
                    loop {
                        let event = tokio::select! {
                            ev = event_stream.next() => match ev {
                                Some(e) => e,
                                None => break,
                            },
                            _ = cancel_token.cancelled() => {
                                info!("Cancellation requested during streaming, cleaning up");
                                if let Err(e) = runner.flush_messages(&session_id).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                }
                                // 取消路径补 sediment(流式镜像)
                                if let Err(e) = runner.sediment_session(&session_id, &messages, journal.as_deref()).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "sediment_session failed");
                                }
                                runner.submit_tool_traces(&session_id).await;
                                let duration = start_time.elapsed().as_millis() as u64;
                                // B21:turn_ended(cancelled)
                                if let Some(g) = turn_guard.take() {
                                    g.end("cancelled", step_count as u64, duration);
                                }
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
                                    // B21:turn_ended(error)
                                    if let Some(g) = turn_guard.take() {
                                        g.end("error", step_count as u64, duration);
                                    }
                                    if let Err(e) = runner.flush_messages(&session_id).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                }
                                    runner.submit_tool_traces(&session_id).await;
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
                                    if let Err(e) = runner.flush_messages(&session_id).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                }
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
                                        // 输出门禁（server io_guard）拒绝收尾的纠偏重试计数
                                        let mut guard_rejections: u32 = 0;
                                        'react: loop {
                                            // H3 预算看门狗:全局时限触达=合法停机
                                            // 面之三——不依赖 LLM 合作,镜像 max_steps 熔断全序列
                                            // (io_response 错误回写→Error 事件→turn_ended→flush
                                            // →tool_traces→Done[blocked 语义 error 结果])
                                            if let Some(budget_secs) = wall_clock_budget_secs {
                                                if start_time.elapsed().as_secs() >= budget_secs {
                                                    let err = AgentError::Internal(format!(
                                                        "全局时限预算耗尽({budget_secs}s)——H3 合法停机(诚实退出优于空转)"
                                                    ));
                                                    if let Some(rid) = request_id {
                                                        let err_str = err.to_string();
                                                        if let Err(e) = runner.evorule_client
                                                            .submit_io_response(&session_id, rid, &serde_json::json!({"error": &err_str}), Some(err_str.as_str()))
                                                            .await
                                                        {
                                                            tracing::warn!(session_id = %session_id, request_id = rid, error = %e, "submit_io_response (budget_exhausted) failed; io_request may hang on engine side");
                                                        }
                                                    }
                                                    yield Ok(AgentEvent::Error(err.clone()));
                                                    let duration = start_time.elapsed().as_millis() as u64;
                                                    if let Some(g) = turn_guard.take() {
                                                        g.end("error", step_count as u64, duration);
                                                    }
                                                    if let Err(e) = runner.flush_messages(&session_id).await {
                                                        tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                                    }
                                                    runner.submit_tool_traces(&session_id).await;
                                                    yield Ok(AgentEvent::Done(AgentResult::error(
                                                        err.to_string(), step_count, duration,
                                                    )));
                                                    return;
                                                }
                                            }
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
                                                        if let Err(e) = runner.evorule_client
                                                            .submit_io_response(&session_id, rid, &serde_json::json!({"error": &err_str}), Some(err_str.as_str()))
                                                            .await
                                                        {
                                                            tracing::warn!(session_id = %session_id, request_id = rid, error = %e, "submit_io_response (max_steps) failed; io_request may hang on engine side");
                                                        }
                                                    }
                                                    yield Ok(AgentEvent::Error(err.clone()));
                                                    let duration = start_time.elapsed().as_millis() as u64;
                                                    // B21:turn_ended(error)
                                                    if let Some(g) = turn_guard.take() {
                                                        g.end("error", step_count as u64, duration);
                                                    }
                                                    if let Err(e) = runner.flush_messages(&session_id).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                }
                                                    runner.submit_tool_traces(&session_id).await;
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
                                            // B21 D3:主动 compaction(阈值触发,先于被动 trim;
                                            // None=未启用/未触发/无 summarizer——原消息直接走被动 trim)
                                            let mut trim_result = match runner
                                                .active_compaction(
                                                    &session_id,
                                                    &messages,
                                                    &goal,
                                                    journal.as_ref(),
                                                )
                                                .await
                                            {
                                                Some(compacted) => ctx.trim_detailed(&compacted),
                                                None => ctx.trim_detailed(&messages),
                                            };
                                            if !trim_result.dropped.is_empty() {
                                                info!(
                                                    dropped = trim_result.dropped.len(),
                                                    "trimmed history messages to fit context window"
                                                );
                                            }
                                            // G10:记忆压缩 + R3 摘要落链（helper 共用）
                                            if let Some(summarizer) = &runner.summarizer {
                                                if !trim_result.dropped.is_empty() {
                                                    if let Err(e) = runner
                                                        .handle_summary_outcome(
                                                            &session_id,
                                                            summarizer,
                                                            &trim_result.dropped,
                                                            &mut trim_result.messages,
                                                            &goal,
                                                            "summarize",
                                                        )
                                                        .await
                                                    {
                                                        warn!(
                                                            %session_id,
                                                            error = %e,
                                                            "R3: summary handling failed, keeping original hint"
                                                        );
                                                    }
                                                }
                                            }
                                            // F-902:压缩事件落 journal（G-6/G-7 账面收敛）
                                            if !trim_result.dropped.is_empty() {
                                                if let Some(j) = &journal {
                                                    let before: usize = messages.iter().map(|m| m.content().len()).sum();
                                                    let after: usize = trim_result.messages.iter().map(|m| m.content().len()).sum();
                                                    if let Err(e) = j.compaction_performed(before, after, true) {
                                                        warn!(%session_id, error = %e, "compaction_performed journal failed");
                                                    }
                                                }
                                            }
                                            trim_result.messages
                                        } else {
                                            messages.clone()
                                        };

                                        // G11(S1 双路径收敛):注入格式指令到 system prompt
                                        // (只影响本次请求的 messages_to_send,不改原 messages)——
                                        // 语义与非流式 run() :3282 一致;R3-b/G-7 指令落链
                                        // (同指令去重,best-effort 留痕)
                                        let mut messages_to_send = messages_to_send;
                                        if let Some(validator) = &runner.output_validator {
                                            let instruction = validator.instruction();
                                            if !instruction.is_empty() {
                                                for msg in &mut messages_to_send {
                                                    if let Message::System { content } = msg {
                                                        content.push_str(instruction);
                                                        break;
                                                    }
                                                }
                                                let needs_land = {
                                                    let landed = runner
                                                        .landed_format_instruction
                                                        .lock()
                                                        .unwrap_or_else(|p| p.into_inner());
                                                    landed.as_deref() != Some(instruction)
                                                };
                                                if needs_land {
                                                    match runner
                                                        .evorule_client
                                                        .update_payload(
                                                            &session_id,
                                                            "__context__.format_instruction",
                                                            &serde_json::json!({ "format_instruction": instruction }),
                                                        )
                                                        .await
                                                    {
                                                        Ok(_fact_id) => {
                                                            let mut landed = runner
                                                                .landed_format_instruction
                                                                .lock()
                                                                .unwrap_or_else(|p| p.into_inner());
                                                            *landed = Some(instruction.to_owned());
                                                            info!(%session_id, "R3: format instruction landed (G-7 closed, streaming)");
                                                        }
                                                        Err(e) => warn!(
                                                            %session_id,
                                                            error = %e,
                                                            "R3: format instruction landing failed (best-effort)——G-7 divergence, doctor flags"
                                                        ),
                                                    }
                                                }
                                            }
                                        }

                                        let serde_messages = match serde_json::to_value(&messages_to_send) {
                                            Ok(v) => v,
                                            Err(e) => {
                                                let err = AgentError::Internal(format!("serialize messages: {}", e));
                                                yield Ok(AgentEvent::Error(err.clone()));
                                                let duration = start_time.elapsed().as_millis() as u64;
                                                runner.submit_tool_traces(&session_id).await;
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
                                        // B21:provider token 真值(Done chunk 采集,llm_called 埋点消费)
                                        let mut react_tokens: Option<crate::agent::translator::TokenUsage> = None;

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
                                                        if let Err(e) = runner.evorule_client
                                                            .submit_io_response(
                                                                &session_id, rid,
                                                                &serde_json::json!({"content": "", "error": "cancelled"}),
                                                                Some("cancelled"),
                                                            )
                                                            .await
                                                        {
                                                            tracing::warn!(session_id = %session_id, request_id = rid, error = %e, "submit_io_response (cancel) failed; io_request may hang on engine side");
                                                        }
                                                    }
                                                    if let Err(e) = runner.flush_messages(&session_id).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                }
                                                    runner.submit_tool_traces(&session_id).await;
                                                    let duration = start_time.elapsed().as_millis() as u64;
                                                    // B21:turn_ended(cancelled)
                                                    if let Some(g) = turn_guard.take() {
                                                        g.end("cancelled", step_count as u64, duration);
                                                    }
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
                                                    // B21:采集 provider token 真值
                                                    react_tokens = resp.token_usage.clone();
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
                                                        if let Err(ie) = runner.evorule_client
                                                            .submit_io_response(&session_id, rid, &err_resp, Some(e.as_str()))
                                                            .await
                                                        {
                                                            tracing::warn!(session_id = %session_id, request_id = rid, error = %ie, "submit_io_response (llm_error) failed; io_request may hang on engine side");
                                                        }
                                                    }
                                                    let err = AgentError::LlmError(e);
                                                    yield Ok(AgentEvent::Error(err.clone()));
                                                    let duration = start_time.elapsed().as_millis() as u64;
                                                    // B21:turn_ended(error)
                                                    if let Some(g) = turn_guard.take() {
                                                        g.end("error", step_count as u64, duration);
                                                    }
                                                    if let Err(e) = runner.flush_messages(&session_id).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                }
                                                    runner.submit_tool_traces(&session_id).await;
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

                                        // B21:llm_called 事件(provider 真值优先,tokens_est 兜底;
                                        // purpose=react,One-LLM-per-step 映射依据)
                                        if let Some(j) = &journal {
                                            let tokens = react_tokens.as_ref().map(|u| {
                                                crate::agent::journal::TokenRecord {
                                                    prompt: u.prompt_tokens as u64,
                                                    completion: u.completion_tokens as u64,
                                                    total: u.total_tokens as u64,
                                                }
                                            });
                                            // tokens_est:近似计数器估算 prompt+completion 总量
                                            let tokens_est = {
                                                use crate::agent::context_window::TokenCounter as _;
                                                let counter = crate::agent::context_window::ApproxTokenCounter::new();
                                                (counter.count_messages(&messages_to_send)
                                                    + counter.count_message(&crate::agent::translator::Message::Assistant {
                                                        content: full_content.clone(),
                                                        tool_calls: None,
                                                    })) as u64
                                            };
                                            if let Err(e) = j.llm_called_react(
                                                model,
                                                request_id,
                                                tokens,
                                                Some(tokens_est),
                                                messages_to_send.len(),
                                                &full_content,
                                            ) {
                                                warn!(%session_id, error = %e, "B21: llm_called journal failed");
                                            }
                                        }

                                        // G13:并行预执行工具(max_parallel_tools > 1 且有多个 tool_calls 时)
                                        // 预执行=管道并行实例(PR-3):产物=已过闸结果,存入 parallel_tool_cache,
                                        // 后续 call_service 命中走缓存收口路径(①-⑤⑧照常仅⑦免重执行)
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

                                                                        // ===== ReAct 分叉:有 tool_calls → 本地执行回喂;无 → 提交收尾 =====
                                        let has_tool_calls = full_tool_calls
                                            .as_ref()
                                            .map(|tcs| !tcs.is_empty())
                                            .unwrap_or(false);

        // 持久化 assistant 消息(同 handle_call_external)
                                                                        // G11(S1 双路径收敛):无 tool_calls 收尾前做结构化输出
                                                                        // 校验——语义与非流式 run() :3365 完全一致:
                                                                        // clean→validate→失败推原始 assistant+System 校正消息
                                                                        // →continue 'react 重试;重试耗尽降级接受 cleaned
                                                                        // (fail-visible,不毁回合)。注意:仅收尾轮校验;
                                                                        // 中间轮(tool_calls 在场)不校验,同 run() 行为。
                                                                        let g11_validated_content: String = if has_tool_calls {
                                                                            // 中间轮(工具调用在场):不校验,原样透传——同 run() 行为
                                                                            full_content.clone()
                                                                        } else {
                                                                            let validation_outcome =
                                                                                if let Some(validator) = &runner.output_validator {
                                                                                    let cleaned =
                                                                                        validator.clean_output(&full_content);
                                                                                    let max_retries = validator.max_retries();
                                                                                    let result = validator.validate(&cleaned);
                                                                                    Some((cleaned, result, max_retries))
                                                                                } else {
                                                                                    None
                                                                                };
                                                                            match validation_outcome {
                                                                                None => full_content.clone(),
                                                                                Some((cleaned, Ok(()), _)) => {
                                                                                    runner.output_format_retries = 0;
                                                                                    cleaned
                                                                                }
                                                                                Some((cleaned, Err(err_msg), max_retries)) => {
                                                                                    if runner.output_format_retries < max_retries {
                                                                                        runner.output_format_retries += 1;
                                                                                        let retry_count = runner.output_format_retries;
                                                                                        // 推原始(未清洗) assistant 到审计链
                                                                                        let a_idx = messages.len();
                                                                                        let a_msg = Message::Assistant {
                                                                                            content: full_content.clone(),
                                                                                            tool_calls: None,
                                                                                        };
                                                                                        messages.push(a_msg.clone());
                                                                                        if let Err(pe) = runner
                                                                                            .persist_message(&session_id, a_idx, a_msg)
                                                                                            .await
                                                                                        {
                                                                                            if let Some(rid) = request_id {
                                                                                                let pe_str = pe.to_string();
                                                                                                let _ = runner.evorule_client
                                                                                                    .submit_io_response(
                                                                                                        &session_id,
                                                                                                        rid,
                                                                                                        &serde_json::json!({"error": &pe_str}),
                                                                                                        Some(pe_str.as_str()),
                                                                                                    )
                                                                                                    .await;
                                                                                            }
                                                                                            yield Err(pe);
                                                                                            return;
                                                                                        }
                                                                                        // 推 System 校正消息(同 run() 模板)
                                                                                        let c_idx = messages.len();
                                                                                        let c_msg = Message::System {
                                                                                            content: format!(
                                                                                                "你的上一次输出不符合要求的格式。校验错误:\n{}\n\n\
                                                                                                 请重新输出,严格符合 JSON Schema 要求,不要包含 markdown 代码块标记。",
                                                                                                err_msg
                                                                                            ),
                                                                                        };
                                                                                        messages.push(c_msg.clone());
                                                                                        if let Err(pe) = runner
                                                                                            .persist_message(&session_id, c_idx, c_msg)
                                                                                            .await
                                                                                        {
                                                                                            if let Some(rid) = request_id {
                                                                                                let pe_str = pe.to_string();
                                                                                                let _ = runner.evorule_client
                                                                                                    .submit_io_response(
                                                                                                        &session_id,
                                                                                                        rid,
                                                                                                        &serde_json::json!({"error": &pe_str}),
                                                                                                        Some(pe_str.as_str()),
                                                                                                    )
                                                                                                    .await;
                                                                                            }
                                                                                            yield Err(pe);
                                                                                            return;
                                                                                        }
                                                                                        info!(
                                                                                            %session_id,
                                                                                            retry = retry_count,
                                                                                            max_retries,
                                                                                            "G11(streaming): output validation failed, requesting LLM retry"
                                                                                        );
                                                                                        continue 'react;
                                                                                    } else {
                                                                                        info!(
                                                                                            %session_id,
                                                                                            max_retries,
                                                                                            "G11(streaming): max retries exhausted, accepting degraded output"
                                                                                        );
                                                                                        runner.output_format_retries = 0;
                                                                                        cleaned
                                                                                    }
                                                                                }
                                                                            }
                                                                        };
                                                                        let full_content = g11_validated_content;

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
                                                if let Err(ie) = runner.evorule_client
                                                    .submit_io_response(&session_id, rid, &serde_json::json!({"error": &err_str}), Some(err_str.as_str()))
                                                    .await
                                                {
                                                    tracing::warn!(session_id = %session_id, request_id = rid, error = %ie, "submit_io_response (persist_failed) failed; io_request may hang on engine side");
                                                }
                                            }
                                            yield Err(e);
                                            return;
                                        }

                                        // ===== ReAct 分叉:有 tool_calls → 本地执行回喂;无 → 提交收尾 =====
                                        if has_tool_calls {
                                            // 有 tool_calls:本地执行每个工具(审批/缓存经 helper),
                                            // tool 消息入列后 continue 'react 发起回喂轮
                                            let tcs = full_tool_calls.unwrap();
                                            for tc in &tcs {
                                                // B21:tool_invoked(本地 ReAct 路径不经 evorule
                                                // IoRequest,evorule_request_id=None,全文内容源=
                                                // transcript payload;call_id 由事件 seq 确定性合成)
                                                let j_call_id = journal.as_ref().and_then(|j| {
                                                    j.tool_invoked(&tc.name, &tc.arguments, None).ok()
                                                });
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
                                                // 写前置查询(Q2 R-4):写族意图→路径历史 advisory(执行前计算,随结果回喂)
                                                let write_advisory = runner
                                                    .write_intent_advisory(&tc.name, &tc.arguments)
                                                    .await;
                                                let outcome_res = match runner
                                                    .execute_tool_stage(&session_id, &tc.name, &tc.arguments, journal.as_deref())
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
                                                        // B21:approval_requested(60s 审批窗开启)
                                                        if let Some(j) = &journal {
                                                            if let Err(e) = j.approval_requested(
                                                                &req.proposal_id,
                                                                &tc.name,
                                                                &req.command,
                                                            ) {
                                                                warn!(%session_id, tool = %tc.name, error = %e, "approval_requested journal failed");
                                                            }
                                                        }
                                                        let res = runner
                                                            .resolve_approval(&session_id, &tc.name, &tc.arguments, req, journal.as_deref())
                                                            .await;
                                                        if let Ok(o) = &res {
                                                            if let Some((req0, decision)) = &o.approval_flow {
                                                                yield Ok(AgentEvent::ApprovalResult {
                                                                    tool_name: tc.name.clone(),
                                                                    approved: decision.approved,
                                                                    approver: decision.approver.clone(),
                                                                    auto_rejected: decision.auto_rejected,
                                                                });
                                                                // B21:approval_resolved(approval_id = proposal_id)
                                                                if let Some(j) = &journal {
                                                                    let label = if decision.approved {
                                                                        "approved"
                                                                    } else if decision.auto_rejected {
                                                                        "auto_rejected"
                                                                    } else {
                                                                        "rejected"
                                                                    };
                                                                    if let Err(e) = j.approval_resolved(&req0.proposal_id, label) {
                                                                        warn!(%session_id, error = %e, "approval_resolved journal failed");
                                                                    }
                                                                }
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
                                                        // 笔记强制回喂 R-2:错误触发
                                                        runner.pending_note_feed = true;
                                                        tool_calls.push(tc.name.clone());
                                                        let err_content = serde_json::json!({
                                                            "error": e.to_string(),
                                                            "tool_name": tc.name,
                                                        })
                                                        .to_string();
                                                        let err_tool_msg = Message::Tool {
                                                            content: err_content.clone(),
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
                                                                if let Err(ie) = runner.evorule_client
                                                                    .submit_io_response(&session_id, rid, &serde_json::json!({"error": &pe_str}), Some(pe_str.as_str()))
                                                                    .await
                                                                {
                                                                    tracing::warn!(session_id = %session_id, request_id = rid, error = %ie, "submit_io_response (persist_failed) failed; io_request may hang on engine side");
                                                                }
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
                                                        // 委托子会话锚落账(delegate 工具:spawn 账 drain,
                                                        // 事件序 tool_invoked → delegate_spawned → tool_result)
                                                        if tc.name == "delegate" {
                                                            runner.flush_delegate_spawns(journal.as_ref());
                                                        }
                                                        // B21:tool_result(error;内容与 transcript 回喂消息一致)
                                                        if let (Some(j), Some(cid)) = (&journal, j_call_id.as_ref()) {
                                                            if let Err(e) = j.tool_result(cid, "error", &err_content) {
                                                                warn!(%session_id, call_id = %cid, error = %e, "tool_result journal failed");
                                                            }
                                                        }
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
                                                // 回喂 LLM 的入列值按上限截断;审计链持久化保留原始全文
                                                let mut raw_content = outcome.final_result.to_string();
                                                if let Some(adv) = write_advisory {
                                                    raw_content.push('\n');
                                                    raw_content.push_str(&adv);
                                                }
                                                // 委托子会话锚落账(delegate 工具:spawn 账 drain,
                                                // 事件序 tool_invoked → delegate_spawned → tool_result)
                                                if tc.name == "delegate" {
                                                    runner.flush_delegate_spawns(journal.as_ref());
                                                }
                                                // B21:tool_result(ok;content = 工具输出全文与 transcript 一致)
                                                if let (Some(j), Some(cid)) = (&journal, j_call_id.as_ref()) {
                                                    if let Err(e) = j.tool_result(cid, "ok", &raw_content) {
                                                        warn!(%session_id, call_id = %cid, error = %e, "tool_result journal failed");
                                                    }
                                                }
                                                let tool_msg = Message::Tool {
                                                    content: truncate_tool_result(
                                                        raw_content.clone(),
                                                        runner.tool_result_max_chars,
                                                    ),
                                                    tool_name: tc.name.clone(),
                                                };
                                                messages.push(tool_msg);
                                                if let Err(e) = runner
                                                    .persist_message(
                                                        &session_id,
                                                        tool_idx,
                                                        Message::Tool {
                                                            content: raw_content,
                                                            tool_name: tc.name.clone(),
                                                        },
                                                    )
                                                    .await
                                                {
                                                    // 持久化失败也不留悬挂在途 io_request(回写后终止)
                                                    if let Some(rid) = request_id {
                                                        let err_str = e.to_string();
                                                        if let Err(ie) = runner.evorule_client
                                                            .submit_io_response(&session_id, rid, &serde_json::json!({"error": &err_str}), Some(err_str.as_str()))
                                                            .await
                                                        {
                                                            tracing::warn!(session_id = %session_id, request_id = rid, error = %ie, "submit_io_response (persist_failed) failed; io_request may hang on engine side");
                                                        }
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
                                                // 输出门禁（server io_guard）enforce 模式以 422 拒绝收尾——
                                                // 纠偏回喂:追加纠正性 user 消息后重试（LLM 下轮如实说明
                                                // 或先调工具）,上限 IO_GUARD_MAX_RETRIES;超限以 error 应答
                                                // 收敛引擎 io_request（error 标记不触门禁）并 fail-visible。
                                                let is_guard_reject =
                                                    matches!(&e, ApiError::ApiError { status: 422, .. });
                                                if is_guard_reject
                                                    && guard_rejections < IO_GUARD_MAX_RETRIES
                                                {
                                                    guard_rejections += 1;
                                                    warn!(
                                                        %session_id,
                                                        request_id = rid,
                                                        round = guard_rejections,
                                                        "io_guard rejected final output; feeding back corrective turn"
                                                    );
                                                    messages.push(Message::User {
                                                        content: IO_GUARD_CORRECTION_PROMPT.to_string(),
                                                    });
                                                    continue 'react;
                                                }
                                                if is_guard_reject {
                                                    let err_str = format!(
                                                        "输出门禁拒绝收尾：纠正重试 {guard_rejections} 次后仍命中（IO_GUARD_REJECTED）"
                                                    );
                                                    let _ = runner.evorule_client
                                                        .submit_io_response(
                                                            &session_id,
                                                            rid,
                                                            &serde_json::json!({"error": &err_str}),
                                                            Some(err_str.as_str()),
                                                        )
                                                        .await;
                                                    yield Err(AgentError::EvoruleError(err_str));
                                                    return;
                                                }
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
                                        // B21:tool_invoked(call_service 路径,evorule_request_id =
                                        // IoRequest.id,审计链 join 键——ATIF 映射表 §五)
                                        let j_call_id = journal.as_ref().and_then(|j| {
                                            j.tool_invoked(&tool_name, &args, request_id).ok()
                                        });

                                        // 审批+执行抽到两阶段 helper(与本地 ReAct 循环共用);
                                        // 事件仍在此处 yield(stream! 宏限制)。Pending 时先
                                        // yield ApprovalRequired 再等决策(帧须在 60s 窗口内
                                        // 到达前端)。非流式路径(run)仍走 handle_call_service
                                        // (内部 maybe_handle_approval,不产事件)
                                        // 判据自检门禁(与非流式 handle_call_service 同款)。
                                        // 拒绝 → 审计留痕 + 合成 tool 消息回喂 LLM + continue 下一个 tc
                                        // (指令不提交引擎——判据不过不存在 done 退出路径)
                                        let args = match apply_acceptance_gate(
                                            runner.acceptance_command.as_deref(),
                                            &args,
                                        )
                                        .await
                                        {
                                            GateOutcome::Allow(a) => a,
                                            GateOutcome::Reject(detail) => {
                                                warn!(%session_id, %detail, "acceptance gate rejected instruction submission");
                                                if let (Some(j), Some(cid)) = (&journal, j_call_id.as_ref()) {
                                                    if let Err(e) = j.tool_result(cid, "rejected", &detail) {
                                                        warn!(%session_id, call_id = %cid, error = %e, "tool_result journal failed");
                                                    }
                                                }
                                                tool_calls.push(tool_name.clone());
                                                let tool_idx = messages.len();
                                                messages.push(Message::Tool {
                                                    content: format!(
                                                        "{{\"status\":\"rejected_by_acceptance_gate\",\"detail\":\"{detail}\"}}"
                                                    ),
                                                    tool_name: tool_name.clone(),
                                                });
                                                if let Err(e) = runner
                                                    .persist_message(&session_id, tool_idx, messages.last().cloned().expect("gate reject tool msg"))
                                                    .await
                                                {
                                                    tracing::warn!(session_id = %session_id, error = %e, "gate reject message persist failed");
                                                }
                                                yield Ok(AgentEvent::Info(format!(
                                                    "acceptance gate rejected: {detail}"
                                                )));
                                                continue;
                                            }
                                        };
                                        // 笔记强制回喂 R-3 段末强制(task_done 判据门放行后):
                                        // 未完成事项(todo)+失败清单(failure 正体)+缺根因草稿
                                        // 非空 → advisory 附进工具结果(诚实分立:判据已过仍放行,
                                        // 清单随沉淀必然在账;草稿催写转正)
                                        let mut gate_note_advisory: Option<String> = None;
                                        if tool_name == "task_done" {
                                            if let Some(mem) = runner.memory.as_ref() {
                                                match mem.fetch_notes_catalog().await {
                                                    Ok(catalog) => {
                                                        let open: Vec<_> = catalog
                                                            .iter()
                                                            .filter(|r| {
                                                                r.key.contains("todo")
                                                                    || (r.key.contains("failure")
                                                                        && !r.key.contains("draft"))
                                                            })
                                                            .collect();
                                                        let drafts: Vec<_> = catalog
                                                            .iter()
                                                            .filter(|r| r.key.contains("draft"))
                                                            .collect();
                                                        if !open.is_empty() || !drafts.is_empty() {
                                                            warn!(
                                                                %session_id,
                                                                open = open.len(),
                                                                drafts = drafts.len(),
                                                                "段末强制回喂:task_done 时仍有未完成事项/未消化失败(判据放行,清单随结果回喂)"
                                                            );
                                                            let mut lines = vec![format!(
                                                                "[段末强制回喂] 判据已过但账面仍有未完成事项 {} 项/缺根因草稿 {} 项(诚实分立;清单随本结果在目,草稿请补记转正):",
                                                                open.len(),
                                                                drafts.len()
                                                            )];
                                                            for r in open.iter().take(5) {
                                                                let t: String =
                                                                    r.value.chars().take(150).collect();
                                                                lines.push(format!("- {}: {}", r.key, t));
                                                            }
                                                            for d in drafts.iter().take(5) {
                                                                lines.push(format!(
                                                                    "- [催写] {} 缺根因假设,请补记",
                                                                    d.key
                                                                ));
                                                            }
                                                            gate_note_advisory = Some(lines.join("
                "));
                                                        }
                                                    }
                                                    Err(e) => {
                                                        warn!(%session_id, error = %e, "段末笔记清单拉取失败(fail-soft,不阻塞放行)");
                                                    }
                                                }
                                            }
                                        }
                                        // 写前置查询(Q2 R-4):写族意图→路径历史 advisory(执行前计算,随结果回喂)
                                        let write_advisory =
                                            runner.write_intent_advisory(&tool_name, &args).await;
                                        let outcome_res = match runner
                                            .execute_tool_stage(&session_id, &tool_name, &args, journal.as_deref())
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
                                                // B21:approval_requested(60s 审批窗开启)
                                                if let Some(j) = &journal {
                                                    if let Err(e) = j.approval_requested(&req.proposal_id, &tool_name, &req.command) {
                                                        warn!(%session_id, tool = %tool_name, error = %e, "approval_requested journal failed");
                                                    }
                                                }
                                                let res = runner
                                                    .resolve_approval(&session_id, &tool_name, &args, req, journal.as_deref())
                                                    .await;
                                                if let Ok(o) = &res {
                                                    if let Some((req0, decision)) = &o.approval_flow {
                                                        yield Ok(AgentEvent::ApprovalResult {
                                                            tool_name: tool_name.clone(),
                                                            approved: decision.approved,
                                                            approver: decision.approver.clone(),
                                                            auto_rejected: decision.auto_rejected,
                                                        });
                                                        // B21:approval_resolved(approval_id = proposal_id)
                                                        if let Some(j) = &journal {
                                                            let label = if decision.approved {
                                                                "approved"
                                                            } else if decision.auto_rejected {
                                                                "auto_rejected"
                                                            } else {
                                                                "rejected"
                                                            };
                                                            if let Err(e) = j.approval_resolved(&req0.proposal_id, label) {
                                                                warn!(%session_id, error = %e, "approval_resolved journal failed");
                                                            }
                                                        }
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
                                                    if let Err(ie) = runner.evorule_client
                                                        .submit_io_response(&session_id, rid, &err_resp, Some(err_str.as_str()))
                                                        .await
                                                    {
                                                        tracing::warn!(session_id = %session_id, request_id = rid, error = %ie, "submit_io_response (tool_exec_error) failed; io_request may hang on engine side");
                                                    }
                                                }
                                                yield Ok(AgentEvent::Error(e.clone()));
                                                // B21:tool_result(error;工具尝试已失败,补记保重放完整)
                                                if let (Some(j), Some(cid)) = (&journal, j_call_id.as_ref()) {
                                                    if let Err(je) = j.tool_result(cid, "error", &e.to_string()) {
                                                        warn!(%session_id, call_id = %cid, error = %je, "tool_result journal failed");
                                                    }
                                                }
                                                let duration = start_time.elapsed().as_millis() as u64;
                                                if let Err(e) = runner.flush_messages(&session_id).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                }
                                                runner.submit_tool_traces(&session_id).await;
                                                // B21:turn_ended(error,优雅终止路径显式收尾)
                                                if let Some(g) = turn_guard.take() {
                                                    g.end("error", step_count as u64, duration);
                                                }
                                                yield Ok(AgentEvent::Done(AgentResult::error(
                                                    e.to_string(), step_count, duration,
                                                )));
                                                return;
                                            }
                                        };
                                        // 审批事件已在两阶段流程中即时 yield(见 Pending 分支)
                                        // 停滞检测(与非流式同款;标记随 tool 消息回喂)
                                        let final_result = {
                                            let mut fr = outcome.final_result;
                                            let verdict = runner.stagnation.observe(
                                                &tool_name,
                                                &args.to_string(),
                                                &fr.to_string(),
                                            );
                                            match verdict {
                                                StagnationVerdict::Normal => {}
                                                StagnationVerdict::Warning { repeat_count } => {
                                                    warn!(%session_id, tool = %tool_name, repeat_count, "stagnation warning (F2)");
                                                    // 笔记强制回喂 R-2:停滞触发,下一轮回喂相关 failure 教训
                                                    runner.pending_note_feed = true;
                                                    if let Some(obj) = fr.as_object_mut() {
                                                        obj.insert(
                                                            "stagnation".to_string(),
                                                            serde_json::json!(STAGNATION_WARNING_MARK),
                                                        );
                                                    }
                                                }
                                                StagnationVerdict::Exhausted => {
                                                    warn!(%session_id, tool = %tool_name, "stagnation EXHAUSTED (F2)——按 H2 阻塞收尾指引");
                                                    runner.pending_note_feed = true;
                                                    if let Some(obj) = fr.as_object_mut() {
                                                        obj.insert(
                                                            "stagnation".to_string(),
                                                            serde_json::json!(STAGNATION_EXHAUSTED_MARK),
                                                        );
                                                    }
                                                }
                                            }
                                            fr
                                        };

                                        // B21:tool_result(ok;content = 工具输出全文与 io_response 一致)
                                        if let (Some(j), Some(cid)) = (&journal, j_call_id.as_ref()) {
                                            if let Err(e) = j.tool_result(cid, "ok", &final_result.to_string()) {
                                                warn!(%session_id, call_id = %cid, error = %e, "tool_result journal failed");
                                            }
                                        }

                                        // 3. 记录 tool_calls + 持久化 tool 消息(同 handle_call_service)
                                        tool_calls.push(tool_name.clone());
                                        let tool_idx = messages.len();
                                        // 回喂 LLM 的入列值按上限截断;审计链持久化保留原始全文
                                        let mut raw_content = final_result.to_string();
                                        if let Some(adv) = gate_note_advisory.take() {
                                            raw_content.push('\n');
                                            raw_content.push_str(&adv);
                                        }
                                        if let Some(adv) = write_advisory {
                                            raw_content.push('\n');
                                            raw_content.push_str(&adv);
                                        }
                                        let tool_msg = Message::Tool {
                                            content: truncate_tool_result(
                                                raw_content.clone(),
                                                runner.tool_result_max_chars,
                                            ),
                                            tool_name: tool_name.clone(),
                                        };
                                        messages.push(tool_msg);
                                        if let Err(e) = runner
                                            .persist_message(
                                                &session_id,
                                                tool_idx,
                                                Message::Tool {
                                                    content: raw_content,
                                                    tool_name: tool_name.clone(),
                                                },
                                            )
                                            .await
                                        {
                                            // 持久化失败也不留悬挂在途 io_request(回写后终止)
                                            if let Some(rid) = request_id {
                                                let err_str = e.to_string();
                                                if let Err(ie) = runner.evorule_client
                                                    .submit_io_response(&session_id, rid, &serde_json::json!({"error": &err_str}), Some(err_str.as_str()))
                                                    .await
                                                {
                                                    tracing::warn!(session_id = %session_id, request_id = rid, error = %ie, "submit_io_response (persist_failed) failed; io_request may hang on engine side");
                                                }
                                            }
                                            // 工具已执行、轨迹已采集：终止前补提交，避免审计链缺口（fail-soft）
                                            runner.submit_tool_traces(&session_id).await;
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
                                            if let Err(e) = runner.evorule_client
                                                .submit_io_response(&session_id, rid, &serde_json::json!({"error": "unsupported io_type"}), Some("unsupported io_type"))
                                                .await
                                            {
                                                tracing::warn!(session_id = %session_id, request_id = rid, error = %e, "submit_io_response (unsupported_io_type) failed; io_request may hang on engine side");
                                            }
                                        }
                                        yield Ok(AgentEvent::Info(format!("Unknown io_type: {}", io_type)));
                                    }
                                }
                            }
                            "Stable" => {
                                let duration = start_time.elapsed().as_millis() as u64;
                                if let Err(e) = runner.flush_messages(&session_id).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                }
                                // C1:会话沉淀（best-effort，摘要+稳定事实→共享空间）
                                if let Err(e) = runner.sediment_session(&session_id, &messages, journal.as_deref()).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "sediment_session failed");
                                }
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
                                runner.submit_tool_traces(&session_id).await;
                                // B21:turn_ended(success)
                                if let Some(g) = turn_guard.take() {
                                    g.end("success", step_count as u64, duration);
                                }
                                yield Ok(AgentEvent::Done(AgentResult::success(
                                    content, step_count, duration, tool_calls,
                                )));
                                return;
                            }
                            "StateTransition" => {
                                // 状态转换,继续循环
                                rewind_budget.reset(REWIND_BUDGET_LIMIT); // H1:正常推进即回满(只罚连续失败)
                            }
                            "Error" => {
                                let msg = event.payload.get("message").and_then(|v| v.as_str()).unwrap_or("unknown error");
                                // H1:回退预算执法——超预算熔断为 fail-visible(与非流式同语义)
                                // 尝试 auto_rewind
                                if let Ok(rewind_version) = runner.auto_rewind(&session_id).await {
                                    if rewind_budget.consume() {
                                        // H1:护栏跨重启连续——消费落账(余额快照),
                                        // 崩溃恢复重放回填,护栏计数不因进程边界清零
                                        if let Some(j) = journal.as_ref() {
                                            if let Err(e) = j
                                                .rewind_budget_consumed(rewind_budget.remaining)
                                            {
                                                tracing::warn!(
                                                    session_id = %session_id,
                                                    error = %e,
                                                    "rewind_budget_consumed journal failed"
                                                );
                                            }
                                        }
                                        yield Ok(AgentEvent::Info(format!("Auto-rewind to version {}", rewind_version)));
                                        continue;
                                    }
                                    tracing::warn!(session_id = %session_id, remaining = 0,
                                        "H1 rewind budget exhausted: 连续 Error 回退达上限(32),熔断为 fail-visible 错误结果");
                                }
                                let duration = start_time.elapsed().as_millis() as u64;
                                if let Err(e) = runner.flush_messages(&session_id).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                }
                                // C1:会话沉淀（best-effort，即使出错也尝试沉淀已收集的对话）
                                if let Err(e) = runner.sediment_session(&session_id, &messages, journal.as_deref()).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "sediment_session failed");
                                }
                                runner.submit_tool_traces(&session_id).await;
                                // B1:Error 熔断收尾 best-effort 中断 server 反应器（fail-soft）
                                runner.interrupt_evorule_session_best_effort(&session_id).await;
                                // B21:turn_ended(error)
                                if let Some(g) = turn_guard.take() {
                                    g.end("error", step_count as u64, duration);
                                }
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
                                if let Err(e) = runner.flush_messages(&session_id).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                }
                                if let Err(e) = runner.sediment_session(&session_id, &messages, journal.as_deref()).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "sediment_session failed");
                                }
                                runner.submit_tool_traces(&session_id).await;
                                // B21:turn_ended(error)
                                if let Some(g) = turn_guard.take() {
                                    g.end("error", step_count as u64, duration);
                                }
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
                    if let Err(e) = runner.flush_messages(&session_id).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "flush_messages failed; buffered messages not yet persisted");
                                }
                    // 断流路径补 sediment(流式镜像)
                    if let Err(e) = runner.sediment_session(&session_id, &messages, journal.as_deref()).await {
                                    tracing::warn!(session_id = %session_id, error = %e, "sediment_session failed");
                                }
                    runner.submit_tool_traces(&session_id).await;
                    // B21:turn_ended(error)
                    if let Some(g) = turn_guard.take() {
                        g.end("error", step_count as u64, duration);
                    }
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
///
/// 元层先行批:max_chars 由调用方从配方生效值传入(runner.tool_result_max_chars)
fn rec_to_message(rec: &MessageRecord, max_chars: usize) -> Option<Message> {
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
            // 重建值同样按上限截断:continuation 恢复路径与运行中回喂的 wire
            // 形态保持一致(审计链始终存原始全文,截断仅作用于回喂 LLM 的值)
            content: truncate_tool_result(rec.content.clone(), max_chars),
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

/// 连续 Error→auto_rewind→continue 回退预算状态机(有界封顶)。
///
/// rewind 路径不耗 step_count——引擎/网络持续 Error 时无界回退循环在此封顶。
/// 语义:Error 事件调用 `consume()`——预算>0 递减返回 true(允许 rewind+continue);
/// 预算耗尽返回 false(熔断:走 Error 收尾路径 fail-visible,不静默)。任意非 Error
/// 事件(正常推进,如 StateTransition)调用 `reset()` 回满——只惩罚"连续"失败,
/// 不惩罚间歇错误。默认 REWIND_BUDGET_LIMIT=32(与 retry 生态上限同量级;非配置面
/// ——引擎侧熔断属可靠性底线,不交由 agent 配置放开)。崩溃恢复时经 `restored()`
/// 按 journal 末次余额回填(护栏跨重启连续,重启不可绕过护栏)。
#[derive(Debug)]
struct RewindBudget {
    remaining: u32,
}

impl RewindBudget {
    fn new(limit: u32) -> Self {
        Self { remaining: limit }
    }

    /// 崩溃恢复回填:按 journal 末次 RewindBudgetConsumed 余额恢复
    fn restored(remaining: u32) -> Self {
        Self { remaining }
    }

    /// Error 事件消费一份预算;返回是否仍允许 rewind+continue
    fn consume(&mut self) -> bool {
        if self.remaining > 0 {
            self.remaining -= 1;
            true
        } else {
            false
        }
    }

    /// 正常推进(任意非 Error 事件)——预算回满
    fn reset(&mut self, limit: u32) {
        self.remaining = limit;
    }
}

/// B21 D3:主动压缩保留轮数(设计默认 N=6,非配置面——压缩窗口结构
/// 参数,与 rewind 预算同属可靠性底线)
const COMPACTION_KEEP_ROUNDS: usize = 6;

/// B21 D3:主动压缩策略(`longSession.compaction.*` 工作台设置键映射)。
///
/// serve 面经 [`AgentRunner::with_compaction_policy`] 注入;CLI 路径不读
/// 设置(None)=不启用,行为零变化。阈值语义:上下文用量(count,CJK 校准
/// 计数)≥ 窗口×thresholdPct 即触发——被动 trim(超 budget 才裁)的前置层。
#[derive(Debug, Clone)]
pub struct CompactionPolicy {
    /// 是否启用主动压缩(关=仅被动 trim,现状行为)
    pub enabled: bool,
    /// 触发阈值:上下文用量占窗口百分比(50..=95,默认 70)
    pub threshold_pct: u32,
    /// 单次压缩最多清除的工具结果条数(0..=200,默认 20;0=不清除,仅摘要)
    pub max_clear_tool_results: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_pct: 70,
            max_clear_tool_results: 20,
        }
    }
}

impl CompactionPolicy {
    /// 从工作台合并设置构造(schema 默认值兜底+越界钳制;键缺失/类型不符
    /// =默认值——设置读取面宁缺毋错,与 merged 层默认值双保险)
    pub fn from_settings(settings: &serde_json::Map<String, serde_json::Value>) -> Self {
        let mut p = Self::default();
        if let Some(v) = settings
            .get("longSession.compaction.enabled")
            .and_then(serde_json::Value::as_bool)
        {
            p.enabled = v;
        }
        if let Some(v) = settings
            .get("longSession.compaction.thresholdPct")
            .and_then(serde_json::Value::as_f64)
        {
            p.threshold_pct = (v.round() as i64).clamp(50, 95) as u32;
        }
        if let Some(v) = settings
            .get("longSession.compaction.maxClearToolResults")
            .and_then(serde_json::Value::as_f64)
        {
            p.max_clear_tool_results = (v.round() as i64).clamp(0, 200) as usize;
        }
        p
    }
}

/// B21 D3 近摘要区边界(纯函数):返回 `(system_end, split)`——
/// `messages[system_end..split]` 为近摘要区,`messages[split..]` 为保留的
/// 最近 `keep_rounds` 轮(轮=User 消息发起;system 前缀=头部连续 System 段)。
/// User 轮数 ≤ keep_rounds 时无近摘要区(全量保留)→ `None`。
fn compaction_region_bounds(messages: &[Message], keep_rounds: usize) -> Option<(usize, usize)> {
    let system_end = messages
        .iter()
        .take_while(|m| matches!(m, Message::System { .. }))
        .count();
    let user_idxs: Vec<usize> = messages[system_end..]
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::User { .. }))
        .map(|(i, _)| i + system_end)
        .collect();
    if user_idxs.len() <= keep_rounds {
        return None;
    }
    Some((system_end, user_idxs[user_idxs.len() - keep_rounds]))
}

/// B21 D3 工具结果引用替代(纯函数):按原文长度从大到小清除至多 `max_clear`
/// 条 tool 消息,原文以 `[cleared: 工具名]` 引用替代(完整原文留存 journal,
/// 可恢复原则)。返回 `(清除后的消息副本, 实际清除条数)`。
fn clear_tool_results(region: &[Message], max_clear: usize) -> (Vec<Message>, usize) {
    if max_clear == 0 {
        return (region.to_vec(), 0);
    }
    let mut tool_idxs: Vec<usize> = region
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::Tool { .. }))
        .map(|(i, _)| i)
        .collect();
    if tool_idxs.is_empty() {
        return (region.to_vec(), 0);
    }
    tool_idxs.sort_by_key(|&i| std::cmp::Reverse(region[i].content().len()));
    let clear_set: std::collections::HashSet<usize> =
        tool_idxs.into_iter().take(max_clear).collect();
    let mut out = region.to_vec();
    let mut cleared = 0usize;
    for (i, msg) in out.iter_mut().enumerate() {
        if clear_set.contains(&i) {
            if let Message::Tool { content, tool_name } = msg {
                *content = format!("[cleared: {tool_name}]");
                cleared += 1;
            }
        }
    }
    (out, cleared)
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod runner_tests;
