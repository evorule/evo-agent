// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Agent API 妯″潡

pub mod agent_api;
pub mod evorule_client;

pub use agent_api::{router, AgentApiState, AgentRunRequest, AgentRunResponse};
pub use evorule_client::{EvoruleApiClient, EvoruleApiError};
