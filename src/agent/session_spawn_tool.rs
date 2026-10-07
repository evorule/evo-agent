// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `session_spawn` 自主开会话工具（自主交接设计 PR-H3，形态 B 自主自旋）。
//!
//! LLM 调用后由执行体在 spawn 发起方会话内完成：护栏三件预检 →
//! `create_session_fork`（server 机制层白得 parent_session_id 因果链 +
//! caller_role=llm 声明继承 + initial_content_hash）→ 组装子会话 runner
//! （父 runner 组件快照重建，见 [`crate::agent::runner::AgentRunner`]
//! `spawn_factory`）→ 自动驱动首轮 goal（读交接档自检，B 全自主裁定）→
//! 返回 child_session_id 供父会话按交接协议收尾。
//!
//! 护栏三件（底线，不可被调焦触及；设计 §3.4）：
//! 1. **链深度硬顶**：深度上溯链权威=server（get_session_metadata 逐跳
//!    读 parent_session_id），子深度 = 父深度+1 > [`MAX_CHAIN_DEPTH`] 拒绝；
//! 2. **单链总预算**：链累计 token 消耗（token_counter 埋点累加，全链共享
//!    同一 `Arc<AtomicU64>`）≥ 预算上限拒绝。默认 = 4× 会话上下文窗口
//!    （K-06 口径：常数族无实证锚，随 budget-report 数据校准）；
//! 3. **熔断**：spawn 同签名（args+result digest，stagnation 三元组摘要
//!    同源构词法）连续 [`SPAWN_REPEAT_HALT`] 次 → 停链（journal
//!    chain_halted + 共享链态置停链标记，链上后续 spawn 预检拒绝）；
//!    子会话侧由 [`ChainWatch`] 观察前 [`CHAIN_WATCH_TURNS`] 轮 turn_ended
//!    error 签名，连续错误达窗口 → 同上停链（turn 守卫触发）。
//!
//! 审计链：spawn 成功由 runner 侧按工具名分支落 journal `session_spawned`
//! （镜像 server 因果链）；停链落 `chain_halted`（session 锚自带）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::agent::journal::JournalWriter;
use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handlers::tool_handler::ToolFunction;

/// 自主开会话工具名
pub const SESSION_SPAWN_TOOL: &str = "session_spawn";

/// 链深度硬顶（默认 3；config 可降属治理写人工面，首版常量）
pub const MAX_CHAIN_DEPTH: u32 = 3;

/// 子会话熔断观察窗口（新会话前 N 轮 turn_ended error 签名）
pub const CHAIN_WATCH_TURNS: u32 = 3;

/// spawn 同签名（args+result digest）连续重复停链阈值
pub const SPAWN_REPEAT_HALT: u32 = 3;

/// 链累计 token 预算默认口径：4 × 会话上下文窗口
/// （链深硬顶 3 + 根会话 1 = 链上至多 4 个会话 × 一个窗口；K-06 口径：
/// 既有常数族拍定值，无实证锚，随 budget-report 数据校准后调）
pub const CHAIN_BUDGET_WINDOW_MULT: u64 = 4;

// ===== 护栏判据（纯函数，测试锁确定性） =====

/// 深度硬顶检查：child_depth 超过 [`MAX_CHAIN_DEPTH`] 即拒
pub fn check_depth(child_depth: u32) -> Result<(), String> {
    if child_depth > MAX_CHAIN_DEPTH {
        Err(format!(
            "session_spawn rejected: chain depth cap reached (child depth {child_depth} > \
             max {MAX_CHAIN_DEPTH}); finish this session and hand over to a human instead"
        ))
    } else {
        Ok(())
    }
}

/// 链预算检查：链累计消耗达上限即拒
pub fn check_budget(used: u64, budget: u64) -> Result<(), String> {
    if used >= budget {
        Err(format!(
            "session_spawn rejected: chain token budget exhausted ({used}/{budget}); \
             finish this session instead of extending the chain"
        ))
    } else {
        Ok(())
    }
}

