// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! I/O Handler 瀹炵幇 鈥斺€?LLM 璋冪敤鍜屽伐鍏疯皟鐢?
pub mod llm_handler;
pub mod tool_handler;

pub use llm_handler::{LlmHandler, StreamChunk};
pub use tool_handler::ToolHandler;
