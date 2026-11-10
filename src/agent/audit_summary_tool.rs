// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 摘要卡工具（audit_summary）——digest 级「最近发生了什么」小视图。
//!
//! 三卡合一（同次调用齐出）：
//! - `recent_events`：journal 最新事件头部清单（seq/ts/type + summary_line，
//!   新在前）——summary_line 由事件 type+锚点 id+短状态构成，禁内联
//!   full_json/full_text/cur_workflow 等大字段（同 journal 查询面禁令）；
//! - `digest`：sediment build_journal_digest 同构投影的轻量结构卡（轮目标/
//!   LLM 调用数/工具分布/成功失败/policy 拦截/审批拒绝/收尾状态）；
//! - `recent_facts`：会话 facts 元数据行（fact_id/path/ts/bytes）——禁
//!   value 全文，超限值只给 blake3 digest（budgeted_fact_rows 同口径）。
//!
//! 硬约束：输出总预算 64KiB（摘要卡定位=小视图，替代 scope=session 全量
//! 拉链检查），超预算截断列表并显式计数；任何输出字段超内联阈值按既有
//! 降级口径 `{truncated,bytes,digest}` blake3 标注，定位键
//! （seq/ts/type/fact_id/path）永不降级——本工具自身不做拉链放大器。
//!
//! 注册形态（audit_query 同构）：占位执行体启动期注册，runner 会话期重绑
//! （client/session_id/journal 在手）；开关 agentTools.audit_summary 缺省
//! 关——显式开启才进 LLM 契约。只读工具：不产生任何写入事实。

use serde_json::{json, Value};

use crate::agent::audit_query_tool::{oversized_value_marker, SESSION_FACTS_VALUE_INLINE_LIMIT};
use crate::agent::journal::JournalWriter;
use crate::agent::sediment::{journal_digest_stats, JournalDigestStats};
use crate::api::evorule_client::{EvoruleApiClient, FactEntry};
use crate::io_handlers::tool_handler::ToolFunction;

/// 工具名
pub const AUDIT_SUMMARY_TOOL: &str = "audit_summary";

/// 清单条数缺省与上限（recent_events / recent_facts 共用一参；摘要卡
/// 定位=小视图，上限低于 audit_query 的 100）
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;

/// 输出总预算（字节）：摘要卡定位=小视图（远小于 audit_query session 面
/// 的 128KiB），超预算截断列表并显式计数——本工具自身不做拉链放大器
const OUTPUT_BUDGET_BYTES: usize = 64 * 1024;

/// 信封开销预留（键名/session_id/计数器/预算测量余量等非列表字节）
const ENVELOPE_SLACK_BYTES: usize = 512;

/// 定位键集合（永不降级：降级清扫对其豁免——丢了定位键就回不了账）
const LOCATOR_KEYS: &[&str] = &["seq", "ts", "type", "fact_id", "path"];

/// 事件锚点键（summary_line 提取白名单，按优先级取前 2 个标量值）
const ANCHOR_KEYS: &[&str] = &[
    "call_id",
    "approval_id",
    "question_id",
    "turn_seq",
    "purpose",
    "tool",
    "model",
    "workflow_id",
    "node_id",
];

/// 状态词键（summary_line 短状态提取，取首个命中）
const STATUS_KEYS: &[&str] = &["status", "decision", "verdict"];

/// 锚点值展示截断长度（纯展示层；锚点 id 正常为短串，超长截断防意外）
const ANCHOR_VALUE_CAP: usize = 64;

/// 状态词展示截断长度
const STATUS_VALUE_CAP: usize = 32;

/// limit 参数解析（双卡共用一口径）：缺省 [`DEFAULT_LIMIT`]，越界
/// fail-visible
fn parse_summary_limit(args: &Value) -> Result<usize, String> {
    match args.get("limit") {
        None => Ok(DEFAULT_LIMIT),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_LIMIT as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| format!("audit_summary: limit must be an integer in 1..={MAX_LIMIT}")),
    }
}

