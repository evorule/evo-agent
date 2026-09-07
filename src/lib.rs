// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
#![warn(unused_imports)]
#![warn(unused_variables)]
#![warn(missing_docs)]

//! Evo-Agent —— AI Agent 编排层，通过 evorule HTTP API 实现完整的 Fact 闭环
//!
//! # 公共契约面（生态公共设施专项 2026-08-30）
//!
//! 下游（数据治理系统、console 等）只应依赖以下契约项；它们受 semver 约束
//! （0.x 阶段 breaking 变更必须升 minor 并记 CHANGELOG）：
//!
//! - [`io_handlers::LlmHandler`] / [`io_handlers::StreamChunk`] / [`io_handlers::ToolHandler`]
//!   —— LLM/工具执行层（OpenAI 兼容，env 优先级 MiniMax > DeepSeek > OpenAI）
//! - [`agent::audited_llm::AuditedLlm`] —— 审计链内 LLM 执行桥（一次性 sidecar 会话协议）
//! - [`api::evorule_client::EvoruleApiClient`] 与 [`api::api_core::ApiError`] —— server HTTP 客户端
//! - [`io_handler::IoHandler`] —— IO 执行器 trait（AuditedLlm 签名依赖）
//! - [`config`] —— 配置加载（`LlmHandler::from_config` 构造契约）
//!
//! 其余模块（builtin_tools / mcp / rule_tools / io_dispatcher / json_convert /
//! metrics）标注 `#[doc(hidden)]`：技术上仍可访问（真收窄留待 0.2.0），
//! 但不在兼容承诺范围内，依赖它们的风险自负。

pub mod agent;
pub mod api;
#[doc(hidden)]
pub mod builtin_tools;
pub mod config;
#[doc(hidden)]
pub mod io_dispatcher;
pub mod io_handler;
pub mod io_handlers;
#[doc(hidden)]
pub mod json_convert;
#[doc(hidden)]
pub mod mcp;
#[doc(hidden)]
pub mod rule_tools;
#[doc(hidden)]
pub mod service_tools;

/// P2-V3 止血（2026-08-27）：审计链旁路调用的全局指标桥
///
/// Summarizer 等"影子调用"直连 LLM、不经 evorule fact 流程，属已知审计
/// 盲区（见 research/issues P2-V3）。在指标系统尚未全程注入前，通过
/// 进程级可选挂钩留痕：未安装回调时零开销空转。
///
/// runner/agent 层代码应通过 [`crate::metrics::bypass_audit`] 上报，
/// 应用入口（main/binary）负责在启动时安装真实回调：
/// `crate::metrics::set_bypass_audit_hook(...)`
#[doc(hidden)]
pub mod metrics {
    use std::sync::Arc;

    type BypassAuditHook = Arc<dyn Fn(&str) + Send + Sync>;

    static BYPASS_AUDIT_HOOK: std::sync::OnceLock<BypassAuditHook> = std::sync::OnceLock::new();

    /// 安装旁路调用上报回调（进程内只生效一次，重复安装被忽略并返回 false）
    pub fn set_bypass_audit_hook(hook: impl Fn(&str) + Send + Sync + 'static) -> bool {
        BYPASS_AUDIT_HOOK.set(Arc::new(hook)).is_ok()
    }

    /// 记录一次绕过审计链的 LLM 调用
    ///
    /// - `purpose`：调用用途标签（如 "summarize"/"session_summary"/"rollup"）
    pub fn bypass_audit(purpose: &str) {
        if let Some(hook) = BYPASS_AUDIT_HOOK.get() {
            hook(purpose);
        }
    }

    type SafetyHitHook = Arc<dyn Fn(&str) + Send + Sync>;

    static SAFETY_HIT_HOOK: std::sync::OnceLock<SafetyHitHook> = std::sync::OnceLock::new();

    /// 安装 L2 SafetyAuditor 命中上报回调（进程内只生效一次，重复安装被忽略并返回 false）
    pub fn set_safety_hit_hook(hook: impl Fn(&str) + Send + Sync + 'static) -> bool {
        SAFETY_HIT_HOOK.set(Arc::new(hook)).is_ok()
    }

    /// 记录一次 L2 SafetyAuditor 对召回内容的审计命中（P5-A3 指标）
    ///
    /// - `rule`：命中的审计规则名
    pub fn safety_hit(rule: &str) {
        if let Some(hook) = SAFETY_HIT_HOOK.get() {
            hook(rule);
        }
    }
}

pub use agent::{
    merge_delegate_tool, AgentConfig, AgentDefinition, AgentDefinitionError,
    AgentDefinitionManager, AgentError, AgentEvent, AgentResult, AgentRunner, DelegateContext,
    LlmResponse, MemoryConfig, MemoryError, MemoryManager, Message, OutputFormat, ToolCall,
    ToolRegistry, ToolSpec, Workflow, WorkflowEngine, WorkflowNode,
    DEFAULT_MAX_CONCURRENT_DELEGATES, DEFAULT_MAX_DELEGATE_DEPTH,
};
// G14:032 MemoryEvent re-export(结构化记忆事件 + 因果链 + 确定性回放)
pub use agent::callback::{CallbackChain, EventCallback, LoggingCallback, MetricsCallback};
pub use agent::memory_event::{
    Emotion, EmotionSubject, Entity, EntityIndex, EntityRef, EntityStatus, EntityType,
    EventExtractor, EventSource, EventType, ExtractionConfig, ExtractionTrigger, FactId,
    MemoryEvent, MemoryEventStore, Narrative, ReplayDirection, ReplayEngine, StoreError,
};
pub use api::evorule_client::EvoruleApiClient;
pub use api::metrics::{Metrics, MetricsError, SharedMetrics};
pub use api::router;
pub use io_handlers::{LlmHandler, StreamChunk, ToolHandler};
pub use mcp::{McpClient, McpToolAdapter, McpToolSpec, McpTransport, StdioTransport};
