//! GovLedger：治理事件写入 evorule 共享账本（B22 A 项）
//!
//! 写入通道=既有 [`EvoruleApiClient::update_payload`]——`shared.*` 前缀
//! 路径由 evorule 服务端 P3 广播进共享表（23 号档 §二「引擎侧零改动」
//! 判断）；查询面（`get_shared_facts`）与防膨胀（`mark_shared_facts_rollup`）
//! 同样复用既有通道。本模块只做：seq 分配、路径构造、事件 schema、
//! best-effort 写入与失败留痕。
//!
//! sidecar 会话管理沿 [`audited_llm`] 建链模式（瞬态错误有界重试），
//! 但为**进程生命周期单会话**（非一次一会话）：创建成功后缓存复用，
//! 写入失败时清空缓存、下次写入时重建（重启分区语义见模块文档）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::warn;

use crate::api::api_core::ApiError;
use crate::api::evorule_client::EvoruleApiClient;

/// gov 命名空间（固定；23 号档 §八风险 3，与 `__memory__`/`shared.{agent_ns}` 隔离）
pub const GOV_NAMESPACE: &str = "gov";

/// 共享账本 gov 子树前缀（`audit_query` scope=gov 查询前缀）
pub const GOV_SHARED_PREFIX: &str = "shared.gov.";

/// sidecar 治理会话 caller_role：**不声明**——server 侧白名单仅 `human | llm`
/// （workspace_service.rs 建链校验），gov 写入者是 serve 进程内部调用
/// （23 号档 §四.3「serve 内部调用，非 agent 工具」），两者皆不是；
/// 沿 sediment 主会话（agent 建链不声明角色）同款前例，声明扩展留
/// evorule 侧缺口登记后另议。此常量仅作文档锚点。
pub const GOV_CALLER_ROLE: Option<&str> = None;

/// 建链重试次数与间隔（瞬态错误有界重试；best-effort 语义下穷尽即失败留痕，不阻断）
const SESSION_SETUP_RETRIES: u32 = 3;
const SESSION_SETUP_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// 治理事件类型（v1 收录六类，23 号档 §3.1 事件类型清单）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovEventKind {
    /// 工作台审批卡请求（G8 审批链，17 号审批层）
    ApprovalRequested,
    /// 工作台审批卡决定
    ApprovalResolved,
    /// policy 三态判定（18 号 §五；判定依据摘要=serve 侧封存锚点的持久化镜像）
    PolicyVerdict,
    /// S4 产物定稿（agent 草稿→人定稿，工作台既有流程）
    S4Publish,
    /// terminal PTY 会话生命周期（create/close 两态，09 号 §四 record-only 留痕）
    TerminalSession(TerminalAction),
    /// B21 journal 重大事件镜像（compaction 完成/resume 恢复）
    B21JournalMilestone,
}

/// terminal 会话动作（create/close 拆两路径段，查询面单段精确）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAction {
    /// 会话创建（open）
    Create,
    /// 会话关闭（close）
    Close,
}

impl GovEventKind {
    /// 路径 kind 段
    pub fn path_segment(&self) -> &'static str {
        match self {
            Self::ApprovalRequested => "approval_requested",
            Self::ApprovalResolved => "approval_resolved",
            Self::PolicyVerdict => "policy_verdict",
            Self::S4Publish => "s4_publish",
            Self::TerminalSession(TerminalAction::Create) => "terminal_session_create",
            Self::TerminalSession(TerminalAction::Close) => "terminal_session_close",
            Self::B21JournalMilestone => "b21_journal_milestone",
        }
    }
}

/// 事件行为者（23 号档 value schema `actor`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovActor {
    /// 人工（工作台操作者）
    User,
    /// agent 自主动作
    Agent,
    /// policy 评审链判定
    Policy,
}

