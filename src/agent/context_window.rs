// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 上下文窗口管理 —— token 计数 + 消息裁剪
//!
//! ## G2 目标
//!
//! 防止长对话爆 context window;在接近上限时自动裁剪历史消息,
//! 保留 system + 最近若干轮(含 tool_call / tool_result 原子对)。
//!
//! ## 计数策略
//!
//! 默认 `ApproxTokenCounter`,无外部依赖:
//! - ASCII 字符:4 chars ≈ 1 token
//! - CJK 字符:1 char ≈ 1 token(中文 1 字 ≈ 1 token 是经验值)
//! - 每条消息固定开销 4 token(role 标签 + 结构分隔符)
//!
//! 精度 ±15%,对裁剪决策足够。如需高精度可后续加 `tiktoken` feature。
//!
//! ## 裁剪策略(KeepSystemKeepLast)
//!
//! 1. 拆分 system / non-system
//! 2. 保留所有 system 消息
//! 3. 从尾部往前累加 non-system,直到接近 budget
//! 4. tool_call + 紧随其后的 tool_result 视为原子对,要么都留要么都丢
//! 5. 中间插入一条 system 提示 "[earlier N messages trimmed]"

use crate::agent::translator::Message;

/// G10:trim 详细结果(含被丢弃的消息,供 summarizer 使用)
#[derive(Debug, Clone)]
pub struct TrimResult {
    /// 裁剪后的消息列表
    pub messages: Vec<Message>,
    /// 被丢弃的非 system 消息(供 ContextSummarizer 生成摘要)
    pub dropped: Vec<Message>,
}

/// Token 计数器 trait
pub trait TokenCounter: Send + Sync + std::fmt::Debug {
    /// 估算 messages 数组的总 token 数(含 role 开销 + 结尾标记)
    fn count_messages(&self, messages: &[Message]) -> usize;

    /// 估算单条消息的 token 数
    fn count_message(&self, msg: &Message) -> usize {
        self.count_messages(std::slice::from_ref(msg))
    }
}

/// 近似计数器(无外部依赖,CJK-aware)
///
/// 适用场景:开发/测试/不允许引入 tiktoken 依赖时。
/// 精度:±15%,对裁剪决策足够。
#[derive(Debug, Clone)]
pub struct ApproxTokenCounter {
    /// 每条消息的固定开销(role 标签 + 结构分隔符)
    per_message_overhead: usize,
    /// ASCII 字符→token 比例的倒数(4 = 4 chars/token)
    ascii_chars_per_token: usize,
}

impl Default for ApproxTokenCounter {
    fn default() -> Self {
        Self {
            per_message_overhead: 4, // <|im_start|>role\n ... <|im_end|>\n
            ascii_chars_per_token: 4,
        }
    }
}

impl ApproxTokenCounter {
    /// 创建默认配置的计数器
    pub fn new() -> Self {
        Self::default()
    }

    /// 估算一段文本的 token 数(CJK-aware)
    ///
    /// - CJK 字符(含全角标点、日韩文):1 char ≈ 1 token
    /// - 其他字符(ASCII / 拉丁):4 chars ≈ 1 token
    fn count_text(&self, s: &str) -> usize {
        let mut cjk = 0usize;
        let mut other = 0usize;
        for ch in s.chars() {
            if is_cjk(ch) {
                cjk += 1;
            } else {
                other += 1;
            }
        }
        let ascii_tokens = other / self.ascii_chars_per_token.max(1);
        // 余数向上取整(零碎字符也算 1 token)
        let ascii_remainder = if !other.is_multiple_of(self.ascii_chars_per_token.max(1)) {
            1
        } else {
            0
        };
        cjk + ascii_tokens + ascii_remainder
    }
}

