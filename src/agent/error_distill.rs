// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 错误→经验自动蒸馏队列（工具族设计档 D8）。
//!
//! `build_check` 在「红→绿」循环闭合时产生候选（见该模块的进程级
//! pending 态与 `distill_candidate` 信封字段），本模块承载候选队列：
//! 工具侧入队（环形帽,超帽丢最旧）、runner 侧在 sediment 时点出队
//! 并经 `MemoryManager::set_scoped(Shared, ...)` 落为 stable fact
//! （键 `llm.errorfix.<sig>`,带 as_of）。
//!
//! 门控在两处双重生效：入队前查 [`crate::agent::recipe::error_distill_gate`]
//! （工具侧,未启用=连信封候选都不产生）；出队后查
//! `MemoryManager::error_distill_config`（runner 侧,未启用=丢弃）。
//! 缺省关=现行为零变化。

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 一条待落账的蒸馏候选
#[derive(Debug, Clone, serde::Serialize)]
pub struct DistillCandidate {
    /// 稳定签名（blake3 前 8 位十六进制;同工作区同错误簇=同签名）
    pub signature: String,
    /// 工作区绝对路径（canonical 形态）
    pub workspace: String,
    /// 红态诊断条数
    pub error_count: usize,
    /// 红态首条错误消息（截断至 120 字符,签名归一前原文）
    pub top_message: String,
    /// 触发红态的检查命令（剖面命令,如 "cargo check --message-format=short"）
    pub command: String,
    /// 红态首次观测时刻（UNIX 秒）
    pub first_seen_epoch: u64,
    /// 绿态确认时刻（UNIX 秒）
    pub resolved_epoch: u64,
}

impl DistillCandidate {
    /// stable fact 键（`llm.errorfix.<sig>`;签名 8 位十六进制）
    pub fn fact_key(&self) -> String {
        format!("llm.errorfix.{}", self.signature)
    }

    /// stable fact 值（JSON 形态;as_of 由落账点补齐——落账时刻即生效时刻）
    pub fn fact_value(&self, as_of_epoch: u64) -> FactPayload {
        FactPayload {
            signature: self.signature.clone(),
            workspace: self.workspace.clone(),
            error_count: self.error_count,
            top_message: self.top_message.clone(),
            command: self.command.clone(),
            first_seen_epoch: self.first_seen_epoch,
            resolved_epoch: self.resolved_epoch,
            as_of_epoch,
        }
    }
}

/// stable fact 值形态（落账 JSON;字段名与信封候选对齐便于审计对读）
#[derive(Debug, Clone, serde::Serialize)]
pub struct FactPayload {
    /// 稳定签名（blake3 前 8 位）
    pub signature: String,
    /// 工作区绝对路径
    pub workspace: String,
    /// 红态诊断条数
    pub error_count: usize,
    /// 红态首条错误消息（截断 120 字符）
    pub top_message: String,
    /// 触发红态的检查命令
    pub command: String,
    /// 红态首次观测时刻（UNIX 秒）
    pub first_seen_epoch: u64,
    /// 绿态确认时刻（UNIX 秒）
    pub resolved_epoch: u64,
    /// 落账时刻（UNIX 秒,stable fact 的 as_of 锚）
    pub as_of_epoch: u64,
}

/// 进程级红态 pending 表（键=canonical 工作区;verdict=failed 记录,
/// pass 闭合取走）。static 形态:工具实例短命,状态须跨调用存活。
static PENDING: OnceLock<Mutex<HashMap<String, PendingError>>> = OnceLock::new();

#[derive(Debug, Clone)]
struct PendingError {
    error_count: usize,
    top_message: String,
    command: String,
    first_seen_epoch: u64,
}

/// 当前 UNIX 秒（落账 as_of 锚）
pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 红态记录（build_check verdict=failed 且 errors>0 时调用;同工作区
/// 覆盖式刷新——保留最新一次红态现场）。
pub fn record_failure(workspace: &str, errors: usize, top_message: String, command: &str) {
    let map = PENDING.get_or_init(|| Mutex::new(HashMap::new()));
    let mut m = map.lock().unwrap_or_else(|e| e.into_inner());
    m.insert(
        workspace.to_string(),
        PendingError {
            error_count: errors,
            top_message: top_message.chars().take(120).collect(),
            command: command.to_string(),
            first_seen_epoch: now_epoch(),
        },
    );
}