/// 展示值截断（纯展示层：锚点 id/状态词超长截断并 … 标记；非账面字段）
fn display_truncate(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        let cut: String = s.chars().take(cap).collect();
        format!("{cut}…")
    }
}

/// summary_line 构造（事件 type+锚点 id+短状态）：白名单键提取，payload
/// 其余键不进卡——禁内联 full_json/full_text/cur_workflow 等大字段
/// （journal 查询面同款禁令）；锚点值超长展示截断。
fn summary_line(ev: &Value) -> String {
    let ty = ev.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
    let payload = ev.get("payload").cloned().unwrap_or(Value::Null);
    let scalar = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    };
    let mut line = ty.to_string();
    let mut anchors = 0usize;
    for key in ANCHOR_KEYS {
        if anchors >= 2 {
            break;
        }
        if let Some(v) = payload.get(*key).and_then(scalar) {
            line.push_str(&format!(
                " {key}={}",
                display_truncate(&v, ANCHOR_VALUE_CAP)
            ));
            anchors += 1;
        }
    }
    for key in STATUS_KEYS {
        if let Some(v) = payload.get(*key).and_then(|v| v.as_str()) {
            line.push(' ');
            line.push_str(&display_truncate(v, STATUS_VALUE_CAP));
            break;
        }
    }
    line
}

/// 输出字段降级清扫（既有降级口径的卡片级兜底）：任意字符串字段序列化
/// 超内联阈值 → `{truncated,bytes,digest}` blake3 标注（与
/// budgeted_fact_rows 单源口径）；定位键（seq/ts/type/fact_id/path）永不
/// 降级。返回是否发生降级。
fn downgrade_oversized_fields(v: &mut Value) -> bool {
    match v {
        // 叶子字符串：序列化超阈值 → 降级标注（bytes=序列化字节数含引号）
        Value::String(_) => {
            let serialized = serde_json::to_string(&*v).unwrap_or_default();
            if serialized.len() > SESSION_FACTS_VALUE_INLINE_LIMIT {
                *v = oversized_value_marker(&serialized);
                true
            } else {
                false
            }
        }
        Value::Object(map) => {
            let mut hit = false;
            for (key, val) in map.iter_mut() {
                // 定位键永不降级（含其子树——值为纯标量，无子树）
                if LOCATOR_KEYS.contains(&key.as_str()) {
                    continue;
                }
                hit |= downgrade_oversized_fields(val);
            }
            hit
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |acc, e| acc | downgrade_oversized_fields(e)),
        _ => false,
    }
}

/// recent_facts 元数据行（fact_id/path/ts/bytes）：禁 value 全文——超限值
/// 只给 blake3 digest（oversized_value_marker 同口径），全文按 fact_id 经
/// 因果链视图回账。ts 取 MemoryRecord 形态 value 的 timestamp（sediment
/// 写入形态）；手工/老数据形态无时点 → null。
fn fact_metadata_row(f: &FactEntry) -> Value {
    let serialized = serde_json::to_string(&f.value).unwrap_or_default();
    let bytes = serialized.len();
    let mut row = json!({
        "fact_id": f.id,
        "path": f.path,
        "ts": f.value.get("timestamp").and_then(|v| v.as_u64()),
        "bytes": bytes,
    });
    if bytes > SESSION_FACTS_VALUE_INLINE_LIMIT {
        // 超限值只给 digest：行自带 bytes，此处仅补 digest 字段（单源口径）
        row["digest"] = oversized_value_marker(&serialized)["digest"].clone();
    }
    row
}

/// digest 卡（sediment 投影同构轻量卡）：轮目标/LLM 调用数/工具分布/
/// 成功失败/policy 拦截/审批拒绝/收尾状态。失败明细（call_id 级）不进
/// 卡——定位归 recent_events 行，保持摘要卡小视图定位。
fn digest_card(stats: JournalDigestStats) -> Value {
    let tool_total: usize = stats.tool_by_name.values().sum();
    let ended = stats
        .ended
        .map(|(status, steps, ms)| json!({"status": status, "steps": steps, "duration_ms": ms}));
    json!({
        "goals": stats.goals,
        "llm_calls": stats.llm_total,
        "llm_react": stats.llm_react,
        "tool_calls": stats.tool_by_name,
        "tool_total": tool_total,
        "tool_ok": stats.tool_ok,
        "tool_failed": tool_total.saturating_sub(stats.tool_ok),
        "policy_blocked": stats.policy_blocked,
        "approvals_rejected": stats.approvals_rejected,
        "ended": ended.unwrap_or(Value::Null),
    })
}

