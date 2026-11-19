// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 矛盾裁决面（阶段 3 F-612）：记忆条目矛盾的确定性检测与规则裁决。
//!
//! 四步（记忆设计档 §5.2）：
//! 1. **候选检测**（确定性）双规：
//!    - 词面相似（分词集 Jaccard ≥ 阈值）+ 词法极性相反（一方命中否定词表、
//!      另一方未命中）；
//!  - 同主题异值（ P2， 正主）：Recipe 独占槽位 key 词形命中 +
//!      值词形集不相交——覆盖「同主题异键异值全肯定极性」类（极性规则
//!      结构性漏检的活体案例）。
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
use crate::agent::recipe::ValueSlot;

/// 权威秩（维度内定序；人工声明>系统派生/机器核验>模型提取>未标注——与
/// 资产化时序「机器上限 Active、Published 永人工」一致）
fn authority_rank(record: &MemoryRecord) -> u8 {
    match record.source.as_deref() {
        Some("user") => 3,
        Some("system") => 2,
        // ledger 层（ P3）：实测核验事实（ L2 约定 source:"ledger"，
        // 携带作废锚）与系统派生同层、高于模型提取——防实测锚被排在 llm
        // 之下致裁决方向反转
        Some(s) if s == "ledger" || s.starts_with("ledger-") => 2,
        Some(s) if s.starts_with("llm") => 1,
        _ => 0,
    }
}

