// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! B22-C 事件型记忆轨道（23 号档 §3.3）——四触发点结构化事件写入
//!
//! 会话过程中的四类结构化事件（工具完成/轮里程碑/审批决定/会话终态）
//! 以 [`MemoryEvent`] 形式落链：
//! - 证据链面：[`MemoryEventStore`] 既有 `write_event` / `write_event_with_cause`
//!   路径（`__memory__.{ns}.events.{id}`，best-effort + 因果绑定）——机制零新建
//! - 共享消费面：[`MemoryManager::set_scoped`]（Shared 域 `events.{id}` 键，
//!   sediment 双写同款先例）——`memory_recall kind=events` 的跨会话检索数据源
//!
//! 因果口径（v1 定夺）：轨道内前序事件 FactId 单链（prev-pointer）——工具
//! 结果不写引擎 Fact、无 FactId 锚，前序事件锚点使 `fact_to_event` 命中、
//! effects 反向链真实成立；轨道首事件为根（cause=None）。
//!
//! 配额/开关（23 号档风险 6 处置）：每轮工具事件最多
//! [`DEFAULT_TURN_QUOTA`] 条（超限丢弃 + 每轮一次 warn 留痕，里程碑重置）；
//! 进程级开关 [`event_ledger_enabled`]（serve 启动期自 settings
//! `agentTools.eventLedger` 安装，CLI 不 init 缺省开）。

use std::sync::OnceLock;

use tokio::sync::Mutex;

use crate::agent::memory_event::event::{EventSource, EventType, FactId, MemoryEvent};
use crate::agent::memory_event::MemoryEventStore;
use crate::api::evorule_client::EvoruleApiClient;

use super::memory::{MemoryManager, MemoryScope};

/// 每轮工具事件配额（23 号档风险 6：默认 5；definition 可配与同 kind 聚合 v1 顺延）
pub const DEFAULT_TURN_QUOTA: usize = 5;

/// 轮里程碑间隔（首轮 + 每 N 轮 checkpoint）
pub const MILESTONE_EVERY_TURNS: u64 = 10;

static EVENT_LEDGER_ENABLED: OnceLock<bool> = OnceLock::new();

/// 安装进程级事件轨道开关（first-wins 幂等；serve 启动期自 settings 安装）
pub fn init_event_ledger_enabled(enabled: bool) {
    let _ = EVENT_LEDGER_ENABLED.set(enabled);
}

