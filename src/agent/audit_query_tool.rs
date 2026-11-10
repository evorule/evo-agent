// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 审计查询工具（audit_query）——治理留痕与会话审计的 agent 只读入口。
//!
//! 四 scope：
//! - `gov`：读共享账本治理分区 `shared.gov.`（B22-A 治理留痕，GovLedger
//!   写入的六类事件 Fact）。`precedent=true` 时输出**先例卡**——同类工具+
//!   参数摘要+当时批准者+档位+时间（会话内批准先例参照）的机制载体。
//! - `shared`：读共享账本任意前缀（get_shared_facts prefix 透传）——含
//!   /mcp 面事件轨道 `shared.mcp.events.EL-*` 的 agent 只读检索入口。
//! - `session`：读当前/指定会话 Fact 列表（get_facts）——limit（缺省 20、
//!   上限 100）+ 字节预算收口，超限大 value 降级 `{truncated,bytes,digest}`
//!   标注（全文按 fact_id 经因果链视图回账）；`fact_id` 给出时回溯因果链
//!   （get_causal_chain_typed，同套 limit 保留链尾）；`verify=true` 时跑链
//!   完整性验证（verify_audit_typed）。
//! - `journal`：读本会话 journal 事件流（JournalWriter::read_lines
//!   只读句柄；type/since_seq/limit 过滤，输出口径与 query_journal 一致——
//!   含超限大字段降级与输出字节预算）。
//!
//! 注册形态（memory_recall 同构）：占位执行体启动期注册，runner 会话期
//! 重绑（client/session_id/journal 在手）；开关 agentTools.audit_query 过滤
//! 三面自动工作。只读工具：不产生任何写入事实。

use serde_json::{json, Value};

use crate::agent::journal::JournalWriter;
use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handlers::tool_handler::ToolFunction;

/// 工具名
pub const AUDIT_QUERY_TOOL: &str = "audit_query";

/// gov 分区共享前缀（GovLedger GOV_SHARED_PREFIX 同字面；不经依赖防环）
const GOV_PREFIX: &str = "shared.gov.";

/// limit 缺省与上限（gov/session 面）
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

/// session facts 单条 value 内联上限（字节）：超限大 value 降级标注（口径
/// 对齐 journal 写入侧 checkpoint 内联阈值）。audit_summary 摘要卡
/// recent_facts 元数据行共用同一口径（单源纪律）。
pub(crate) const SESSION_FACTS_VALUE_INLINE_LIMIT: usize = 4096;

/// session facts 输出总字节预算（字节）：value 为任意 JSON（含媒体 base64
/// 可能单条巨大），条数 limit 之外再按字节收口
const SESSION_FACTS_BUDGET_BYTES: usize = 128 * 1024;

/// limit 参数解析（gov/shared/session 统一口径）：缺省 [`DEFAULT_LIMIT`]，
/// 越界 fail-visible
fn parse_limit(args: &Value) -> Result<usize, String> {
    match args.get("limit") {
        None => Ok(DEFAULT_LIMIT),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_LIMIT as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| format!("audit_query: limit must be an integer in 1..={MAX_LIMIT}")),
    }
}

/// gov 事件 kind 路径段枚举（路径形态 `shared.gov.gov.{kind}.{ts}.{seq}`）
const GOV_KINDS: &[&str] = &[
    "approval_requested",
    "approval_resolved",
    "policy_verdict",
    "s4_publish",
    "terminal_session_create",
    "terminal_session_close",
    "b21_journal_milestone",
];

/// 从 gov Fact 路径取 kind 路径段（第三段；形态不符返回 None）
fn gov_path_kind(path: &str) -> Option<&str> {
    path.split('.').nth(3)
}