/// 判断字符是否属于 CJK(中文/日文/韩文)或全角标点
///
/// 这些字符在 BPE 分词后通常 1 char ≈ 1 token。
fn is_cjk(ch: char) -> bool {
    let c = ch as u32;
    // CJK 统一表意文字(常用汉字)
    (0x4E00..=0x9FFF).contains(&c)
    // CJK 扩展 A
    || (0x3400..=0x4DBF).contains(&c)
    // CJK 兼容表意文字
    || (0xF900..=0xFAFF).contains(&c)
    // 平假名
    || (0x3040..=0x309F).contains(&c)
    // 片假名
    || (0x30A0..=0x30FF).contains(&c)
    // 韩文音节
    || (0xAC00..=0xD7AF).contains(&c)
    // 全角标点(CJK Symbols and Punctuation)
    || (0x3000..=0x303F).contains(&c)
    // 半角/全角形式
    || (0xFF00..=0xFFEF).contains(&c)
}

impl TokenCounter for ApproxTokenCounter {
    fn count_messages(&self, messages: &[Message]) -> usize {
        if messages.is_empty() {
            return 0;
        }
        let mut total = 0usize;
        for msg in messages {
            total += self.per_message_overhead;
            total += self.count_text(msg.content());
            if let Message::Assistant { tool_calls, .. } = msg {
                if let Some(calls) = tool_calls {
                    for tc in calls {
                        // tool_name + arguments JSON
                        total += self.count_text(&tc.name);
                        let args_str = tc.arguments.to_string();
                        total += self.count_text(&args_str);
                    }
                }
            }
            if let Message::Tool { tool_name, .. } = msg {
                total += self.count_text(tool_name);
            }
        }
        // 结尾 <|start|>assistant<|message|> 等
        total + 3
    }
}

/// 裁剪策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum TrimStrategy {
    /// 保留 system + 最近 N 条(丢弃中间),tool_call/tool_result 原子对
    #[default]
    KeepSystemKeepLast,
    /// 不裁剪(让 LLM 自己报错,仅计数用于观测)
    None,
}


/// 上下文窗口管理器
#[derive(Debug)]
pub struct ContextWindowManager {
    counter: Box<dyn TokenCounter>,
    max_tokens: usize,
    /// 预留给响应的 token 数(默认 max_tokens 的 1/4)
    reserve_for_response: usize,
    strategy: TrimStrategy,
}

impl ContextWindowManager {
    /// 创建一个 manager
    ///
    /// - `max_tokens`:模型 context window 大小(如 8192)
    /// - `reserve_for_response`:预留给响应的 token 数(如 2048),
    ///   实际可用的输入 token = `max_tokens - reserve_for_response`
    pub fn new(
        counter: Box<dyn TokenCounter>,
        max_tokens: usize,
        reserve_for_response: usize,
        strategy: TrimStrategy,
    ) -> Self {
        Self {
            counter,
            max_tokens,
            reserve_for_response,
            strategy,
        }
    }

    /// 使用默认 `ApproxTokenCounter` 构造
    pub fn with_approx_counter(
        max_tokens: usize,
        reserve_for_response: usize,
        strategy: TrimStrategy,
    ) -> Self {
        Self::new(
            Box::new(ApproxTokenCounter::new()),
            max_tokens,
            reserve_for_response,
            strategy,
        )
    }

    /// 实际可用于输入消息的 token 预算
    pub fn budget(&self) -> usize {
        self.max_tokens.saturating_sub(self.reserve_for_response)
    }

    /// 估算 messages 的 token 数
    pub fn count(&self, messages: &[Message]) -> usize {
        self.counter.count_messages(messages)
    }

    /// 裁剪 messages 使其 token 数 ≤ budget
    ///
    /// 返回 `(裁剪后的 messages, dropped_count)`。
    /// `dropped_count` 是被丢弃的非 system 消息条数(不含插入的提示)。
    pub fn trim(&self, messages: &[Message]) -> (Vec<Message>, usize) {
        let result = self.trim_detailed(messages);
        (result.messages, result.dropped.len())
    }

