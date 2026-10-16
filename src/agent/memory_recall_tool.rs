// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 记忆主动消费工具（memory_recall）——把既有记忆账本读取包装为显式工具。
//!
//! 记忆回流此前只有「被动回注」一条路（runner 启动期 recall_context 注入
//! system prompt）；本工具补全「主动消费」——agent 在会话中按需检索稳定
//! 事实与事件账本，检索词/分区/限额自选。
//!
//! 读取面（零新存储访问）：直接读 evorule shared facts 两分区前缀
//! `shared.{ns}.stable.*` / `shared.{ns}.events.*`（与 recall_context 内部
//! 同一读取原语 get_shared_facts），无 prompt 预算/usage 计数/adjudication
//! 等注入面副作用——工具返回值即账本条目视图。query 为大小写不敏感子串
//! 匹配（词法口径；语义检索走 B14 索引面，本工具不依赖）。
//!
//! 注册形态（handover_read 同构）：占位执行体启动期注册（default_safe_toolkit，
//! 未接线时调用 fail-visible 如实报错），runner 会话期重绑（namespace /
//! client 在手）——开关过滤（agentTools.memory_recall）/声明收紧/LLM 契约
//! 三面自动工作。只读工具：不产生任何写入事实。

use serde_json::{json, Value};

use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handlers::tool_handler::ToolFunction;

/// 工具名
pub const MEMORY_RECALL_TOOL: &str = "memory_recall";

/// limit 缺省与上限（防巨量返回撑爆上下文）
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

/// kind 参数 → 账本分区前缀集（缺省 all = 两分区全查）
fn kind_prefixes(kind: &str, namespace: &str) -> Result<Vec<String>, String> {
    match kind {
        "all" => Ok(vec![
            format!("shared.{namespace}.stable."),
            format!("shared.{namespace}.events."),
        ]),
        "stable" => Ok(vec![format!("shared.{namespace}.stable.")]),
        "events" => Ok(vec![format!("shared.{namespace}.events.")]),
        other => Err(format!(
            "memory_recall: unknown kind '{other}' (expected \"all\" | \"stable\" | \"events\")"
        )),
    }
}

/// 参数抽取 + 校验（fail-visible：kind 枚举/limit 范围错误一次报清）
fn extract_args(args: &Value) -> Result<(Option<String>, String, String, usize), String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let kind = args
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("all")
        .to_string();
    let namespace_override = args
        .get("namespace")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let limit = match args.get("limit") {
        None => DEFAULT_LIMIT,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_LIMIT as u64).contains(n))
            .ok_or_else(|| format!("memory_recall: limit must be an integer in 1..={MAX_LIMIT}"))?
            as usize,
    };
    Ok((query, kind, namespace_override.unwrap_or_default(), limit))
}

/// 按 path 去重取最新版本（version 最大/后出现者胜——与 recall 侧
/// latest_entries_by_path 同语义的 entry 级简版；不解析 MemoryRecord，
/// 解析失败在工具面如实透出原值而非跳过）
fn latest_by_path(
    facts: Vec<crate::api::evorule_client::SharedFactEntry>,
) -> Vec<crate::api::evorule_client::SharedFactEntry> {
    let mut latest: std::collections::BTreeMap<
        String,
        crate::api::evorule_client::SharedFactEntry,
    > = Default::default();
    for fact in facts {
        match latest.get(&fact.path) {
            Some(prev) if prev.version > fact.version => {}
            _ => {
                latest.insert(fact.path.clone(), fact);
            }
        }
    }
    latest.into_values().collect()
}

/// `memory_recall` 执行体（多分区读取 → query 过滤 → 新序排序 → limit 截断）
pub(crate) async fn memory_recall_exec(
    namespace: &str,
    client: &EvoruleApiClient,
    args: &Value,
) -> Result<Value, String> {
    let (query, kind, namespace_override, limit) = extract_args(args)?;
    // namespace 显式指定优先（默认当前 ns；跨 ns 读取权限
    // 由 server 侧权限模型兜底，agent 侧只读不过度设防）
    let ns: &str = if namespace_override.is_empty() {
        namespace
    } else {
        &namespace_override
    };
    let prefixes = kind_prefixes(&kind, ns)?;
    let mut facts = Vec::new();
    for prefix in &prefixes {
        let batch = client
            .get_shared_facts(Some(prefix))
            .await
            .map_err(|e| format!("memory_recall: ledger unreachable ({e})"))?;
        facts.extend(batch);
    }
    let total_found = facts.len();
    let mut entries = latest_by_path(facts);
    // query 过滤（value 序列化后大小写不敏感子串；query 空 = 全收）
    if let Some(q) = &query {
        let q_lower = q.to_lowercase();
        entries.retain(|f| f.value.to_string().to_lowercase().contains(&q_lower));
    }
    // 时间序近似：fact_id 单调递增，降序 = 新在前
    entries.sort_by_key(|a| std::cmp::Reverse(a.fact_id));
    let returned = entries.len().min(limit);
    let items: Vec<Value> = entries
        .into_iter()
        .take(limit)
        .map(|f| {
            json!({
                "kind": if f.path.contains(".stable.") { "stable" } else { "events" },
                "path": f.path,
                "fact_id": f.fact_id,
                "version": f.version,
                "source_session_id": f.source_session_id,
                "value": f.value,
            })
        })
        .collect();
    if items.is_empty() {
        return Ok(json!({
            "status": "empty",
            "total_raw_facts": total_found,
            "note": "no memory entries matched; broadening the query or checking the \
                     namespace may help (query/namespace parameters are optional)",
        }));
    }
    Ok(json!({
        "status": "ok",
        "total_raw_facts": total_found,
        "returned": returned,
        "entries": items,
    }))
}