/// 事件轨道开关（未 init 时缺省开——CLI 直跑路径无 serve settings）
pub fn event_ledger_enabled() -> bool {
    *EVENT_LEDGER_ENABLED.get().unwrap_or(&true)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 轨道事件 ID（会话内递增；路径安全单段）
fn event_id(seq: u64) -> String {
    format!("EL-{seq:06}")
}

/// T1 工具调用完成事件（成败皆记；EventType=SystemObservation）
pub(crate) fn build_tool_event(
    seq: u64,
    tool: &str,
    ok: bool,
    duration_ms: u64,
    error: Option<&str>,
    ts: u64,
) -> MemoryEvent {
    let mut content = serde_json::json!({
        "tool": tool,
        "ok": ok,
        "duration_ms": duration_ms,
    });
    if let Some(err) = error {
        content["error"] = serde_json::Value::String(err.to_string());
    }
    MemoryEvent::new_root(
        &event_id(seq),
        EventType::SystemObservation,
        ts,
        EventSource::ToolResult,
    )
    .with_content(content)
}

/// T2 会话里程碑事件（首轮/每 N 轮 checkpoint；Custom("session_milestone")）
pub(crate) fn build_milestone_event(seq: u64, turn: u64, ts: u64) -> MemoryEvent {
    MemoryEvent::new_root(
        &event_id(seq),
        EventType::Custom("session_milestone".to_string()),
        ts,
        EventSource::SystemObservation,
    )
    .with_content(serde_json::json!({
        "milestone": "turn_checkpoint",
        "turn": turn,
    }))
}

/// T3 审批决定事件（Custom("approval_observation")）
pub(crate) fn build_approval_event(
    seq: u64,
    approval_id: &str,
    verdict: &str,
    ts: u64,
) -> MemoryEvent {
    MemoryEvent::new_root(
        &event_id(seq),
        EventType::Custom("approval_observation".to_string()),
        ts,
        EventSource::SystemObservation,
    )
    .with_content(serde_json::json!({
        "approval_id": approval_id,
        "verdict": verdict,
    }))
}

/// T4 会话终态事件（Stable/Error 与 sediment 并联；summary_ref v1 恒 null）
pub(crate) fn build_session_end_event(seq: u64, turns: u64, ts: u64) -> MemoryEvent {
    MemoryEvent::new_root(
        &event_id(seq),
        EventType::SystemObservation,
        ts,
        EventSource::SystemObservation,
    )
    .with_content(serde_json::json!({
        "turns": turns,
        "summary_ref": serde_json::Value::Null,
    }))
}

/// 轨道内部状态（互斥保护；写入面全 best-effort 不阻断会话）
struct LedgerInner {
    store: MemoryEventStore,
    memory: MemoryManager,
    seq: u64,
    /// 轨道内前序事件 FactId（cause 单链锚；仅真实 FactId>0 时推进）
    prev_fact: Option<FactId>,
    turn_written: usize,
    turn_warned: bool,
}

impl LedgerInner {
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// T1 配额闸（每轮 [`DEFAULT_TURN_QUOTA`] 条；超限丢弃 + 每轮一次 warn）
    fn try_admit_tool_event(&mut self) -> bool {
        if self.turn_written >= DEFAULT_TURN_QUOTA {
            if !self.turn_warned {
                self.turn_warned = true;
                tracing::warn!(
                    quota = DEFAULT_TURN_QUOTA,
                    "event ledger: per-turn tool-event quota exceeded; further tool events dropped this turn"
                );
            }
            return false;
        }
        self.turn_written += 1;
        true
    }

    /// 落链：证据链面（根/cause 链）+ 共享消费面（sediment 双写同款）；
    /// 全程 best-effort，失败 warn 留痕不阻断
    async fn persist(&mut self, event: MemoryEvent) {
        let fact_id = match self.prev_fact {
            Some(prev) => match self.store.write_event_with_cause(event.clone(), prev).await {
                Ok(fid) => fid,
                Err(e) => {
                    tracing::warn!(event_id = %event.event_id, error = %e, "event ledger: store write (with cause) failed");
                    0
                }
            },
            None => match self.store.write_event(event.clone()).await {
                Ok(fid) => fid,
                Err(e) => {
                    tracing::warn!(event_id = %event.event_id, error = %e, "event ledger: store write failed");
                    0
                }
            },
        };
        // 共享消费面：memory_recall kind=events 检索源（sediment 双写同款键形态）
        match serde_json::to_string(&event) {
            Ok(value) => {
                let key = format!("events.{}", event.event_id);
                if let Err(e) = self
                    .memory
                    .set_scoped(MemoryScope::Shared, &key, &value)
                    .await
                {
                    tracing::warn!(event_id = %event.event_id, error = %e, "event ledger: shared-face dual write failed");
                }
            }
            Err(e) => {
                tracing::warn!(event_id = %event.event_id, error = %e, "event ledger: serialize for shared face failed");
            }
        }
        // cause 单链锚推进：仅真实 FactId 参与链（离线/失败时下一事件仍为根）
        if fact_id > 0 {
            self.prev_fact = Some(fact_id);
        }
    }
}

/// B22-C 事件型记忆轨道——四触发点写入入口（runner 持有 [`Arc`]，&self 可用）
pub struct EventLedger {
    inner: Mutex<LedgerInner>,
}

// B24:AgentApiState derive(Debug) 需要；内部为 tokio Mutex 串行化状态，
// Debug 输出无诊断价值，非穷尽形态即可。
impl std::fmt::Debug for EventLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventLedger").finish_non_exhaustive()
    }
}

impl EventLedger {
    /// 由 runner 既有记忆件构造（与 MemoryEventStore/MemoryManager 同 namespace + client）
    pub fn new(store: MemoryEventStore, namespace: &str, client: EvoruleApiClient) -> Self {
        Self {
            inner: Mutex::new(LedgerInner {
                store,
                memory: MemoryManager::new(namespace, client),
                seq: 0,
                prev_fact: None,
                turn_written: 0,
                turn_warned: false,
            }),
        }
    }

    /// 会话建立后同步 session_id（证据链面与共享面两侧）
    pub async fn set_session_id(&self, session_id: &str) {
        let mut g = self.inner.lock().await;
        g.store.set_session_id(session_id);
        g.memory.set_session_id(session_id);
    }

    /// T2 轮顶：重置本轮配额；首轮/每 N 轮写里程碑事件
    pub async fn begin_turn(&self, turn: u64) {
        if !event_ledger_enabled() {
            return;
        }
        let mut g = self.inner.lock().await;
        g.turn_written = 0;
        g.turn_warned = false;
        if turn == 1 || turn.is_multiple_of(MILESTONE_EVERY_TURNS) {
            let seq = g.next_seq();
            let event = build_milestone_event(seq, turn, now_secs());
            g.persist(event).await;
        }
    }

    /// T1 工具调用完成（成败皆记；配额内准入，返回是否实际落链）
    pub async fn record_tool_completion(
        &self,
        tool: &str,
        ok: bool,
        duration_ms: u64,
        error: Option<&str>,
    ) -> bool {
        if !event_ledger_enabled() {
            return false;
        }
        let mut g = self.inner.lock().await;
        if !g.try_admit_tool_event() {
            return false;
        }
        let seq = g.next_seq();
        let event = build_tool_event(seq, tool, ok, duration_ms, error, now_secs());
        g.persist(event).await;
        true
    }