/// 引用指针（摘要+引用级，不存全文——隐私边界：指令/产物全文已在 journal
/// 与会话事实，gov 只存治理摘要+指针，23 号档 §3.1 schema 注记）
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovRefs {
    /// 关联 B21 journal 事件序号（两链互指，23 号档 §四.5）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_seq: Option<u64>,
    /// 关联审批卡 id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    /// 关联引擎 Fact id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_id: Option<u64>,
}

/// 治理事件 value schema（23 号档 §3.1）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovEvent {
    /// 事件 id：`G` + 6 位零填充进程内序号（示例 `G000123`）
    pub event_id: String,
    /// 事件类型（六类 kind，路径段由 [`GovEventKind::path_segment`] 承载）
    pub kind: GovEventKind,
    /// unix 秒
    pub ts: u64,
    /// 事件关联的 agent/PTY 会话 id（gov sidecar 会话 id 是载体，不落此处）
    pub session_id: String,
    /// 行为者（user/agent/policy）
    pub actor: GovActor,
    /// 治理摘要（一句话；全文指针走 refs——不存全文，隐私边界）
    pub summary: String,
    /// 引用指针（journal/approval/fact 三类互指锚）
    #[serde(default)]
    pub refs: GovRefs,
}

/// 进程内 seq 分配器（单调递增，跨 kind 共用一个计数；事件路径
/// `{ts}.{seq}` 双段在跨进程重启场景下仍保证唯一）
#[derive(Debug, Default)]
pub struct GovSeq(AtomicU64);

