// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 摘要保真判据（规格修正批交付物 B）：G10 摘要替换的自动对照。
//!
//! 定位：把"原文在账可对照"（I5 原则条款）升级为**每次压缩自动对照**——
//! 被裁剪消息中确定性提取锚点（零 LLM），对照摘要文本命中情况，
//! journal 落 `summary_fidelity_scan` 事件（I2ScanReport 同款形态），
//! ratio<0.5 时 warn（fail-visible，与降级通知同口径）。
//!
//! 锚两类（确定性提取）：
//! - goal 关键词：被裁剪的首条 user 消息经分词取 top-8（频次降序+字典序）；
//! - 工具结论锚：被裁剪的工具结果 = 工具名×结果首句（截 64 字符）。
//!
//! 判定：锚字符串在摘要文本中大小写不敏感 contains；ratio=hit/anchors。
//! 边界：纯机械事实（工具名本身）不算锚——锚=结论内容；空摘要=无法判定，
//! 事件照落（summary_empty 置位）但不告警（无可判定对象≠保真失败）。

use crate::agent::translator::Message;

/// 单次扫描结果
#[derive(Debug, Clone, PartialEq)]
pub struct FidelityScan {
    /// 被裁剪消息数
    pub trimmed_n: usize,
    /// 锚点数
    pub anchors_n: usize,
    /// 命中锚点数
    pub hit_n: usize,
    /// 保真比（hit/anchors；无锚点时=1.0——无锚可失，不虚报失败）
    pub ratio: f64,
    /// 摘要为空（跳过判定；事件照落）
    pub summary_empty: bool,
}

/// goal 关键词锚上限
const GOAL_KEYWORD_MAX: usize = 8;
/// 工具结论首句截断长度（字符）
const TOOL_ANCHOR_MAX_CHARS: usize = 64;

/// 从被裁剪消息提取锚点（确定性，零 LLM）
pub fn extract_anchors(dropped: &[Message]) -> Vec<String> {
    let mut anchors: Vec<String> = Vec::new();
    // goal 关键词：首条 user 消息分词 top-8（频次降序+字典序 tie-break）
    if let Some(Message::User { content }) = dropped.iter().find(|m| matches!(m, Message::User { .. })) {
        let mut freq: std::collections::BTreeMap<String, usize> = Default::default();
        for t in crate::agent::memory::tokenize_for_match(content) {
            *freq.entry(t).or_insert(0) += 1;
        }
        let mut ranked: Vec<(String, usize)> = freq.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        anchors.extend(ranked.into_iter().take(GOAL_KEYWORD_MAX).map(|(t, _)| t));
    }
    // 工具结论锚：工具名×结果首句（截断）
    for m in dropped {
        if let Message::Tool { content, tool_name } = m {
            let first_sentence = content
                .split(['.', '。', '\n'])
                .find(|s| !s.trim().is_empty())
                .unwrap_or("")
                .trim();
            let mut anchor = format!("{tool_name}: {}", first_sentence);
            if anchor.chars().count() > TOOL_ANCHOR_MAX_CHARS {
                anchor = anchor.chars().take(TOOL_ANCHOR_MAX_CHARS).collect();
            }
            if !anchor.trim().is_empty() {
                anchors.push(anchor);
            }
        }
    }
    anchors
}

/// 判定：锚在摘要中大小写不敏感 contains；返回 (hit_n, anchors_n)
pub fn check_fidelity(summary: &str, anchors: &[String]) -> (usize, usize) {
    if anchors.is_empty() {
        return (0, 0);
    }
    let hay = summary.to_lowercase();
    let hit = anchors.iter().filter(|a| hay.contains(&a.to_lowercase())).count();
    (hit, anchors.len())
}

/// 单次扫描（提取+判定+组装结果；ratio 四舍五入至 4 位小数）
pub fn scan(dropped: &[Message], summary: &str, summary_empty: bool) -> FidelityScan {
    let anchors = extract_anchors(dropped);
    let anchors_n = anchors.len();
    let (hit_n, _) = check_fidelity(summary, &anchors);
    let ratio = if anchors_n == 0 {
        1.0
    } else {
        (hit_n as f64 / anchors_n as f64 * 10_000.0).round() / 10_000.0
    };
    FidelityScan {
        trimmed_n: dropped.len(),
        anchors_n,
        hit_n,
        ratio,
        summary_empty,
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    fn msg_user(content: &str) -> Message {
        Message::User { content: content.to_string() }
    }

    fn msg_tool(name: &str, content: &str) -> Message {
        Message::Tool { content: content.to_string(), tool_name: name.to_string() }
    }

    #[test]
    fn test_full_hit_ratio_one() {
        // 全含 → ratio=1（24 号批次三验收例 1）
        let dropped = vec![
            msg_user("部署服务并验证构建产物"),
            msg_tool("shell_exec", "build succeeded. 产物已生成"),
        ];
        let scan = scan(
            &dropped,
            "已部署服务并验证构建产物；shell_exec: build succeeded. 产物已生成",
            false,
        );
        // goal 关键词(分词器 CJK bigram 去重后取 8)+工具结论锚各 1
        assert!(scan.anchors_n >= 2);
        assert_eq!(scan.hit_n, scan.anchors_n);
        assert_eq!(scan.ratio, 1.0);
        assert!(!scan.summary_empty);
    }

    #[test]
    fn test_zero_hit_ratio_zero() {
        // 全不含 → ratio=0（验收例 2：判定面）
        let dropped = vec![msg_user("部署服务并验证构建产物")];
        let scan = scan(&dropped, "摘要内容完全无关", false);
        assert!(scan.anchors_n > 0);
        assert_eq!(scan.hit_n, 0);
        assert_eq!(scan.ratio, 0.0);
    }

    #[test]
    fn test_empty_summary_flagged_but_scanned() {
        // 空摘要 → summary_empty 置位（跳过判定语义），事件面字段齐全
        let dropped = vec![msg_user("部署服务")];
        let scan = scan(&dropped, "", true);
        assert!(scan.summary_empty);
        assert_eq!(scan.trimmed_n, 1);
    }

    #[test]
    fn test_tool_anchor_first_sentence_truncated() {
        // 工具结论锚=工具名×首句；超长截 64 字符
        let long = format!("{}. 后续内容不影响锚", "x".repeat(100));
        let dropped = vec![msg_tool("file_read", &long)];
        let anchors = extract_anchors(&dropped);
        assert_eq!(anchors.len(), 1);
        assert!(anchors[0].starts_with("file_read: "));
        assert!(anchors[0].chars().count() <= 64);
    }

    #[test]
    fn test_goal_keywords_unique_capped() {
        // 分词器内部去重:CJK bigram/整词唯一化后按字典序取 8(确定性;
        // 12 词超上限恰取 8)
        let dropped = vec![msg_user("迁移 迁移 迁移 校验 校验 备份 上线 监控 告警 回滚 文档 评审")];
        let anchors = extract_anchors(&dropped);
        assert_eq!(anchors.len(), 8);
        let mut uniq = anchors.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(anchors, uniq, "锚点无重复");
    }
}
