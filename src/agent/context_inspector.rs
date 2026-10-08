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

/// 机制分区标记(权威源;F-201 哨兵同源共用——新增机制分区须同步;
/// ## Notes=双通道笔记强制回喂分区、## Skills/Procedures=程序型记忆
/// 注入分区,均随机制落成同步在案)
pub const MECHANISM_SECTION_MARKERS: &[&str] = &[
    "## Stable Facts",
    "## Previous Sessions",
    "## Relevant Events",
    "## Recall Degradation Notices",
    "【能力边界声明】",
    "【可用技能清单】",
    "【规范入口索引】",
    "## Notes",
    "## Skills/Procedures",
];

/// 禁令词形 v2(双语对称扩充,行为变更批:英文词形以词边界匹配,窗口适配)
const PROHIBITION_MARKS: &[&str] = &[
    "禁止",
    "不得",
    "不要",
    "不能",
    "不允许",
    "禁用",
    "严禁",
    "切勿",
    "不可",
    "拒绝",
    "never",
    "must not",
    "do not",
    "should not",
    "cannot",
    "forbidden",
    "prohibited",
    "avoid",
];
/// 声明词形 v2(双语对称扩充)
const AFFIRMATION_MARKS: &[&str] = &[
    "可以",
    "允许",
    "应当",
    "应该",
    "支持",
    "推荐",
    "必须",
    "务必",
    "建议",
    "能够",
    "must",
    "should",
    "can",
    "may",
    "allowed",
    "supported",
    "recommended",
    "enabled",
];
/// 禁令/声明词与 token 的邻近窗口:中文形 12 字符(历史口径不变)/英文形 24
/// 字符(英文词距更长,窗口适配;行为变更点之二)
const MARK_WINDOW_CHARS: usize = 12;
const MARK_WINDOW_CHARS_ASCII: usize = 24;

/// I2 词表（数据化载体）：机制内建 v2 双语表为缺省；definition.i2_lexicon
/// 声明即整体覆盖（per-agent 语言风格适配）。确定性纯数据。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct I2Lexicon {
    /// 禁令词形（空 = 内建 v2 表）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prohibition: Vec<String>,
    /// 声明词形（空 = 内建 v2 表）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affirmation: Vec<String>,
}

