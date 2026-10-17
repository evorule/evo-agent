// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 记忆墓碑工具（memory_tombstone）——沉淀污染环的治理出口（O-487②）。
//!
//! sediment 沉淀可能把 LLM 幻觉结论（如虚构的工具清单断言）固化为跨会话
//! 共享事实（`shared.{ns}.stable.*` / `shared.{ns}.events.*`），后续会话经
//! auto_recall 回注形成污染环。实况校验锚（落账前拒写）拦截新增污染；
//! 本工具补全存量出口：对已固化的错误事实写 null 墓碑——append-only 账本
//! 下原始数据永在账，latest-wins 语义（最新版本 value == null → 整条 path
//! 抑制，`memory.rs` 墓碑过滤同源）使该条目在召回/检索视图不可见。
//!
//! 写面（零新存储原语）：复用 `EvoruleApiClient::update_payload` 写 null
//! （与 `Memory::remove_scoped` 同一持久化通路）；落碑前经 `get_shared_facts`
//! 精确核对目标 path 存在——不存在的 path 落 null 即凭空造账（payload 污染
//! +审计噪声，`Memory::clear` 修复注记同款纪律），fail-visible 拒绝。
//!
//! 写面收敛（读侧不过度设防、写侧收紧）：仅接受本命名空间记忆账本两分区
//! （`shared.{ns}.stable.` / `shared.{ns}.events.` 前缀），不提供 namespace
//! 覆写（memory_recall 的跨 ns 读取豁免不适用于写入面）；reason 强制非空，
//! 随工具调用入 journal 审计链与审批提案（理由永久可追溯）。
//!
//! 注册形态（memory_recall 同构）：占位执行体启动期注册（default_safe_toolkit，
//! 未接线时调用 fail-visible 如实报错），runner 会话期重绑（namespace /
//! client / session_id 在手）——开关过滤（agentTools.memory_tombstone，默认
//! 关）/声明收紧/LLM 契约三面自动工作。治理分级：Sensitive + ManualDefault
//! （P2 事前意图裁决在通道 + candidate 审批模式首调出 needs_approval 提案，
//! session_spawn 双闸同款）。

use serde_json::{json, Value};

use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handlers::tool_handler::ToolFunction;

/// 工具名
pub const MEMORY_TOMBSTONE_TOOL: &str = "memory_tombstone";

/// 参数抽取 + 校验（fail-visible：path 前缀/存在性/reason 空实一次报清）
///
/// 返回 `(path, reason)`；path 仅接受本命名空间记忆账本两分区前缀。
fn validate_args(args: &Value, namespace: &str) -> Result<(String, String), String> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "memory_tombstone: missing required param: path".to_string())?
        .to_string();
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "memory_tombstone: missing required param: reason (tombstoning without a \
             reason is not permitted; the reason is permanently attached to the \
             journal/audit trail)"
                .to_string()
        })?
        .to_string();
    let stable_prefix = format!("shared.{namespace}.stable.");
    let events_prefix = format!("shared.{namespace}.events.");
    if !path.starts_with(&stable_prefix) && !path.starts_with(&events_prefix) {
        return Err(format!(
            "memory_tombstone: path must target this namespace's memory ledger \
             ('{stable_prefix}*' or '{events_prefix}*'); got '{path}'"
        ));
    }
    Ok((path, reason))
}

/// `memory_tombstone` 执行体（存在性核对 → null 墓碑落账）
pub(crate) async fn memory_tombstone_exec(
    namespace: &str,
    session_id: &str,
    client: &EvoruleApiClient,
    args: &Value,
) -> Result<Value, String> {
    let (path, reason) = validate_args(args, namespace)?;
    // 存在性核对：精确 path 匹配（get_shared_facts 前缀读 + 全等过滤）；
    // 不存在的 path 落 null = 凭空造账，拒绝
    let facts = client
        .get_shared_facts(Some(path.as_str()))
        .await
        .map_err(|e| format!("memory_tombstone: ledger unreachable ({e})"))?;
    if !facts.iter().any(|f| f.path == path) {
        return Err(format!(
            "memory_tombstone: target path '{path}' not found in the ledger \
             (tombstoning a non-existent path would create a phantom entry; \
             verify the path via memory_recall first)"
        ));
    }
    // null 墓碑：latest-wins 语义下整条 path 视图抑制（append-only 原值永在账）
    let fact_id = client
        .update_payload(session_id, &path, &json!(null))
        .await
        .map_err(|e| format!("memory_tombstone: tombstone persist failed ({e})"))?;
    Ok(json!({
        "status": "tombstoned",
        "path": path,
        "fact_id": fact_id,
        "reason": reason,
        "note": "latest version is now null: the entry is view-invisible \
                 (recall/search filter it out); the original value remains in \
                 the append-only ledger",
    }))
}