/// 序列化字节数（失败按超预算处理，fail-safe 收口）
fn serialized_len(v: &Value) -> usize {
    serde_json::to_string(v)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
}

/// `audit_summary` 执行体（三卡装配 + 双层预算纪律）
pub(crate) async fn audit_summary_exec(
    session_id: &str,
    client: &EvoruleApiClient,
    journal: Option<&JournalWriter>,
    args: &Value,
) -> Result<Value, String> {
    let limit = parse_summary_limit(args)?;
    let writer = journal.ok_or_else(|| {
        "audit_summary: journal not available (no journal in this session)".to_string()
    })?;
    let lines = writer
        .read_lines()
        .map_err(|e| format!("audit_summary: journal read failed ({e})"))?;

    // 卡一：recent_events——journal 最新事件（新在前，seq 降序），行 =
    // 定位键（seq/ts/type）+ summary_line（禁 payload 平铺，零大字段内联）
    let total_events = lines.len();
    let skip = total_events.saturating_sub(limit);
    let mut event_rows: Vec<Value> = Vec::with_capacity(total_events - skip);
    for line in lines[skip..].iter().rev() {
        let ev = serde_json::to_value(&line.event)
            .map_err(|e| format!("audit_summary: journal event encode failed: {e}"))?;
        event_rows.push(json!({
            "seq": line.seq,
            "ts": line.ts,
            "type": ev.get("type").and_then(|v| v.as_str()).unwrap_or("unknown"),
            "summary_line": summary_line(&ev),
        }));
    }
    let events_truncated = total_events > event_rows.len();

    // 卡二：digest——sediment 投影同构轻量卡（journal_digest_stats 单源）
    let digest = digest_card(journal_digest_stats(&lines));

    // 卡三：recent_facts——元数据行（禁 value 全文），预算内装载
    let facts = client
        .get_facts(session_id, None)
        .await
        .map_err(|e| format!("audit_summary: session facts query failed ({e})"))?;
    let total_facts = facts.len();
    let mut sorted: Vec<&FactEntry> = facts.iter().collect();
    sorted.sort_by_key(|f| std::cmp::Reverse(f.id));
    let facts_count_truncated = total_facts > limit;

    let mut out = json!({
        "status": if total_events == 0 && total_facts == 0 { "empty" } else { "ok" },
        "session_id": session_id,
        "total_events": total_events,
        "returned_events": event_rows.len(),
        "recent_events": event_rows,
        "digest": digest,
        "total_facts": total_facts,
        "returned_facts": 0,
        "recent_facts": [],
    });
    // 字节预算：总预算 − 已占用（events+digest+信封余量）→ facts 行预算
    // 内装载（budgeted_fact_rows 同款收口哲学：字节预算先于条数上限生效）
    let base_bytes = serialized_len(&out);
    let remaining = OUTPUT_BUDGET_BYTES.saturating_sub(base_bytes + ENVELOPE_SLACK_BYTES);
    let mut acc = 0usize;
    let mut fact_rows: Vec<Value> = Vec::new();
    let mut budget_truncated = false;
    // 超限 value 降级发生标记（元数据行 digest 标注=value 降级，须显式回显）
    let mut value_downgraded = false;
    for f in sorted.into_iter().take(limit) {
        let row = fact_metadata_row(f);
        if row.get("digest").is_some() {
            value_downgraded = true;
        }
        let size = serialized_len(&row);
        if acc + size > remaining {
            budget_truncated = true;
            break;
        }
        acc += size;
        fact_rows.push(row);
    }
    out["returned_facts"] = json!(fact_rows.len());
    out["recent_facts"] = Value::Array(fact_rows);

    // 字段级降级清扫（定位键豁免）——既有降级口径的卡片级兜底；
    // recent_facts 超限 value 的 digest 降级同样计入
    let field_truncated = downgrade_oversized_fields(&mut out) || value_downgraded;

    // 总预算兜底（构造性达标后的保险丝）：超限先裁 facts 尾行再裁 events
    let mut truncated = events_truncated || facts_count_truncated || budget_truncated;
    while serialized_len(&out) > OUTPUT_BUDGET_BYTES {
        let popped = if out["returned_facts"].as_u64().unwrap_or(0) > 0 {
            let arr = out["recent_facts"]
                .as_array_mut()
                .expect("facts is an array");
            arr.pop();
            out["returned_facts"] = json!(arr.len());
            true
        } else {
            let arr = out["recent_events"]
                .as_array_mut()
                .expect("events is an array");
            match arr.pop() {
                Some(_) => {
                    out["returned_events"] = json!(arr.len());
                    true
                }
                None => false,
            }
        };
        if !popped {
            break;
        }
        truncated = true;
    }
    out["truncated"] = json!(truncated);
    out["field_truncated"] = json!(field_truncated);
    if field_truncated {
        out["note"] = json!(
            "oversized fields are downgraded to digest+length markers; full text stays on the ledger (fetch by fact_id via the causal-chain view)"
        );
    }
    Ok(out)
}