/// spawn 同签名重复观察器（stagnation 三元组 digest 同源构词法；任一新
/// digest 即计数重置——确定性、可回放）
#[derive(Default)]
struct SpawnRepeatGuard {
    last_digest: Option<u64>,
    count: u32,
}

impl SpawnRepeatGuard {
    /// 观察一次（args+result 联合摘要）；返回 true = 连续重复达停链阈值
    fn observe(&mut self, digest: u64) -> bool {
        if self.last_digest == Some(digest) {
            self.count += 1;
        } else {
            self.last_digest = Some(digest);
            self.count = 1;
        }
        self.count >= SPAWN_REPEAT_HALT
    }
}

fn spawn_digest(args: &str, result: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (args, result).hash(&mut h);
    h.finish()
}

// ===== 链共享态（全链一个实例，随组件快照沿链传递） =====

/// 会话链共享运行态：停链标记 + 链累计 token 计量 + spawn 同签名观察。
/// 子会话经 runner 组件快照继承同一 `Arc`——链上任一成员置停链后，
/// 全链后续 spawn 预检一致拒绝（停链语义）。
pub struct ChainRuntimeState {
    halted: Mutex<Option<String>>,
    tokens_used: Arc<AtomicU64>,
    budget_tokens: u64,
    spawn_repeats: Mutex<SpawnRepeatGuard>,
}

impl ChainRuntimeState {
    /// 构造（budget_tokens = 链累计 token 预算上限）
    pub fn new(budget_tokens: u64) -> Self {
        Self {
            halted: Mutex::new(None),
            tokens_used: Arc::new(AtomicU64::new(0)),
            budget_tokens,
            spawn_repeats: Mutex::new(SpawnRepeatGuard::default()),
        }
    }

    /// 置停链标记（幂等：保留首个原因）
    pub fn halt(&self, reason: String) {
        let mut g = self.halted.lock().unwrap_or_else(|p| p.into_inner());
        if g.is_none() {
            *g = Some(reason);
        }
    }

    /// 停链原因（None = 链未停）
    pub fn halt_reason(&self) -> Option<String> {
        self.halted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// 链 token 计量器（全链共享同一 Arc；runner 以 with_token_counter 注入）
    pub fn token_counter(&self) -> Arc<AtomicU64> {
        self.tokens_used.clone()
    }

    /// 链累计消耗快照
    pub fn tokens_used(&self) -> u64 {
        self.tokens_used.load(Ordering::Relaxed)
    }

    /// 链预算上限
    pub fn budget_tokens(&self) -> u64 {
        self.budget_tokens
    }

    /// spawn 同签名观察（见 [`SpawnRepeatGuard`]）；true = 触发停链判据
    fn observe_spawn(&self, args: &str, result: &str) -> bool {
        let digest = spawn_digest(args, result);
        self.spawn_repeats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .observe(digest)
    }
}

// ===== 子会话侧轮结局观察（TurnEndGuard 挂接） =====

/// 会话链熔断观察器（每会话本地窗口状态；跨轮持续）。
/// 窗口 = 本会话前 [`CHAIN_WATCH_TURNS`] 轮：连续 `error` 轮达窗口数 →
/// 停链（返回判据文本，由 turn 守卫落 journal chain_halted）。
/// `success` 重置计数（有真实进展即不是同类失败）；`cancelled`/`aborted`
/// 不计（人工接管/降级路径非自主失败）。
pub struct ChainWatch {
    shared: Arc<ChainRuntimeState>,
    window_turns: u32,
    consecutive_errors: u32,
}

impl ChainWatch {
    pub(crate) fn new(shared: Arc<ChainRuntimeState>) -> Self {
        Self {
            shared,
            window_turns: CHAIN_WATCH_TURNS,
            consecutive_errors: 0,
        }
    }

