// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Agent orchestration layer -- AI Agent run loop, tool registry, and memory manager.
pub mod approval;
pub mod callback;
pub mod context_window;
pub mod definition;
pub mod delegate;
pub mod memory;
pub mod memory_event;
pub mod output_validator;
pub mod runner;
pub mod sediment;
pub mod summarizer;
pub mod tool_registry;
pub mod translator;
pub mod workflow;

pub use approval::{
    ApprovalCallback, ApprovalRequest, AutoApprove, CliApproval, DenyAll, HttpApproval,
    HTTP_APPROVAL_TIMEOUT_SECS,
};
pub use callback::{CallbackChain, EventCallback, LoggingCallback, MetricsCallback};
pub use context_window::{
    ApproxTokenCounter, ContextWindowManager, TokenCounter, TrimResult, TrimStrategy,
};
pub use definition::{
    AgentDefinition, AgentDefinitionError, AgentDefinitionManager, MemoryConfig, OutputFormat,
};
pub use delegate::{DelegateContext, DEFAULT_MAX_CONCURRENT_DELEGATES};
pub use memory::{MemoryError, MemoryManager};
pub use memory_event::{
    Emotion, EmotionSubject, Entity, EntityIndex, EntityRef, EntityStatus, EntityType,
    EventExtractor, EventSource, EventType, ExtractionConfig, ExtractionTrigger, FactId,
    MemoryEvent, MemoryEventStore, Narrative, ReplayDirection, ReplayEngine, StoreError,
};
pub use runner::{
    merge_delegate_tool, AgentConfig, AgentError, AgentEvent, AgentResult, AgentRunner,
    DEFAULT_MAX_DELEGATE_DEPTH,
};
pub use tool_registry::{ToolRegistry, ToolSpec};
pub use translator::{LlmResponse, Message, ToolCall};
pub use workflow::{Workflow, WorkflowEngine, WorkflowNode};