    /// G10:裁剪并返回详细结果(含被丢弃的消息)
    ///
    /// 与 `trim()` 逻辑相同,但把被丢弃的非 system 消息收集到 `dropped` 字段,
    /// 供 `ContextSummarizer` 生成摘要。`trim()` 内部调此方法。
    pub fn trim_detailed(&self, messages: &[Message]) -> TrimResult {
        let budget = self.budget();
        let total = self.counter.count_messages(messages);

        if total <= budget {
            return TrimResult {
                messages: messages.to_vec(),
                dropped: Vec::new(),
            };
        }

        match self.strategy {
            TrimStrategy::None => TrimResult {
                messages: messages.to_vec(),
                dropped: Vec::new(),
            },
            TrimStrategy::KeepSystemKeepLast => self.trim_keep_system_keep_last(messages, budget),
        }
    }

    /// 实现策略:保留 system + 最近若干轮(tool_call/tool_result 原子对)
    fn trim_keep_system_keep_last(&self, messages: &[Message], budget: usize) -> TrimResult {
        // 1. 拆分 system / non-system
        let mut system_msgs: Vec<Message> = Vec::new();
        let mut non_system: Vec<(usize, Message)> = Vec::new(); // (原 idx, msg)
        for (i, msg) in messages.iter().enumerate() {
            if matches!(msg, Message::System { .. }) {
                system_msgs.push(msg.clone());
            } else {
                non_system.push((i, msg.clone()));
            }
        }

        if non_system.is_empty() {
            return TrimResult {
                messages: system_msgs,
                dropped: Vec::new(),
            };
        }

        // 2. 计算 system 消息占用的 token
        let system_tokens = self.counter.count_messages(&system_msgs);
        // hint 消息约 13 token("[earlier N messages trimmed due to context window limit]")
        // buffer 按 budget 的 5% 预留,最低 5 token,应对计数误差
        let hint_overhead = 15usize;
        let buffer = (budget / 20).max(5);
        let available_for_recent = budget
            .saturating_sub(system_tokens)
            .saturating_sub(hint_overhead)
            .saturating_sub(buffer);

        if available_for_recent == 0 {
            // 极端情况:system 消息本身已占满 budget,只能保留 system
            let dropped: Vec<Message> = non_system.into_iter().map(|(_, m)| m).collect();
            let dropped_count = dropped.len();
            let mut result = system_msgs.clone();
            if dropped_count > 0 {
                result.push(Message::System {
                    content: format!(
                        "[earlier {} messages trimmed due to context window limit]",
                        dropped_count
                    ),
                });
            }
            return TrimResult {
                messages: result,
                dropped,
            };
        }

        // 3. 从尾部往前累加,tool_call/tool_result 视为原子对
        let mut kept: Vec<(usize, Message)> = Vec::new();
        let mut kept_tokens = 0usize;
        let mut i = non_system.len();
        while i > 0 {
            i -= 1;
            let (_, msg) = &non_system[i];
            // 如果是 tool_result(Message::Tool),它前面的 assistant 可能含 tool_calls,
            // 二者必须一起保留(否则 LLM 收到孤立的 tool_result 会报错)
            let mut group: Vec<(usize, Message)> = vec![non_system[i].clone()];
            if let Message::Tool { .. } = msg {
                // 向前找紧邻的 Assistant(含 tool_calls)
                if i > 0 {
                    if let Message::Assistant { tool_calls, .. } = &non_system[i - 1].1 {
                        if tool_calls.as_ref().map(|c| !c.is_empty()).unwrap_or(false) {
                            group.insert(0, non_system[i - 1].clone());
                            i -= 1; // 跳过已合并的 assistant
                        }
                    }
                }
            }
            // 估算 group 的 token
            let group_msgs: Vec<Message> = group.iter().map(|(_, m)| m.clone()).collect();
            let group_tokens = self.counter.count_messages(&group_msgs);
            if kept_tokens + group_tokens > available_for_recent && !kept.is_empty() {
                // 加上这个 group 会超预算,停止
                break;
            }
            kept_tokens += group_tokens;
            // prepend(kept 是反序累加,最后要反转)
            let mut new_kept = group;
            new_kept.extend(kept);
            kept = new_kept;
        }

        // 4. 收集被丢弃的消息(非 system 且不在 kept 中的)
        let kept_indices: std::collections::HashSet<usize> =
            kept.iter().map(|(idx, _)| *idx).collect();
        let dropped: Vec<Message> = non_system
            .iter()
            .filter(|(idx, _)| !kept_indices.contains(idx))
            .map(|(_, m)| m.clone())
            .collect();
        let dropped_count = dropped.len();

        // 5. 组装结果:system + (可选 hint) + kept(按原顺序)
        // 注意:hint 插在 system 和 kept 之间,kept 的最后一条是最近的消息
        let mut result = system_msgs.clone();
        if dropped_count > 0 {
            result.push(Message::System {
                content: format!(
                    "[earlier {} messages trimmed due to context window limit]",
                    dropped_count
                ),
            });
        }
        for (_, m) in kept {
            result.push(m);
        }

        TrimResult {
            messages: result,
            dropped,
        }
    }
}