/// 绿态闭合（build_check verdict=pass 时调用）：有 pending 即产出候选
/// （签名=blake3(工作区+top_message) 前 8 位十六进制——同工作区同错误
/// 簇=同签名,复现刷新不重复落账）并取走 pending。
///
/// `command` 参数语义=绿态时的检查命令;候选 command 取红态记录值
/// （pending.command）,故本参数仅作对读校验位,当前未消费。
pub fn resolve_if_pending(workspace: &str, _command: &str) -> Option<DistillCandidate> {
    let map = PENDING.get_or_init(|| Mutex::new(HashMap::new()));
    let mut m = map.lock().unwrap_or_else(|e| e.into_inner());
    let pending = m.remove(workspace)?;
    let digest_src = format!("{}|{}", workspace, pending.top_message);
    let digest = evorule_hash::prefixed(&evorule_hash::json_digest(&digest_src));
    let signature = digest
        .trim_start_matches("blake3:")
        .chars()
        .take(8)
        .collect::<String>();
    Some(DistillCandidate {
        signature,
        workspace: workspace.to_string(),
        error_count: pending.error_count,
        top_message: pending.top_message,
        command: pending.command,
        first_seen_epoch: pending.first_seen_epoch,
        resolved_epoch: now_epoch(),
    })
}

fn queue() -> &'static Mutex<VecDeque<DistillCandidate>> {
    static Q: OnceLock<Mutex<VecDeque<DistillCandidate>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// 入队（环形帽:超过 cap 丢最旧）。同签名已入队则刷新该条（红态复现
/// 时更新计数与时间窗,不重复占帽）。
pub fn enqueue(candidate: DistillCandidate, cap: usize) {
    let cap = cap.max(1);
    let mut q = queue().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = q
        .iter_mut()
        .find(|c| c.signature == candidate.signature && c.workspace == candidate.workspace)
    {
        *existing = candidate;
        return;
    }
    while q.len() >= cap {
        q.pop_front();
    }
    q.push_back(candidate);
}

/// 出队全部候选（runner 在 sediment 时点 drain;出队即清,重复 drain=空）。
pub fn drain() -> Vec<DistillCandidate> {
    let mut q = queue().lock().unwrap_or_else(|e| e.into_inner());
    q.drain(..).collect()
}

/// 当前队列长度（诊断/测试用）
pub fn pending_len() -> usize {
    queue().lock().unwrap_or_else(|e| e.into_inner()).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(sig: &str) -> DistillCandidate {
        DistillCandidate {
            signature: sig.to_string(),
            workspace: "D:/ws".to_string(),
            error_count: 3,
            top_message: "boom".to_string(),
            command: "cargo check".to_string(),
            first_seen_epoch: 100,
            resolved_epoch: 200,
        }
    }

    #[test]
    fn enqueue_then_drain_empties() {
        // 用独立签名避免与其他测试共享进程队列（static 跨测试存活）
        let sig = format!("a{}", std::process::id());
        enqueue(cand(&sig), 5);
        assert!(pending_len() >= 1);
        let drained = drain();
        assert!(drained.iter().any(|c| c.signature == sig));
        assert!(!drain().iter().any(|c| c.signature == sig));
        let key = cand(&sig).fact_key();
        assert!(key.starts_with("llm.errorfix."));
    }

    #[test]
    fn same_signature_refreshes_in_place() {
        let sig = format!("b{}", std::process::id());
        enqueue(cand(&sig), 5);
        let before = pending_len();
        enqueue(cand(&sig), 5); // 同签名同工作区=刷新不新增
        assert_eq!(pending_len(), before);
    }

    #[test]
    fn cap_evicts_oldest() {
        let sig1 = format!("c1-{}", std::process::id());
        let sig2 = format!("c2-{}", std::process::id());
        enqueue(cand(&sig1), 1);
        enqueue(cand(&sig2), 1); // 帽 1:最旧被挤掉
        let drained = drain();
        assert!(drained.iter().all(|c| c.signature != sig1));
        assert!(drained.iter().any(|c| c.signature == sig2));
    }
}
