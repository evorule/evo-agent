// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! I/O Handler 实现 —— LLM 调用和工具调用
pub mod llm_direct;
pub mod llm_handler;
pub mod tool_handler;

pub use llm_handler::{LlmHandler, StreamChunk};
pub use tool_handler::ToolHandler;