impl Clone for ContextWindowManager {
    /// Clone 时新建 Box(因为 `Box<dyn Trait>` 不自动 Clone)
    fn clone(&self) -> Self {
        // ApproxTokenCounter 是默认且唯一实现,直接复用
        // 如果未来加更多实现,需要用 Any 或 trait_clone 模式
        Self {
            counter: Box::new(ApproxTokenCounter::new()),
            max_tokens: self.max_tokens,
            reserve_for_response: self.reserve_for_response,
            strategy: self.strategy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::translator::ToolCall;
    use serde_json::json;

    fn user(content: &str) -> Message {
        Message::User {
            content: content.to_string(),
        }
    }

    fn system(content: &str) -> Message {
        Message::System {
            content: content.to_string(),
        }
    }

    fn assistant_with_tool_call(content: &str, tool_name: &str) -> Message {
        Message::Assistant {
            content: content.to_string(),
            tool_calls: Some(vec![ToolCall {
                name: tool_name.to_string(),
                arguments: json!({"q": "test"}),
            }]),
        }
    }

    fn tool_result(content: &str, tool_name: &str) -> Message {
        Message::Tool {
            content: content.to_string(),
            tool_name: tool_name.to_string(),
        }
    }

    // ========== ApproxTokenCounter 测试 ==========

    #[test]
    fn test_count_empty() {
        let c = ApproxTokenCounter::new();
        assert_eq!(c.count_messages(&[]), 0);
    }

    #[test]
    fn test_count_ascii() {
        // 4 chars ≈ 1 token,per_message_overhead=4,结尾 +3
        // "hello" = 5 chars → 2 tokens (5/4=1 余 1,向上取整)
        let c = ApproxTokenCounter::new();
        let msgs = vec![user("hello")];
        let total = c.count_messages(&msgs);
        // 4 (overhead) + 2 (text) + 3 (tail) = 9
        assert_eq!(total, 9);
    }

    #[test]
    fn test_count_cjk_one_char_per_token() {
        // 中文 1 char ≈ 1 token
        let c = ApproxTokenCounter::new();
        let msgs = vec![user("你好世界")]; // 4 个汉字 → 4 tokens
        let total = c.count_messages(&msgs);
        // 4 (overhead) + 4 (cjk) + 3 (tail) = 11
        assert_eq!(total, 11);
    }

    #[test]
    fn test_count_mixed_cjk_ascii() {
        let c = ApproxTokenCounter::new();
        // "你好 hello" = 2 CJK + 6 ASCII (含空格)
        // CJK: 2 tokens
        // ASCII: 6/4 = 1 余 2 → 2 tokens
        // 总 text: 4 tokens
        let msgs = vec![user("你好 hello")];
        let total = c.count_messages(&msgs);
        // 4 (overhead) + 4 (text) + 3 (tail) = 11
        assert_eq!(total, 11);
    }

    #[test]
    fn test_count_assistant_with_tool_calls() {
        let c = ApproxTokenCounter::new();
        let msgs = vec![assistant_with_tool_call("calling", "search")];
        let total = c.count_messages(&msgs);
        // overhead 4 + content "calling" (7 chars → 2 tokens)
        // + tool_name "search" (6 chars → 2 tokens)
        // + args {"q":"test"} (12 chars → 3 tokens)
        // + tail 3
        // = 4 + 2 + 2 + 3 + 3 = 14
        assert_eq!(total, 14);
    }

    // ========== ContextWindowManager.trim 测试 ==========

    #[test]
    fn test_trim_no_trimming_when_under_budget() {
        let mgr = ContextWindowManager::with_approx_counter(8192, 2048, TrimStrategy::default());
        let msgs = vec![system("sys"), user("hi")];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert_eq!(dropped, 0);
        assert_eq!(trimmed.len(), 2);
    }

    #[test]
    fn test_trim_keeps_system_when_over_budget() {
        // 极小 budget,强制裁剪
        let mgr = ContextWindowManager::with_approx_counter(
            50, // 总 budget
            10, // reserve
            TrimStrategy::KeepSystemKeepLast,
        );
        // budget = 40
        // system "sys prompt long enough" ≈ overhead 4 + 6 tokens = 10
        // 每条 user "hello" ≈ 4 + 2 = 6 tokens
        let msgs = vec![
            system("sys prompt long enough"),
            user("hello"),
            user("hello"),
            user("hello"),
            user("hello"),
            user("hello"),
            user("hello"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert!(dropped > 0, "should drop some messages");
        // system 必须保留
        assert!(matches!(trimmed[0], Message::System { .. }));
        // 最后一条必须是最近的 user("hello")
        assert!(matches!(trimmed.last(), Some(Message::User { .. })));
        // 应包含 hint 消息
        let has_hint = trimmed
            .iter()
            .any(|m| m.content().contains("[earlier") && m.content().contains("messages trimmed"));
        assert!(has_hint, "should contain trim hint");
    }

    #[test]
    fn test_trim_tool_call_pair_atomic() {
        // tool_call(assistant) + tool_result(tool) 必须一起保留或一起丢
        let mgr =
            ContextWindowManager::with_approx_counter(60, 10, TrimStrategy::KeepSystemKeepLast);
        // budget = 50
        let msgs = vec![
            system("sys"),
            user("q1"),
            assistant_with_tool_call("thinking", "search"),
            tool_result("result1", "search"),
            user("q2"),
            assistant_with_tool_call("thinking2", "search"),
            tool_result("result2", "search"),
            user("q3"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        let _ = dropped;

        // 验证:如果有 tool_result,前面必有对应的 assistant(tool_calls)
        for i in 0..trimmed.len() {
            if let Message::Tool { .. } = &trimmed[i] {
                assert!(i > 0, "tool_result at index {} has no preceding message", i);
                match &trimmed[i - 1] {
                    Message::Assistant { tool_calls, .. } => {
                        assert!(
                            tool_calls.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
                            "tool_result at {} is preceded by assistant without tool_calls",
                            i
                        );
                    }
                    _ => panic!(
                        "tool_result at {} is preceded by non-assistant message: {:?}",
                        i,
                        trimmed[i - 1]
                    ),
                }
            }
        }
    }

    #[test]
    fn test_trim_strategy_none_no_trimming() {
        let mgr = ContextWindowManager::with_approx_counter(10, 5, TrimStrategy::None);
        let msgs = vec![system("sys"), user("hello world this is long")];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert_eq!(dropped, 0);
        assert_eq!(trimmed.len(), msgs.len());
    }

    #[test]
    fn test_trim_extreme_system_fills_budget() {
        // system 消息本身超 budget,所有 non-system 被丢
        let mgr = ContextWindowManager::with_approx_counter(20, 5, TrimStrategy::default());
        // budget = 15
        // system "this is a very long system prompt" ≈ 4 + 9 = 13 tokens
        // 已用 13,剩 2 给 non-system(不够任何一条)
        let msgs = vec![
            system("this is a very long system prompt"),
            user("hello"),
            user("world"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert_eq!(dropped, 2);
        // system 保留 + hint 插入
        assert!(matches!(trimmed[0], Message::System { .. }));
        // 后面应有 hint 提示
        assert!(trimmed.iter().any(|m| m.content().contains("[earlier")));
    }

    #[test]
    fn test_trim_preserves_order_of_kept_messages() {
        let mgr = ContextWindowManager::with_approx_counter(80, 20, TrimStrategy::default());
        // budget = 60
        let msgs = vec![
            system("sys"),
            user("msg1"),
            user("msg2 with more text to inflate tokens"),
            user("msg3"),
            user("msg4 with more text to inflate tokens"),
            user("msg5"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        // 验证:kept 部分的相对顺序与原数组一致
        let _ = dropped;
        let mut prev_idx = usize::MAX;
        // 找出 trimmed 中各消息在原数组的位置
        for m in &trimmed {
            if let Message::User { content } = m {
                if let Some(idx) = msgs.iter().position(|orig| {
                    if let Message::User { content: oc } = orig {
                        oc == content
                    } else {
                        false
                    }
                }) {
                    if prev_idx != usize::MAX {
                        assert!(
                            idx > prev_idx,
                            "kept messages out of order: {} after {}",
                            idx,
                            prev_idx
                        );
                    }
                    prev_idx = idx;
                }
            }
        }
    }

    #[test]
    fn test_budget_calculation() {
        let mgr = ContextWindowManager::with_approx_counter(8192, 2048, TrimStrategy::default());
        assert_eq!(mgr.budget(), 6144);
    }

    #[test]
    fn test_count_method() {
        let mgr = ContextWindowManager::with_approx_counter(8192, 2048, TrimStrategy::default());
        let msgs = vec![system("sys"), user("hello")];
        let count = mgr.count(&msgs);
        assert!(count > 0);
    }

    #[test]
    fn test_clone_preserves_config() {
        let mgr = ContextWindowManager::with_approx_counter(4096, 1024, TrimStrategy::None);
        let cloned = mgr.clone();
        assert_eq!(cloned.budget(), 3072);
        assert_eq!(cloned.strategy, TrimStrategy::None);
    }

    #[test]
    fn test_is_cjk_basic() {
        // 汉字
        assert!(is_cjk('你'));
        assert!(is_cjk('好'));
        assert!(is_cjk('世'));
        assert!(is_cjk('界'));
        // 平假名
        assert!(is_cjk('あ'));
        // 片假名
        assert!(is_cjk('ア'));
        // 韩文
        assert!(is_cjk('한'));
        // 全角标点
        assert!(is_cjk('，'));
        assert!(is_cjk('。'));
        // ASCII
        assert!(!is_cjk('a'));
        assert!(!is_cjk('Z'));
        assert!(!is_cjk(' '));
        assert!(!is_cjk('1'));
    }

    /// 集成测试:100 条消息,context_window=8192,验证 trim 后不超 budget
    #[test]
    fn test_integration_large_history_fits_budget() {
        // 用更小的 budget 触发裁剪(每条消息约 30 token,100 条 = 3000 token)
        let mgr = ContextWindowManager::with_approx_counter(1024, 256, TrimStrategy::default());
        // budget = 768
        let mut msgs = vec![system("You are a helpful assistant.")];
        // 每条 user 消息约 30 token,100 条 = 3000+ token,远超 768
        for i in 0..100 {
            msgs.push(user(&format!(
                "message number {:03} with some padding text to inflate the token count a bit more",
                i
            )));
        }
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert!(dropped > 0, "should drop some messages");
        let final_count = mgr.count(&trimmed);
        assert!(
            final_count <= 768 + 50,
            "trimmed messages {} tokens exceed budget 768",
            final_count
        );
        // system 保留
        assert!(matches!(trimmed[0], Message::System { .. }));
        // 最后一条是最近的 message number 099
        let last = trimmed.last().unwrap();
        assert!(last.content().contains("message number 099"));
    }
}
