// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! I/O Handler trait

use tier0_tcb::JsonValue;

/// I/O 鎿嶄綔缁撴灉绫诲瀷
pub type IoResult = Result<JsonValue, String>;

/// I/O Handler trait 鈥斺€?瀹氫箟 I/O 鎵ц鎺ュ彛
#[async_trait::async_trait]
pub trait IoHandler: Send + Sync {
    /// 鎵ц I/O 鎿嶄綔
    async fn execute(&self, params: &JsonValue) -> IoResult;
}
