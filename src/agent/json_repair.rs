// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! LLM 输出 JSON 容错公共件：截断修复 + max_tokens 自适应。
//!
//! 会话摘要/知识候选提取/候选巩固共用的确定性容错——LLM 输出超
//! max_tokens 时 JSON 在字符串中间被切（半途 EOF），直接解析整包失败
//! 即产物静默丢失（fail-silent 无账）。本模块提供：
//! - [`repair_truncated_json`]：对半途 EOF 的 JSON 做字符串感知的
//!   括号闭合 + 尾元素修剪，产出候选修复文本
//! - [`repair_invalid_escapes`]：字符串值内的非法转义序列字面化
//! - [`from_str_with_repair`]：解析 + 截断/非法转义修复兜底一步到位
//! - [`adaptive_max_tokens`]：按输入规模自适应 max_tokens（512-8192）

/// 摘要 max_tokens 下限（输入极小时；长会话截断缺陷修复后实际值随输入规模自适应）
///
/// 缺陷背景：固定 512 在长会话（51k token prompt）下必然截断——模型对长输入
/// 倾向更长的输出且可能夹杂格式噪音，512 上限把摘要硬截在半句。
/// 修复 = [`adaptive_max_tokens`] 按输入规模放大，512 保留为下限。
const SUMMARY_MAX_TOKENS: u64 = 512;

/// 摘要 max_tokens 自适应上限（防长输入下 512 硬截断）
const SUMMARY_MAX_TOKENS_CAP: u64 = 8192;

/// 按输入规模自适应摘要 max_tokens
///
/// - 输入 token 估算 = 字符数 / 3（中英混合粗估）
/// - max_tokens = clamp(估算值 / 4, 512, 8192)：摘要长度随输入次线性增长，
///   /4 给足 JSON 结构与格式噪音余量；下限 512 保证小会话完整表述，
///   上限 8192 封顶防成本失控
pub(crate) fn adaptive_max_tokens(input_chars: usize) -> u64 {
    let est_input_tokens = (input_chars / 3) as u64;
    (est_input_tokens / 4).clamp(SUMMARY_MAX_TOKENS, SUMMARY_MAX_TOKENS_CAP)
}

/// 截断修复：对半途 EOF 的 JSON 文本做字符串感知的括号闭合 + 尾元素修剪
///
/// 产出零到多个候选修复文本（惰性迭代，由调用方逐个尝试解析，全部经真实
/// parse 验证，不保证语义完整——截断丢失的内容不可恢复，只保住已完整的
/// 部分）。候选生成策略（按序）：
/// 1. 值字符串半途截断 → 补闭合引号后逐层闭合括号
/// 2. 尾元素修剪 → 从最后一个安全点（闭合括号之后 / 逗号之前）**向前
///    逐个**回退尝试：截断常落在末元素中途且元素内部含逗号，只回退最后
///    一个安全点会保留无法解析的残缺元素，更早的安全点才能保住已完整
///    的前缀元素
/// 3. 兜底：非字符串态直接逐层闭合（数值截断等场景）
///
/// 失败形态锚：max_tokens 半途 EOF（实测 col 1523/1905/2221 形态），
/// 修复目标是保住已完整的前缀内容，与单次 LLM 重试构成双兜底。
pub(crate) fn repair_truncated_json(text: &str) -> impl Iterator<Item = String> + '_ {
    fn close_brackets(s: &mut String, snap: &[char]) {
        for c in snap.iter().rev() {
            s.push(if *c == '{' { '}' } else { ']' });
        }
    }

    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    // 全部安全截断点（字节位置, 此刻的括号栈快照），按出现序累积
    let mut safe_points: Vec<(usize, Vec<char>)> = Vec::new();

    for (i, ch) in text.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => stack.push(ch),
            '}' | ']' => {
                stack.pop();
                safe_points.push((i + ch.len_utf8(), stack.clone()));
            }
            ',' => safe_points.push((i, stack.clone())),
            _ => {}
        }
    }

    // 候选一：字符串感知闭合（值字符串半途 EOF）
    let first = in_string.then(|| {
        let mut s = text.to_string();
        s.push('"');
        close_brackets(&mut s, &stack);
        s
    });
    // 候选二系：尾元素修剪（安全点从最后向前逐个回退）
    let prunes = safe_points.into_iter().rev().map(|(cut, snap)| {
        let mut s = text[..cut].to_string();
        close_brackets(&mut s, &snap);
        s
    });
    // 候选三：兜底直接闭合（非字符串态；含数值截断场景）
    let last = (!stack.is_empty() && !in_string).then(|| {
        let mut s = text.to_string();
        close_brackets(&mut s, &stack);
        s
    });

    first.into_iter().chain(prunes).chain(last)
}

