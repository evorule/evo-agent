// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 工具意图信号契约（tool_intent.v1）—— 信号生产方与规则资产的版本化契约
//!
//! ## 背景
//! P2 治理级工具的意图信号（`set meta_tool.pending_tool_intent`）此前为
//! 私有约定无契约：规则层 enforce 只能按信号实现现状写匹配字段，字段漂移
//! 即静默失配。本模块把信号 value 的字段集与版本号固化为可序列化契约，
//! 信号生产方（[`crate::agent::runner::tool_intent_signal`] 经
//! [`ToolIntentV1::from_resolved`] 派生）与信号消费方（规则资产
//! 00_constraint_tool_intent_adjudication 的 enforce）从此对齐同一 schema。
//!
//! ## v1 字段集（序列化形状由结构测试锁定）
//! - `tool_name`：工具名（规则层 enforce 匹配键）；
//! - `target_scope`：file 族路径快筛结果（可缺省——非 file 族治理工具无此字段，
//!   与既有「无 scope 字段」形态一致）；
//! - `args`：脱敏+截断后的 args（P1 轨迹同纪律；v1 兼容承载，规则层与回放比对可用）；
//! - `args_digest`：args 规范化摘要（`"blake3:"+64hex`，evorule-hash 规范化
//!   JSON 序列化，键序无关；体积/敏感面的回放比对锚点，恒在场）；
//! - `session_ref`：主会话 id（裁决账面审计关联；可缺省，由管道调用方上下文补齐）；
//! - `schema_ver`：契约版本号，恒 [`TOOL_INTENT_SCHEMA_VER`]。
//!
//! ## 演进纪律
//! 增量字段只增不改（v1 兼容）；破坏性变更必须升 v2 并双规则集并行过渡。

use serde::Serialize;
use serde_json::Value;

/// 契约版本号（结构测试锁定）
pub const TOOL_INTENT_SCHEMA_VER: &str = "tool_intent.v1";

/// 工具意图信号契约（tool_intent.v1）
///
/// 契约锁定字段集/版本号/摘要口径；键序经 serde_json 规范化（字典序），
/// 不作次序承诺——规则层 enforce 以字段路径匹配，不依赖键序。
#[derive(Debug, Clone, Serialize)]
pub struct ToolIntentV1 {
    /// 工具名（规则层 enforce 匹配键）
    pub tool_name: String,
    /// 目标范围快筛结果（file 族才有；缺省=无此字段）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_scope: Option<String>,
    /// 脱敏+截断后的 args（P1 轨迹同纪律；v1 兼容承载）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// args 规范化摘要（`"blake3:"+64hex`；恒在场）
    pub args_digest: String,
    /// 主会话 id（裁决账面审计关联；缺省=无会话关联）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_ref: Option<String>,
    /// 契约版本号
    pub schema_ver: &'static str,
}

/// args 规范化摘要（B 族纪律：哈希一律 evorule-hash，禁自写 blake3）
///
/// 口径：脱敏+截断后 args 的 `evorule_hash::json_digest`（serde_json 规范化
/// 序列化，键序无关）+ `prefixed` 自描述前缀（`"blake3:"+64hex`，与 journal
/// digest 形态一致）。
pub fn args_digest(sanitized_args: &Value) -> String {
    evorule_hash::prefixed(&evorule_hash::json_digest(sanitized_args))
}

