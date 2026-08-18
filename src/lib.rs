// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
#![warn(unused_imports)]
#![warn(unused_variables)]
#![warn(missing_docs)]

//! Evo-Agent 鈥斺€?AI Agent 缂栨帓灞傦紝閫氳繃 evorule HTTP API 瀹炵幇瀹屾暣鐨?Fact 闂幆

pub mod agent;
pub mod api;
pub mod builtin_tools;
pub mod config;
pub mod io_dispatcher;
pub mod io_handler;
pub mod io_handlers;
pub mod json_convert;
pub mod mcp;
pub mod rule_tools;

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
