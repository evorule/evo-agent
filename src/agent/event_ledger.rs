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

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tokio::sync::Mutex;

use crate::agent::memory_event::event::{EventSource, EventType, FactId, MemoryEvent};
use crate::agent::memory_event::MemoryEventStore;
use crate::api::api_core::ApiError;
use crate::api::evorule_client::EvoruleApiClient;

use super::memory::{MemoryError, MemoryManager, MemoryScope, PersistOutcome};

/// 每轮工具事件配额（23 号档风险 6：默认 5；definition 可配与同 kind 聚合 v1 顺延）
pub const DEFAULT_TURN_QUOTA: usize = 5;

/// 轮里程碑间隔（首轮 + 每 N 轮 checkpoint）
pub const MILESTONE_EVERY_TURNS: u64 = 10;

/// O-482:/mcp 面合成治理会话状态文件名（{workdir}/data/ 下；本地持久化
/// 数字会话 id——server 不支持指定会话名，幂等开具靠此文件）
pub const GOV_SESSION_STATE_FILE: &str = "mcp_gov_session.json";

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

// ===== O-482:/mcp 面合成治理会话（开具/复用/自愈） =====

/// 读取合成治理会话状态文件（无/损坏/空 id → None，fail-soft）
fn read_gov_session_state(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("session_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 原子写回状态文件（同目录 temp + rename；失败仅 warn——自愈路径会重开具）
fn write_gov_session_state(path: &Path, session_id: &str) {
    let payload = serde_json::json!({
        "session_id": session_id,
        "created_ts": now_secs(),
    });
    let body = match serde_json::to_vec_pretty(&payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "event ledger: gov session state encode failed");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, &body).and_then(|()| std::fs::rename(&tmp, path)) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "event ledger: gov session state write failed (self-heal will re-provision)"
        );
    }
}

/// 开具新合成治理会话（caller_role="llm"）+ 原子写回；失败 fail-soft None。
/// 角色口径：server 契约仅接受 human | llm（workspace_service 声明校验），
/// "service" 非合法声明——合成治理会话主体是自动化审计调用，按 ai-plugin
/// sidecar 先例（审计型 sidecar 会话）声明 llm。
async fn provision_gov_session(client: &EvoruleApiClient, path: &Path) -> Option<String> {
    match client.create_session(None, Some("llm")).await {
        Ok(sid) => {
            write_gov_session_state(path, &sid);
            Some(sid)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "event ledger: gov session provisioning failed; event ledger stays CacheOnly until self-heal"
            );
            None
        }
    }
}

/// 合成治理会话开具/复用决策：状态文件在手 → 探活复用（404 判死重开具；
/// 网络等其他错误不判死，fail-soft None）；无/损坏 → 直接开具
async fn ensure_mcp_gov_session(client: &EvoruleApiClient, path: &Path) -> Option<String> {
    match read_gov_session_state(path) {
        Some(sid) => match client.get_session_metadata(&sid).await {
            Ok(_) => Some(sid),
            Err(ApiError::SessionNotFound) => provision_gov_session(client, path).await,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "event ledger: gov session probe failed; keeping current state"
                );
                None
            }
        },
        None => provision_gov_session(client, path).await,
    }
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
    /// O-482:自愈重开具用 client（与 store/memory 同源副本）
    client: EvoruleApiClient,
    seq: u64,
    /// 轨道内前序事件 FactId（cause 单链锚；仅真实 FactId>0 时推进）
    prev_fact: Option<FactId>,
    turn_written: usize,
    turn_warned: bool,
    /// O-482:开具/注入待办（写入失败传感器置位；下一次 persist 前置重开具重注入）
    needs_ensure: bool,
}

impl LedgerInner {
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// O-482 J-b 链续接：回读账面末端事件（复用活会话且进程内链为空时）——
    /// EL 序号续编 + prev_fact 接锚旧链末端 + store 种子回填（effects 反向
    /// 链维护不缺位）。新会话账面为空 → 返回 None 保持根。
    async fn fetch_chain_tail(&mut self, sid: &str) -> Option<(u64, FactId)> {
        let facts = self
            .client
            .get_facts(sid, Some("__memory__.mcp.events."))
            .await
            .ok()?;
        let tail = facts.iter().max_by_key(|f| f.id)?;
        // path 形态 __memory__.mcp.events.EL-000005 → 末段为事件 ID
        let eid = tail.path.split('.').next_back()?;
        let seq: u64 = eid.strip_prefix("EL-")?.parse().ok()?;
        let source = self.store.read_event(eid).await.ok().flatten();
        self.store.seed_chain_anchor(tail.id, eid, source);
        Some((seq, tail.id))
    }