impl GovSeq {
    /// 取下一序号（从 1 起）
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 当前已分配序号（观测用）
    pub fn peek(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// 事件路径构造（纯函数）：`shared.gov.{ns}.{kind}.{ts}.{seq}`
pub fn gov_event_path(ns: &str, kind: &GovEventKind, ts: u64, seq: u64) -> String {
    format!("{GOV_SHARED_PREFIX}{ns}.{}.{ts}.{seq}", kind.path_segment())
}

/// 事件 id 构造：`G` + 6 位零填充序号
pub fn gov_event_id(seq: u64) -> String {
    format!("G{seq:06}")
}

/// unix 秒（测试注入用 pub(crate) 纯包装）
pub(crate) fn unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// GovLedger：治理事件写入器（进程生命周期单 sidecar 会话 + best-effort 语义）
///
/// 挂载方式：进程级单例（[`init_ledger`] / [`current_ledger`]）——
/// AgentApiState 位于 agent_api.rs（B22 零触碰面），不新增字段。
pub struct GovLedger {
    client: EvoruleApiClient,
    ns: &'static str,
    /// sidecar 会话缓存（None=未建/已失效，下次写入时重建）
    session: Mutex<Option<String>>,
    seq: GovSeq,
}

impl GovLedger {
    /// 创建写入器（不建链——首条写入时惰性 ensure）
    pub fn new(client: EvoruleApiClient) -> Self {
        Self {
            client,
            ns: GOV_NAMESPACE,
            session: Mutex::new(None),
            seq: GovSeq::default(),
        }
    }

    /// 已分配事件数（观测用）
    pub fn allocated(&self) -> u64 {
        self.seq.peek()
    }

    /// 确保 sidecar 治理会话可用（创建成功即缓存复用；瞬态错误有界重试）
    pub async fn ensure_session(&self) -> Result<String, ApiError> {
        let mut guard = self.session.lock().await;
        if let Some(sid) = guard.as_ref() {
            return Ok(sid.clone());
        }
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match self.client.create_session(None, GOV_CALLER_ROLE).await {
                Ok(sid) => {
                    *guard = Some(sid.clone());
                    return Ok(sid);
                }
                Err(e) if attempt <= SESSION_SETUP_RETRIES => {
                    warn!(
                        attempt,
                        error = %e,
                        "gov_ledger: sidecar session setup failed, retrying (best-effort)"
                    );
                    tokio::time::sleep(SESSION_SETUP_RETRY_DELAY).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 记录一条治理事件（best-effort：失败 warn 留痕并返回 Err，不阻断治理操作）
    ///
    /// 失败路径清空会话缓存，下次写入自动重建（evorule 重启恢复后自愈）。
    pub async fn record(
        &self,
        kind: GovEventKind,
        session_id: &str,
        actor: GovActor,
        summary: String,
        refs: GovRefs,
    ) -> Result<GovEvent, ApiError> {
        let seq = self.seq.next();
        let event = GovEvent {
            event_id: gov_event_id(seq),
            kind,
            ts: unix_ts(),
            session_id: session_id.to_string(),
            actor,
            summary,
            refs,
        };
        // 路径在此处用本地 seq 构造（并发下 peek 会串号）
        let path = gov_event_path(self.ns, &kind, event.ts, seq);
        self.write(&event, &path).await.map(|_| event)
    }

    /// 底层写入（session ensure + update_payload；失败清缓存）
    async fn write(&self, event: &GovEvent, path: &str) -> Result<Option<u64>, ApiError> {
        let session_id = match self.ensure_session().await {
            Ok(sid) => sid,
            Err(e) => {
                warn!(
                    event_id = %event.event_id,
                    kind = event.kind.path_segment(),
                    error = %e,
                    "gov_ledger: record dropped (evorule unreachable) — best-effort, governance ops unaffected"
                );
                return Err(e);
            }
        };
        let value = match serde_json::to_value(event) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    event_id = %event.event_id,
                    error = %e,
                    "gov_ledger: event serialize failed — best-effort, record dropped"
                );
                return Err(ApiError::InvalidResponse);
            }
        };
        match self.client.update_payload(&session_id, path, &value).await {
            Ok(fact_id) => Ok(fact_id),
            Err(e) => {
                // 写入失败：会话缓存作废（可能 evorule 重启/会话失效），下次重建
                *self.session.lock().await = None;
                warn!(
                    event_id = %event.event_id,
                    kind = event.kind.path_segment(),
                    %path,
                    error = %e,
                    "gov_ledger: record failed — best-effort, governance ops unaffected"
                );
                Err(e)
            }
        }
    }
}

// =============================================================================
// 进程级单例（serve 面）
// =============================================================================

static LEDGER: OnceLock<Arc<GovLedger>> = OnceLock::new();

/// serve 启动时安装进程级 GovLedger（幂等：重复调用返回既有实例）
///
/// CLI 模式不调用本函数——治理事件为工作台 serve 面，CLI 调用面以
/// [`current_ledger`] 判空 no-op。
pub fn init_ledger(client: EvoruleApiClient) -> Arc<GovLedger> {
    LEDGER
        .get_or_init(|| Arc::new(GovLedger::new(client)))
        .clone()
}

/// 取当前进程 GovLedger（未初始化 → None）
pub fn current_ledger() -> Option<Arc<GovLedger>> {
    LEDGER.get().cloned()
}

// =============================================================================
// 单测：seq 分配 / 路径构造 / schema / best-effort 失败留痕（PR1 判据）
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_construction_covers_all_six_kinds() {
        let ts = 1_790_400_000u64;
        let cases = [
            (
                GovEventKind::ApprovalRequested,
                "shared.gov.gov.approval_requested.1790400000.1",
            ),
            (
                GovEventKind::ApprovalResolved,
                "shared.gov.gov.approval_resolved.1790400000.2",
            ),
            (
                GovEventKind::PolicyVerdict,
                "shared.gov.gov.policy_verdict.1790400000.3",
            ),
            (
                GovEventKind::S4Publish,
                "shared.gov.gov.s4_publish.1790400000.4",
            ),
            (
                GovEventKind::TerminalSession(TerminalAction::Create),
                "shared.gov.gov.terminal_session_create.1790400000.5",
            ),
            (
                GovEventKind::TerminalSession(TerminalAction::Close),
                "shared.gov.gov.terminal_session_close.1790400000.6",
            ),
            (
                GovEventKind::B21JournalMilestone,
                "shared.gov.gov.b21_journal_milestone.1790400000.7",
            ),
        ];
        for (i, (kind, expected)) in cases.iter().enumerate() {
            assert_eq!(
                &gov_event_path(GOV_NAMESPACE, kind, ts, (i + 1) as u64),
                expected,
                "kind={}",
                kind.path_segment()
            );
            assert!(expected.starts_with(GOV_SHARED_PREFIX));
        }
    }

    #[test]
    fn event_id_is_zero_padded_g_prefix() {
        assert_eq!(gov_event_id(1), "G000001");
        assert_eq!(gov_event_id(123), "G000123");
        assert_eq!(gov_event_id(1_234_567), "G1234567");
    }

    #[test]
    fn seq_allocator_is_monotonic_from_one() {
        let seq = GovSeq::default();
        assert_eq!(seq.peek(), 0);
        assert_eq!(seq.next(), 1);
        assert_eq!(seq.next(), 2);
        assert_eq!(seq.next(), 3);
        assert_eq!(seq.peek(), 3);
    }

    #[test]
    fn gov_event_schema_roundtrip_and_refs_omission() {
        let event = GovEvent {
            event_id: gov_event_id(1),
            kind: GovEventKind::ApprovalResolved,
            ts: 1_790_400_000,
            session_id: "agent-session-ref".to_string(),
            actor: GovActor::User,
            summary: "shell_exec cargo test 批准（manual 档，人工批）".to_string(),
            refs: GovRefs {
                journal_seq: Some(42),
                approval_id: Some("A-17".to_string()),
                fact_id: None,
            },
        };
        let value = serde_json::to_value(&event).unwrap();
        // 回读一致（先反序列化再检查键集——from_value 消费 value）
        let back: GovEvent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, event);
        // 隐私边界：value 仅含 schema 七键（摘要+引用级；键序按 Map 排序比较）
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut keys_sorted = keys.clone();
        keys_sorted.sort_unstable();
        let mut expected_sorted = vec![
            "event_id",
            "kind",
            "ts",
            "session_id",
            "actor",
            "summary",
            "refs",
        ];
        expected_sorted.sort_unstable();
        assert_eq!(keys_sorted, expected_sorted);
        // None 引用段省略（不写 null）
        assert_eq!(value["refs"]["journal_seq"], 42);
        assert_eq!(value["refs"]["approval_id"], "A-17");
        assert!(value["refs"].get("fact_id").is_none());
    }

