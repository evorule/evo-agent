// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! F-905 I2 检查器(初版)——组装后 system 分区间字面级冲突扫描。
//!
//! 口径(收尾清偿设计档 C-3/06 T-7):约束 vs 记忆/能力自述的**确定性矛盾
//! 模式**(否定对、禁令与声明互斥);输出=报告(journal I2ScanReport 事件,
//! 经 ATIF context_management 步 REST 可查),**不阻断会话**(与 F-201
//! 加载拒载分层:一个管进门前,一个管进门后)。
//!
//! 初版确定性子集(词法固定、无 LLM、无语义推断):
//! - 候选 token = 声明能力名(definition.tools + skills 名,调用方传入);
//! - 禁令形态 = 禁止词与 token 邻近([`MARK_WINDOW_CHARS`] 字符窗口);
//! - 分区 = 机制标记切分(基底段 + 机制分区;标记集与 F-201 R3 哨兵同源);
//! - 冲突 = 同一 token 跨分区「禁令×声明」对:能力自述分区含 token 即视为
//!   声明面;其余分区取 affirmation 词形为声明面。
//! 扩词表/扩规则 = 行为变更(功能规格版本纪律)。

use serde::{Deserialize, Serialize};

/// 机制分区标记(权威源;F-201 R3 哨兵同源共用——新增机制分区须同步)
pub const MECHANISM_SECTION_MARKERS: &[&str] = &[
    "## Stable Facts",
    "## Previous Sessions",
    "## Relevant Events",
    "## Recall Degradation Notices",
    "【能力边界声明】",
    "【可用技能清单】",
    "【规范入口索引】",
];

/// 禁令词形(确定性词表)
const PROHIBITION_MARKS: &[&str] = &["禁止", "不得", "不要", "不能", "不允许", "禁用"];
/// 声明词形(确定性词表)
const AFFIRMATION_MARKS: &[&str] = &["可以", "允许", "应当", "应该", "支持", "推荐"];
/// 禁令/声明词与 token 的邻近窗口(字符数)
const MARK_WINDOW_CHARS: usize = 12;
/// 冲突摘录上限(字符)
const EXCERPT_MAX_CHARS: usize = 80;

/// 单条冲突记录(journal/ATIF 序列化面)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct I2ConflictRecord {
    /// 冲突形态:deny_vs_capability(禁令×能力自述)| deny_vs_affirmation(禁令×肯定声明)
    pub kind: String,
    /// 冲突 token(声明能力名)
    pub token: String,
    /// 禁令所在分区名
    pub section_a: String,
    /// 声明所在分区名
    pub section_b: String,
    /// 禁令行摘录
    pub excerpt_a: String,
    /// 声明行摘录
    pub excerpt_b: String,
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 分区切分:按机制标记首次出现位置切段;首个标记前的文本=基底段
/// (含 F-101 身份段);返回有序 (分区名, 文本) 表,标记未出现则跳过。
fn split_sections(system_prompt: &str) -> Vec<(String, String)> {
    let mut cuts: Vec<(usize, &str)> = MECHANISM_SECTION_MARKERS
        .iter()
        .filter_map(|m| system_prompt.find(m).map(|p| (p, *m)))
        .collect();
    cuts.sort_by_key(|(p, _)| *p);
    let mut out = Vec::with_capacity(cuts.len() + 1);
    let base_end = cuts.first().map(|(p, _)| *p).unwrap_or(system_prompt.len());
    out.push((
        "(基底段)".to_string(),
        system_prompt[..base_end].to_string(),
    ));
    for (i, (start, marker)) in cuts.iter().enumerate() {
        let end = cuts
            .get(i + 1)
            .map(|(p, _)| *p)
            .unwrap_or(system_prompt.len());
        out.push((marker.to_string(), system_prompt[*start..end].to_string()));
    }
    out
}

fn is_capability_section(name: &str) -> bool {
    name == "【能力边界声明】" || name == "【可用技能清单】"
}

/// 在分区文本中找「mark 邻近 token」的首个命中行(返回摘录)
fn find_marked_line(text: &str, token: &str, marks: &[&str]) -> Option<String> {
    for line in text.lines() {
        let mut search_from = 0usize;
        while let Some(rel) = line[search_from..].find(token) {
            let tok_start = search_from + rel;
            let tok_end = tok_start + token.len();
            let win_start = floor_char_boundary(line, tok_start.saturating_sub(MARK_WINDOW_CHARS));
            let win_end = ceil_char_boundary(line, (tok_end + MARK_WINDOW_CHARS).min(line.len()));
            let window = &line[win_start..win_end];
            if marks.iter().any(|m| window.contains(m)) {
                return Some(truncate_chars(line.trim(), EXCERPT_MAX_CHARS));
            }
            search_from = tok_end;
        }
    }
    None
}