/// 先例卡（approval_requested/resolved 按 approval_id 配对后的审计视图）
fn precedent_card(requested: Option<&Value>, resolved: Option<&Value>) -> Value {
    let summary_tool = |v: Option<&Value>| -> String {
        // approval_requested.summary 形态「{tool} 请求批准：{command}」——
        // 截取「请求批准：」前段为工具名（写入侧同字面，见 hooks.rs）
        v.and_then(|r| r.get("summary"))
            .and_then(|s| s.as_str())
            .and_then(|s| s.split("请求批准：").next())
            .unwrap_or("unknown")
            .trim()
            .to_string()
    };
    json!({
        "tool": summary_tool(requested),
        "params_summary": requested.and_then(|r| r.get("summary")).cloned().unwrap_or(json!(null)),
        "approver": resolved.and_then(|r| r.get("actor")).cloned().unwrap_or(json!(null)),
        "decision": resolved
            .and_then(|r| r.get("summary"))
            .and_then(|s| s.as_str())
            .map(|s| s.split('；').next().unwrap_or(s).split('（').next().unwrap_or(s).trim().to_string())
            .unwrap_or_default(),
        "reason": resolved.and_then(|r| r.get("summary")).cloned().unwrap_or(json!(null)),
        "requested_ts": requested.and_then(|r| r.get("ts")).cloned().unwrap_or(json!(null)),
        "resolved_ts": resolved.and_then(|r| r.get("ts")).cloned().unwrap_or(json!(null)),
        "approval_id": resolved
            .and_then(|r| r.get("refs"))
            .and_then(|s| s.get("approval_id"))
            .cloned()
            .unwrap_or(json!(null)),
    })
}

/// `audit_query` 执行体（三 scope 分派）
pub(crate) async fn audit_query_exec(
    session_id: &str,
    client: &EvoruleApiClient,
    journal: Option<&JournalWriter>,
    args: &Value,
) -> Result<Value, String> {
    let scope = args.get("scope").and_then(|v| v.as_str()).ok_or(
        "audit_query: 'scope' is required (\"gov\" | \"shared\" | \"session\" | \"journal\")",
    )?;
    match scope {
        "gov" => gov_scope(client, args).await,
        "shared" => shared_scope(client, args).await,
        "session" => session_scope(session_id, client, args).await,
        "journal" => journal_scope(journal, args),
        other => Err(format!(
            "audit_query: unknown scope '{other}' (expected \"gov\" | \"shared\" | \"session\" | \"journal\")"
        )),
    }
}