    /// T3 审批决定（低频，不受工具配额约束）
    pub async fn record_approval(&self, approval_id: &str, verdict: &str) -> bool {
        if !event_ledger_enabled() {
            return false;
        }
        let mut g = self.inner.lock().await;
        let seq = g.next_seq();
        let event = build_approval_event(seq, approval_id, verdict, now_secs());
        g.persist(event).await;
        true
    }

    /// T4 会话终态（Stable/Error；与 sediment 并联，每会话至多一次）
    pub async fn record_session_end(&self, turns: u64) -> bool {
        if !event_ledger_enabled() {
            return false;
        }
        let mut g = self.inner.lock().await;
        let seq = g.next_seq();
        let event = build_session_end_event(seq, turns, now_secs());
        g.persist(event).await;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://127.0.0.1:1")
    }

    #[test]
    fn build_tool_event_fields() {
        let e = build_tool_event(1, "shell_exec", true, 120, None, 1790400000);
        assert_eq!(e.event_id, "EL-000001");
        assert_eq!(e.event_type, EventType::SystemObservation);
        assert_eq!(e.source, EventSource::ToolResult);
        assert_eq!(e.timestamp, 1790400000);
        assert_eq!(e.content["tool"], "shell_exec");
        assert_eq!(e.content["ok"], true);
        assert_eq!(e.content["duration_ms"], 120);
        assert!(e.content.get("error").is_none());
        assert!(e.cause.is_none(), "root event until chained");
    }

    #[test]
    fn build_tool_event_with_error() {
        let e = build_tool_event(2, "http_get", false, 30, Some("timeout"), 1790400000);
        assert_eq!(e.event_id, "EL-000002");
        assert_eq!(e.content["ok"], false);
        assert_eq!(e.content["error"], "timeout");
    }

    #[test]
    fn build_milestone_and_approval_and_session_end() {
        let m = build_milestone_event(3, 10, 1790400000);
        assert_eq!(
            m.event_type,
            EventType::Custom("session_milestone".to_string())
        );
        assert_eq!(m.content["milestone"], "turn_checkpoint");
        assert_eq!(m.content["turn"], 10);

        let a = build_approval_event(4, "A-17", "approved", 1790400000);
        assert_eq!(
            a.event_type,
            EventType::Custom("approval_observation".to_string())
        );
        assert_eq!(a.content["approval_id"], "A-17");
        assert_eq!(a.content["verdict"], "approved");

        let s = build_session_end_event(5, 12, 1790400000);
        assert_eq!(s.event_type, EventType::SystemObservation);
        assert_eq!(s.content["turns"], 12);
        assert!(s.content["summary_ref"].is_null());
    }

    #[test]
    fn tool_event_quota_per_turn() {
        let mut inner = LedgerInner {
            store: MemoryEventStore::new("agent_test", dead_client()),
            memory: MemoryManager::new("agent_test", dead_client()),
            seq: 0,
            prev_fact: None,
            turn_written: 0,
            turn_warned: false,
        };
        let admitted: Vec<bool> = (0..7).map(|_| inner.try_admit_tool_event()).collect();
        assert_eq!(
            admitted,
            vec![true, true, true, true, true, false, false],
            "first {DEFAULT_TURN_QUOTA} admitted per turn, excess dropped"
        );
        assert!(inner.turn_warned, "one-time warn flag set on first drop");
        // 里程碑重置：新一轮配额恢复
        inner.turn_written = 0;
        inner.turn_warned = false;
        assert!(inner.try_admit_tool_event());
    }

    #[tokio::test]
    async fn record_tool_completion_quota_and_seq() {
        let ledger = EventLedger::new(
            MemoryEventStore::new("agent_test", dead_client()),
            "agent_test",
            dead_client(),
        );
        ledger.set_session_id("sess-el-test").await;
        let mut results = Vec::new();
        for _ in 0..6 {
            results.push(ledger.record_tool_completion("echo", true, 1, None).await);
        }
        assert_eq!(
            results,
            vec![true, true, true, true, true, false],
            "quota enforcement visible at record entry"
        );
        // 低频事件不受工具配额约束
        assert!(ledger.record_approval("A-1", "approved").await);
        assert!(ledger.record_session_end(3).await);
    }

    #[test]
    fn switch_install_is_idempotent() {
        // first-wins 幂等：重复安装不 panic。并发测试无进程隔离保证（与
        // governance::init_ledger 测试同口径），故此处只安装 true 保证本
        // crate 内依赖「缺省开」的用例可确定执行；关闭分支为 record 入口
        // 单行早退，语义同 governance::current_ledger 判空 no-op 先例
        init_event_ledger_enabled(true);
        init_event_ledger_enabled(true);
        assert!(event_ledger_enabled());
    }
}