    #[test]
    fn gov_event_schema_has_minimal_surface_without_refs() {
        let event = GovEvent {
            event_id: gov_event_id(2),
            kind: GovEventKind::TerminalSession(TerminalAction::Create),
            ts: 1_790_400_000,
            session_id: "pty-1".to_string(),
            actor: GovActor::Agent,
            summary: "terminal session opened".to_string(),
            refs: GovRefs::default(),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert!(value["refs"].as_object().unwrap().is_empty());
        let back: GovEvent = serde_json::from_value(value).unwrap();
        assert_eq!(back, event);
    }

    /// best-effort 失败留痕：evorule 不可达 → record 返回 Err（调用面不阻断）、
    /// 序号已消耗（留痕语义：失败也计序）、后续 record 自动重试建链。
    #[tokio::test]
    async fn record_best_effort_failure_leaves_warn_and_recovers_retry_path() {
        // 不可达端口：连接立即拒绝（无监听），不挂测试
        let ledger = GovLedger::new(EvoruleApiClient::new("http://127.0.0.1:1"));
        let err = ledger
            .record(
                GovEventKind::ApprovalRequested,
                "sess-1",
                GovActor::Agent,
                "shell_exec rm -rf 请求批准".to_string(),
                GovRefs::default(),
            )
            .await;
        assert!(err.is_err(), "unreachable evorule must yield Err");
        assert_eq!(ledger.allocated(), 1, "failed record still consumes seq");
        // 二次 record：会话缓存仍空，走完整重建路径（不因首败卡死）
        let err2 = ledger
            .record(
                GovEventKind::S4Publish,
                "sess-1",
                GovActor::User,
                "artifact finalized".to_string(),
                GovRefs::default(),
            )
            .await;
        assert!(err2.is_err());
        assert_eq!(ledger.allocated(), 2);
    }

    /// 成功路径：桩 HTTP 服务（200 应答）验证 record 全链路——建会话→
    /// update_payload 落 `shared.gov.*` 路径→事件体 schema 正确。
    #[tokio::test]
    async fn record_success_writes_gov_path_via_existing_channel() {
        use std::sync::mpsc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (tx, rx) = mpsc::channel::<String>();
        let tx_server = tx.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let tx = tx_server.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = tx.send(req.clone());
                    // create_session 应答 {"session_id": u64}；payload 应答带 fact_id
                    let body = if req.contains("/payload") {
                        r#"{"success":true,"message":"ok","data":null,"fact_id":118}"#
                    } else {
                        r#"{"success":true,"session_id":4242}"#
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let ledger = GovLedger::new(EvoruleApiClient::new(&format!("http://{addr}")));
        let event = ledger
            .record(
                GovEventKind::ApprovalResolved,
                "agent-sess-9",
                GovActor::Policy,
                "shell_exec cargo test 批准（policy 档，评审通过）".to_string(),
                GovRefs {
                    journal_seq: Some(7),
                    approval_id: Some("A-3".to_string()),
                    fact_id: None,
                },
            )
            .await
            .expect("stub server must accept record");
        assert_eq!(event.event_id, "G000001");
        assert_eq!(event.kind, GovEventKind::ApprovalResolved);

        server.abort();
        drop(tx);
        // 首个请求 = create_session（不声明 caller_role——server 白名单仅
        // human|llm，gov 写入者为 serve 内部调用，沿 sediment 主会话前例）
        let create_req = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            create_req.starts_with("POST /api/sessions"),
            "got: {create_req}"
        );
        assert!(
            !create_req.contains("caller_role"),
            "gov sidecar session must not declare a caller_role: {create_req}"
        );
        // 次个请求 = update_payload 落 shared.gov.* 路径，事件体摘要级
        let payload_req = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(payload_req.contains("/payload"), "got: {payload_req}");
        let path_kv = payload_req
            .split("\"path\":\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .unwrap_or_default()
            .to_string();
        // 路径形态：shared.gov.gov.<kind>.<ts>.<seq>
        assert!(
            path_kv.starts_with("shared.gov.gov.approval_resolved."),
            "got path: {path_kv}"
        );
        let tail = path_kv
            .trim_start_matches("shared.gov.gov.approval_resolved.")
            .split('.')
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(tail.len(), 2, "path tail must be <ts>.<seq>: {path_kv}");
        assert_eq!(tail[1], "1", "seq segment must match first record");
        assert!(
            tail[0].parse::<u64>().is_ok(),
            "ts segment must be unix seconds: {path_kv}"
        );
        assert!(payload_req.contains(r#""kind":"approval_resolved""#));
        assert!(payload_req.contains(r#""actor":"policy""#));
        assert!(payload_req.contains("评审通过"));
    }

    /// 进程级单例：init 幂等、current 在未初始化进程为 None。
    /// 注：单测进程内 init 会影响同进程其他测试——本测试只断言幂等性，
    /// 不断言 current_ledger 为 None（并发测试无进程隔离保证）。
    #[test]
    fn singleton_init_is_idempotent() {
        let a = init_ledger(EvoruleApiClient::new("http://127.0.0.1:1"));
        let b = init_ledger(EvoruleApiClient::new("http://127.0.0.1:2"));
        assert!(Arc::ptr_eq(&a, &b), "init_ledger must be idempotent");
        assert!(current_ledger().is_some());
    }
}