/// scope=gov：治理留痕分区查询（kind 过滤 / 先例卡视图）
async fn gov_scope(client: &EvoruleApiClient, args: &Value) -> Result<Value, String> {
    let kind_filter = args
        .get("kind")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    if let Some(k) = kind_filter {
        if !GOV_KINDS.contains(&k) {
            return Err(format!(
                "audit_query: unknown gov kind '{k}' (expected one of: {})",
                GOV_KINDS.join(" | ")
            ));
        }
    }
    let precedent = args
        .get("precedent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let limit = parse_limit(args)?;
    let facts = client
        .get_shared_facts(Some(GOV_PREFIX))
        .await
        .map_err(|e| format!("audit_query: gov ledger unreachable ({e})"))?;
    // kind 过滤（path 第三段）；fact_id 降序 = 新在前
    let mut events: Vec<&crate::api::evorule_client::SharedFactEntry> = facts
        .iter()
        .filter(|f| kind_filter.is_none_or(|k| gov_path_kind(&f.path) == Some(k)))
        .collect();
    let total = events.len();
    events.sort_by_key(|e| std::cmp::Reverse(e.fact_id));

    if precedent {
        // 先例卡：approval_requested/resolved 按 refs.approval_id 配对；
        // 卡序随 resolved 时间倒序（最近的先例最相关）
        let mut cards = Vec::new();
        for resolved in events
            .iter()
            .filter(|e| gov_path_kind(&e.path) == Some("approval_resolved"))
        {
            let aid = resolved
                .value
                .get("refs")
                .and_then(|r| r.get("approval_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let requested = aid.as_ref().and_then(|id| {
                facts
                    .iter()
                    .find(|f| {
                        gov_path_kind(&f.path) == Some("approval_requested")
                            && f.value
                                .get("refs")
                                .and_then(|r| r.get("approval_id"))
                                .and_then(|v| v.as_str())
                                == Some(id.as_str())
                    })
                    .map(|f| &f.value)
            });
            let mut card = precedent_card(requested, Some(&resolved.value));
            card["path"] = json!(resolved.path);
            cards.push(card);
        }
        let returned = cards.len().min(limit);
        cards.truncate(limit);
        return Ok(json!({
            "status": if cards.is_empty() { "empty" } else { "ok" },
            "scope": "gov",
            "view": "precedent",
            "total": total,
            "returned": returned,
            "precedents": cards,
        }));
    }

    let items: Vec<Value> = events
        .iter()
        .take(limit)
        .map(|e| {
            json!({
                "path": e.path,
                "kind": gov_path_kind(&e.path).unwrap_or("unknown"),
                "fact_id": e.fact_id,
                "value": e.value,
            })
        })
        .collect();
    Ok(json!({
        "status": if items.is_empty() { "empty" } else { "ok" },
        "scope": "gov",
        "view": "events",
        "total": total,
        "returned": items.len(),
        "events": items,
    }))
}

/// scope=shared：共享账本面查询（get_shared_facts prefix 透传；/mcp 面事件轨道 `shared.mcp.events.EL-*` 的 agent 只读检索入口）。
/// 输出口径与 gov events 视图同构：fact_id 降序（新在前）+ limit 截取。
async fn shared_scope(client: &EvoruleApiClient, args: &Value) -> Result<Value, String> {
    let prefix = args
        .get("prefix")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let limit = parse_limit(args)?;
    let facts = client
        .get_shared_facts(prefix)
        .await
        .map_err(|e| format!("audit_query: shared facts query failed ({e})"))?;
    let mut events: Vec<&crate::api::evorule_client::SharedFactEntry> = facts.iter().collect();
    let total = events.len();
    events.sort_by_key(|e| std::cmp::Reverse(e.fact_id));
    let items: Vec<Value> = events
        .iter()
        .take(limit)
        .map(|e| {
            json!({
                "path": e.path,
                "fact_id": e.fact_id,
                "value": e.value,
            })
        })
        .collect();
    Ok(json!({
        "status": if items.is_empty() { "empty" } else { "ok" },
        "scope": "shared",
        "view": "events",
        "total": total,
        "returned": items.len(),
        "events": items,
    }))
}

/// scope=session：会话 Fact 列表 / 因果链回溯 / 链完整性验证
async fn session_scope(
    current_session: &str,
    client: &EvoruleApiClient,
    args: &Value,
) -> Result<Value, String> {
    let session_id = args
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(current_session);
    let verify = args
        .get("verify")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(fid) = args.get("fact_id").and_then(|v| v.as_u64()) {
        let limit = parse_limit(args)?;
        let chain = client
            .get_causal_chain_typed(session_id, fid)
            .await
            .map_err(|e| format!("audit_query: causal chain query failed ({e})"))?;
        // 链视图套条数 limit：保留链尾（离查询 fact 最近的 N 个），收口显式
        let chain_total = chain.chain.len();
        let chain_length = chain.chain_length;
        let mut links = chain.chain;
        let truncated = chain_total > limit;
        if truncated {
            links.drain(..chain_total - limit);
        }
        return Ok(json!({
            "status": "ok",
            "scope": "session",
            "session_id": session_id,
            "view": "causal_chain",
            "chain_total": chain_total,
            "chain_length": chain_length,
            "returned": links.len(),
            "truncated": truncated,
            "chain": links,
        }));
    }
    if verify {
        let verdict = client
            .verify_audit_typed(session_id)
            .await
            .map_err(|e| format!("audit_query: audit verify failed ({e})"))?;
        return Ok(json!({
            "status": "ok",
            "scope": "session",
            "session_id": session_id,
            "view": "verify",
            "verdict": verdict,
        }));
    }
    let limit = parse_limit(args)?;
    let facts = client
        .get_facts(session_id, None)
        .await
        .map_err(|e| format!("audit_query: session facts query failed ({e})"))?;
    let (rows, total, truncated, downgraded) = budgeted_fact_rows(&facts, limit);
    let mut out = json!({
        "status": if rows.is_empty() { "empty" } else { "ok" },
        "scope": "session",
        "session_id": session_id,
        "view": "facts",
        "total": total,
        "returned": rows.len(),
        "truncated": truncated,
        "field_truncated": downgraded,
        "facts": rows,
    });
    if downgraded {
        out["note"] = json!(
            "oversized fact values are downgraded to digest+length markers; full text stays on the audit ledger (fetch by fact_id via the causal-chain view)"
        );
    }
    Ok(out)
}

/// 超限大 value 的降级标注（既有口径 `{truncated,bytes,digest}` blake3——
/// budgeted_fact_rows 与 audit_summary 摘要卡共用单源）。
/// `serialized` = value 的 JSON 序列化字节（bytes 取其长度，含引号/转义）。
pub(crate) fn oversized_value_marker(serialized: &str) -> Value {
    json!({
        "truncated": true,
        "bytes": serialized.len(),
        "digest": crate::agent::journal::evorule_digest(serialized),
    })
}

/// session facts 视图装配：fact_id 降序（新在前）+ 条数 limit + 字节预算。
/// 预算放不下的大 value 降级为 `{truncated,bytes,digest}` 标注（全文按
/// fact_id 经 causal-chain 视图回账）；小 value 超预算即收口。
/// 返回（行集、全量条数、收口标记、字段降级标记）。
fn budgeted_fact_rows(
    facts: &[crate::api::evorule_client::FactEntry],
    limit: usize,
) -> (Vec<Value>, usize, bool, bool) {
    let total = facts.len();
    let mut sorted: Vec<&crate::api::evorule_client::FactEntry> = facts.iter().collect();
    sorted.sort_by_key(|f| std::cmp::Reverse(f.id));
    let count_truncated = total > limit;
    let mut rows = Vec::new();
    let mut acc = 0usize;
    let mut downgraded = false;
    let mut budget_truncated = false;
    for f in sorted.into_iter().take(limit) {
        let serialized = serde_json::to_string(&f.value).unwrap_or_default();
        let size = serialized.len();
        if size > SESSION_FACTS_VALUE_INLINE_LIMIT {
            // 大 value 一律降级（与 journal 查询同口径：单条内联阈值封顶，
            // 全文按 fact_id 经 causal-chain 视图回账）
            let marker = oversized_value_marker(&serialized);
            let marker_size = serde_json::to_string(&marker).map(|s| s.len()).unwrap_or(0);
            if acc + marker_size > SESSION_FACTS_BUDGET_BYTES {
                budget_truncated = true;
                break;
            }
            acc += marker_size;
            downgraded = true;
            rows.push(json!({
                "fact_id": f.id,
                "path": f.path,
                "value": marker,
                "version": f.version,
            }));
        } else if acc + size <= SESSION_FACTS_BUDGET_BYTES {
            acc += size;
            rows.push(json!({
                "fact_id": f.id,
                "path": f.path,
                "value": f.value,
                "version": f.version,
            }));
        } else {
            // 小 value 放不下：预算已收口
            budget_truncated = true;
            break;
        }
    }
    (rows, total, count_truncated || budget_truncated, downgraded)
}

/// scope=journal：本会话 journal 事件流（type/since_seq/limit 过滤；
/// 输出口径与 query_journal 一致——seq/ts/type + payload 平铺）
fn journal_scope(journal: Option<&JournalWriter>, args: &Value) -> Result<Value, String> {
    let writer = journal.ok_or_else(|| {
        "audit_query: journal not available (no journal in this session)".to_string()
    })?;
    let event_type = args
        .get("type")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let since_seq = args.get("since_seq").and_then(|v| v.as_u64());
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(100)
        .clamp(1, 500);

    let lines = writer
        .read_lines()
        .map_err(|e| format!("audit_query: journal read failed ({e})"))?;
    let total_lines = lines.len();
    let mut matched: Vec<Value> = Vec::new();
    for line in &lines {
        if let Some(s) = since_seq {
            if line.seq < s {
                continue;
            }
        }
        let ev = serde_json::to_value(&line.event)
            .map_err(|e| format!("audit_query: journal event encode failed: {e}"))?;
        let ty = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(ft) = &event_type {
            if ty != ft {
                continue;
            }
        }
        let mut row = json!({"seq": line.seq, "ts": line.ts, "type": ty});
        if let Some(payload) = ev.get("payload") {
            if let Some(obj) = payload.as_object() {
                for (k, v) in obj {
                    row[k.clone()] = v.clone();
                }
            }
        }
        matched.push(row);
    }
    let matched_total = matched.len();
    let (events, truncated, field_truncated) =
        crate::builtin_tools::accounting::budget_journal_output(matched, limit);
    let mut out = json!({
        "status": if events.is_empty() { "empty" } else { "ok" },
        "scope": "journal",
        "total_lines": total_lines,
        "matched": matched_total,
        "truncated": truncated,
        "field_truncated": field_truncated,
        "events": events,
    });
    if field_truncated {
        out["note"] = json!(crate::builtin_tools::accounting::journal_budget_note());
    }
    Ok(out)
}

/// `audit_query` 执行器（占位/接线双态）
pub struct AuditQueryTool {
    session_id: Option<String>,
    client: Option<EvoruleApiClient>,
    journal: Option<std::sync::Arc<JournalWriter>>,
}

impl AuditQueryTool {
    /// 占位构造（启动期 default_safe_toolkit 注册；调用 fail-visible 报错）
    pub fn unwired() -> Self {
        Self {
            session_id: None,
            client: None,
            journal: None,
        }
    }

    /// 接线构造（runner 会话期重绑；journal 无则 journal scope fail-visible）
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
                AUDIT_QUERY_TOOL
            )),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for AuditQueryTool {
    fn is_wired(&self) -> bool {
        // journal 句柄缺席不降 wired：journal scope 调用时 fail-visible，
        // gov/session scope 完全可用（wired 构造的 None 透传语义）
        self.session_id.is_some() && self.client.is_some()
    }

    async fn call(&self, args: &Value) -> Result<Value, String> {
        let (session_id, client, journal) = self.parts()?;
        audit_query_exec(session_id, client, journal, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gov_path_kind_extraction() {
        assert_eq!(
            gov_path_kind("shared.gov.gov.approval_resolved.1790401900.1"),
            Some("approval_resolved")
        );
        // 契约=取第 4 段；gov 前缀由调用点 get_shared_facts(prefix) 过滤保证，
        // 非 gov 路径本函数不做判别（如 "shared.other.stable.x" → Some("x")）
        assert_eq!(gov_path_kind("shared.other.stable.x"), Some("x"));
        assert_eq!(gov_path_kind("short"), None);
    }

    #[test]
    fn test_precedent_card_assembly() {
        let requested = json!({
            "event_id": "G000001", "kind": "approval_requested", "ts": 100,
            "session_id": "s1", "actor": "agent",
            "summary": "shell_exec 请求批准：cargo test",
            "refs": {"approval_id": "ap-1"}
        });
        let resolved = json!({
            "event_id": "G000002", "kind": "approval_resolved", "ts": 120,
            "session_id": "s1", "actor": "user",
            "summary": "approved（人工批准）",
            "refs": {"approval_id": "ap-1"}
        });
        let card = precedent_card(Some(&requested), Some(&resolved));
        assert_eq!(card["tool"], "shell_exec");
        assert_eq!(card["approver"], "user");
        assert_eq!(card["decision"], "approved");
        assert_eq!(card["approval_id"], "ap-1");
        assert_eq!(card["requested_ts"], 100);
    }

    #[test]
    fn test_precedent_card_handles_missing_sides() {
        let card = precedent_card(None, None);
        assert_eq!(card["tool"], "unknown");
        assert!(card["approver"].is_null());
        // 无 resolved 时 decision 为空串不 panic
        let card2 = precedent_card(Some(&json!({"summary": "x 请求批准：y"})), None);
        assert_eq!(card2["tool"], "x");
    }

    #[test]
    fn test_unwired_fails_visible() {
        let tool = AuditQueryTool::unwired();
        let err = match tool.parts() {
            Err(e) => e,
            Ok(_) => panic!("unwired placeholder must fail-visible"),
        };
        assert!(err.contains("not wired"), "{err}");
    }

    #[tokio::test]
    async fn test_scope_dispatch_errors() {
        let client = crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1");
        let err = audit_query_exec("s1", &client, None, &json!({"scope": "bogus"}))
            .await
            .unwrap_err();
        assert!(err.contains("unknown scope"), "{err}");
        let err = audit_query_exec("s1", &client, None, &json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("'scope' is required"), "{err}");
        let err = audit_query_exec("s1", &client, None, &json!({"scope": "journal"}))
            .await
            .unwrap_err();
        assert!(err.contains("journal not available"), "{err}");
    }

    #[tokio::test]
    async fn test_shared_scope_dispatch_and_validation() {
        // scope=shared 分派 + limit 校验前置（不触网即拒）
        let client = crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1");
        let err = audit_query_exec("s1", &client, None, &json!({"scope": "shared", "limit": 0}))
            .await
            .unwrap_err();
        assert!(err.contains("limit must be an integer in 1..=100"), "{err}");
        let err = audit_query_exec(
            "s1",
            &client,
            None,
            &json!({"scope": "shared", "limit": 101}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("limit must be an integer in 1..=100"), "{err}");
        // 合法参数（含 prefix 透传）→ dead client → fail-visible 网络错误
        let err = audit_query_exec(
            "s1",
            &client,
            None,
            &json!({"scope": "shared", "prefix": "shared.mcp.events."}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("shared facts query failed"), "{err}");
    }

    // ---- session facts / causal_chain 视图：limit + 字节预算（mock server 直测）----

    fn fact_json(i: u64, value: Value) -> Value {
        json!({"fact_id": i, "path": format!("audit.f{i}"), "value": value, "version": i})
    }

    #[tokio::test]
    async fn session_facts_view_applies_default_limit_and_truncation() {
        let mut server = mockito::Server::new_async().await;
        let facts: Vec<Value> = (1u64..=150)
            .map(|i| fact_json(i, json!(format!("v{i}"))))
            .collect();
        let _m = server
            .mock("GET", "/api/sessions/s-test/facts")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::to_string(&facts).unwrap())
            .create_async()
            .await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let out = audit_query_exec("s-test", &client, None, &json!({"scope": "session"}))
            .await
            .unwrap();
        assert_eq!(out["total"], 150, "total must report the full fact count");
        assert_eq!(out["returned"], 20, "default limit must cap the facts view");
        assert_eq!(out["truncated"], true);
        let rows = out["facts"].as_array().unwrap();
        assert_eq!(rows.len(), 20);
        assert_eq!(rows[0]["fact_id"], 150, "newest fact first");
    }

    #[tokio::test]
    async fn session_facts_view_downgrades_oversized_value_to_digest() {
        let mut server = mockito::Server::new_async().await;
        let big = "x".repeat(100 * 1024);
        let facts = json!([fact_json(1, json!(big)), fact_json(2, json!("small"))]);
        let _m = server
            .mock("GET", "/api/sessions/s-test/facts")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(facts.to_string())
            .create_async()
            .await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let out = audit_query_exec(
            "s-test",
            &client,
            None,
            &json!({"scope": "session", "limit": 50}),
        )
        .await
        .unwrap();
        let rows = out["facts"].as_array().unwrap();
        let big_row = rows.iter().find(|r| r["fact_id"] == 1).unwrap();
        assert_eq!(big_row["value"]["truncated"], true);
        assert!(
            big_row["value"]["bytes"].as_u64().unwrap() >= 100_000,
            "bytes must reflect the serialized value size"
        );
        let digest = big_row["value"]["digest"].as_str().unwrap();
        assert!(digest.starts_with("blake3:"), "digest口径 = blake3 前缀");
        assert_eq!(big_row["path"], "audit.f1", "fact_id/path 保留供全文回账");
        // 小 value 原样内联
        let small_row = rows.iter().find(|r| r["fact_id"] == 2).unwrap();
        assert_eq!(small_row["value"], "small");
        assert_eq!(out["field_truncated"], true);
        let note = out["note"].as_str().unwrap();
        assert!(note.contains("digest"), "downgrade must be visible: {note}");
    }

    #[tokio::test]
    async fn session_facts_view_budget_stops_before_count_limit() {
        let mut server = mockito::Server::new_async().await;
        // 150 条 × ~3KB value（< 内联阈值 4096，不触发降级路径）：limit=50
        // 挡不住全部，字节预算更先收口
        let facts: Vec<Value> = (1u64..=150)
            .map(|i| fact_json(i, json!("p".repeat(3 * 1024))))
            .collect();
        let _m = server
            .mock("GET", "/api/sessions/s-test/facts")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::to_string(&facts).unwrap())
            .create_async()
            .await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let out = audit_query_exec(
            "s-test",
            &client,
            None,
            &json!({"scope": "session", "limit": 50}),
        )
        .await
        .unwrap();
        assert_eq!(out["total"], 150);
        assert!(
            out["returned"].as_u64().unwrap() < 50,
            "byte budget must stop loading before the count limit"
        );
        assert_eq!(out["truncated"], true);
    }

    #[tokio::test]
    async fn causal_chain_view_applies_limit_and_keeps_tail() {
        let mut server = mockito::Server::new_async().await;
        let links: Vec<Value> = (0u64..50)
            .map(|i| {
                json!({
                    "fact_id": i,
                    "fact_type": "tool_result",
                    "logical_time": i,
                    "content_hash": format!("blake3:{i:064x}"),
                    "prev_hash": format!("blake3:{:064x}", i.saturating_sub(1)),
                    "cause": if i == 0 { Value::Null } else { json!(i - 1) },
                })
            })
            .collect();
        let body = json!({"session_id": 1, "fact_id": 49, "chain_length": 50, "chain": links});
        let _m = server
            .mock("GET", "/api/sessions/s-test/audit/causal/49")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body.to_string())
            .create_async()
            .await;
        let client = crate::api::evorule_client::EvoruleApiClient::new(&server.url());
        let out = audit_query_exec(
            "s-test",
            &client,
            None,
            &json!({"scope": "session", "fact_id": 49}),
        )
        .await
        .unwrap();
        assert_eq!(out["chain_total"], 50);
        assert_eq!(out["returned"], 20, "default limit must cap the chain view");
        assert_eq!(out["truncated"], true);
        let chain = out["chain"].as_array().unwrap();
        assert_eq!(chain.len(), 20);
        assert_eq!(
            chain[0]["fact_id"], 30,
            "chain tail (nearest to query) kept"
        );
    }
}
