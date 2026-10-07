// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 自主交接双工具（handover_write / handover_read）——会话间状态的结构化读写。
//!
//! 交接档=新会话的聚焦信号重注入：会话收尾时 LLM 调 `handover_write` 落
//! 结构化交接档，续接会话首动作 `handover_read` 读档核对（交接叙事档 +
//! query_journal 事实账双源互证）。
//!
//! 落点与 schema（自主交接设计 §3.2）：
//! - 交接档落 evorule shared facts `shared.{namespace}.handovers.{id}`，
//!   `id = handover-YYYYMMDD-HHMMSS`（UTC，系统生成；字典序=时间序）；
//! - LLM 必填 6 字段（goal/done/todo_next/anchors/env_state/verification），
//!   缺项/空值 fail-visible 一次列明全部缺项后拒写；handover_version /
//!   id / parent_session 系统填（LLM 无权指定，防身份漂移）；
//! - `handover_read` 返回交接档+完整性校验（缺字段 → `missing_fields`
//!   警示，fail-visible 警示不拒读——读侧审计面暴露残档）。
//!
//! 注册形态（查账工具族同构）：占位执行体启动期注册（default_safe_toolkit，
//! 未接线时调用 fail-visible 如实报错），runner 会话期重绑（session_id /
//! namespace / client 在手）——开关过滤（agentTools.handover）/声明收紧/
//! LLM 契约三面自动工作，零特殊通路。Standard 免裁决 + AutoPolicy；每次
//! 成功写入落 journal `handover_written` 事件（runner 侧按工具名分支）。

use serde_json::{json, Value};

use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handlers::tool_handler::ToolFunction;

/// 交接写工具名
pub const HANDOVER_WRITE_TOOL: &str = "handover_write";
/// 交接读工具名
pub const HANDOVER_READ_TOOL: &str = "handover_read";

/// 交接档 schema 必填字段（LLM 面；handover_version/id/parent_session 系统填不列）
pub const HANDOVER_REQUIRED_FIELDS: &[&str] = &[
    "goal",
    "done",
    "todo_next",
    "anchors",
    "env_state",
    "verification",
];

/// 交接档结构校验（纯函数）：返回缺失/空值必填字段列表（空 = 通过）
///
/// 判空口径：字符串 trim 后非空；数组/对象非空；其余类型视为缺。
pub fn handover_schema_missing(handover: &Value) -> Vec<String> {
    let mut missing = Vec::new();
    for field in HANDOVER_REQUIRED_FIELDS {
        let present = match handover.get(*field) {
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
            _ => false,
        };
        if !present {
            missing.push((*field).to_string());
        }
    }
    missing
}

/// unix 秒 → UTC 时间戳串 `YYYYMMDD-HHMMSS`（确定性儒略日算法，无外部依赖）
fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    // Howard Hinnant civil_from_days（公有域算法；与 memory_tool 日期族同源）
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    let d = doy - (153 * mp + 2) / 5 + 1;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 参数抽取 + schema 校验（fail-visible：缺/空必填一次列明全部缺项后拒写）
fn extract_handover_fields(args: &Value) -> Result<Value, String> {
    let mut missing: Vec<String> = Vec::new();
    let string_field = |name: &str, missing: &mut Vec<String>| -> Option<String> {
        args.get(name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                missing.push(name.to_string());
                None
            })
    };
    let string_list_field = |name: &str, missing: &mut Vec<String>| -> Option<Vec<String>> {
        match args.get(name).and_then(|v| v.as_array()) {
            Some(items) if !items.is_empty() => Some(
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
            ),
            _ => {
                missing.push(name.to_string());
                None
            }
        }
    };
    let object_field = |name: &str, missing: &mut Vec<String>| -> Option<Value> {
        match args.get(name).and_then(|v| v.as_object()) {
            Some(o) if !o.is_empty() => Some(Value::Object(o.clone())),
            _ => {
                missing.push(name.to_string());
                None
            }
        }
    };
    let goal = string_field("goal", &mut missing);
    let done = string_list_field("done", &mut missing);
    let todo_next = string_list_field("todo_next", &mut missing);
    let anchors = object_field("anchors", &mut missing);
    let env_state = object_field("env_state", &mut missing);
    let verification = string_field("verification", &mut missing);
    if !missing.is_empty() {
        return Err(format!(
            "handover_write: missing/empty required field(s): {} — a handover without \
             them is not a handover (schema fail-visible); fill them and retry",
            missing.join("、")
        ));
    }
    Ok(json!({
        "goal": goal,
        "done": done,
        "todo_next": todo_next,
        "anchors": anchors,
        "env_state": env_state,
        "verification": verification,
    }))
}

