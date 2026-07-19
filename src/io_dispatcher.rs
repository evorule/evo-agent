// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! I/O Dispatcher - 鏍规嵁 IoType 鍒嗗彂鍒板搴?handler

use tier0_tcb::JsonValue;

use crate::io_handler::{IoHandler, IoResult};
use crate::io_handlers::{llm_handler::LlmHandler, tool_handler::ToolHandler};

/// I/O 鍒嗗彂鍣
#[derive(Debug, Clone)]
pub struct IoDispatcher {
    llm: LlmHandler,
    tool: ToolHandler,
}

impl IoDispatcher {
    /// 鍒涘缓鏂扮殑 I/O 鍒嗗彂鍣
    pub fn new(llm: LlmHandler, tool: ToolHandler) -> Self {
        Self { llm, tool }
    }

    /// 鏍规嵁 IoType 鍒嗗彂鍒板搴?handler
    pub async fn dispatch(&self, io_type: &str, params: &JsonValue) -> IoResult {
        match io_type {
            "call_external" => self.llm.execute(params).await,
            "call_service" => self.tool.execute(params).await,
            _ => Err(format!("unsupported io_type: {}", io_type)),
        }
    }
}

/// IoDispatcher 鏋勫缓鍣
#[derive(Debug, Clone)]
pub struct IoDispatcherBuilder {
    llm_handler: Option<LlmHandler>,
    tool_handler: Option<ToolHandler>,
}

impl IoDispatcherBuilder {
    /// 鍒涘缓鏂扮殑鏋勫缓鍣
    pub fn new() -> Self {
        Self {
            llm_handler: None,
            tool_handler: None,
        }
    }

    /// 璁剧疆 LLM handler
    pub fn with_llm_handler(mut self, handler: LlmHandler) -> Self {
        self.llm_handler = Some(handler);
        self
    }

    /// 璁剧疆宸ュ叿 handler
    pub fn with_tool_handler(mut self, handler: ToolHandler) -> Self {
        self.tool_handler = Some(handler);
        self
    }

    /// 鏋勫缓 IoDispatcher
    pub fn build(self) -> IoDispatcher {
        IoDispatcher::new(
            self.llm_handler
                .unwrap_or_else(|| LlmHandler::with_defaults()),
            self.tool_handler.unwrap_or_else(|| ToolHandler::new()),
        )
    }
}

/// I/O 浜嬩欢
#[derive(Debug, Clone)]
pub struct IoEvent {
    /// 浼氳瘽 ID
    pub session_id: String,
    /// 浜嬩欢璐熻浇
    pub payload: IoEventPayload,
}

/// I/O 浜嬩欢璐熻浇
#[derive(Debug, Clone)]
pub enum IoEventPayload {
    /// I/O 璇锋眰
    IoRequest(Box<JsonValue>),
    /// I/O 鍝嶅簲
    IoResponse(Box<JsonValue>),
}
