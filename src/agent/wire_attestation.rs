// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 字节级存证层：对每次 LLM 调用实际发送的消息集合做确定性哈希存证。
//!
//! 审计边界背景：生产流式路径下，一个回合只有首次 LLM 调用作为 IoRequest/IoResponse
//! 过引擎；工具回喂后的中间轮在应用层本地执行，其请求内容此前只能由链上材料
//! （transcript 全文 + 确定性裁剪 + 链上摘要）确定性**重建**。本模块把这一缺口收窄为
//! **发送时刻的存证**：每次调用落一条 payload 事实（与 transcript 同走 FactsLog 哈希链），
//! 记录实际发送消息集合的 BLAKE3 哈希与边界信息。验证语义：重建哈希 == 存证哈希 ⇒ 可信；
//! 哈希不等 ⇒ 组装行为与账面不符（或代码版本漂移——由配方指纹区分）。
//!
//! 成本：每调用一条 payload 事实（数百字节），无原始字节保留（存文层不在此层）。

use blake3::Hasher;
use serde::Serialize;

/// 存证记录（serde struct 序列化，字段序=声明序，跨版本稳定）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WireAttestation {
    /// 会话内调用序（从 0 起；与 journal LlmCalled 的顺序一致）
    pub seq: u64,
    pub model: String,
    /// blake3(canonical_input_json)，"blake3:"+64hex
    pub wire_hash: String,
    /// canonical_input_json 的字节长度
    pub wire_len: u64,
    /// 本次实际发送的消息数（裁剪/G10 后）
    pub messages_sent: usize,
    /// 送发前被裁剪的消息数（0=未裁剪）
    pub trimmed_count: usize,
    pub purpose: String,
    /// Unix 秒
    pub ts: u64,
}

/// 存证事实的 payload 路径（与 transcript 同走 payload 事实入链）。
pub fn attestation_path(seq: u64) -> String {
    format!("__audit__.wire_attestation.{}", seq)
}

/// 被哈希的 canonical 输入：messages + 影响生成的关键参数。
/// 字段序=struct 声明序；serde_json 序列化跨同版本确定。
#[derive(Serialize)]
struct AttestedInput<'a> {
    model: &'a str,
    temperature: f64,
    messages: &'a serde_json::Value,
}

/// canonical JSON 字节（哈希输入）。
pub fn canonical_input_json(model: &str, temperature: f64, messages: &serde_json::Value) -> String {
    let input = AttestedInput {
        model,
        temperature,
        messages,
    };
    serde_json::to_string(&input).unwrap_or_default()
}

/// 计算一次调用的存证：返回 (wire_hash, wire_len)。
/// `messages_to_send` = 裁剪/G10 后实际发送的消息数组（JSON 数组）。
pub fn compute(
    model: &str,
    temperature: f64,
    messages_to_send: &serde_json::Value,
) -> (String, u64) {
    let canonical = canonical_input_json(model, temperature, messages_to_send);
    let mut hasher = Hasher::new();
    hasher.update(canonical.as_bytes());
    let hash = format!("blake3:{}", hasher.finalize().to_hex());
    let len = canonical.as_bytes().len() as u64;
    (hash, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(n: usize) -> serde_json::Value {
        serde_json::json!([
            {"role": "system", "content": "demo"},
            {"role": "user", "content": format!("q{}", n)},
        ])
    }

    #[test]
    fn same_input_same_hash_different_input_different_hash() {
        let (h1, l1) = compute("m", 0.3, &msgs(1));
        let (h2, _) = compute("m", 0.3, &msgs(1));
        let (h3, _) = compute("m", 0.3, &msgs(2));
        assert_eq!(h1, h2, "同输入必同哈希");
        assert_ne!(h1, h3, "异输入必异哈希");
        assert!(h1.starts_with("blake3:"));
        assert_eq!(l1, canonical_input_json("m", 0.3, &msgs(1)).len() as u64);
    }

    #[test]
    fn temperature_change_changes_hash() {
        let (h1, _) = compute("m", 0.3, &msgs(1));
        let (h2, _) = compute("m", 0.7, &msgs(1));
        assert_ne!(h1, h2, "生成参数参与哈希");
    }

    #[test]
    fn path_shape_is_stable() {
        assert_eq!(attestation_path(0), "__audit__.wire_attestation.0");
        assert_eq!(attestation_path(17), "__audit__.wire_attestation.17");
    }

    #[test]
    fn record_serialization_field_order_is_stable() {
        let r = WireAttestation {
            seq: 3,
            model: "m".into(),
            wire_hash: "blake3:ab".into(),
            wire_len: 42,
            messages_sent: 2,
            trimmed_count: 0,
            purpose: "react".into(),
            ts: 1792211621,
        };
        let s1 = serde_json::to_string(&r).unwrap();
        let s2 = serde_json::to_string(&r).unwrap();
        assert_eq!(s1, s2);
        assert!(
            s1.starts_with("{\"seq\":3,\"model\":\"m\""),
            "字段序=声明序: {s1}"
        );
    }
}
