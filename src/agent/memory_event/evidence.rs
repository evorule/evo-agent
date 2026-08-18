// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! B4 记忆证据类型定义（07c）。
//!
//! 本文件在 B1（07b）阶段创建为**最小骨架**——仅结构体定义，无方法。
//! 方法（evidence_for / verify_batch / attach_evidence / shared_evidence）在 B4（07c C-4/C-4b）阶段补充。
//!
//! B1 需要 `MemoryEvidence` 类型存在，因为 `MemoryRecord.evidence` 字段引用它。

use serde::{Deserialize, Serialize};

/// /audit/causal/{fact_id} 链条中的单个节点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalLink {
    pub fact_id: u64,
    pub fact_type: String,
    pub logical_time: u64,
    pub content_hash: String,
    pub prev_hash: String,
    #[serde(default)]
    pub cause: Option<u64>,
}

/// /audit/causal/{fact_id} 完整响应（类型化）
#[derive(Debug, Clone, Deserialize)]
pub struct CausalChain {
    pub session_id: u64,
    pub fact_id: u64,
    pub chain_length: usize,
    pub chain: Vec<CausalLink>,
}

/// /audit/verify 完整响应（类型化）
#[derive(Debug, Clone, Deserialize)]
pub struct AuditVerify {
    pub verified: bool,
    pub session_id: u64,
    #[serde(default)]
    pub fact_count: Option<u64>,
    #[serde(default)]
    pub last_hash: Option<String>,
}

/// B4：记忆证据 —— 三段式证明
///
/// 展示任何记忆时可出示「源 FactId + 哈希链验证 + 引擎因果链」三段证明。
/// `MemoryRecord.evidence` 存储时恒 None，仅展示/审计时按需填充（attach_evidence）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryEvidence {
    /// 所属会话（agent 侧字符串 sid；共享证据溯源时为**源会话** sid）。
    /// 注：与 `CausalChain.session_id: u64`（server 数字 sid）表示同一会话、类型不同，勿混用。
    pub session_id: String,
    /// 身份锚点：本记忆对应 Fact（KV=其 PayloadUpdate；事件=identity）
    pub fact_id: u64,
    /// 记忆路径（若有）
    pub path: Option<String>,
    /// 整链完整性（verify_audit_typed）
    pub verified: bool,
    pub last_hash: Option<String>,
    /// 因果锚点：事件 cause 指向的源 FactId（KV 为 None）
    pub cause_fact_id: Option<u64>,
    /// 引擎因果链（cause 源 Fact 的链；KV/无 cause 时为空）
    pub chain: Vec<CausalLink>,
    /// 验证失败原因（离线/超时）
    pub error: Option<String>,
}

impl MemoryEvidence {
    /// 是否通过整链验证
    pub fn is_verified(&self) -> bool {
        self.verified
    }

    /// 紧凑渲染：`[fact#42 ✓ chain=3]` 或 `[fact#42 ✗ chain=0]`
    pub fn render_compact(&self) -> String {
        let ok = if self.verified { "✓" } else { "✗" };
        format!("[fact#{} {} chain={}]", self.fact_id, ok, self.chain.len())
    }
}

/// 批量验证汇总
#[derive(Debug, Clone, Default)]
pub struct BatchVerifyReport {
    pub verified: bool,
    pub fact_count: usize,
    pub verified_facts: Vec<u64>,
}

/// B4：带证据的叙述（narrate_with_evidence 输出）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NarrativeWithEvidence {
    /// LLM 生成的自然语言叙述
    pub text: String,
    /// 引用的事件 ID 列表
    pub cited_events: Vec<String>,
    /// 引用的 Fact ID 列表
    #[serde(default)]
    pub cited_facts: Vec<u64>,
    /// event_id → 紧凑证据标记
    #[serde(default)]
    pub evidence: std::collections::BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_compact_verified() {
        let ev = MemoryEvidence {
            fact_id: 42,
            verified: true,
            chain: vec![
                CausalLink {
                    fact_id: 42,
                    fact_type: "PayloadUpdate".to_string(),
                    logical_time: 1,
                    content_hash: "abc".to_string(),
                    prev_hash: String::new(),
                    cause: None,
                },
                CausalLink {
                    fact_id: 43,
                    fact_type: "PayloadUpdate".to_string(),
                    logical_time: 2,
                    content_hash: "def".to_string(),
                    prev_hash: "abc".to_string(),
                    cause: Some(42),
                },
                CausalLink {
                    fact_id: 44,
                    fact_type: "PayloadUpdate".to_string(),
                    logical_time: 3,
                    content_hash: "ghi".to_string(),
                    prev_hash: "def".to_string(),
                    cause: Some(43),
                },
            ],
            ..Default::default()
        };
        let s = ev.render_compact();
        assert!(s.contains("fact#42"), "rendered: {}", s);
        assert!(s.contains("✓"), "rendered: {}", s);
        assert!(s.contains("chain=3"), "rendered: {}", s);
    }

    #[test]
    fn test_render_compact_unverified() {
        let ev = MemoryEvidence {
            fact_id: 7,
            verified: false,
            chain: vec![],
            ..Default::default()
        };
        let s = ev.render_compact();
        assert!(s.contains("fact#7"), "rendered: {}", s);
        assert!(s.contains("✗"), "rendered: {}", s);
        assert!(s.contains("chain=0"), "rendered: {}", s);
    }

    #[test]
    fn test_batch_verify_report_default() {
        let report = BatchVerifyReport::default();
        assert!(!report.verified);
        assert_eq!(report.fact_count, 0);
        assert!(report.verified_facts.is_empty());
    }

    #[test]
    fn test_narrative_with_evidence_default() {
        let n = NarrativeWithEvidence {
            text: String::new(),
            cited_events: Vec::new(),
            cited_facts: Vec::new(),
            evidence: std::collections::BTreeMap::new(),
        };
        assert!(n.text.is_empty());
        assert!(n.cited_events.is_empty());
        assert!(n.cited_facts.is_empty());
        assert!(n.evidence.is_empty());
    }

    #[test]
    fn test_is_verified() {
        let ev_true = MemoryEvidence {
            verified: true,
            ..Default::default()
        };
        assert!(ev_true.is_verified());

        let ev_false = MemoryEvidence {
            verified: false,
            ..Default::default()
        };
        assert!(!ev_false.is_verified());
    }

    #[test]
    fn test_narrative_with_evidence_serialize_roundtrip() {
        let mut evidence = std::collections::BTreeMap::new();
        evidence.insert("E001".to_string(), "[fact#10 ✓ chain=2]".to_string());
        let n = NarrativeWithEvidence {
            text: "叙述文本".to_string(),
            cited_events: vec!["E001".to_string()],
            cited_facts: vec![10],
            evidence,
        };
        let json = serde_json::to_string(&n).unwrap();
        let restored: NarrativeWithEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.text, "叙述文本");
        assert_eq!(restored.cited_events, vec!["E001"]);
        assert_eq!(restored.cited_facts, vec![10]);
        assert_eq!(
            restored.evidence.get("E001").unwrap(),
            "[fact#10 ✓ chain=2]"
        );
    }
}
