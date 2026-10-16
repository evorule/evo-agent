// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Agent API 模块

pub mod agent_api;
pub mod api_core;
pub mod approval_api;
pub mod auth;
pub mod completion_api;
pub mod console_sidecar;
pub mod evorule_client;
pub mod file_api;
pub mod fs_watch;
pub mod git_api;
pub mod human_gate;
pub mod io_contract;
pub mod llm_ops;
pub mod lsp_api;
pub mod mcp_api;
pub mod metrics;
pub mod orchestration_api;
pub mod pty_api;
pub mod serve_tools;
pub mod session_index;
pub mod settings;
pub mod skill_api;
pub mod snapshots;
pub mod task_api;
pub mod workspace_client;
pub mod ws_handler;

pub use agent_api::{router, router_with_auth, AgentApiState, AgentRunRequest, AgentRunResponse};
pub use api_core::ApiError;
pub use auth::AuthConfig;
pub use evorule_client::EvoruleApiClient;
pub use metrics::{Metrics, MetricsError, SessionActiveGuard, SharedMetrics, SseConnectionGuard};
