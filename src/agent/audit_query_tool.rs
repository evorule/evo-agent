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
//! - `session`：读当前/指定会话 Fact 列表（get_facts）；`fact_id` 给出时
//!   回溯因果链（get_causal_chain_typed）；`verify=true` 时跑链完整性验证
//!   （verify_audit_typed）。
//! - `journal`：读本会话 B21 journal 事件流（JournalWriter::read_lines
//!   只读句柄；type/since_seq/limit 过滤，输出口径与 query_journal 一致）。
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
    let limit = match args.get("limit") {
        None => DEFAULT_LIMIT,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_LIMIT as u64).contains(n))
            .ok_or_else(|| format!("audit_query: limit must be an integer in 1..={MAX_LIMIT}"))?
            as usize,
    };
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
    let limit = match args.get("limit") {
        None => DEFAULT_LIMIT,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_LIMIT as u64).contains(n))
            .ok_or_else(|| format!("audit_query: limit must be an integer in 1..={MAX_LIMIT}"))?
            as usize,
    };
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
        let chain = client
            .get_causal_chain_typed(session_id, fid)
            .await
            .map_err(|e| format!("audit_query: causal chain query failed ({e})"))?;
        return Ok(json!({
            "status": "ok",
            "scope": "session",
            "session_id": session_id,
            "view": "causal_chain",
            "chain": chain,
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
    let facts = client
        .get_facts(session_id, None)
        .await
        .map_err(|e| format!("audit_query: session facts query failed ({e})"))?;
    let items: Vec<Value> = facts
        .iter()
        .map(|f| {
            json!({
                "fact_id": f.id,
                "path": f.path,
                "value": f.value,
                "version": f.version,
            })
        })
        .collect();
    Ok(json!({
        "status": if items.is_empty() { "empty" } else { "ok" },
        "scope": "session",
        "session_id": session_id,
        "view": "facts",
        "total": items.len(),
        "facts": items,
    }))
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
    let truncated = matched_total > limit;
    if truncated {
        matched.drain(..matched_total - limit);
    }
    Ok(json!({
        "status": if matched.is_empty() { "empty" } else { "ok" },
        "scope": "journal",
        "total_lines": total_lines,
        "matched": matched_total,
        "truncated": truncated,
        "events": matched,
    }))
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
}
