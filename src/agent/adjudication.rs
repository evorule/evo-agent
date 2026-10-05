// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 矛盾裁决面（阶段 3 F-612）：记忆条目矛盾的确定性检测与规则裁决。
//!
//! 四步（记忆设计档 §5.2）：
//! 1. **候选检测**（确定性）：词面相似（分词集 Jaccard ≥ 阈值）+ 词法极性
//!    相反（一方命中否定词表、另一方未命中）→ 矛盾候选对；
//! 2. **裁决**（规则）：authority ▸ confidence ▸ freshness 序（维度序由
//!    Recipe.adjudication.order 声明）；**胜者为 wire 视图呈现项**；
//! 3. **败者处理**：同 path 写新版本（同内容 + lifecycle_state=Superseded +
//!    superseded_by 指向胜者）——append-only 不删除、仍可查询（RL-A1）；
//! 4. 裁决记录落链（`shared.{ns}.adjudication.{pair-hash}`，确定性路径，
//!    已存在则跳过=幂等零版本空转）；LLM 可 sidecar 提议疑似矛盾，
//!    **裁决权在规则**（与 LLM 直判分野）。
//!
//! 与机器闸 M3 的关系：同一冲突扫描函数族（确定性成对扫描→候选→处置），
//! M3 作用于知识资产域（重复提案拒/域重叠警告），本模块作用于记忆条目域
//! ——跨仓函数级归一为演进项，接口形状对齐。
//!
//! 门控：`Recipe.adjudication.enabled`（缺省关=既有 agent 零影响）；
//! 极性词表与维度序均为 Recipe 数据（策略数据化）。

use crate::agent::memory::{tokenize_for_match, MemoryRecord};

/// 权威秩（维度内定序；人工声明>受信管道>模型提取>未标注——与资产化时序
/// 「机器上限 Active、Published 永人工」一致）
fn authority_rank(record: &MemoryRecord) -> u8 {
    match record.source.as_deref() {
        Some("user") => 3,
        Some("system") => 2,
        Some(s) if s.starts_with("llm") => 1,
        _ => 0,
    }
}

/// 词法极性：文本命中任一否定词=否定极性
fn negative_polarity(text: &str, markers: &[String]) -> bool {
    let lower = text.to_lowercase();
    markers.iter().any(|m| !m.is_empty() && lower.contains(&m.to_lowercase()))
}

/// 词面相似度：分词集 Jaccard（确定性；零向量红线内）
fn lexical_similarity(a: &str, b: &str) -> f32 {
    let mut sa: Vec<String> = tokenize_for_match(a);
    sa.sort();
    sa.dedup();
    let mut sb: Vec<String> = tokenize_for_match(b);
    sb.sort();
    sb.dedup();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.iter().filter(|t| sb.contains(t)).count();
    let union = sa.len() + sb.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f32 / union as f32
    }
}

/// 矛盾候选对判定（确定性）：词面相似 ≥ 阈值 且 极性相反
fn is_contradiction_pair(
    a: &MemoryRecord,
    b: &MemoryRecord,
    markers: &[String],
    threshold: f32,
) -> bool {
    let text_a = format!("{} {}", a.key, a.value);
    let text_b = format!("{} {}", b.key, b.value);
    lexical_similarity(&text_a, &text_b) >= threshold
        && negative_polarity(&text_a, markers) != negative_polarity(&text_b, markers)
}

/// 裁决：按维度序（Recipe.adjudication.order）比较，返回胜者下标。
/// 维度值：authority=来源权威秩；confidence=自评置信；freshness=时间戳。
/// 平局保持原序（先到者胜，确定性）。
fn adjudicate_pair(
    a: &MemoryRecord,
    b: &MemoryRecord,
    order: &[String],
) -> usize {
    use std::cmp::Ordering;
    for d in order {
        let ord = match d.as_str() {
            "authority" => {
                authority_rank(a).cmp(&authority_rank(b))
            }
            "confidence" => a
                .confidence
                .unwrap_or(0.0)
                .total_cmp(&b.confidence.unwrap_or(0.0)),
            "freshness" => a.timestamp.cmp(&b.timestamp),
            _ => continue,
        };
        if ord != Ordering::Equal {
            return if ord == Ordering::Greater { 0 } else { 1 };
        }
    }
    0
}

/// 对一组已按路径去重的稳定条目做矛盾扫描，返回候选对（下标对）与
/// 每对的胜者下标（0=前者胜 1=后者胜）。
pub fn scan_and_adjudicate(
    records: &[MemoryRecord],
    markers: &[String],
    threshold: f32,
    order: &[String],
) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for i in 0..records.len() {
        for j in (i + 1)..records.len() {
            if is_contradiction_pair(&records[i], &records[j], markers, threshold) {
                let winner = adjudicate_pair(&records[i], &records[j], order);
                out.push((i, j, winner));
            }
        }
    }
    out
}

