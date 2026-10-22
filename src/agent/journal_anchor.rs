// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! journal 链 → 引擎审计链锚定执行器（防整链重算的外部锚定）
//!
//! journal 本地链防意外（seq 连续+逐行三查），但本地磁盘写攻击者可整链重算。
//! 本执行器把 journal 链尾经引擎 `PayloadUpdate` 写入 evorule 会话审计链
//! （受保护路径 `__journal__.anchor`）——journal 被锚进引擎链后，本地整链
//! 重算与锚不符即暴露（与 evorule 哨兵前缀锚定同哲学）。
//!
//! 锚定策略（v1）：会话 Stable/Error 终态锚（每会话至少一锚）；行数阈值
//! 锚（每 64 行）为后续增强——已知边界如实标注，不假充已覆盖。
//!
//! 失败语义（best-effort，与 write_event 同模式）：锚定失败保留计数下次
//! 补锚；验证报告中锚缺失如实标注 unanchored-tail。

use crate::agent::journal::JournalWriter;
use crate::api::evorule_client::EvoruleApiClient;

/// 引擎 payload 中的受保护锚定路径（与 __memory__/gov 命名空间隔离）
pub const JOURNAL_ANCHOR_PATH: &str = "__journal__.anchor";

/// 锚定阈值（预留：达到该行数时终态之外的中间锚；v1 仅终态锚）
pub const ANCHOR_EVERY_LINES: u64 = 64;

/// 把 journal 链尾锚入引擎审计链（幂等：无新行直接 Ok）
pub async fn anchor_to_engine_chain(
    client: &EvoruleApiClient,
    session_id: &str,
    writer: &JournalWriter,
) -> Result<(), String> {
    if writer.lines_since_anchor() == 0 {
        return Ok(());
    }
    let value = serde_json::json!({
        "chain_tail": writer.chain_tail(),
        "last_seq": writer.last_seq(),
    });
    client
        .update_payload(session_id, JOURNAL_ANCHOR_PATH, &value)
        .await
        .map_err(|e| e.to_string())?;
    writer.mark_anchored();
    Ok(())
}