impl ToolIntentV1 {
    /// 从 resolve_tool_intent 的规范字段输出派生 v1 契约
    ///
    /// `session_ref` 由管道调用方上下文补齐进解析输出后一并派生；
    /// `args_digest` 恒在场（args 缺省时按 JSON null 的规范化摘要计，
    /// 摘要存在性不随字段缺省漂移）。
    pub fn from_resolved(resolved: &Value) -> Self {
        let args = resolved.get("args").cloned();
        let args_digest = args_digest(args.as_ref().unwrap_or(&Value::Null));
        Self {
            tool_name: resolved
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            target_scope: resolved
                .get("target_scope")
                .and_then(Value::as_str)
                .map(str::to_string),
            args,
            args_digest,
            session_ref: resolved
                .get("session_ref")
                .and_then(Value::as_str)
                .map(str::to_string),
            schema_ver: TOOL_INTENT_SCHEMA_VER,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// 结构测试：v1 契约序列化形状（字段名/版本号/摘要形态）锁定
    #[test]
    fn contract_serializes_v1_field_set() {
        let resolved = serde_json::json!({
            "tool_name": "file_create",
            "target_scope": "out_of_sandbox",
            "args": {"path": "a.txt"},
            "session_ref": "42"
        });
        let v = serde_json::to_value(ToolIntentV1::from_resolved(&resolved))
            .expect("contract must serialize");
        // 字段集锁定：可选字段在场时恰为 v1 六字段（键序经 serde_json
        // 规范化，契约锁字段集与取值，不锁 Map 迭代序）
        let mut keys: Vec<&str> = v
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "args",
                "args_digest",
                "schema_ver",
                "session_ref",
                "target_scope",
                "tool_name"
            ]
        );
        assert_eq!(v["tool_name"], "file_create");
        assert_eq!(v["target_scope"], "out_of_sandbox");
        assert_eq!(v["args"]["path"], "a.txt");
        assert_eq!(v["session_ref"], "42");
        assert_eq!(v["schema_ver"], TOOL_INTENT_SCHEMA_VER);
        assert_eq!(v["schema_ver"], "tool_intent.v1");
        // 摘要形态：自描述前缀 + 64hex
        let digest = v["args_digest"].as_str().expect("digest must be a string");
        assert!(
            digest.starts_with("blake3:"),
            "digest must be prefixed: {digest}"
        );
        assert_eq!(digest.len(), "blake3:".len() + 64);
    }

    /// 结构测试：可选字段缺省即不写（与既有「无 scope 字段」形态一致），摘要恒在场
    #[test]
    fn contract_omits_absent_optional_fields() {
        let resolved = serde_json::json!({"tool_name": "git_stage", "args": {"files": ["a"]}});
        let v = serde_json::to_value(ToolIntentV1::from_resolved(&resolved)).expect("serialize");
        let mut keys: Vec<&str> = v
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["args", "args_digest", "schema_ver", "tool_name"]);
        assert!(v.get("target_scope").is_none());
        assert!(v.get("session_ref").is_none());
        assert!(v["args_digest"]
            .as_str()
            .expect("digest")
            .starts_with("blake3:"));
    }

    /// 摘要口径：规范化（键序无关、确定性）且有区分度
    #[test]
    fn args_digest_is_canonical_and_discriminating() {
        let a = serde_json::json!({"path": "a.txt", "mode": "w"});
        let b = serde_json::json!({"mode": "w", "path": "a.txt"});
        assert_eq!(args_digest(&a), args_digest(&b), "键序不得影响摘要");
        assert_eq!(args_digest(&a), args_digest(&a), "同输入必须同摘要");
        assert_ne!(
            args_digest(&a),
            args_digest(&serde_json::json!({"path": "b.txt", "mode": "w"})),
            "不同参数必须不同摘要"
        );
    }

    /// 结构测试：信号指令形态（中性 set + attr 固定）不变，value 为 v1 契约形态
    #[test]
    fn signal_form_is_neutral_set_with_contract_value() {
        let resolved = serde_json::json!({
            "tool_name": "file_delete",
            "target_scope": "out_of_sandbox",
            "args": {"path": "x"},
            "session_ref": "42"
        });
        let sig = crate::agent::runner::tool_intent_signal(&resolved);
        assert_eq!(sig["type"], "set");
        assert_eq!(sig["params"]["attr"], "meta_tool.pending_tool_intent");
        assert_eq!(sig["params"]["operation"], "set");
        assert_eq!(sig["params"]["value"]["tool_name"], "file_delete");
        assert_eq!(sig["params"]["value"]["target_scope"], "out_of_sandbox");
        assert_eq!(sig["params"]["value"]["args"]["path"], "x");
        assert_eq!(sig["params"]["value"]["session_ref"], "42");
        assert_eq!(sig["params"]["value"]["schema_ver"], "tool_intent.v1");
        assert!(sig["params"]["value"]["args_digest"]
            .as_str()
            .expect("digest")
            .starts_with("blake3:"));
    }
}