/// `memory_tombstone` 执行器（占位/接线双态）
pub struct MemoryTombstoneTool {
    namespace: Option<String>,
    session_id: Option<String>,
    client: Option<EvoruleApiClient>,
}

impl MemoryTombstoneTool {
    /// 占位构造（启动期 default_safe_toolkit 注册；调用 fail-visible 报错）
    pub fn unwired() -> Self {
        Self {
            namespace: None,
            session_id: None,
            client: None,
        }
    }

    /// 接线构造（runner 会话期重绑）
    pub fn wired(namespace: String, session_id: String, client: EvoruleApiClient) -> Self {
        Self {
            namespace: Some(namespace),
            session_id: Some(session_id),
            client: Some(client),
        }
    }

    fn parts(&self) -> Result<(&str, &str, &EvoruleApiClient), String> {
        match (&self.namespace, &self.session_id, &self.client) {
            (Some(ns), Some(sid), Some(c)) => Ok((ns, sid, c)),
            _ => Err(format!(
                "{}: not wired to a session yet (registered as a startup placeholder; \
                 it is re-bound once a run session exists)",
                MEMORY_TOMBSTONE_TOOL
            )),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for MemoryTombstoneTool {
    async fn call(&self, args: &Value) -> Result<Value, String> {
        let (namespace, session_id, client) = self.parts()?;
        memory_tombstone_exec(namespace, session_id, client, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_args_accepts_ledger_prefixes() {
        let args = json!({
            "path": "shared.default.stable.llm.m3.prefers_dark",
            "reason": "  hallucinated conclusion  ",
        });
        let (path, reason) = validate_args(&args, "default").unwrap();
        assert_eq!(path, "shared.default.stable.llm.m3.prefers_dark");
        assert_eq!(reason, "hallucinated conclusion", "reason trim 后入账");
        let (path, _) = validate_args(
            &json!({"path": "shared.default.events.E001", "reason": "stale"}),
            "default",
        )
        .unwrap();
        assert_eq!(path, "shared.default.events.E001");
    }

    #[test]
    fn test_validate_args_rejects_missing_or_empty_params() {
        assert!(validate_args(&json!({}), "default").is_err());
        assert!(validate_args(&json!({"path": "  "}), "default").is_err());
        assert!(validate_args(&json!({"path": "shared.default.stable.x"}), "default").is_err());
        assert!(
            validate_args(&json!({"path": "shared.default.stable.x", "reason": "   "}), "default")
                .is_err(),
            "空 reason 视同缺失"
        );
    }

    #[test]
    fn test_validate_args_rejects_out_of_ledger_paths() {
        // 跨命名空间写拒绝（读侧豁免不适用于写面）
        assert!(validate_args(
            &json!({"path": "shared.other.stable.x", "reason": "r"}),
            "default"
        )
        .is_err());
        // 非记忆账本分区拒绝（session/work/knowledge_candidates 等不在收敛面）
        assert!(validate_args(
            &json!({"path": "shared.default.sessions.s1.summary", "reason": "r"}),
            "default"
        )
        .is_err());
        assert!(validate_args(
            &json!({"path": "session.default.topic", "reason": "r"}),
            "default"
        )
        .is_err());
    }

    #[test]
    fn test_unwired_fails_visible() {
        let tool = MemoryTombstoneTool::unwired();
        let err = tool.parts().unwrap_err();
        assert!(err.contains("not wired"), "{err}");
        let wired = MemoryTombstoneTool::wired(
            "ns1".to_string(),
            "s1".to_string(),
            crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1"),
        );
        assert!(wired.parts().is_ok());
    }
}