    /// 观察一轮结局（turn_seq 1-based）；Some(reason) = 触发停链
    pub(crate) fn observe(&mut self, turn_seq: u64, status: &str) -> Option<String> {
        if turn_seq > self.window_turns as u64 {
            return None; // 观察窗口已关闭
        }
        if status == "success" {
            self.consecutive_errors = 0;
            return None;
        }
        if status != "error" {
            return None;
        }
        self.consecutive_errors += 1;
        if self.consecutive_errors >= self.window_turns {
            let reason = format!(
                "chain halted: {0} consecutive errored turns within first {0} turns of a \
                 spawned session",
                self.window_turns
            );
            self.shared.halt(reason.clone());
            Some(reason)
        } else {
            None
        }
    }
}

// ===== 首轮 goal（B 全自主：spawn 执行体自动驱动） =====

/// 子会话首轮 goal：指向交接档的自检续接协议（聚焦信号重注入）。
pub fn compose_first_goal(parent_session_id: &str, chain_depth: u32) -> String {
    format!(
        "[session chain] You are a continuation session forked from parent session \
         {parent_session_id} (chain depth {chain_depth}). First action: call handover_read \
         to load the latest handover document, verify its `verification` criterion to confirm \
         a successful continuation, then work through `todo_next` in order. If handover_read \
         returns empty (no handover document), report that honestly and wait for instructions \
         instead of guessing the task."
    )
}

// ===== 执行体（占位/接线双态，查账工具族同构） =====

/// `session_spawn` 执行体（占位启动期注册 fail-visible；会话期重绑接线）
pub struct SessionSpawnTool {
    wiring: Option<SpawnWiring>,
}

struct SpawnWiring {
    /// 父会话 ID（spawn 发起方）
    session_id: String,
    client: EvoruleApiClient,
    /// 链共享态（runner 组件快照沿链传递）
    chain: Arc<ChainRuntimeState>,
    /// 父会话 journal（停链 chain_halted 落账锚；None = 无 journal 会话，
    /// 停链标记照置、账面镜像在 tool_result，语义事件缺席如实降级）
    journal: Option<Arc<JournalWriter>>,
    /// 子 runner 工厂（runner.spawn_factory 产出；组件快照重建 + 链态注入）
    factory: Arc<dyn Fn(&str) -> crate::agent::runner::AgentRunner + Send + Sync>,
    /// candidate 首调审批锚（PR-H4 验收修复）：本会话首次 spawn 实际执行前
    /// 为 false——evaluate_proposal 据此出 needs_approval 提案；fork 成功
    /// （spawn 真实发生）后置 true，同会话后续 spawn 不再提案（仍有护栏
    /// 三件+P2 事前意图裁决）。拒绝后重试继续出提案——不给人拒后重试
    /// 绕过审批的口子（fail-closed）。
    first_spawn_done: AtomicBool,
}

impl SessionSpawnTool {
    /// 占位构造（启动期 default_safe_toolkit 注册；调用 fail-visible 报错）
    pub fn unwired() -> Self {
        Self { wiring: None }
    }

    /// 接线构造（runner 会话期重绑）
    pub fn wired(
        session_id: String,
        client: EvoruleApiClient,
        chain: Arc<ChainRuntimeState>,
        journal: Option<Arc<JournalWriter>>,
        factory: Arc<dyn Fn(&str) -> crate::agent::runner::AgentRunner + Send + Sync>,
    ) -> Self {
        Self {
            wiring: Some(SpawnWiring {
                session_id,
                client,
                chain,
                journal,
                factory,
                first_spawn_done: AtomicBool::new(false),
            }),
        }
    }