impl I2Lexicon {
    /// 机制内建 v2 双语表
    pub fn builtin() -> Self {
        Self {
            prohibition: PROHIBITION_MARKS.iter().map(|s| s.to_string()).collect(),
            affirmation: AFFIRMATION_MARKS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// 词形命中判定（词边界感知）：ASCII 边缘词形（英文）要求首尾字符非
/// ASCII 字母数字——"can" 不得命中 "scan"；非 ASCII 词形（中文）维持
/// 子串语义（无词边界概念，历史行为逐字节保真）。
fn contains_mark(window: &str, mark: &str) -> bool {
    if !mark.is_ascii() {
        return window.contains(mark);
    }
    // 英文形大小写不敏感(Never/never 同义;大小写即书写风格非语义)
    let (window_l, mark_l) = (window.to_lowercase(), mark.to_lowercase());
    let window = window_l.as_str();
    let mark = mark_l.as_str();
    let bytes = window.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = window[search_from..].find(mark) {
        let start = search_from + rel;
        let end = start + mark.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        search_from = start + 1;
    }
    false
}

/// 词形窗口（按词形语言分型：ASCII=24 字符 / 中文=12 字符）
fn window_chars_for(mark: &str) -> usize {
    if mark.is_ascii() {
        MARK_WINDOW_CHARS_ASCII
    } else {
        MARK_WINDOW_CHARS
    }
}
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
    /// 语义精判结论(两级通路第二级;None=未精判——旧账面/语义级关闭/缓存外
    /// 首轮失败,serde default 保旧 journal 反序列化兼容)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_verdict: Option<I2Verdict>,
}

/// 语义精判结论(裁决三态+理由;verdict 为观测注释不进控制流——I2 报告
/// 定位继承,误判不改变 agent 行为)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct I2Verdict {
    /// contradiction(真矛盾)| benign(良性共存/辖域不同/词形撞车)| uncertain(无法判定)
    pub verdict: String,
    /// 裁决理由(截断至 [`VERDICT_RATIONALE_MAX_CHARS`] 字符)
    pub rationale: String,
}

/// 语义精判用途标签(AuditedLlm 审计链命令事实;journal LlmCalled.purpose
/// 同值——sidecar 用途不映射 ATIF 步,既有口径)
pub const I2_SEMANTIC_PURPOSE: &str = "i2_semantic";
/// 语义精判 sidecar 整体超时(单候选裁决输入小,低于审计执行器默认 90s)
pub const I2_SEMANTIC_TIMEOUT_SECS: u64 = 30;
/// 裁决理由截断上限(字符)
const VERDICT_RATIONALE_MAX_CHARS: usize = 120;

impl I2Verdict {
    /// 无法判定结论(sidecar 失败/超时/输出不可解析的统一兜底;fail-soft
    /// 与字面级结论同兜底,会话不阻断)
    pub fn uncertain(rationale: &str) -> Self {
        Self {
            verdict: "uncertain".to_string(),
            rationale: truncate_chars(rationale, VERDICT_RATIONALE_MAX_CHARS),
        }
    }
}

/// 会话内裁决缓存键(确定性:六元组规范化序列化的 evorule-hash digest;
/// 同候选对同键=每候选每会话至多一次语义精判)
pub fn adjudication_cache_key(record: &I2ConflictRecord) -> String {
    let normalized = [
        record.kind.as_str(),
        record.token.as_str(),
        record.section_a.as_str(),
        record.section_b.as_str(),
        record.excerpt_a.as_str(),
        record.excerpt_b.as_str(),
    ]
    .join("\u{1f}");
    crate::agent::journal::evorule_digest(&normalized)
}

/// 语义精判调用参数构造(确定性纯函数:同候选对同字节;temperature=0 降方差;
/// 严格 JSON 三态输出要求,自由文本仅 rationale 字段且截 120 字)
pub fn build_adjudication_params(model: &str, record: &I2ConflictRecord) -> serde_json::Value {
    let user_content = format!(
        "上下文分区间疑似矛盾候选,请裁决是否为真实矛盾。\n\n\
         候选 token(能力名):{token}\n\
         冲突形态:{kind}\n\n\
         文本甲(分区:{section_a}):\n{excerpt_a}\n\n\
         文本乙(分区:{section_b}):\n{excerpt_b}\n\n\
         分区背景:机制分区(以 ## 或【开头)是系统注入的权威面(记忆事实/边界/技能清单);\n\
         (基底段)是代理定义声明面。\n\n\
         裁决问题:文本乙对能力「{token}」的表述,是否与文本甲的限制构成真实矛盾\n\
         (限制确实禁止了表述所授予的使用)?若限制有不同辖域/条件/语义,或仅为词形撞车,\n\
         则为良性共存。\n\n\
         只输出一个 JSON 对象,不要输出任何其他文本:\n\
         {{\"verdict\":\"contradiction|benign|uncertain\",\"rationale\":\"不超过120字的理由\"}}",
        token = record.token,
        kind = record.kind,
        section_a = record.section_a,
        excerpt_a = record.excerpt_a,
        section_b = record.section_b,
        excerpt_b = record.excerpt_b,
    );
    serde_json::json!({
        "model": model,
        "temperature": 0.0,
        "max_tokens": 300,
        "messages": [
            {"role": "system", "content": "你是上下文一致性审计员:对候选矛盾做中立裁决,只输出被要求的 JSON 对象。"},
            {"role": "user", "content": user_content}
        ]
    })
}

/// 裁决响应解析(确定性:严格 JSON 三态;围栏包裹剥离;非法输出/越值
/// verdict→uncertain 兜底——与 sidecar 失败同款 fail-soft)
pub fn parse_adjudication_response(content: &str) -> I2Verdict {
    let trimmed = content.trim();
    // 剥离 markdown 代码围栏(模型偶发包裹 ```json ... ```)
    let stripped = if trimmed.starts_with("```") {
        let inner = trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```");
        inner.trim()
    } else {
        trimmed
    };
    let parsed: Option<serde_json::Value> = serde_json::from_str(stripped).ok().or_else(|| {
        // 容忍前后杂文:取首个 '{' 到末个 '}' 的窗口重试
        let (a, b) = (stripped.find('{'), stripped.rfind('}'));
        match (a, b) {
            (Some(a), Some(b)) if a < b => serde_json::from_str(&stripped[a..=b]).ok(),
            _ => None,
        }
    });
    let verdict = parsed
        .as_ref()
        .and_then(|v| v.get("verdict"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let rationale = parsed
        .as_ref()
        .and_then(|v| v.get("rationale"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match verdict {
        "contradiction" | "benign" | "uncertain" => I2Verdict {
            verdict: verdict.to_string(),
            rationale: truncate_chars(rationale, VERDICT_RATIONALE_MAX_CHARS),
        },
        _ => I2Verdict::uncertain("语义裁决输出不可解析"),
    }
}

/// 语义精判编排(两级通路第二级):候选逐个解析 verdict——缓存优先
/// (命中零 LLM 调用),未命中走 fetch(sidecar 审计链内执行),失败→uncertain
/// 兜底。verdict 并入候选记录后返回;fetch 成功时经 journal 落 sidecar
/// token 账面(LlmCalled purpose=i2_semantic,失败不落——事件口径=成功完成时写)。
pub async fn adjudicate_candidates<F, Fut>(
    mut conflicts: Vec<I2ConflictRecord>,
    cache: &std::sync::Mutex<std::collections::HashMap<String, I2Verdict>>,
    model: &str,
    mut fetch: F,
    journal: Option<&crate::agent::journal::JournalWriter>,
) -> Vec<I2ConflictRecord>
where
    F: FnMut(&I2ConflictRecord) -> Fut,
    Fut: std::future::Future<
        Output = Result<(String, Option<crate::agent::journal::TokenRecord>), String>,
    >,
{
    for rec in conflicts.iter_mut() {
        let key = adjudication_cache_key(rec);
        // 锁作用域收窄:命中判定先持锁取值,fetch 在锁外执行(不跨 await 持锁)
        let cached = cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned();
        let verdict = match cached {
            Some(v) => v,
            None => {
                let verdict = match fetch(rec).await {
                    Ok((content, tokens)) => {
                        if let Some(j) = journal {
                            let _ = j.llm_called(
                                model,
                                I2_SEMANTIC_PURPOSE,
                                None,
                                tokens,
                                None,
                                1,
                                &truncate_chars(&content, 200),
                            );
                        }
                        parse_adjudication_response(&content)
                    }
                    Err(e) => I2Verdict::uncertain(&format!("语义裁决通路不可用: {e}")),
                };
                cache
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(key, verdict.clone());
                verdict
            }
        };
        rec.semantic_verdict = Some(verdict);
    }
    conflicts
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
            // 词形命中:逐词形独立窗口(ASCII 形 24/中文形 12)+词边界判定
            let hit = marks.iter().any(|m| {
                let w = window_chars_for(m);
                let ws = floor_char_boundary(line, tok_start.saturating_sub(w));
                let we = ceil_char_boundary(line, (tok_end + w).min(line.len()));
                contains_mark(&line[ws..we], m)
            });
            if hit {
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
    inspect_system_sections_with(system_prompt, capability_tokens, &I2Lexicon::builtin())
}

/// 同上（数据化入参版）：词表由调用方注入（definition.i2_lexicon 声明
/// 覆盖,或机制内建 v2 表）。
pub fn inspect_system_sections_with(
    system_prompt: &str,
    capability_tokens: &[String],
    lexicon: &I2Lexicon,
) -> Vec<I2ConflictRecord> {
    let sections = split_sections(system_prompt);
    let prohibition: Vec<&str> = lexicon.prohibition.iter().map(String::as_str).collect();
    let affirmation: Vec<&str> = lexicon.affirmation.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for token in capability_tokens {
        if token.trim().is_empty() {
            continue;
        }
        for (ai, (name_a, text_a)) in sections.iter().enumerate() {
            let Some(excerpt_a) = find_marked_line(text_a, token, &prohibition) else {
                continue;
            };
            for (bi, (name_b, text_b)) in sections.iter().enumerate() {
                if ai == bi {
                    continue;
                }
                let hit_b = if is_capability_section(name_b) {
                    first_line_with(text_b, token).map(|e| ("deny_vs_capability", e))
                } else {
                    find_marked_line(text_b, token, &affirmation)
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
                        semantic_verdict: None,
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

    // ===== 词表扩充 v2（双语+词边界+窗口+数据化） =====

    #[test]
    fn english_prohibition_and_affirmation_detected() {
        // 双语补齐:英文词形进入候选(此前中文-only=对英文语料盲扫)
        let sp = "Never use web_search without approval.

【能力边界声明】
可用工具:web_search";
        let r =
            inspect_system_sections_with(sp, &["web_search".to_string()], &I2Lexicon::builtin());
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].excerpt_a.contains("Never"), "{r:?}");

        let sp2 = "You must not call git_push on protected branches.

## Stable Facts
- 推荐 git_push 做推送";
        let r2 =
            inspect_system_sections_with(sp2, &["git_push".to_string()], &I2Lexicon::builtin());
        assert_eq!(r2.len(), 1, "must not × 推荐配对: {r2:?}");
    }

    #[test]
    fn english_word_boundary_rejects_interior_matches() {
        // 词边界:英文词形不得内嵌命中
        let sp = "scanner allowed for diagnostics.

【能力边界声明】
- scanner";
        let r = inspect_system_sections_with(sp, &["scanner".to_string()], &I2Lexicon::builtin());
        assert!(r.is_empty(), "无禁令=无配对(内嵌词形不伪命中): {r:?}");
    }

    #[test]
    fn english_window_adapted_for_ascii_marks() {
        // 窗口适配:ASCII 词形 24 字符——长英文禁令距 token 超过旧 12 窗仍检出
        let sp = "web_search is prohibited from being used in production clusters.

【能力边界声明】
- web_search";
        let r =
            inspect_system_sections_with(sp, &["web_search".to_string()], &I2Lexicon::builtin());
        assert_eq!(r.len(), 1, "24 字符窗口检出长距英文禁令: {r:?}");
    }

    #[test]
    fn lexicon_override_is_wholesale() {
        // 数据化:词表声明即整体覆盖(声明集外的内建词形失效)
        let custom = I2Lexicon {
            prohibition: vec!["封禁".to_string()],
            affirmation: vec!["开放".to_string()],
        };
        let sp = "封禁使用 web_search。开放调试。

【能力边界声明】
- web_search";
        let r = inspect_system_sections_with(sp, &["web_search".to_string()], &custom);
        assert_eq!(r.len(), 1, "自定义词形配对: {r:?}");
        // 内建词形在覆盖后失效
        let sp2 = "禁止使用 web_search。

【能力边界声明】
- web_search";
        assert!(inspect_system_sections_with(sp2, &["web_search".to_string()], &custom).is_empty());
    }

    #[test]
    fn multibyte_window_no_panic() {
        // 禁令词与 token 间含多字节中文——窗口裁剪须走字符边界
        let sp = "治理裁定:根据规约禁止调用 git_push 命令。\n\n## Stable Facts\n- 推荐使用 git_push 做推送";
        let r = inspect_system_sections(sp, &["git_push".to_string()]);
        assert_eq!(r.len(), 1, "{r:?}");
    }
    // ===== 语义精判(两级通路第二级) =====

    fn sample_record() -> I2ConflictRecord {
        I2ConflictRecord {
            kind: "deny_vs_capability".to_string(),
            token: "web_search".to_string(),
            section_a: "(基底段)".to_string(),
            section_b: "【能力边界声明】".to_string(),
            excerpt_a: "禁止把 web_search 用于批量抓取".to_string(),
            excerpt_b: "可用工具:web_search、file_read".to_string(),
            semantic_verdict: None,
        }
    }

    #[test]
    fn adjudication_params_deterministic() {
        // 确定性:同候选对同字节(temperature=0 固定,消息序固定)
        let a = build_adjudication_params("m1", &sample_record());
        let b = build_adjudication_params("m1", &sample_record());
        assert_eq!(a, b);
        assert_eq!(a["temperature"], serde_json::json!(0.0));
        // 候选不同→参数不同
        let mut other = sample_record();
        other.token = "file_read".to_string();
        assert_ne!(a, build_adjudication_params("m1", &other));
    }

    #[test]
    fn adjudication_parse_three_states_and_garbage() {
        // 三态直出
        for v in ["contradiction", "benign", "uncertain"] {
            let body = format!("{{\"verdict\":\"{v}\",\"rationale\":\"理由\"}}");
            let got = parse_adjudication_response(&body);
            assert_eq!(got.verdict, v);
            assert_eq!(got.rationale, "理由");
        }
        // 围栏包裹剥离
        let fenced = "```json
{\"verdict\":\"benign\",\"rationale\":\"辖域不同\"}
```";
        assert_eq!(parse_adjudication_response(fenced).verdict, "benign");
        // 前后杂文窗口提取
        let noisy = "结论如下:{\"verdict\":\"contradiction\",\"rationale\":\"真矛盾\"} 以上。";
        assert_eq!(parse_adjudication_response(noisy).verdict, "contradiction");
        // 非法 JSON/越值 verdict→uncertain
        assert_eq!(
            parse_adjudication_response("完全不是 JSON").verdict,
            "uncertain"
        );
        assert_eq!(
            parse_adjudication_response("{\"verdict\":\"maybe\",\"rationale\":\"x\"}").verdict,
            "uncertain"
        );
        // 理由截断 120 字符
        let long = "长".repeat(300);
        let body = format!("{{\"verdict\":\"benign\",\"rationale\":\"{long}\"}}");
        let got = parse_adjudication_response(&body);
        assert_eq!(got.rationale.chars().count(), 120);
    }

    #[test]
    fn adjudication_cache_key_deterministic() {
        assert_eq!(
            adjudication_cache_key(&sample_record()),
            adjudication_cache_key(&sample_record())
        );
        let mut other = sample_record();
        other.excerpt_a = "不同的禁令行".to_string();
        assert_ne!(
            adjudication_cache_key(&sample_record()),
            adjudication_cache_key(&other)
        );
    }

    #[test]
    fn record_serde_backward_compatible() {
        // 旧账面(无 semantic_verdict 字段)反序列化→None;新形态round-trip
        let old = r#"{"kind":"deny_vs_capability","token":"t","section_a":"a","section_b":"b","excerpt_a":"x","excerpt_b":"y"}"#;
        let rec: I2ConflictRecord = serde_json::from_str(old).expect("old journal line parses");
        assert!(rec.semantic_verdict.is_none());
        let with_verdict = I2ConflictRecord {
            semantic_verdict: Some(I2Verdict {
                verdict: "benign".to_string(),
                rationale: "辖域不同".to_string(),
            }),
            ..sample_record()
        };
        let ser = serde_json::to_string(&with_verdict).unwrap();
        let de: I2ConflictRecord = serde_json::from_str(&ser).unwrap();
        assert_eq!(de, with_verdict);
        // None 序列化时字段不出现(账面瘦身)
        let ser_none = serde_json::to_string(&sample_record()).unwrap();
        assert!(!ser_none.contains("semantic_verdict"));
    }

    #[tokio::test]
    async fn adjudicate_cache_hit_and_failsoft() {
        use std::sync::Mutex as SM;
        let cache: SM<std::collections::HashMap<String, I2Verdict>> =
            SM::new(std::collections::HashMap::new());
        // 计数 fetch:成功返回 benign JSON
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_c = calls.clone();
        let mut fetch = |_rec: &I2ConflictRecord| {
            let c = calls_c.clone();
            async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok::<(String, Option<crate::agent::journal::TokenRecord>), String>((
                    "{\"verdict\":\"benign\",\"rationale\":\"辖域限定,非矛盾\"}".to_string(),
                    Some(crate::agent::journal::TokenRecord {
                        prompt: 10,
                        completion: 5,
                        total: 15,
                    }),
                ))
            }
        };
        // 无 journal:成功路径 verdict 解析+缓存写入
        let out = adjudicate_candidates(vec![sample_record()], &cache, "m", &mut fetch, None).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].semantic_verdict.as_ref().unwrap().verdict, "benign");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        // 同候选再跑:缓存命中,fetch 零新增调用
        let out2 =
            adjudicate_candidates(vec![sample_record()], &cache, "m", &mut fetch, None).await;
        assert_eq!(out2[0].semantic_verdict.as_ref().unwrap().verdict, "benign");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "缓存命中零新增调用"
        );
        // 失败路径:Err→uncertain 兜底(fail-soft)
        let mut fetch_err = |_rec: &I2ConflictRecord| async move {
            Err::<(String, Option<crate::agent::journal::TokenRecord>), String>(
                "sidecar down".to_string(),
            )
        };
        let mut other = sample_record();
        other.token = "another_tool".to_string();
        let out3 = adjudicate_candidates(vec![other], &cache, "m", &mut fetch_err, None).await;
        assert_eq!(
            out3[0].semantic_verdict.as_ref().unwrap().verdict,
            "uncertain"
        );
        assert!(out3[0]
            .semantic_verdict
            .as_ref()
            .unwrap()
            .rationale
            .contains("sidecar down"));
    }

    #[tokio::test]
    async fn adjudicate_journal_token_record_on_success() {
        // sidecar token 账面:fetch 成功时 LlmCalled(purpose=i2_semantic) 落账;
        // 失败不落(事件口径=成功完成时写)
        use crate::agent::journal::{JournalWriter, TokenRecord};
        let dir = tempfile::tempdir().unwrap();
        let w = JournalWriter::open(dir.path(), "s-i2").unwrap();
        let cache: std::sync::Mutex<std::collections::HashMap<String, I2Verdict>> =
            std::sync::Mutex::new(std::collections::HashMap::new());
        let mut fetch = |_rec: &I2ConflictRecord| async move {
            Ok::<(String, Option<TokenRecord>), String>((
                "{\"verdict\":\"contradiction\",\"rationale\":\"真矛盾\"}".to_string(),
                Some(TokenRecord {
                    prompt: 10,
                    completion: 5,
                    total: 15,
                }),
            ))
        };
        let out = adjudicate_candidates(
            vec![sample_record()],
            &cache,
            "test-model",
            &mut fetch,
            Some(&w),
        )
        .await;
        assert_eq!(
            out[0].semantic_verdict.as_ref().unwrap().verdict,
            "contradiction"
        );
        let lines =
            crate::agent::journal::read_all(&JournalWriter::path_for(dir.path(), "s-i2")).unwrap();
        assert_eq!(lines.len(), 1, "恰一笔 sidecar 账");
        let ok = matches!(
            &lines[0].event,
            crate::agent::journal::JournalEvent::LlmCalled { purpose, tokens, .. }
                if purpose == "i2_semantic"
                    && tokens.as_ref().map(|t| t.total) == Some(15)
        );
        assert!(ok, "LlmCalled(i2_semantic) 含 token 账面");
    }
}