/// 解析 + 截断/非法转义修复兜底。候选顺序按内容保留度从高到低：
/// 1. 原文直接解析（零修复）
/// 2. 非法转义字面化后直接解析（无损修复：全部内容保留）
/// 3. 非法转义字面化后的截断候选（末尾孤立 `\` 等形态须先字面化才能闭合）
/// 4. 原文截断候选（有损修复：保已完整前缀）
///
/// 任一解析成功即采用（`repaired = true`）；全部失败返回原始解析错误
/// （由调用方决定重试/放弃路径，行为与无修复时一致）。
pub(crate) fn from_str_with_repair<T: serde::de::DeserializeOwned>(
    text: &str,
) -> Result<(T, bool), serde_json::Error> {
    match serde_json::from_str::<T>(text) {
        Ok(v) => Ok((v, false)),
        Err(first) => {
            // 非法转义字面化（无损）：字符串值内 `\X`（X 非合法转义字符）
            // 按“模型想输出字面反斜杠”解释改写为 `\\X`
            let sanitized = repair_invalid_escapes(text);
            if let Some(s) = sanitized.as_deref() {
                if let Ok(v) = serde_json::from_str::<T>(s) {
                    return Ok((v, true));
                }
            }
            for cand in sanitized
                .as_deref()
                .into_iter()
                .flat_map(repair_truncated_json)
            {
                if let Ok(v) = serde_json::from_str::<T>(&cand) {
                    return Ok((v, true));
                }
            }
            // 原文截断候选（有损，最后兜底）
            for cand in repair_truncated_json(text) {
                if let Ok(v) = serde_json::from_str::<T>(&cand) {
                    return Ok((v, true));
                }
            }
            Err(first)
        }
    }
}