/// 词法极性：文本命中任一否定词=否定极性
fn negative_polarity(text: &str, markers: &[String]) -> bool {
    let lower = text.to_lowercase();
    markers
        .iter()
        .any(|m| !m.is_empty() && lower.contains(&m.to_lowercase()))
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
fn adjudicate_pair(a: &MemoryRecord, b: &MemoryRecord, order: &[String]) -> usize {
    use std::cmp::Ordering;
    for d in order {
        let ord = match d.as_str() {
            "authority" => authority_rank(a).cmp(&authority_rank(b)),
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

/// 值词形抽取（确定性； P2）：value 文本按空白与常见标点切词，词元含
/// 路径分隔符（`\`、`/`）或纯数字（含点分）即「具体值词元」，小写化、去尾
/// 标点。ASCII `:` 不入切词集（盘符 `c:\...` 与 URL 的可判定性）；中文标点
/// （全角）在切词集内。
pub(crate) fn extract_value_tokens(text: &str) -> Vec<String> {
    let lowered = text.to_lowercase();
    let mut out = Vec::new();
    for tok in lowered.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '，' | '。'
                    | '；'
                    | '、'
                    | '：'
                    | '！'
                    | '？'
                    | '（'
                    | '）'
                    | '《'
                    | '》'
                    | '「'
                    | '」'
                    | '【'
                    | '】'
                    | ','
                    | ';'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '"'
                    | '\''
                    | '`'
                    | '!'
            )
    }) {
        let tok = tok.trim_matches(|c: char| matches!(c, '.' | '。' | ',' | '，' | ';' | '；'));
        if tok.is_empty() {
            continue;
        }
        let numeric = tok.chars().all(|c| c.is_ascii_digit() || c == '.');
        if tok.contains('\\') || tok.contains('/') || numeric {
            out.push(tok.to_string());
        }
    }
    out
}

/// 槽位判定（首个命中槽；key 词形子串包含，小写化）——只匹配记录的 key
/// 字段而非 value 体：正文提及≠断言（「工作区列表：D:\a、D:\b」类并列
/// 提及不得触发单值槽位）。
fn first_common_slot(a: &MemoryRecord, b: &MemoryRecord, slots: &[ValueSlot]) -> Option<usize> {
    let ka = a.key.to_lowercase();
    let kb = b.key.to_lowercase();
    slots.iter().position(|s| {
        let hit = |k: &str| {
            s.key_tokens
                .iter()
                .any(|t| !t.is_empty() && k.contains(&t.to_lowercase()))
        };
        hit(&ka) && hit(&kb)
    })
}

/// 同主题异值冲突扫描（ P2， 正主）：同独占槽位（key 词形命中）
/// 且双方值词形集均非空且不相交 → 矛盾候选对，裁决复用维度序。**值相交=
/// 并存合法**（多路径/多端口类），不判矛盾；槽位外主题不检——语义等价
/// 交由 Recipe 槽位数据扩充（与否定词表同一保守纪律），不做词法猜测。
pub fn scan_value_conflicts(
    records: &[MemoryRecord],
    slots: &[ValueSlot],
    order: &[String],
) -> Vec<(usize, usize, usize, String)> {
    let mut out = Vec::new();
    for i in 0..records.len() {
        for j in (i + 1)..records.len() {
            let Some(slot) = first_common_slot(&records[i], &records[j], slots) else {
                continue;
            };
            let vi = extract_value_tokens(&records[i].value);
            let vj = extract_value_tokens(&records[j].value);
            if vi.is_empty() || vj.is_empty() {
                continue;
            }
            if vi.iter().any(|t| vj.contains(t)) {
                continue;
            }
            let winner = adjudicate_pair(&records[i], &records[j], order);
            out.push((i, j, winner, slots[slot].name.clone()));
        }
    }
    out
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

    #[test]
    fn test_authority_rank_ledger_tier() {
        let order = vec![
            "authority".to_string(),
            "confidence".to_string(),
            "freshness".to_string(),
        ];
        // ledger 实测锚 > llm 提取（authority 维生效）
        let ledger = rec("k", "实测锚版本", Some("ledger"), 0.5, 100);
        let llm = rec("k2", "模型提取版本", Some("llm"), 0.9, 200);
        assert_eq!(
            adjudicate_pair(&ledger, &llm, &order),
            0,
            "ledger(2) > llm(1)"
        );
        let ledger_prefixed = rec("k", "实测锚版本", Some("ledger-cmd"), 0.5, 100);
        assert_eq!(adjudicate_pair(&ledger_prefixed, &llm, &order), 0);
        // user 声明仍居首（Published 永人工）
        let user = rec("k3", "人工声明版本", Some("user"), 0.5, 100);
        assert_eq!(
            adjudicate_pair(&user, &ledger, &order),
            0,
            "user(3) > ledger(2)"
        );
    }

    #[test]
    fn test_extract_value_tokens() {
        assert_eq!(
            extract_value_tokens("当前工作区绑定状态 = C:\\Users\\A\\Documents\\evo-agent"),
            vec!["c:\\users\\a\\documents\\evo-agent"]
        );
        // 纯文本无具体值 → 空集
        assert!(extract_value_tokens("缓存开关默认开启").is_empty());
        // 数字（端口/版本类）
        assert_eq!(extract_value_tokens("服务端口 8080 已就绪"), vec!["8080"]);
        assert_eq!(extract_value_tokens("版本 1.2.3"), vec!["1.2.3"]);
        // URL
        assert_eq!(
            extract_value_tokens("see https://example.com/x"),
            vec!["https://example.com/x"]
        );
        // 空格拆分的 Windows 路径尾段仍为独立词元
        assert_eq!(extract_value_tokens("D:\\AI AGENT"), vec!["d:\\ai"]);
    }

    fn ws_slots() -> Vec<ValueSlot> {
        vec![ValueSlot {
            name: "workspace_root".to_string(),
            key_tokens: vec![
                "current_working_directory".to_string(),
                "sandbox_root".to_string(),
                "当前工作区".to_string(),
            ],
        }]
    }

    fn default_order() -> Vec<String> {
        vec![
            "authority".to_string(),
            "confidence".to_string(),
            "freshness".to_string(),
        ]
    }

    #[test]
    fn test_value_conflict_o12_live_case() {
        //  活体案例①（同主题异键异值、全肯定极性——极性规则结构性漏检，
        //  P2 值冲突规则的正主场景）
        let records = vec![
            rec(
                "当前工作区绑定状态",
                "C:\\Users\\A\\Documents\\evo-agent",
                Some("user"),
                0.5,
                1000,
            ),
            rec(
                "session.sandbox_root",
                "D:\\evorule-video-engine",
                Some("llm"),
                0.5,
                2000,
            ),
            rec(
                "current_working_directory",
                "D:\\AI AGENT",
                Some("llm"),
                0.5,
                3000,
            ),
        ];
        let pairs = scan_value_conflicts(&records, &ws_slots(), &default_order());
        assert_eq!(pairs.len(), 3, "三键三值两两异值=三对");
        assert_eq!(pairs[0], (0, 1, 0, "workspace_root".to_string()));
        assert_eq!(pairs[1], (0, 2, 0, "workspace_root".to_string()));
        assert_eq!(pairs[2], (1, 2, 1, "workspace_root".to_string()));
        // 维度序实绩:user 权威(0)胜 llm 对;llm 平局时 freshness 新者(2)胜
    }

    #[test]
    fn test_value_conflict_negatives() {
        let order = default_order();
        // 同槽同值 → 并存合法不判
        let a = rec(
            "stable.user.工作区根",
            "D:\\shared\\root",
            Some("user"),
            0.5,
            100,
        );
        let b = rec(
            "session.sandbox_root",
            "D:\\shared\\root",
            Some("llm"),
            0.5,
            200,
        );
        assert!(
            scan_value_conflicts(&[a, b], &ws_slots(), &order).is_empty(),
            "值相交=并存"
        );
        // 槽位外异值（端口类并列合法）→ 不检
        let p1 = rec("service.port", "8080", Some("llm"), 0.5, 100);
        let p2 = rec("metrics.port", "9090", Some("llm"), 0.5, 200);
        assert!(
            scan_value_conflicts(&[p1, p2], &ws_slots(), &order).is_empty(),
            "槽位外主题不检"
        );
        // 提及≠主张：正文含路径但 key 不在槽 → 不检
        let m1 = rec("构建记录", "在 D:\\repo 构建通过", Some("llm"), 0.5, 100);
        let m2 = rec("测试笔记", "于 D:\\other 验证失败", Some("llm"), 0.5, 200);
        assert!(
            scan_value_conflicts(&[m1, m2], &ws_slots(), &order).is_empty(),
            "key 不在槽不检"
        );
        // 空槽位表 → 零检出
        let c = rec("current_working_directory", "D:\\x", Some("llm"), 0.5, 100);
        let d = rec("session.sandbox_root", "D:\\y", Some("llm"), 0.5, 200);
        assert!(scan_value_conflicts(&[c, d], &[], &order).is_empty());
    }
}