/// 裁决记录与败者标记的确定性路径（pair-hash 截断；同对同路径=幂等）
pub fn adjudication_path(namespace: &str, winner_path: &str, loser_path: &str) -> String {
    let hex = evorule_hash::digest(format!("{winner_path}|{loser_path}").as_bytes());
    let short = &hex[..hex.len().min(12)];
    format!("shared.{namespace}.adjudication.{short}")
}

#[cfg(test)]
mod tests {

    use super::*;

    fn rec(key: &str, value: &str, source: Option<&str>, conf: f32, ts: u64) -> MemoryRecord {
        let mut r = MemoryRecord::new(key, value, ts);
        r.source = source.map(String::from);
        r.confidence = Some(conf);
        r
    }

    #[test]
    fn test_polarity_and_similarity() {
        assert!(negative_polarity("禁止部署", &["禁止".into()]));
        assert!(!negative_polarity("允许部署", &["禁止".into()]));
        // 相似:同主体异极性文本词面高重叠
        let sim = lexical_similarity("缓存开关默认开启", "缓存开关默认不开启");
        assert!(sim >= 0.6, "{sim}");
        let low = lexical_similarity("缓存开关默认开启", "今天天气晴朗");
        assert!(low < 0.3, "{low}");
    }

    #[test]
    fn test_contradiction_pair_detection() {
        let markers = vec!["不".to_string(), "禁止".to_string()];
        let a = rec("k.a", "缓存开关默认开启", Some("llm"), 0.6, 100);
        let b = rec("k.b", "缓存开关默认不开启", Some("llm"), 0.6, 200);
        assert!(is_contradiction_pair(&a, &b, &markers, 0.5));
        // 同极性不成对
        let c = rec("k.c", "缓存开关确实默认开启", Some("llm"), 0.6, 300);
        assert!(!is_contradiction_pair(&a, &c, &markers, 0.6));
    }

    #[test]
    fn test_adjudicate_authority_confidence_freshness() {
        let order = vec![
            "authority".to_string(),
            "confidence".to_string(),
            "freshness".to_string(),
        ];
        // authority:user(3) > llm(1) → 前者胜(0)
        let user_rec = rec("k", "人工声明版本", Some("user"), 0.5, 100);
        let llm_rec = rec("k2", "模型提取版本", Some("llm"), 0.9, 200);
        assert_eq!(adjudicate_pair(&user_rec, &llm_rec, &order), 0);
        // authority 平 → confidence 高者胜(1)
        let low = rec("k", "v", Some("user"), 0.3, 100);
        let high = rec("k2", "v2", Some("user"), 0.9, 100);
        assert_eq!(adjudicate_pair(&low, &high, &order), 1);
        // authority/confidence 平 → freshness 新者胜(1)
        let old = rec("k", "v", Some("user"), 0.5, 100);
        let new = rec("k2", "v2", Some("user"), 0.5, 200);
        assert_eq!(adjudicate_pair(&old, &new, &order), 1);
        // 全平 → 先到者胜(0,确定性)
        let same = rec("k2", "v2", Some("user"), 0.5, 100);
        assert_eq!(adjudicate_pair(&old, &same, &order), 0);
    }

    #[test]
    fn test_scan_and_adjudicate_finds_pairs() {
        let markers = vec!["不".to_string()];
        let order = vec![
            "authority".to_string(),
            "confidence".to_string(),
            "freshness".to_string(),
        ];
        let records = vec![
            rec("k.a", "缓存开关默认开启", Some("user"), 0.5, 100),
            rec("k.b", "完全无关的另一条事实内容", None, 0.5, 150),
            rec("k.c", "缓存开关默认不开启", Some("llm"), 0.9, 300),
        ];
        let pairs = scan_and_adjudicate(&records, &markers, 0.5, &order);
        assert_eq!(pairs.len(), 1);
        let (i, j, winner) = pairs[0];
        assert_eq!((i, j), (0, 2));
        assert_eq!(winner, 0, "authority:user > llm");
    }

    #[test]
    fn test_adjudication_path_deterministic() {
        let p1 = adjudication_path("ns", "shared.ns.stable.a", "shared.ns.stable.b");
        let p2 = adjudication_path("ns", "shared.ns.stable.a", "shared.ns.stable.b");
        assert_eq!(p1, p2);
        assert!(p1.starts_with("shared.ns.adjudication."));
        assert_ne!(
            p1,
            adjudication_path("ns", "shared.ns.stable.b", "shared.ns.stable.a")
        );
    }
}