/// 非法转义修复：字符串值内出现 JSON 不允许的转义形态（如 `\资`、`\'`，
/// 即反斜杠后跟非合法转义字符；或字符串末尾孤立 `\`）时 serde 拒收整包——
/// 把字符串内的非法 `\X` 字面化为 `\\X`（按“模型想输出字面反斜杠”解释，
/// 最贴近原意）；合法转义（`\" \\ \/ \b \f \n \r \t \uXXXX`）与字符串外
/// 内容原样保留。仅在确有非法转义时返回 `Some`（无改动返回 `None`，
/// 调用方免无效二次解析）。
///
/// 已知边界：`\u` 后非四位十六进制的形态不在修复面（原样透传，由调用方
/// 的截断/重试链兜底）。
pub(crate) fn repair_invalid_escapes(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escape = false;
    let mut fixed = false;
    for ch in text.chars() {
        if escape {
            escape = false;
            let valid = matches!(ch, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u');
            if valid {
                out.push('\\');
                out.push(ch);
            } else {
                // 非法 \X → 字面化 \\X
                out.push_str("\\\\");
                out.push(ch);
                fixed = true;
            }
            continue;
        }
        if in_string {
            match ch {
                '\\' => escape = true,
                '"' => {
                    in_string = false;
                    out.push(ch);
                }
                _ => out.push(ch),
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
        }
        out.push(ch);
    }
    // 字符串内末尾孤立反斜杠：字面化（截断修复链随后补闭合）
    if escape && in_string {
        out.push_str("\\\\");
        fixed = true;
    }
    fixed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========== adaptive_max_tokens 测试 ==========

    #[test]
    fn test_adaptive_max_tokens_small_input_floors_at_512() {
        // 小输入（甚至 0 字符）→ 下限 512
        assert_eq!(adaptive_max_tokens(0), 512);
        assert_eq!(adaptive_max_tokens(3000), 512); // est 1000 tok / 4 = 250 → 512
    }

    #[test]
    fn test_adaptive_max_tokens_scales_with_input() {
        // 51k token 级输入（长会话实测场景）：~153k 字符 → est 51000 /4 = 12750 → 封顶 8192
        assert_eq!(adaptive_max_tokens(153_000), 8192);
        // 中等输入：60k 字符 → est 20000 /4 = 5000
        assert_eq!(adaptive_max_tokens(60_000), 5000);
    }

    #[test]
    fn test_adaptive_max_tokens_caps_at_8192() {
        // 超大输入 → 上限 8192（防成本失控）
        assert_eq!(adaptive_max_tokens(10_000_000), 8192);
    }

    // ========== repair_truncated_json / from_str_with_repair 测试 ==========

    #[derive(Debug, serde::Deserialize)]
    struct SampleOut {
        summary: String,
    }

    #[test]
    fn repair_truncated_json_mid_string_value() {
        // 值字符串半途 EOF：补引号 + 闭合括号后可解析，保住已完整键值
        let cands: Vec<String> = repair_truncated_json(r#"{"summary":"部分摘"#).collect();
        let ok = cands
            .iter()
            .find_map(|c| serde_json::from_str::<SampleOut>(c).ok());
        assert!(ok.is_some(), "candidates: {cands:?}");
        assert_eq!(ok.unwrap().summary, "部分摘");
    }

    #[test]
    fn repair_truncated_json_prunes_partial_array_element() {
        // 数组末元素中途截断（元素内部含逗号 + 值字符串半途 EOF）：
        // 只回退最后一个安全点会保留无法解析的残缺元素——向前回退到
        // 元素边界才能保住已完整的前缀元素
        #[derive(Debug, serde::Deserialize)]
        struct Elem {
            name: String,
            note: String,
        }
        #[derive(Debug, serde::Deserialize)]
        struct Envelope {
            items: Vec<Elem>,
        }
        let text = r#"{"items":[{"name":"完整元素","note":"正文"},{"name":"部分截"#;
        let cands: Vec<String> = repair_truncated_json(text).collect();
        let ok = cands
            .iter()
            .find_map(|c| serde_json::from_str::<Envelope>(c).ok());
        assert!(ok.is_some(), "candidates: {cands:?}");
        assert_eq!(ok.as_ref().unwrap().items.len(), 1);
        assert_eq!(ok.as_ref().unwrap().items[0].name, "完整元素");
        assert_eq!(ok.as_ref().unwrap().items[0].note, "正文");
    }

    #[test]
    fn repair_truncated_json_no_candidates_for_garbage() {
        // 非 JSON 文本：无候选（调用方走 LLM 重试兜底）
        let cands: Vec<String> = repair_truncated_json("not json at all").collect();
        assert!(cands.is_empty());
    }

    #[test]
    fn from_str_with_repair_returns_flag_on_repair() {
        // 截断文本 → 修复解析成功且 repaired=true
        let (out, repaired) = from_str_with_repair::<SampleOut>(r#"{"summary":"半途"#).unwrap();
        assert!(repaired);
        assert_eq!(out.summary, "半途");
    }

    #[test]
    fn from_str_with_repair_complete_text_no_repair() {
        // 完整文本直接解析，repaired=false（零行为变化）
        let (out, repaired) = from_str_with_repair::<SampleOut>(r#"{"summary":"全"}"#).unwrap();
        assert!(!repaired);
        assert_eq!(out.summary, "全");
    }

    #[test]
    fn from_str_with_repair_garbage_keeps_original_error() {
        // 非 JSON 文本：修复无候选，返回原始错误
        assert!(from_str_with_repair::<SampleOut>("not json at all").is_err());
    }

    // ========== 非法转义修复测试 ==========

    #[test]
    fn repair_invalid_escapes_literalizes_in_string_bad_escape() {
        // 字符串值内非法转义（反斜杠后跟非合法转义字符）：字面化为 \\X，
        // 字符串外内容与合法转义原样保留
        let text = r#"{"summary":"路径 D:\资料 ok"}"#;
        let out = repair_invalid_escapes(text).expect("invalid escape present");
        assert_eq!(out, r#"{"summary":"路径 D:\\资料 ok"}"#);
    }

    #[test]
    fn repair_invalid_escapes_returns_none_when_clean() {
        // 无非法转义（全部合法转义形态/无反斜杠）：None——调用方免二次解析
        assert!(repair_invalid_escapes(r#"{"a":"\n\t\"\\ ok"}"#).is_none());
        assert!(repair_invalid_escapes("no backslash at all").is_none());
        assert!(repair_invalid_escapes(r#"{"a":"b"}"#).is_none());
    }

    #[test]
    fn from_str_with_repair_recovers_invalid_escape() {
        // 非法转义形态（非截断）：转义字面化救回，全内容保留
        let (out, repaired) =
            from_str_with_repair::<SampleOut>(r#"{"summary":"路径 D:\资料"}"#).unwrap();
        assert!(repaired);
        assert_eq!(out.summary, r"路径 D:\资料");
    }
}
