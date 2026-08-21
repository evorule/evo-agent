// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Agent API 妯″潡

pub mod agent_api;
pub mod api_core;
pub mod auth;
pub mod evorule_client;
pub mod llm_ops;
pub mod metrics;
pub mod serve_tools;
pub mod workspace_client;
pub mod ws_handler;

pub use agent_api::{router, router_with_auth, AgentApiState, AgentRunRequest, AgentRunResponse};
pub use api_core::ApiError;
pub use auth::AuthConfig;
pub use evorule_client::EvoruleApiClient;
pub use metrics::{Metrics, MetricsError, SessionActiveGuard, SharedMetrics, SseConnectionGuard};
