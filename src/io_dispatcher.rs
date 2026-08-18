// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! I/O Dispatcher - dispatches based on IoType to corresponding handler

use evorule_tcb::JsonValue;

use crate::io_handler::{IoHandler, IoResult};
use crate::io_handlers::{llm_handler::LlmHandler, tool_handler::ToolHandler};

/// I/O dispatcher
#[derive(Debug, Clone)]
pub struct IoDispatcher {
    llm: LlmHandler,
    tool: ToolHandler,
}

impl IoDispatcher {
    /// Create new I/O dispatcher
    pub fn new(llm: LlmHandler, tool: ToolHandler) -> Self {
        Self { llm, tool }
    }

    /// Dispatch based on IoType to corresponding handler
    pub async fn dispatch(&self, io_type: &str, params: &JsonValue) -> IoResult {
        match io_type {
            "call_external" => self.llm.execute(params).await,
            "call_service" => self.tool.execute(params).await,
            _ => Err(format!("unsupported io_type: {}", io_type)),
        }
    }
}

/// IoDispatcher builder
#[derive(Debug, Clone)]
pub struct IoDispatcherBuilder {
    llm_handler: Option<LlmHandler>,
    tool_handler: Option<ToolHandler>,
}

impl IoDispatcherBuilder {
    /// Create new builder
    pub fn new() -> Self {
        Self {
            llm_handler: None,
            tool_handler: None,
        }
    }

    /// Set LLM handler
    pub fn with_llm_handler(mut self, handler: LlmHandler) -> Self {
        self.llm_handler = Some(handler);
        self
    }

    /// Set tool handler
    pub fn with_tool_handler(mut self, handler: ToolHandler) -> Self {
        self.tool_handler = Some(handler);
        self
    }

    /// Build IoDispatcher
    pub fn build(self) -> IoDispatcher {
        IoDispatcher::new(
            self.llm_handler
                .unwrap_or_else(LlmHandler::with_defaults),
            self.tool_handler.unwrap_or_else(ToolHandler::new),
        )
    }
}

/// I/O event
#[derive(Debug, Clone)]
pub struct IoEvent {
    /// Session ID
    pub session_id: String,
    /// Event payload
    pub payload: IoEventPayload,
}

/// I/O event payload
#[derive(Debug, Clone)]
pub enum IoEventPayload {
    /// I/O request
    IoRequest(Box<JsonValue>),
    /// I/O response
    IoResponse(Box<JsonValue>),
}