/// 交接档文档组装（纯函数）：系统字段（handover_version/id/parent_session）
/// + LLM 六字段。返回 (fact_path, id, doc)。
fn compose_handover_doc(
    namespace: &str,
    session_id: &str,
    secs: u64,
    fields: Value,
) -> (String, String, Value) {
    let id = format!("handover-{}", utc_stamp(secs));
    let path = format!("shared.{namespace}.handovers.{id}");
    let mut doc = json!({
        "handover_version": "1.0",
        "id": id,
        "parent_session": session_id,
    });
    if let (Some(dst), Some(src)) = (doc.as_object_mut(), fields.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    let doc_id = doc["id"].as_str().unwrap_or_default().to_string();
    (path, doc_id, doc)
}

/// `handover_write` 执行体（schema fail-visible → 系统组装 → 落 shared facts）
pub(crate) async fn handover_write_exec(
    namespace: &str,
    client: &EvoruleApiClient,
    session_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let fields = extract_handover_fields(args)?;
    let (path, id, doc) = compose_handover_doc(namespace, session_id, now_secs(), fields);
    client
        .update_payload(session_id, &path, &doc)
        .await
        .map_err(|e| format!("handover_write: persist failed ({e})"))?;
    Ok(json!({
        "status": "ok",
        "path": path,
        "id": id,
        "schema_ok": true,
        "note": "handover recorded; the continuation session's first action should be \
                 handover_read and verify the verification criterion",
    }))
}

/// `handover_read` 执行体（缺省最新 / 指定 id；完整性校验警示不拒读）
pub(crate) async fn handover_read_exec(
    namespace: &str,
    client: &EvoruleApiClient,
    args: &Value,
) -> Result<Value, String> {
    let prefix = format!("shared.{namespace}.handovers.");
    let facts = client
        .get_shared_facts(Some(&prefix))
        .await
        .map_err(|e| format!("handover_read: ledger unreachable ({e})"))?;
    if facts.is_empty() {
        return Ok(json!({
            "status": "empty",
            "note": "no handover documents found for this namespace",
        }));
    }
    let requested = args
        .get("handover_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // id 构词（时间戳）保证字典序=时间序：缺省取字典序最大；指定 id 精确匹配
    let picked = match requested {
        Some(want) => facts
            .iter()
            .find(|f| f.path.rsplit('.').next().is_some_and(|t| t == want))
            .ok_or_else(|| format!("handover_read: handover '{want}' not found"))?,
        None => facts
            .iter()
            .max_by(|a, b| a.path.cmp(&b.path))
            .ok_or_else(|| "handover_read: ledger empty".to_string())?,
    };
    let id = picked
        .path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_string();
    let missing = handover_schema_missing(&picked.value);
    let mut out = json!({
        "status": "ok",
        "id": id,
        "path": picked.path,
        "handover": picked.value,
    });
    if !missing.is_empty() {
        out["missing_fields"] = json!(missing);
        out["warning"] = json!(
            "handover completeness check failed (missing mandatory fields) — treat the \
             remaining fields with care; fail-visible warning, not a refusal to read"
        );
    }
    Ok(out)
}

/// `handover_write` 执行器（占位/接线双态；会话期重绑——session_id 在手）
pub struct HandoverWriteTool {
    namespace: Option<String>,
    client: Option<EvoruleApiClient>,
    session_id: Option<String>,
}

impl HandoverWriteTool {
    /// 占位构造（启动期 default_safe_toolkit 注册；调用 fail-visible 报错）
    pub fn unwired() -> Self {
        Self {
            namespace: None,
            client: None,
            session_id: None,
        }
    }

    /// 接线构造（runner 会话期重绑；wire_accounting 同构）
    pub fn wired(namespace: String, client: EvoruleApiClient, session_id: String) -> Self {
        Self {
            namespace: Some(namespace),
            client: Some(client),
            session_id: Some(session_id),
        }
    }

    fn parts(&self) -> Result<(&str, &EvoruleApiClient, &str), String> {
        match (&self.namespace, &self.client, &self.session_id) {
            (Some(ns), Some(c), Some(sid)) => Ok((ns, c, sid)),
            _ => Err(format!(
                "{}: not wired to a session yet (registered as a startup placeholder; \
                 it is re-bound once a run session exists)",
                HANDOVER_WRITE_TOOL
            )),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for HandoverWriteTool {
    async fn call(&self, args: &Value) -> Result<Value, String> {
        let (namespace, client, session_id) = self.parts()?;
        handover_write_exec(namespace, client, session_id, args).await
    }
}

/// `handover_read` 执行器（占位/接线双态）
pub struct HandoverReadTool {
    namespace: Option<String>,
    client: Option<EvoruleApiClient>,
}

impl HandoverReadTool {
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
                HANDOVER_READ_TOOL
            )),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for HandoverReadTool {
    async fn call(&self, args: &Value) -> Result<Value, String> {
        let (namespace, client) = self.parts()?;
        handover_read_exec(namespace, client, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handover_schema_missing_detects_missing_and_empty() {
        let full = json!({
            "goal": "g", "done": ["a"], "todo_next": ["b"],
            "anchors": {"k": "v"}, "env_state": {"HEAD": "x"},
            "verification": "v",
        });
        assert!(handover_schema_missing(&full).is_empty());
        // 缺字段
        let no_verification = json!({
            "goal": "g", "done": ["a"], "todo_next": ["b"],
            "anchors": {"k": "v"}, "env_state": {"HEAD": "x"},
        });
        assert_eq!(
            handover_schema_missing(&no_verification),
            vec!["verification"]
        );
        // 空串/空数组/空对象
        let empties = json!({
            "goal": "  ", "done": [], "todo_next": ["b"],
            "anchors": {}, "env_state": {"HEAD": "x"},
            "verification": "v",
        });
        assert_eq!(
            handover_schema_missing(&empties),
            vec!["goal", "done", "anchors"]
        );
        // 全缺
        assert_eq!(handover_schema_missing(&json!({})).len(), 6);
    }

    #[test]
    fn handover_write_extract_rejects_missing_fields_fail_visible() {
        // 缺 verification：错误信息列明缺项
        let args = json!({
            "goal": "g", "done": ["a"], "todo_next": ["b"],
            "anchors": {"k": "v"}, "env_state": {"HEAD": "x"},
        });
        let err = extract_handover_fields(&args).unwrap_err();
        assert!(err.contains("verification"), "{err}");
        // 缺 done + anchors：一次列全（不逐个挤牙膏）
        let args = json!({
            "goal": "g", "todo_next": ["b"],
            "env_state": {"HEAD": "x"}, "verification": "v",
        });
        let err = extract_handover_fields(&args).unwrap_err();
        assert!(err.contains("done") && err.contains("anchors"), "{err}");
    }

    #[test]
    fn handover_write_extract_assembles_valid_fields() {
        let args = json!({
            "goal": "g", "done": ["a"], "todo_next": ["b"],
            "anchors": {"k": "v"}, "env_state": {"HEAD": "x"},
            "verification": "v",
        });
        let fields = extract_handover_fields(&args).unwrap();
        assert!(handover_schema_missing(&fields).is_empty());
    }

    #[test]
    fn compose_handover_doc_fills_system_fields() {
        let fields = json!({
            "goal": "g", "done": ["a"], "todo_next": ["b"],
            "anchors": {"k": "v"}, "env_state": {"HEAD": "x"},
            "verification": "v",
        });
        let (path, id, doc) = compose_handover_doc("general", "sess-1", 1_791_500_000, fields);
        assert!(
            path.starts_with("shared.general.handovers.handover-"),
            "{path}"
        );
        assert_eq!(path, format!("shared.general.handovers.{id}"));
        assert_eq!(doc["parent_session"], json!("sess-1"));
        assert_eq!(doc["handover_version"], json!("1.0"));
        assert_eq!(doc["goal"], json!("g"));
        assert_eq!(doc["env_state"], json!({"HEAD": "x"}));
    }

    #[test]
    fn utc_stamp_is_deterministic() {
        // 确定性锚：epoch 原点与整日/整时推进
        assert_eq!(utc_stamp(0), "19700101-000000");
        assert_eq!(utc_stamp(3_600), "19700101-010000");
        assert_eq!(utc_stamp(86_400), "19700102-000000");
        // 跨年推进（1970-12-31 → 1971-01-01）
        assert_eq!(utc_stamp(365 * 86_400), "19710101-000000");
        // 闰年含 2 月 29 日（1972 年；epoch+789 天 = 1972-02-29）
        assert_eq!(utc_stamp(789 * 86_400), "19720229-000000");
    }

    #[tokio::test]
    async fn unwired_tools_fail_visible() {
        let w = HandoverWriteTool::unwired();
        let err = w.call(&serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("not wired"), "{err}");
        let r = HandoverReadTool::unwired();
        let err = r.call(&serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("not wired"), "{err}");
    }
}