/// `audit_summary` 执行器（占位/接线双态）
pub struct AuditSummaryTool {
    session_id: Option<String>,
    client: Option<EvoruleApiClient>,
    journal: Option<std::sync::Arc<JournalWriter>>,
}

impl AuditSummaryTool {
    /// 占位构造（启动期 default_safe_toolkit 注册；调用 fail-visible 报错）
    pub fn unwired() -> Self {
        Self {
            session_id: None,
            client: None,
            journal: None,
        }
    }

    /// 接线构造（runner 会话期重绑；journal 无则调用 fail-visible）
    pub fn wired(
        session_id: String,
        client: EvoruleApiClient,
        journal: Option<std::sync::Arc<JournalWriter>>,
    ) -> Self {
        Self {
            session_id: Some(session_id),
            client: Some(client),
            journal,
        }
    }

    fn parts(&self) -> Result<(&str, &EvoruleApiClient, Option<&JournalWriter>), String> {
        match (&self.session_id, &self.client) {
            (Some(sid), Some(c)) => Ok((sid, c, self.journal.as_deref())),
            _ => Err(format!(
                "{}: not wired to a session yet (registered as a startup placeholder; \
                 it is re-bound once a run session exists)",
                AUDIT_SUMMARY_TOOL
            )),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for AuditSummaryTool {
    fn is_wired(&self) -> bool {
        // journal 句柄缺席不降 wired：调用时 fail-visible（wired 构造的
        // None 透传语义，与 audit_query 同款）
        self.session_id.is_some() && self.client.is_some()
    }

    async fn call(&self, args: &Value) -> Result<Value, String> {
        let (session_id, client, journal) = self.parts()?;
        audit_summary_exec(session_id, client, journal, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::journal::JournalEvent;
    use serde_json::json;

    // ---- 纯函数面 ----

    #[test]
    fn test_unwired_fails_visible() {
        let tool = AuditSummaryTool::unwired();
        let err = match tool.parts() {
            Err(e) => e,
            Ok(_) => panic!("unwired placeholder must fail-visible"),
        };
        assert!(err.contains("not wired"), "{err}");
        let wired = AuditSummaryTool::wired(
            "s1".to_string(),
            crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1"),
            None,
        );
        assert!(wired.parts().is_ok());
    }

    #[test]
    fn test_summary_line_type_anchor_status() {
        // summary_line = type + 锚点 id（≤2 个，白名单序）+ 短状态；零大字段
        let ev = json!({
            "type": "tool_invoked",
            "payload": {"call_id": "t7", "tool": "shell_exec", "args_digest": "blake3:abc"}
        });
        assert_eq!(summary_line(&ev), "tool_invoked call_id=t7 tool=shell_exec");
        let ev = json!({
            "type": "tool_result",
            "payload": {"call_id": "t7", "status": "error", "size_bytes": 3, "content_digest": "blake3:x"}
        });
        assert_eq!(summary_line(&ev), "tool_result call_id=t7 error");
        let ev = json!({
            "type": "approval_resolved",
            "payload": {"approval_id": "ap-1", "decision": "rejected"}
        });
        assert_eq!(
            summary_line(&ev),
            "approval_resolved approval_id=ap-1 rejected"
        );
        // goal 不进 summary_line（锚点 id 只取 turn_seq；长目标归 digest 卡）
        let ev = json!({
            "type": "turn_started",
            "payload": {"turn_seq": 2, "goal": "一个可能很长的用户目标……"}
        });
        assert_eq!(summary_line(&ev), "turn_started turn_seq=2");
    }

    #[test]
    fn test_summary_line_caps_long_anchor_values() {
        // 锚点值超长截断（… 标记），行内零大字段
        let ev = json!({
            "type": "llm_called",
            "payload": {"purpose": "react", "model": "m".repeat(200), "response": "y".repeat(10_000)}
        });
        let line = summary_line(&ev);
        assert!(
            line.starts_with("llm_called purpose=react model="),
            "{line}"
        );
        assert!(
            line.contains('…'),
            "long anchor value must be capped: {line}"
        );
        assert!(
            line.len() < 256,
            "summary_line must stay short: {}",
            line.len()
        );
    }

    #[test]
    fn test_downgrade_oversized_fields_locator_exempt() {
        // 既有降级口径：超限字符串字段 → {truncated,bytes,digest} blake3；
        // 定位键（seq/ts/type/fact_id/path）永不降级
        let big = "x".repeat(10 * 1024);
        let mut v = json!({
            "seq": 1,
            "ts": 5,
            "type": "tool_result",
            "fact_id": 9,
            "path": format!("/w/{big}"),
            "note": big.clone(),
            "goals": [big.clone(), "small".to_string()],
        });
        let hit = downgrade_oversized_fields(&mut v);
        assert!(hit, "oversized fields present, must downgrade");
        // 定位键原样保留
        assert_eq!(v["seq"], 1);
        assert_eq!(v["ts"], 5);
        assert_eq!(v["type"], "tool_result");
        assert_eq!(v["fact_id"], 9);
        assert_eq!(v["path"], format!("/w/{big}"));
        // 超限字段降级标注（bytes=序列化字节数含引号；digest=blake3 前缀）
        assert_eq!(v["note"]["truncated"], true);
        assert_eq!(v["note"]["bytes"].as_u64().unwrap(), 10 * 1024 + 2);
        assert!(v["note"]["digest"].as_str().unwrap().starts_with("blake3:"));
        // 数组元素同样降级，小元素原样
        assert_eq!(v["goals"][0]["truncated"], true);
        assert_eq!(v["goals"][1], "small");
        // 无超限字段 → 不动
        let mut small = json!({"note": "tiny", "list": ["a", "b"]});
        assert!(!downgrade_oversized_fields(&mut small));
        assert_eq!(small["note"], "tiny");
    }

    #[tokio::test]
    async fn test_limit_validation_and_journal_absence() {
        // limit 校验前置（fail-visible，1..=50）；journal 缺席如实报错
        let client = crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1");
        let err = audit_summary_exec("s1", &client, None, &json!({"limit": 0}))
            .await
            .unwrap_err();
        assert!(err.contains("limit must be an integer in 1..=50"), "{err}");
        let err = audit_summary_exec("s1", &client, None, &json!({"limit": 51}))
            .await
            .unwrap_err();
        assert!(err.contains("limit must be an integer in 1..=50"), "{err}");
        let err = audit_summary_exec("s1", &client, None, &json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("journal not available"), "{err}");
    }

    // ---- 装配面（mockito facts 桩 + 真 journal 写手）----

    fn fact_json(i: u64, path: &str, value: serde_json::Value) -> serde_json::Value {
        json!({"fact_id": i, "path": path, "value": value, "version": 1})
    }

    #[tokio::test]
    async fn three_cards_assembled_events_digest_facts() {
        let mut server = mockito::Server::new_async().await;
        let dir = tempfile::tempdir().unwrap();
        let jw = JournalWriter::open(dir.path(), "s-test").unwrap();
        jw.write_event(JournalEvent::TurnStarted {
            turn_seq: 1,
            goal: "修复构建".to_string(),
        })
        .unwrap();
        jw.llm_called("m1", "react", None, None, None, 3, None, "resp")
            .unwrap();
        let cid = jw
            .tool_invoked("shell_exec", &json!({"cmd": "cargo build"}), None)
            .unwrap();
        jw.tool_result(&cid, "ok", "done", None).unwrap();
        jw.approval_resolved("ap-1", "rejected").unwrap();
        let facts = json!([
            fact_json(
                3,
                "audit.big",
                json!({"timestamp": 1790000000u64, "body": "v3"})
            ),
            fact_json(
                2,
                "audit.small",
                json!({"timestamp": 1789999000u64, "body": "v2"})
            ),
        ]);
        let _m = server
            .mock("GET", "/api/sessions/s-test/facts")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(facts.to_string())
            .create_async()
            .await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let out = audit_summary_exec("s-test", &client, Some(&jw), &json!({}))
            .await
            .unwrap();
        assert_eq!(out["status"], "ok");
        // 卡一：journal 最新事件（新在前），行键恰为 seq/ts/type/summary_line
        let evs = out["recent_events"].as_array().unwrap();
        assert_eq!(evs.len(), 5);
        assert_eq!(evs[0]["type"], "approval_resolved");
        assert_eq!(evs[0]["seq"], 5);
        assert!(evs[0]["summary_line"].as_str().unwrap().contains("ap-1"));
        for e in evs {
            let obj = e.as_object().unwrap();
            assert_eq!(
                obj.len(),
                4,
                "event row must carry locator keys + summary_line only: {obj:?}"
            );
        }
        // 卡二：digest 投影（sediment build_journal_digest 同构轻量卡）
        let d = &out["digest"];
        assert_eq!(d["llm_calls"], 1);
        assert_eq!(d["llm_react"], 1);
        assert_eq!(d["tool_calls"]["shell_exec"], 1);
        assert_eq!(d["tool_ok"], 1);
        assert_eq!(d["tool_failed"], 0);
        assert_eq!(d["approvals_rejected"], 1);
        assert!(d["goals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g == "修复构建"));
        assert!(d["ended"].is_null(), "no turn_ended recorded yet");
        // 卡三：facts 元数据行（fact_id/path/ts/bytes；禁 value 全文）
        let fr = out["recent_facts"].as_array().unwrap();
        assert_eq!(fr.len(), 2);
        assert_eq!(fr[0]["fact_id"], 3);
        assert_eq!(fr[0]["path"], "audit.big");
        assert_eq!(fr[0]["ts"], 1790000000u64);
        assert!(fr[0]["bytes"].as_u64().unwrap() > 0);
        assert!(
            fr[0].get("value").is_none(),
            "metadata rows must never carry full values"
        );
        assert!(
            fr[0].get("digest").is_none(),
            "small values need no digest marker"
        );
        assert_eq!(out["truncated"], false);
    }

    #[tokio::test]
    async fn oversized_fact_value_downgraded_to_digest_metadata() {
        // 超限值只给 digest（budgeted_fact_rows 同口径）；小值无 digest 键；
        // value 全文永不出卡片
        let mut server = mockito::Server::new_async().await;
        let dir = tempfile::tempdir().unwrap();
        let jw = JournalWriter::open(dir.path(), "s-test2").unwrap();
        let big = "x".repeat(100 * 1024);
        let facts = json!([
            fact_json(1, "audit.f1", json!(big)),
            fact_json(2, "audit.f2", json!("small")),
        ]);
        let _m = server
            .mock("GET", "/api/sessions/s-test2/facts")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(facts.to_string())
            .create_async()
            .await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let out = audit_summary_exec("s-test2", &client, Some(&jw), &json!({}))
            .await
            .unwrap();
        let fr = out["recent_facts"].as_array().unwrap();
        let big_row = fr.iter().find(|r| r["fact_id"] == 1).unwrap();
        assert_eq!(big_row["path"], "audit.f1", "path 定位键保留供全文回账");
        assert!(
            big_row["bytes"].as_u64().unwrap() >= 100_000,
            "bytes must reflect the serialized value size"
        );
        assert!(
            big_row["digest"].as_str().unwrap().starts_with("blake3:"),
            "digest口径 = blake3 前缀"
        );
        assert!(big_row.get("value").is_none(), "超限值全文禁出");
        let small_row = fr.iter().find(|r| r["fact_id"] == 2).unwrap();
        assert!(small_row.get("digest").is_none());
        assert_eq!(out["field_truncated"], true, "降级发生必须显式标记");
        let note = out["note"].as_str().unwrap();
        assert!(note.contains("digest"), "downgrade must be visible: {note}");
    }

    #[tokio::test]
    async fn output_budget_64kib_trims_facts_and_marks_truncated() {
        // 总预算 64KiB：定位键 path 本体巨大（永降级豁免）→ 行预算收口，
        // 截断列表 + 显式计数，且最终输出不超预算
        let mut server = mockito::Server::new_async().await;
        let dir = tempfile::tempdir().unwrap();
        let jw = JournalWriter::open(dir.path(), "s-test3").unwrap();
        let facts: Vec<serde_json::Value> = (1u64..=50)
            .map(|i| {
                fact_json(
                    i,
                    &format!("audit.long{}.{}", i, "p".repeat(2048)),
                    json!(format!("v{i}")),
                )
            })
            .collect();
        let _m = server
            .mock("GET", "/api/sessions/s-test3/facts")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::to_string(&facts).unwrap())
            .create_async()
            .await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let out = audit_summary_exec("s-test3", &client, Some(&jw), &json!({"limit": 50}))
            .await
            .unwrap();
        assert_eq!(out["total_facts"], 50);
        let returned = out["returned_facts"].as_u64().unwrap();
        assert!(
            returned < 50,
            "byte budget must stop loading before the count limit, got {returned}"
        );
        assert_eq!(out["truncated"], true);
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(
            serialized.len() <= 64 * 1024,
            "output must stay within the total budget, got {}",
            serialized.len()
        );
    }

    #[tokio::test]
    async fn events_newest_first_with_count_truncation() {
        // journal 头部清单：缺省 20 条、新在前（seq 降序），超出显式计数
        let mut server = mockito::Server::new_async().await;
        let dir = tempfile::tempdir().unwrap();
        let jw = JournalWriter::open(dir.path(), "s-test4").unwrap();
        for i in 0..15u64 {
            let cid = jw
                .tool_invoked("file_read", &json!({"path": format!("f{i}")}), None)
                .unwrap();
            jw.tool_result(&cid, "ok", "r", None).unwrap();
        }
        let facts = json!([]);
        let _m = server
            .mock("GET", "/api/sessions/s-test4/facts")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(facts.to_string())
            .create_async()
            .await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let out = audit_summary_exec("s-test4", &client, Some(&jw), &json!({}))
            .await
            .unwrap();
        assert_eq!(out["total_events"], 30);
        let evs = out["recent_events"].as_array().unwrap();
        assert_eq!(evs.len(), 20, "default limit must cap the events card");
        assert_eq!(out["returned_events"], 20);
        assert_eq!(evs[0]["seq"], 30, "newest event first");
        assert_eq!(evs[0]["type"], "tool_result");
        assert_eq!(out["truncated"], true, "count truncation must be visible");
        let fr = out["recent_facts"].as_array().unwrap();
        assert!(fr.is_empty());
        assert_eq!(out["status"], "ok");
    }
}