/// 在分区文本中找首个含 token 的行(无词形要求——能力自述分区即声明面)
fn first_line_with(text: &str, token: &str) -> Option<String> {
    text.lines()
        .find(|l| l.contains(token))
        .map(|l| truncate_chars(l.trim(), EXCERPT_MAX_CHARS))
}

/// F-905 I2 检查器(初版):对组装完成的 system 文本做分区间字面级冲突扫描。
/// `capability_tokens` = 声明能力名(工具名/skill 名);返回冲突报告(空=无冲突)。
pub fn inspect_system_sections(
    system_prompt: &str,
    capability_tokens: &[String],
) -> Vec<I2ConflictRecord> {
    let sections = split_sections(system_prompt);
    let mut out = Vec::new();
    for token in capability_tokens {
        if token.trim().is_empty() {
            continue;
        }
        for (ai, (name_a, text_a)) in sections.iter().enumerate() {
            let Some(excerpt_a) = find_marked_line(text_a, token, PROHIBITION_MARKS) else {
                continue;
            };
            for (bi, (name_b, text_b)) in sections.iter().enumerate() {
                if ai == bi {
                    continue;
                }
                let hit_b = if is_capability_section(name_b) {
                    first_line_with(text_b, token).map(|e| ("deny_vs_capability", e))
                } else {
                    find_marked_line(text_b, token, AFFIRMATION_MARKS)
                        .map(|e| ("deny_vs_affirmation", e))
                };
                if let Some((kind, excerpt_b)) = hit_b {
                    out.push(I2ConflictRecord {
                        kind: kind.to_string(),
                        token: token.clone(),
                        section_a: name_a.clone(),
                        section_b: name_b.clone(),
                        excerpt_a: excerpt_a.clone(),
                        excerpt_b,
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_sections_base_and_markers() {
        let sp = "你是助手。\n\n## Stable Facts\n- a\n\n【能力边界声明】\n- b";
        let secs = split_sections(sp);
        assert_eq!(secs[0].0, "(基底段)");
        assert!(secs[0].1.starts_with("你是助手"));
        assert!(secs.iter().any(|(n, _)| n == "## Stable Facts"));
        assert!(secs.iter().any(|(n, _)| n == "【能力边界声明】"));
        // 无标记文本 = 仅基底段
        let only_base = split_sections("plain prompt");
        assert_eq!(only_base.len(), 1);
        assert_eq!(only_base[0].0, "(基底段)");
    }

    #[test]
    fn deny_vs_capability_conflict_reported() {
        let sp = "你是助手。禁止使用 web_search 工具。\n\n【能力边界声明】\n可用工具:web_search、file_read";
        let r = inspect_system_sections(sp, &["web_search".to_string()]);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].kind, "deny_vs_capability");
        assert_eq!(r[0].token, "web_search");
        assert_eq!(r[0].section_a, "(基底段)");
        assert_eq!(r[0].section_b, "【能力边界声明】");
        assert!(r[0].excerpt_a.contains("禁止"));
        assert!(r[0].excerpt_b.contains("web_search"));
    }

    #[test]
    fn deny_vs_affirmation_conflict_reported() {
        let sp = "你可以使用 git_push 完成任务。\n\n## Stable Facts\n- 禁止使用 git_push(治理裁定)";
        let r = inspect_system_sections(sp, &["git_push".to_string()]);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].kind, "deny_vs_affirmation");
        assert_eq!(r[0].section_a, "## Stable Facts");
        assert_eq!(r[0].section_b, "(基底段)");
    }

    #[test]
    fn clean_system_no_conflicts() {
        let sp = "你可以使用 file_read。\n\n## Stable Facts\n- file_read 常用只读工具\n\n【能力边界声明】\n可用工具:file_read";
        let r = inspect_system_sections(sp, &["file_read".to_string()]);
        assert!(r.is_empty(), "{r:?}");
    }

    #[test]
    fn deterministic_same_input_same_report() {
        let sp = "禁止使用 web_search。\n\n【能力边界声明】\n- web_search";
        let a = inspect_system_sections(sp, &["web_search".to_string()]);
        let b = inspect_system_sections(sp, &["web_search".to_string()]);
        assert_eq!(a, b);
    }

    #[test]
    fn multibyte_window_no_panic() {
        // 禁令词与 token 间含多字节中文——窗口裁剪须走字符边界
        let sp = "治理裁定:根据规约禁止调用 git_push 命令。\n\n## Stable Facts\n- 推荐使用 git_push 做推送";
        let r = inspect_system_sections(sp, &["git_push".to_string()]);
        assert_eq!(r.len(), 1, "{r:?}");
    }
}
