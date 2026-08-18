// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! I/O Handler trait

use evorule_tcb::JsonValue;

/// I/O operation result type
pub type IoResult = Result<JsonValue, String>;

/// I/O Handler trait -- defines I/O execution interface
#[async_trait::async_trait]
pub trait IoHandler: Send + Sync {
    /// Execute I/O operation
    async fn execute(&self, params: &JsonValue) -> IoResult;
}