/// `memory_recall` 执行器（占位/接线双态）
pub struct MemoryRecallTool {
    namespace: Option<String>,
    client: Option<EvoruleApiClient>,
}

impl MemoryRecallTool {
    /// 占位构造（启动期 default_safe_toolkit 注册；调用 fail-visible 报错）
    pub fn unwired() -> Self {
        Self {
            namespace: None,
            client: None,
        }
    }

    /// 接线构造（runner 会话期重绑）
    pub fn wired(namespace: String, client: EvoruleApiClient) -> Self {
        Self {
            namespace: Some(namespace),
            client: Some(client),
        }
    }

    fn parts(&self) -> Result<(&str, &EvoruleApiClient), String> {
        match (&self.namespace, &self.client) {
            (Some(ns), Some(c)) => Ok((ns, c)),
            _ => Err(format!(
                "{}: not wired to a session yet (registered as a startup placeholder; \
                 it is re-bound once a run session exists)",
                MEMORY_RECALL_TOOL
            )),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for MemoryRecallTool {
    async fn call(&self, args: &Value) -> Result<Value, String> {
        let (namespace, client) = self.parts()?;
        memory_recall_exec(namespace, client, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kind_prefixes_mapping() {
        let all = kind_prefixes("all", "ns1").unwrap();
        assert_eq!(all, vec!["shared.ns1.stable.", "shared.ns1.events."]);
        assert_eq!(
            kind_prefixes("stable", "ns1").unwrap(),
            vec!["shared.ns1.stable."]
        );
        assert_eq!(
            kind_prefixes("events", "ns1").unwrap(),
            vec!["shared.ns1.events."]
        );
        assert!(kind_prefixes("kv", "ns1").is_err());
    }

    #[test]
    fn test_extract_args_defaults() {
        let (q, kind, ns, limit) = extract_args(&json!({})).unwrap();
        assert!(q.is_none() && kind == "all" && ns.is_empty() && limit == 20);
        let (q, _, _, _) = extract_args(&json!({"query": "  deploy  "})).unwrap();
        assert_eq!(q.as_deref(), Some("deploy"), "query trim 后空判定");
        let (_, _, ns, _) = extract_args(&json!({"namespace": "other-ns"})).unwrap();
        assert_eq!(ns, "other-ns");
        let (_, _, ns, _) = extract_args(&json!({"namespace": "  "})).unwrap();
        assert_eq!(ns, "", "空 namespace 视为缺省(用当前 ns)");
    }

    #[test]
    fn test_extract_args_rejects_bad_limit_and_kind_passthrough() {
        assert!(extract_args(&json!({"limit": 0})).is_err());
        assert!(extract_args(&json!({"limit": 101})).is_err());
        assert!(extract_args(&json!({"limit": 100})).is_ok());
        // kind 枚举校验在 kind_prefixes（执行体路径），extract_args 透传
        let (_, kind, _, _) = extract_args(&json!({"kind": "stable"})).unwrap();
        assert_eq!(kind, "stable");
    }

    #[test]
    fn test_latest_by_path_keeps_highest_version() {
        let f = |id: u64, path: &str, ver: u64| crate::api::evorule_client::SharedFactEntry {
            fact_id: id,
            path: path.to_string(),
            value: json!("x"),
            source_session_id: 1,
            version: ver,
            origin_fact_id: None,
        };
        let entries = vec![f(1, "a", 1), f(2, "a", 3), f(3, "b", 1), f(4, "a", 2)];
        let got = latest_by_path(entries);
        assert_eq!(got.len(), 2);
        let a = got.iter().find(|e| e.path == "a").unwrap();
        assert_eq!(a.fact_id, 2, "version 3 wins over 1/2");
    }

    #[test]
    fn test_unwired_fails_visible() {
        let tool = MemoryRecallTool::unwired();
        let err = tool.parts().unwrap_err();
        assert!(err.contains("not wired"), "{err}");
        let wired = MemoryRecallTool::wired(
            "ns1".to_string(),
            crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1"),
        );
        assert!(wired.parts().is_ok());
    }
}