    /// O-482:合成治理会话开具/复用并注入（needs_ensure 消费者；gov 路径
    /// 未装 = v1 语义 no-op）。探活复用（get_session_metadata Ok）→ 注入；
    /// 404 判死 → 重开具 + 原子写回 → 注入新会话（cause 链锚重置：旧
    /// FactId 属旧会话，跨会话 cause 无效）；其余错误 fail-soft 维持现状，
    /// 传感器下次失败会再武装。session 在手 fast path 零探活直落。
    async fn ensure_gov_session(&mut self, gov_path: Option<&PathBuf>) {
        let Some(path) = gov_path else {
            self.needs_ensure = false;
            return;
        };
        self.needs_ensure = false;
        let Some(sid) = ensure_mcp_gov_session(&self.client, path).await else {
            return;
        };
        let prev_sid = self.store.session_id().map(str::to_string);
        if prev_sid.as_deref() != Some(sid.as_str()) {
            if self.prev_fact.is_none() {
                // 进程内链为空（重启首注入/开具失败后补注入）：回读账面末端
                // 续链——EL 序号续编 + 接锚（新会话账面为空则保持根）
                if let Some((seq, fid)) = self.fetch_chain_tail(&sid).await {
                    self.seq = seq;
                    self.prev_fact = Some(fid);
                }
            } else {
                // 在途失效换新会话：旧 FactId 属旧会话，cause 锚与序号清零重来
                self.prev_fact = None;
                self.seq = 0;
            }
        }
        self.store.set_session_id(&sid);
        self.memory.set_session_id(&sid);
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
    async fn persist(&mut self, event: MemoryEvent, gov_path: Option<&PathBuf>) {
        // O-482 自愈前置：待开具时先重开具重注入（session 在手 fast path
        // 零探活直落；未装 gov 路径 = v1 语义 no-op）
        if self.needs_ensure {
            self.ensure_gov_session(gov_path).await;
        }
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
        // O-482 自愈传感器：证据链面未落（session 未设静默 / HTTP 失败 /
        // 404 类）→ 待重开具；本轮事件照旧 best-effort 不阻断
        if fact_id == 0 {
            self.needs_ensure = true;
        }
        // 共享消费面：memory_recall kind=events 检索源（sediment 双写同款键形态）
        match serde_json::to_string(&event) {
            Ok(value) => {
                let key = format!("events.{}", event.event_id);
                match self
                    .memory
                    .set_scoped(MemoryScope::Shared, &key, &value)
                    .await
                {
                    Ok(PersistOutcome::Persisted { .. }) => {}
                    // O-482 自愈传感器：session 在手但 HTTP 失败（404 类/瞬时）
                    Ok(PersistOutcome::CacheOnly) => {
                        self.needs_ensure = true;
                        tracing::warn!(event_id = %event.event_id, "event ledger: shared-face dual write CacheOnly; gov session re-ensure armed");
                    }
                    // O-482 自愈传感器：session 未设（注入前/失效后）
                    Err(e) => {
                        if matches!(e, MemoryError::SessionNotSet) {
                            self.needs_ensure = true;
                        }
                        tracing::warn!(event_id = %event.event_id, error = %e, "event ledger: shared-face dual write failed");
                    }
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
    /// O-482:合成治理会话状态文件（serve 启动期安装一次；未装 = v1 语义）
    gov_session_path: OnceLock<PathBuf>,
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
                memory: MemoryManager::new(namespace, client.clone()),
                client,
                seq: 0,
                prev_fact: None,
                turn_written: 0,
                turn_warned: false,
                needs_ensure: false,
            }),
            gov_session_path: OnceLock::new(),
        }
    }

    /// O-482:安装合成治理会话状态文件路径（serve 启动期一次；first-wins）。
    /// 安装后 persist 自愈通道生效（开具/复用/注入）；未安装 = v1 语义。
    pub fn set_gov_session_path(&self, path: PathBuf) {
        let _ = self.gov_session_path.set(path);
    }

    /// O-482:合成治理会话开具/复用并注入（serve 启动期一次；fail-soft——
    /// 失败仅 warn，事件轨道维持 v1 CacheOnly，后续写入经自愈路径补开具）
    pub async fn ensure_gov_session_now(&self) {
        let mut g = self.inner.lock().await;
        g.ensure_gov_session(self.gov_session_path.get()).await;
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
            g.persist(event, self.gov_session_path.get()).await;
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
        g.persist(event, self.gov_session_path.get()).await;
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
        g.persist(event, self.gov_session_path.get()).await;
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
        g.persist(event, self.gov_session_path.get()).await;
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
            client: dead_client(),
            seq: 0,
            prev_fact: None,
            turn_written: 0,
            turn_warned: false,
            needs_ensure: false,
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
    fn gov_session_state_file_roundtrip_and_corruption() {
        let tmp = std::env::temp_dir().join(format!(
            "o482-el-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let p = tmp.join(GOV_SESSION_STATE_FILE);
        // 无文件 → None
        assert!(read_gov_session_state(&p).is_none(), "missing file → None");
        // 损坏内容 → None（fail-soft，不 panic）
        std::fs::write(&p, "not json at all").unwrap();
        assert!(read_gov_session_state(&p).is_none(), "corrupt file → None");
        // 正常写读往返 + 无 temp 残留（rename 原子性）
        write_gov_session_state(&p, "sess-42");
        assert_eq!(
            read_gov_session_state(&p).as_deref(),
            Some("sess-42"),
            "roundtrip via atomic write"
        );
        assert!(
            !p.with_extension("json.tmp").exists(),
            "atomic rename leaves no temp residue"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn ensure_mcp_gov_session_fail_soft_with_dead_client() {
        let tmp = std::env::temp_dir().join(format!(
            "o482-el-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let p = tmp.join(GOV_SESSION_STATE_FILE);
        let client = dead_client();
        // 无状态文件 + server 不可达 → 开具失败 → None 且不落状态文件
        assert!(
            ensure_mcp_gov_session(&client, &p).await.is_none(),
            "provisioning failure stays None (fail-soft)"
        );
        assert!(!p.exists(), "failed provisioning must not write state");
        // 有状态文件 + server 不可达（网络错误 ≠ 死亡判定）→ None 且不改写文件
        std::fs::write(&p, r#"{"session_id":"sess-7"}"#).unwrap();
        assert!(
            ensure_mcp_gov_session(&client, &p).await.is_none(),
            "probe network failure stays None (not judged dead)"
        );
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(
            after.contains("sess-7"),
            "probe network failure must not rewrite state"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn gov_session_self_heal_flag_lifecycle() {
        let mut inner = LedgerInner {
            store: MemoryEventStore::new("agent_test", dead_client()),
            memory: MemoryManager::new("agent_test", dead_client()),
            client: dead_client(),
            seq: 0,
            prev_fact: None,
            turn_written: 0,
            turn_warned: false,
            needs_ensure: false,
        };
        // 传感器：写入失败（fact_id=0）武装自愈待办
        let e1 = build_tool_event(1, "echo", true, 1, None, now_secs());
        inner.persist(e1, None).await;
        assert!(
            inner.needs_ensure,
            "store write fail (fact_id=0) arms self-heal"
        );
        // ensure 消费待办（gov 路径未装 = v1 no-op 开具 + 解除武装）
        inner.ensure_gov_session(None).await;
        assert!(!inner.needs_ensure, "ensure pass disarms the flag");
        // 全链路：待办武装的下一条 persist 前置 ensure（解除）后，本轮写入
        // 失败再次武装——传感器逐条生效，直到开具成功写入落账
        inner.needs_ensure = true;
        let e2 = build_tool_event(2, "echo", true, 1, None, now_secs());
        inner.persist(e2, None).await;
        assert!(
            inner.needs_ensure,
            "post-ensure write failure re-arms (sensor per persist)"
        );
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