    /// 首次 spawn 真实执行后由 call() 标记（evaluate_proposal 停止出提案）
    fn mark_first_spawn_done(&self) {
        if let Some(w) = &self.wiring {
            w.first_spawn_done.store(true, Ordering::Relaxed);
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for SessionSpawnTool {
    /// 管道⑤评估单源（PR-4 收编）——PR-H4 验收修复补实现：Sensitive+
    /// ManualDefault 声明档（tool_manifest 双闸）的执行面兑现。candidate
    /// 首调=needs_approval 提案（设计档 §四权限面口径）：本会话首次 spawn
    /// 执行前每次调用都出提案（批准前重试不绕审批=fail-closed），fork 成功
    /// 后同会话不再提案。unwired（启动期占位）返回 None——call 期
    /// fail-visible 与占位语义一致。
    fn evaluate_proposal(&self, _args: &Value) -> Option<Value> {
        let w = self.wiring.as_ref()?;
        if w.first_spawn_done.load(Ordering::Relaxed) {
            return None;
        }
        Some(json!({
            "status": "needs_approval",
            "category": "candidate",
            "description": "spawn a child session that continues the current task from the \
                            latest handover document (autonomous handover chain)",
            "risk": "opens a new agent session which auto-runs with this session's tool surface",
            "alternative": "finish this session and open a new session manually"
        }))
    }

    async fn call(&self, args: &Value) -> Result<Value, String> {
        let Some(w) = &self.wiring else {
            return Err(format!(
                "{SESSION_SPAWN_TOOL}: not wired to a session yet (registered as a startup \
                 placeholder; it is re-bound once a run session exists)"
            ));
        };
        // 护栏 0：链已停 → 一律拒绝（停链可查，fail-visible 带原因）
        if let Some(reason) = w.chain.halt_reason() {
            return Err(format!("session_spawn rejected: chain halted ({reason})"));
        }
        // 护栏 1：链深度硬顶（深度上溯链权威=server）
        let hops = chain_ancestor_hops(&w.client, &w.session_id).await?;
        let child_depth = hops + 1;
        check_depth(child_depth)?;
        // 护栏 2：链累计 token 预算
        let used = w.chain.tokens_used();
        check_budget(used, w.chain.budget_tokens())?;

        // fork：server 机制层白得 parent_session_id 因果链 + 声明继承 + 内容哈希
        let child_id = w
            .client
            .create_session_fork(&w.session_id, None)
            .await
            .map_err(|e| format!("session_spawn: fork failed ({e})"))?;
        // spawn 真实发生：candidate 首调审批锚落定（同会话后续调用不再提案）
        self.mark_first_spawn_done();

        // B 全自主（项目方裁定）：子会话首轮 goal 自动驱动——组件快照重建
        // runner，run_continuation 注入首条 goal（读交接档自检），后台排水
        // 事件流（G16 续跑语义，ws_handler 同构）。
        let first_goal = compose_first_goal(&w.session_id, child_depth);
        let child_runner = (w.factory)(&child_id);
        let mut stream = child_runner.run_continuation(child_id.clone(), first_goal.clone());
        tokio::spawn(async move {
            use futures_util::StreamExt;
            while let Some(item) = stream.next().await {
                if let Err(e) = item {
                    tracing::warn!(error = %e, "session_spawn: child continuation errored");
                }
            }
        });

        let result = json!({
            "status": "spawned",
            "child_session_id": child_id,
            "parent_session_id": w.session_id,
            "chain_depth": child_depth,
            "first_goal": first_goal,
            "note": "child session auto-started with a handover self-check goal. Per the \
                     handover protocol: finish this session with a closing summary after \
                     spawning (write the handover BEFORE spawning if not yet written).",
        });

        // 护栏 3：spawn 同签名熔断（成功 fork 的 child_id 唯一 → 摘要必新；
        // 连续同签名重复（同一拒绝反复触发/同形态失败）达阈值 → 停链）
        if w.chain
            .observe_spawn(&args.to_string(), &result.to_string())
        {
            let reason =
                "chain halted: session_spawn invoked with identical signature 3 times in a row"
                    .to_string();
            w.chain.halt(reason.clone());
            if let Some(j) = &w.journal {
                if let Err(e) = j.chain_halted(&w.session_id, &reason) {
                    tracing::warn!(error = %e, "chain_halted journal append failed");
                }
            }
            return Err(format!(
                "session_spawn rejected: {reason} (this and prior attempts are recorded)"
            ));
        }

        Ok(result)
    }
}

/// 深度上溯（权威=server）：返回会话的祖先跳数（根=0）。有环防护：超过
/// 深度硬顶 +1 跳即停止（调用方按超深拒绝，不无限上溯）。
async fn chain_ancestor_hops(client: &EvoruleApiClient, session_id: &str) -> Result<u32, String> {
    let mut sid = session_id.to_string();
    let mut hops = 0u32;
    loop {
        if hops > MAX_CHAIN_DEPTH {
            // 已经深过硬顶+1：无需继续上溯，调用方按 child_depth 拒绝
            return Ok(hops);
        }
        let meta = client
            .get_session_metadata(&sid)
            .await
            .map_err(|e| format!("session_spawn: session metadata unreachable ({e})"))?;
        match meta.parent_session_id {
            Some(parent) => {
                hops += 1;
                sid = parent;
            }
            None => return Ok(hops),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_guard_rejects_beyond_cap() {
        assert!(check_depth(0).is_ok());
        assert!(check_depth(MAX_CHAIN_DEPTH).is_ok());
        assert!(check_depth(MAX_CHAIN_DEPTH + 1).is_err());
        let err = check_depth(MAX_CHAIN_DEPTH + 1).unwrap_err();
        assert!(err.contains("depth cap"), "{err}");
    }

    #[test]
    fn budget_guard_rejects_at_exhaustion() {
        assert!(check_budget(0, 100).is_ok());
        assert!(check_budget(99, 100).is_ok());
        assert!(check_budget(100, 100).is_err());
        assert!(check_budget(101, 100).is_err());
        let err = check_budget(100, 100).unwrap_err();
        assert!(err.contains("budget exhausted"), "{err}");
    }

    #[test]
    fn spawn_repeat_guard_halts_at_threshold_and_resets() {
        let mut g = SpawnRepeatGuard::default();
        assert!(!g.observe(7));
        assert!(!g.observe(7));
        assert!(g.observe(7)); // 第 3 次同签名 → 停链判据
                               // 新签名重置
        let mut g2 = SpawnRepeatGuard::default();
        assert!(!g2.observe(1));
        assert!(!g2.observe(2));
        assert!(!g2.observe(1));
        assert!(!g2.observe(1));
        assert!(g2.observe(1)); // 连续计数:1,2,1,1,1 → 第 3 次连续 1
    }

    #[test]
    fn spawn_digest_is_deterministic_and_discriminating() {
        assert_eq!(spawn_digest("a", "b"), spawn_digest("a", "b"));
        assert_ne!(spawn_digest("a", "b"), spawn_digest("a", "c"));
        assert_ne!(spawn_digest("a", "b"), spawn_digest("b", "a"));
    }

    #[test]
    fn chain_state_halt_is_idempotent_first_reason_wins() {
        let st = ChainRuntimeState::new(1000);
        assert!(st.halt_reason().is_none());
        st.halt("first".into());
        st.halt("second".into());
        assert_eq!(st.halt_reason().as_deref(), Some("first"));
    }

    #[test]
    fn chain_state_token_counter_shared_across_clones() {
        let st = Arc::new(ChainRuntimeState::new(5000));
        let counter = st.token_counter();
        counter.fetch_add(42, Ordering::Relaxed);
        assert_eq!(st.tokens_used(), 42);
        assert_eq!(st.budget_tokens(), 5000);
    }

    #[test]
    fn chain_watch_halts_after_consecutive_errors_in_window() {
        let shared = Arc::new(ChainRuntimeState::new(1000));
        let mut w = ChainWatch::new(shared.clone());
        // 前 3 轮连续 error → 第 3 轮触发停链
        assert!(w.observe(1, "error").is_none());
        assert!(w.observe(2, "error").is_none());
        let reason = w.observe(3, "error").expect("halt at window boundary");
        assert!(reason.contains("consecutive errored turns"), "{reason}");
        assert!(shared.halt_reason().is_some());
        // 窗口外轮不再观察
        assert!(w.observe(4, "error").is_none());
    }

    #[test]
    fn chain_watch_success_resets_and_non_error_ignored() {
        let shared = Arc::new(ChainRuntimeState::new(1000));
        let mut w = ChainWatch::new(shared.clone());
        assert!(w.observe(1, "error").is_none());
        assert!(w.observe(2, "success").is_none());
        assert!(w.observe(3, "error").is_none());
        assert!(shared.halt_reason().is_none());
        // cancelled 不计为自主失败
        assert!(w.observe(3, "cancelled").is_none());
        assert!(w.observe(3, "aborted").is_none());
        assert!(shared.halt_reason().is_none());
        // success 重置后需重新累计（先一个真实 success 清零，再逐次累计）
        assert!(w.observe(1, "success").is_none());
        assert!(w.observe(1, "error").is_none());
        assert!(w.observe(2, "error").is_none());
        assert!(w.observe(3, "error").is_some());
        assert!(shared.halt_reason().is_some());
    }

    #[test]
    fn chain_watch_window_closes_after_window_turns() {
        let shared = Arc::new(ChainRuntimeState::new(1000));
        let mut w = ChainWatch::new(shared);
        // 窗口内 2 次 error 后窗口关闭（第 4 轮起不再观察）
        assert!(w.observe(1, "error").is_none());
        assert!(w.observe(2, "error").is_none());
        assert!(w.observe(3, "success").is_none());
        assert!(w.observe(4, "error").is_none());
        assert!(w.observe(9, "error").is_none());
    }

    #[test]
    fn first_goal_points_at_handover_self_check() {
        let goal = compose_first_goal("sess-42", 2);
        assert!(goal.contains("sess-42"), "{goal}");
        assert!(goal.contains("handover_read"), "{goal}");
        assert!(goal.contains("verification"), "{goal}");
        assert!(goal.contains("depth 2"), "{goal}");
    }

    #[tokio::test]
    async fn unwired_tool_fails_visible() {
        let t = SessionSpawnTool::unwired();
        let err = t.call(&json!({})).await.unwrap_err();
        assert!(err.contains("not wired"), "{err}");
    }

    #[tokio::test]
    async fn unwired_proposal_is_none() {
        // 占位态不产提案：call 期 fail-visible 与占位语义一致
        let t = SessionSpawnTool::unwired();
        assert!(t.evaluate_proposal(&json!({})).is_none());
    }

    #[tokio::test]
    async fn first_spawn_proposes_until_executed() {
        // candidate 首调审批（PR-H4 验收修复）：首次执行前每次调用出
        // needs_approval 提案（拒绝后重试不绕审批=fail-closed）；fork 成功
        // （mark_first_spawn_done）后同会话不再提案。
        let client = crate::api::evorule_client::EvoruleApiClient::new("http://localhost:1");
        let chain = Arc::new(ChainRuntimeState::new(1_000));
        let factory: Arc<dyn Fn(&str) -> crate::agent::runner::AgentRunner + Send + Sync> =
            Arc::new(|sid| panic!("factory must not be invoked by evaluate_proposal (got {sid})"));
        let t = SessionSpawnTool::wired("1".to_string(), client, chain, None, factory);
        let p = t
            .evaluate_proposal(&json!({}))
            .expect("first call proposes");
        assert_eq!(p["status"], "needs_approval");
        assert_eq!(p["category"], "candidate");
        assert!(p["description"].as_str().unwrap().contains("child session"));
        // 拒绝后重试：提案持续在场（fail-closed）
        assert!(t.evaluate_proposal(&json!({})).is_some());
        // fork 成功标记后：同会话后续调用不再提案
        t.mark_first_spawn_done();
        assert!(t.evaluate_proposal(&json!({})).is_none());
    }
}
