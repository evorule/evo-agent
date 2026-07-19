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
pub mod io_dispatcher;
pub mod io_handler;
pub mod io_handlers;
pub mod json_convert;

pub use agent::{
    AgentConfig, AgentDefinition, AgentDefinitionError, AgentDefinitionManager, AgentError,
    AgentResult, AgentRunner, DelegateContext, DEFAULT_MAX_DELEGATE_DEPTH, MemoryConfig,
    MemoryError, MemoryManager, OutputFormat, ToolRegistry, ToolSpec, LlmResponse, Message, ToolCall,
};
pub use api::router;
pub use api::evorule_client::EvoruleApiClient;
pub use io_handlers::{LlmHandler, ToolHandler};
