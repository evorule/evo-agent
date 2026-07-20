// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Agent orchestration layer -- AI Agent run loop, tool registry, and memory manager.
pub mod definition;
pub mod delegate;
pub mod memory;
pub mod runner;
pub mod tool_registry;
pub mod translator;

pub use definition::{
    AgentDefinition, AgentDefinitionError, AgentDefinitionManager, MemoryConfig, OutputFormat,
};
pub use delegate::DelegateContext;
pub use memory::{MemoryError, MemoryManager};
pub use runner::{
    AgentConfig, AgentError, AgentResult, AgentRunner, DEFAULT_MAX_DELEGATE_DEPTH,
    merge_delegate_tool,
};
pub use tool_registry::{ToolRegistry, ToolSpec};
pub use translator::{LlmResponse, Message, ToolCall};