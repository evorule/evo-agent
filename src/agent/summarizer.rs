// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G10:记忆压缩/摘要 —— Context Window 裁剪后用 LLM 生成摘要,替代简单丢弃
//!
//! ## 设计动机
//!
//! `ContextWindowManager::trim_detailed()` 裁剪历史消息时,默认插入
//! `[earlier N messages trimmed]` 占位提示。这对 LLM 来说信息量为零 —— 它
//! 完全不知道之前聊了什么。
//!
//! G10 的做法:把被裁剪的消息送给一个**摘要 LLM**,生成一段简洁摘要,
//! 替换占位提示。这样 LLM 虽然看不到原始消息,但能从摘要中恢复关键上下文。
//!
//! ## Q9 策略选择:Strategy B(阈值触发)
//!
//! 不每次裁剪都调 LLM(太贵),设一个阈值 `summary_threshold`(默认 5):
//! - 裁剪消息数 < 阈值 → 不调 LLM,保留原 `[earlier N messages trimmed]` 提示
//! - 裁剪消息数 ≥ 阈值 → 调 LLM 生成摘要
//!
//! 理由:丢弃 1-2 条消息时 LLM 通常不受影响;丢弃 5+ 条才值得花一次 API 调用
//! 生成摘要。
//!
//! ## 摘要模型
//!
//! 摘要可以用与主对话不同的(更便宜的)模型,通过 `summary_model` 配置。
//! 如果未配置,fallback 到 `LlmHandler` 的 `default_model`。

use evorule_tcb::JsonValue;
use tracing::warn;

use crate::agent::audited_llm::AuditedLlm;
use crate::agent::translator::{LlmResponse, Message};
use crate::io_handler::IoHandler;
use crate::io_handlers::LlmHandler;
use crate::json_convert::serde_to_tcb;

/// G10:摘要触发的最小裁剪消息数(Q9 Strategy B,默认 5)
pub const DEFAULT_SUMMARY_THRESHOLD: usize = 5;

/// G10:摘要最大 token 数(限制摘要长度,避免摘要本身占用过多 context)
const SUMMARY_MAX_TOKENS: u64 = 512;

/// C1:整会话摘要 + 稳定事实 LLM 输出结构
///
/// `summarize_session()` 让 LLM 一次调用同时产出摘要和稳定事实列表，
/// 避免对同一段对话发两次 LLM 请求。
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SessionSummaryOut {
    /// 整会话摘要文本
    pub summary: String,
    /// 稳定事实列表（用户偏好、决策、约束等跨会话信息）
    #[serde(default)]
    pub stable_facts: Vec<StableFactOut>,
}

/// C1:单条稳定事实 LLM 输出结构
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct StableFactOut {
    /// 事实 key（如 "preferred_language"、"timezone"）
    pub key: String,
    /// 事实 value（如 "zh-CN"、"Asia/Shanghai"）
    pub value: String,
    /// 置信度 0.0-1.0（LLM 自评，缺失时默认 0.0）
    #[serde(default)]
    pub confidence: f32,
}

/// G10:摘要系统提示(中文,引导 LLM 生成结构化摘要)
const DEFAULT_SUMMARY_PROMPT: &str = "\
你是对话摘要助手。请将以下对话历史压缩为简洁的摘要,保留:\n\
1. 用户的意图和核心请求\n\
2. 关键信息、实体和数据\n\
3. 已做出的决定和结论\n\
4. 尚未解决的问题或待办事项\n\n\
要求:\n\
- 用中文输出,不超过 300 字\n\
- 不要编造对话中不存在的信息\n\
- 不要包含寒暄、客套等无关内容\n\
- 用要点格式(1. 2. 3.)组织,便于快速阅读";

/// G10:上下文摘要器 —— 裁剪消息后用 LLM 生成摘要
///
/// 由 `AgentRunner::from_definition` 在 `summary_model` 配置时自动构造。
/// 如果未配置 `summary_model`,runner 不持有 summarizer,裁剪时只保留原 hint。
///
/// # 工作流
///
/// ```text
/// trim_detailed() → TrimResult { messages, dropped }
///                        │
///                        ▼
///              dropped.len() >= threshold?
///                   │           │
///                  否           是
///                   │           │
///                   ▼           ▼
///            保留原 hint    summarize_dropped(dropped)
///                               │
///                               ▼
///                      替换 hint 为 LLM 生成的摘要
/// ```
#[derive(Debug, Clone)]
pub struct ContextSummarizer {
    /// 摘要用的 LLM handler(从主 handler clone 而来)
    llm: LlmHandler,
    /// P2-V3 结构性修复：审计链执行器（None = 直连，仅供测试/独立用途）
    ///
    /// 挂载后所有摘要类 LLM 调用经一次性 sidecar 会话走完整
    /// call_external 审计协议（prompt/response 均为事实）；
    /// 生产构造点 `AgentRunner::from_definition` 必挂。
    audited: Option<AuditedLlm>,
    /// 摘要模型名称(None = fallback 到 llm.default_model)
    summary_model: Option<String>,
    /// 摘要系统提示
    summary_prompt: String,
    /// Q9 Strategy B:触发摘要的最小裁剪消息数
    summary_threshold: usize,
}

impl ContextSummarizer {
    /// 创建摘要器
    ///
    /// - `llm`:从主 LlmHandler clone 而来(共享配置/API key,独立调用)
    /// - `summary_model`:摘要模型名称,None 时用 llm 的 default_model
    pub fn new(llm: LlmHandler, summary_model: Option<String>) -> Self {
        Self {
            llm,
            audited: None,
            summary_model,
            summary_prompt: DEFAULT_SUMMARY_PROMPT.to_string(),
            summary_threshold: DEFAULT_SUMMARY_THRESHOLD,
        }
    }

    /// 挂载审计链执行器（P2-V3 结构性修复）
    ///
    /// 挂载后 LLM 调用经 evorule 审计链（sidecar 会话），
    /// 未挂载时保持直连（旁路计数器留痕）。
    pub fn with_auditor(mut self, audited: AuditedLlm) -> Self {
        self.audited = Some(audited);
        self
    }

    /// 自定义摘要阈值(Q9 Strategy B,测试用)
    pub fn with_threshold(mut self, threshold: usize) -> Self {
        self.summary_threshold = threshold;
        self
    }

    /// 自定义摘要系统提示(测试用)
    pub fn with_prompt(mut self, prompt: &str) -> Self {
        self.summary_prompt = prompt.to_string();
        self
    }

    /// 当前摘要阈值(只读访问)
    pub fn threshold(&self) -> usize {
        self.summary_threshold
    }

    /// LLM 调用分流：有审计执行器走 sidecar 审计链，否则直连 + 旁路计数
    ///
    /// P2-V3 结构性修复后，生产路径（from_definition 构造）恒走审计分支；
    /// 直连分支仅存在于未挂载 auditor 的场景（单元测试/独立使用），
    /// 属显式配置而非静默兜底。
    async fn call_llm(&self, purpose: &str, params: &JsonValue) -> Result<JsonValue, String> {
        match &self.audited {
            Some(audited) => audited.execute(purpose, params).await,
            None => {
                // P2-V3 止血指标：此调用不经审计链，计数留痕
                crate::metrics::bypass_audit(purpose);
                self.llm.execute(params).await
            }
        }
    }

    /// 摘要模型名称(只读访问)
    pub fn summary_model(&self) -> Option<&str> {
        self.summary_model.as_deref()
    }

    /// G10:对被裁剪的消息生成摘要
    ///
    /// # 返回值
    ///
    /// - `Ok("")`:被裁剪消息为空,或低于阈值(Q9 Strategy B),不生成摘要
    /// - `Ok(summary)`:摘要生成成功,格式为 `[earlier conversation summary]\n{内容}`
    /// - `Err(e)`:LLM 调用或解析失败,调用方应 fallback 到原 hint
    ///
    /// # 参数
    ///
    /// - `dropped`:`trim_detailed()` 返回的被裁剪消息列表
    pub async fn summarize_dropped(&self, dropped: &[Message]) -> Result<String, String> {
        // 空列表:无需摘要
        if dropped.is_empty() {
            return Ok(String::new());
        }

        // Q9 Strategy B:低于阈值不调 LLM(避免小裁剪浪费 token)
        if dropped.len() < self.summary_threshold {
            tracing::debug!(
                dropped = dropped.len(),
                threshold = self.summary_threshold,
                "G10: dropped below threshold, skipping summary"
            );
            return Ok(String::new());
        }

        tracing::debug!(
            dropped = dropped.len(),
            model = ?self.summary_model,
            "G10: generating summary for dropped messages"
        );

        // 构造 messages 数组:system prompt + dropped messages
        // Message 实现了 Serialize(tag = "role"),直接序列化为 {role, content, ...}
        let mut messages_vec: Vec<serde_json::Value> = Vec::with_capacity(dropped.len() + 1);
        messages_vec.push(serde_json::json!({
            "role": "system",
            "content": self.summary_prompt,
        }));
        for msg in dropped {
            let serialized = serde_json::to_value(msg)
                .map_err(|e| format!("serialize dropped message: {}", e))?;
            messages_vec.push(serialized);
        }
        let messages_json = serde_json::Value::Array(messages_vec);

        // 构造 LLM 调用参数
        // 用 serde_json::Value 构造再转 JsonValue,确保类型正确
        // (temperature 必须是 number,不是 string)
        let mut params_map = serde_json::Map::new();
        if let Some(model) = &self.summary_model {
            params_map.insert(
                "model".to_string(),
                serde_json::Value::String(model.clone()),
            );
        }
        params_map.insert(
            "temperature".to_string(),
            serde_json::json!(0.0), // temperature=0 保证摘要确定性
        );
        params_map.insert(
            "max_tokens".to_string(),
            serde_json::json!(SUMMARY_MAX_TOKENS),
        );
        params_map.insert("messages".to_string(), messages_json);
        let params_json = serde_json::Value::Object(params_map);
        let params = serde_to_tcb(&params_json);

        // 调用 LLM（经审计链或直连，见 call_llm 分流说明）
        let result = self.call_llm("summarize", &params).await?;

        // 解析响应为 LlmResponse
        let response: LlmResponse = serde_json::from_str(&result.to_string())
            .map_err(|e| format!("parse summary LLM response: {}", e))?;

        let summary = response.content.trim();
        if summary.is_empty() {
            warn!("G10: LLM returned empty summary, keeping original hint");
            return Ok(String::new());
        }

        Ok(format!("[earlier conversation summary]\n{}", summary))
    }

    /// C1:整会话摘要 + 稳定事实（一次调用，返回结构化 JSON）
    ///
    /// 与 `summarize_dropped` 的区别：
    /// - `summarize_dropped` 是上下文窗口裁剪时的即时压缩（输入是 Message 列表，输出纯文本）
    /// - `summarize_session` 是会话结束时的整段沉淀（输入是拼接后的对话文本，输出结构化 JSON）
    /// - `summarize_session` 额外提取稳定事实（用户偏好、决策、约束），供跨会话共享
    ///
    /// # 返回值
    ///
    /// - `Ok(out)`:摘要 + 稳定事实（stable_facts 可能为空）
    /// - `Err(e)`:LLM 调用或 JSON 解析失败，调用方应 best-effort 跳过
    ///
    /// # 参数
    ///
    /// - `conversation`:拼接后的对话纯文本（由 `sediment::conversation_text()` 生成）
    pub async fn summarize_session(&self, conversation: &str) -> Result<SessionSummaryOut, String> {
        let prompt = format!(
            "Summarize the following conversation and extract stable facts \
             (user preferences, decisions, constraints).\n\
             Output JSON: {{\"summary\": \"...\", \"stable_facts\": \
             [{{\"key\": \"...\", \"value\": \"...\", \"confidence\": 0.9}}]}}\n\n\
             Conversation:\n{}",
            conversation
        );

        let mut messages_vec: Vec<serde_json::Value> = Vec::with_capacity(2);
        messages_vec.push(serde_json::json!({
            "role": "system",
            "content": "你是对话摘要助手。请总结对话并提取稳定事实（用户偏好、决策、约束）。只输出 JSON，不要输出其他内容。",
        }));
        messages_vec.push(serde_json::json!({
            "role": "user",
            "content": prompt,
        }));

        let mut params_map = serde_json::Map::new();
        if let Some(model) = &self.summary_model {
            params_map.insert(
                "model".to_string(),
                serde_json::Value::String(model.clone()),
            );
        }
        params_map.insert(
            "temperature".to_string(),
            serde_json::json!(0.0), // temperature=0 保证最大确定性
        );
        params_map.insert(
            "max_tokens".to_string(),
            serde_json::json!(SUMMARY_MAX_TOKENS),
        );
        params_map.insert(
            "messages".to_string(),
            serde_json::Value::Array(messages_vec),
        );
        let params_json = serde_json::Value::Object(params_map);
        let params = serde_to_tcb(&params_json);

        // 调用 LLM（经审计链或直连，见 call_llm 分流说明）
        let result = self.call_llm("session_summary", &params).await?;

        // 解析响应为 LlmResponse
        let response: LlmResponse = serde_json::from_str(&result.to_string())
            .map_err(|e| format!("parse session summary LLM response: {}", e))?;

        // 从可能包含 markdown 代码块的文本中提取 JSON
        let json_str = extract_json_from_text(&response.content);
        let out: SessionSummaryOut = serde_json::from_str(&json_str).map_err(|e| {
            format!(
                "parse session summary JSON: {} (raw: {})",
                e, response.content
            )
        })?;

        Ok(out)
    }

    /// C4 第三级:把多条旧摘要合并为一条 rollup 摘要
    ///
    /// 当共享空间的 L1 会话摘要数量超过 `summary_rollup_threshold` 时，
    /// `sediment::rollup_old_summaries` 取最旧的若干条调用本方法合并为一条，
    /// 减少召回时的注入条数和 token 占用。
    ///
    /// # 返回值
    ///
    /// - `Ok("")`:输入为空
    /// - `Ok(s)`:合并后的摘要文本（单条输入时原样返回）
    /// - `Err(e)`:LLM 调用或解析失败
    pub async fn rollup_summaries(&self, old_summaries: &[String]) -> Result<String, String> {
        if old_summaries.is_empty() {
            return Ok(String::new());
        }
        if old_summaries.len() == 1 {
            return Ok(old_summaries[0].clone());
        }

        let combined = old_summaries.join("\n---\n");
        let prompt = format!(
            "You are a summarization assistant. Combine the following session summaries into a single concise summary. \
            Preserve key facts, decisions, and preferences. Output only the summary text.\n\n\
            Summaries to combine:\n{}",
            combined
        );

        let mut messages_vec: Vec<serde_json::Value> = Vec::with_capacity(2);
        messages_vec.push(serde_json::json!({
            "role": "system",
            "content": "You are a summarization assistant.",
        }));
        messages_vec.push(serde_json::json!({
            "role": "user",
            "content": prompt,
        }));

        let mut params_map = serde_json::Map::new();
        if let Some(model) = &self.summary_model {
            params_map.insert(
                "model".to_string(),
                serde_json::Value::String(model.clone()),
            );
        }
        params_map.insert("temperature".to_string(), serde_json::json!(0.0));
        params_map.insert(
            "max_tokens".to_string(),
            serde_json::json!(SUMMARY_MAX_TOKENS),
        );
        params_map.insert(
            "messages".to_string(),
            serde_json::Value::Array(messages_vec),
        );
        let params_json = serde_json::Value::Object(params_map);
        let params = serde_to_tcb(&params_json);

        // 调用 LLM（经审计链或直连，见 call_llm 分流说明）
        let result = self.call_llm("rollup", &params).await?;
        let response: LlmResponse = serde_json::from_str(&result.to_string())
            .map_err(|e| format!("parse rollup summary LLM response: {}", e))?;

        Ok(response.content)
    }
}

/// C1:从可能包含 markdown 代码块的文本中提取 JSON
///
/// LLM 输出经常被包裹在 ```json ... ``` 代码块中，此函数尝试多种策略
/// 提取纯 JSON 文本：
/// 1. 直接以 `{` 开头 → 原样返回
/// 2. 包含 ```json ... ``` → 提取代码块内容
/// 3. 包含 ``` ... ``` → 提取代码块内容（跳过语言标识）
/// 4. 包含 `{` ... `}` → 截取第一个到最后一个大括号之间的内容
fn extract_json_from_text(text: &str) -> String {
    let trimmed = text.trim();

    // 尝试直接解析
    if trimmed.starts_with('{') {
        return trimmed.to_string();
    }

    // 尝试从 ```json ... ``` 中提取
    if let Some(start) = trimmed.find("```json") {
        let after_json = &trimmed[start + 7..];
        if let Some(end) = after_json.find("```") {
            return after_json[..end].trim().to_string();
        }
    }

    // 尝试从 ``` ... ``` 中提取
    if let Some(start) = trimmed.find("```") {
        let after_code = &trimmed[start + 3..];
        // 跳过语言标识(如 json)
        let after_lang = if after_code.starts_with("json") {
            &after_code[4..]
        } else {
            after_code
        };
        if let Some(end) = after_lang.find("```") {
            return after_lang[..end].trim().to_string();
        }
    }

    // 尝试找到第一个 { 和最后一个 }
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            return trimmed[start..=end].to_string();
        }
    }

    trimmed.to_string()
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

    fn assistant(content: &str) -> Message {
        Message::Assistant {
            content: content.to_string(),
            tool_calls: None,
        }
    }

    fn system(content: &str) -> Message {
        Message::System {
            content: content.to_string(),
        }
    }

    fn make_dropped(n: usize) -> Vec<Message> {
        (0..n).map(|i| user(&format!("message {}", i))).collect()
    }

    // ========== 基础结构测试 ==========

    #[test]
    fn test_summarizer_new_with_model() {
        let llm = LlmHandler::mock("summary content");
        let s = ContextSummarizer::new(llm, Some("gpt-4o-mini".to_string()));
        assert_eq!(s.summary_model(), Some("gpt-4o-mini"));
        assert_eq!(s.threshold(), DEFAULT_SUMMARY_THRESHOLD);
    }

    #[test]
    fn test_summarizer_new_without_model() {
        let llm = LlmHandler::mock("summary content");
        let s = ContextSummarizer::new(llm, None);
        assert_eq!(s.summary_model(), None);
        assert_eq!(s.threshold(), DEFAULT_SUMMARY_THRESHOLD);
    }

    #[test]
    fn test_summarizer_with_threshold() {
        let llm = LlmHandler::mock("summary");
        let s = ContextSummarizer::new(llm, None).with_threshold(3);
        assert_eq!(s.threshold(), 3);
    }

    #[test]
    fn test_summarizer_with_custom_prompt() {
        let llm = LlmHandler::mock("summary");
        let s = ContextSummarizer::new(llm, None).with_prompt("custom prompt");
        assert_eq!(s.summary_prompt, "custom prompt");
    }

    #[test]
    fn test_default_threshold_is_5() {
        assert_eq!(DEFAULT_SUMMARY_THRESHOLD, 5);
    }

    // ========== summarize_dropped 测试 ==========

    #[tokio::test]
    async fn test_summarize_empty_dropped_returns_empty() {
        let llm = LlmHandler::mock("should not be called");
        let s = ContextSummarizer::new(llm, None);
        let result = s.summarize_dropped(&[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_summarize_below_threshold_returns_empty() {
        // 阈值 5,只丢弃 3 条 → 不调 LLM
        let llm = LlmHandler::mock("should not be called");
        let s = ContextSummarizer::new(llm, None); // threshold = 5
        let dropped = make_dropped(3);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.is_empty(), "below threshold should return empty");
    }

    #[tokio::test]
    async fn test_summarize_at_threshold_calls_llm() {
        // 阈值 5,丢弃正好 5 条 → 调 LLM
        let llm = LlmHandler::mock("这是对话摘要");
        let s = ContextSummarizer::new(llm, None);
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("[earlier conversation summary]"));
        assert!(result.contains("这是对话摘要"));
    }

    #[tokio::test]
    async fn test_summarize_above_threshold_calls_llm() {
        let llm = LlmHandler::mock("摘要内容");
        let s = ContextSummarizer::new(llm, None);
        let dropped = make_dropped(10);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("[earlier conversation summary]"));
        assert!(result.contains("摘要内容"));
    }

    #[tokio::test]
    async fn test_summarize_with_custom_threshold() {
        // 阈值设为 2,丢弃 3 条 → 调 LLM
        let llm = LlmHandler::mock("short summary");
        let s = ContextSummarizer::new(llm, None).with_threshold(2);
        let dropped = make_dropped(3);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn test_summarize_with_custom_threshold_below() {
        // 阈值设为 10,丢弃 5 条 → 不调 LLM
        let llm = LlmHandler::mock("should not be called");
        let s = ContextSummarizer::new(llm, None).with_threshold(10);
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_summarize_result_format() {
        let llm = LlmHandler::mock("用户讨论了天气和行程安排");
        let s = ContextSummarizer::new(llm, Some("summary-model".to_string()));
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(
            result.starts_with("[earlier conversation summary]\n"),
            "result should start with summary header, got: {}",
            result
        );
    }

    #[tokio::test]
    async fn test_summarize_empty_llm_response_returns_empty() {
        // LLM 返回空内容 → 返回空字符串(fallback 到原 hint)
        let llm = LlmHandler::mock("");
        let s = ContextSummarizer::new(llm, None);
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.is_empty(), "empty LLM response should return empty");
    }

    #[tokio::test]
    async fn test_summarize_whitespace_only_llm_response_returns_empty() {
        let llm = LlmHandler::mock("   \n  \t  ");
        let s = ContextSummarizer::new(llm, None);
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(
            result.is_empty(),
            "whitespace-only response should return empty after trim"
        );
    }

    #[tokio::test]
    async fn test_summarize_with_mixed_message_types() {
        // 混合 System/User/Assistant/Tool 消息
        let llm = LlmHandler::mock("混合消息摘要");
        let s = ContextSummarizer::new(llm, None);
        let dropped = vec![
            user("你好"),
            assistant("你好!有什么可以帮你的?"),
            Message::Tool {
                content: "搜索结果: 今天晴天".to_string(),
                tool_name: "search".to_string(),
            },
            assistant("今天天气不错"),
            user("那我们去公园吧"),
        ];
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("混合消息摘要"));
    }

    #[tokio::test]
    async fn test_summarize_with_assistant_tool_calls() {
        // Assistant 消息带 tool_calls
        let llm = LlmHandler::mock("工具调用摘要");
        let s = ContextSummarizer::new(llm, None);
        let dropped = vec![
            user("帮我查天气"),
            Message::Assistant {
                content: "好的,我来查".to_string(),
                tool_calls: Some(vec![ToolCall {
                    name: "search".to_string(),
                    arguments: json!({"q": "北京天气"}),
                }]),
            },
            Message::Tool {
                content: "北京今天 25°C 晴".to_string(),
                tool_name: "search".to_string(),
            },
            assistant("北京今天 25 度,晴天"),
            user("谢谢"),
        ];
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("工具调用摘要"));
    }

    #[tokio::test]
    async fn test_summarize_threshold_boundary() {
        // 边界测试:threshold=5
        // 4 条 → 不调
        let llm = LlmHandler::mock("summary");
        let s = ContextSummarizer::new(llm, None); // threshold=5
        assert!(s
            .summarize_dropped(&make_dropped(4))
            .await
            .unwrap()
            .is_empty());
        // 5 条 → 调
        let llm = LlmHandler::mock("summary");
        let s = ContextSummarizer::new(llm, None);
        assert!(!s
            .summarize_dropped(&make_dropped(5))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn test_summarize_threshold_zero_always_calls() {
        // threshold=0 → 即使 1 条也调 LLM
        let llm = LlmHandler::mock("single message summary");
        let s = ContextSummarizer::new(llm, None).with_threshold(0);
        let dropped = make_dropped(1);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(!result.is_empty(), "threshold=0 should always call LLM");
    }

    #[tokio::test]
    async fn test_summarize_with_summary_model_configured() {
        // 配置 summary_model 时,params 中应包含 model 字段
        // mock 不关心 params,只验证流程通畅
        let llm = LlmHandler::mock("model-specific summary");
        let s = ContextSummarizer::new(llm, Some("gpt-4o-mini".to_string()));
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("model-specific summary"));
        assert_eq!(s.summary_model(), Some("gpt-4o-mini"));
    }

    #[tokio::test]
    async fn test_summarize_dropped_preserves_order() {
        // 验证 dropped 消息按原顺序传给 LLM(mock 不验证,但确保不 panic)
        let llm = LlmHandler::mock("ordered summary");
        let s = ContextSummarizer::new(llm, None);
        let dropped = vec![
            user("第一条"),
            user("第二条"),
            user("第三条"),
            user("第四条"),
            user("第五条"),
        ];
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("ordered summary"));
    }

    #[test]
    fn test_summarizer_clone() {
        let llm = LlmHandler::mock("test");
        let s = ContextSummarizer::new(llm, Some("model".to_string())).with_threshold(3);
        let s2 = s.clone();
        assert_eq!(s.threshold(), s2.threshold());
        assert_eq!(s.summary_model(), s2.summary_model());
    }

    // ========== C1: summarize_session 测试 ==========

    #[tokio::test]
    async fn test_summarize_session_returns_structured_json() {
        let mock_json = r#"{"summary":"用户讨论了Rust学习","stable_facts":[{"key":"language","value":"Rust","confidence":0.95}]}"#;
        let llm = LlmHandler::mock(mock_json);
        let s = ContextSummarizer::new(llm, None);
        let result = s
            .summarize_session("User: 我想学Rust\nAssistant: 好的")
            .await
            .unwrap();
        assert_eq!(result.summary, "用户讨论了Rust学习");
        assert_eq!(result.stable_facts.len(), 1);
        assert_eq!(result.stable_facts[0].key, "language");
        assert_eq!(result.stable_facts[0].value, "Rust");
        assert!((result.stable_facts[0].confidence - 0.95).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_summarize_session_with_markdown_code_block() {
        // LLM 输出被包裹在 ```json ... ``` 中
        let mock_response = "```json\n{\"summary\":\"摘要\",\"stable_facts\":[]}\n```";
        let llm = LlmHandler::mock(mock_response);
        let s = ContextSummarizer::new(llm, None);
        let result = s.summarize_session("User: hi").await.unwrap();
        assert_eq!(result.summary, "摘要");
        assert!(result.stable_facts.is_empty());
    }

    #[tokio::test]
    async fn test_summarize_session_no_stable_facts_field() {
        // stable_facts 字段缺失时，serde(default) 应填充空 Vec
        let mock_json = r#"{"summary":"只有摘要"}"#;
        let llm = LlmHandler::mock(mock_json);
        let s = ContextSummarizer::new(llm, None);
        let result = s.summarize_session("User: hi").await.unwrap();
        assert_eq!(result.summary, "只有摘要");
        assert!(result.stable_facts.is_empty());
    }

    #[tokio::test]
    async fn test_summarize_session_invalid_json_returns_error() {
        let llm = LlmHandler::mock("not json at all");
        let s = ContextSummarizer::new(llm, None);
        let result = s.summarize_session("User: hi").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_summarize_session_with_model_configured() {
        let mock_json = r#"{"summary":"摘要","stable_facts":[]}"#;
        let llm = LlmHandler::mock(mock_json);
        let s = ContextSummarizer::new(llm, Some("gpt-4o-mini".to_string()));
        let result = s.summarize_session("User: hi").await.unwrap();
        assert_eq!(result.summary, "摘要");
    }

    #[tokio::test]
    async fn test_summarize_session_fact_without_confidence() {
        // confidence 字段缺失时，serde(default) 应填充 0.0
        let mock_json = r#"{"summary":"摘要","stable_facts":[{"key":"k","value":"v"}]}"#;
        let llm = LlmHandler::mock(mock_json);
        let s = ContextSummarizer::new(llm, None);
        let result = s.summarize_session("User: hi").await.unwrap();
        assert_eq!(result.stable_facts[0].confidence, 0.0);
    }

    // ========== C4: rollup_summaries 测试 ==========

    // ========== P2-V3 结构性修复：with_auditor 分流验证 ==========

    #[tokio::test]
    async fn test_summarize_dropped_routes_through_audited_llm() {
        // 挂载 auditor 后走 sidecar 审计链：LLM 结果来自审计路径的独立 mock，
        // 直连 handler 的返回内容不应出现
        let mut server = mockito::Server::new_async().await;
        let create_mock = server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"session_id":31}"#)
            .create_async()
            .await;
        let sse = concat!(
            "data: {\"type\":\"IoRequest\",\"id\":7}\n\n",
            "data: {\"type\":\"Stable\"}\n\n"
        );
        let events_mock = server
            .mock("GET", "/api/sessions/31/events")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse)
            .create_async()
            .await;
        let command_mock = server
            .mock("POST", "/api/sessions/31/command")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({
                    "instruction": {
                        "params": {"audit_purpose": "summarize"}
                    }
                }),
            ))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        let io_response_mock = server
            .mock("POST", "/api/sessions/31/io_response")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let direct = LlmHandler::mock("DIRECT-PATH-MARKER");
        let audited = crate::agent::audited_llm::AuditedLlm::new(
            crate::api::evorule_client::EvoruleApiClient::new(&server.url()),
            LlmHandler::mock(r#"{"content":"audited summary content"}"#),
        );
        let s = ContextSummarizer::new(direct, None)
            .with_auditor(audited)
            .with_threshold(2);
        let dropped = vec![user("a"), user("b"), user("c")];
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(
            result.contains("audited summary content"),
            "should use audited path, got: {result}"
        );
        assert!(
            !result.contains("DIRECT-PATH-MARKER"),
            "direct path must not be hit when auditor attached"
        );
        // 协议四端点全部被调用 → 证明走了完整 sidecar 审计回路
        create_mock.assert_async().await;
        events_mock.assert_async().await;
        command_mock.assert_async().await;
        io_response_mock.assert_async().await;
    }


    #[tokio::test]
    async fn test_rollup_summaries_empty() {
        let llm = LlmHandler::mock("should not be called");
        let s = ContextSummarizer::new(llm, None);
        let result = s.rollup_summaries(&[]).await.unwrap();
        assert!(result.is_empty(), "empty input should return empty string");
    }

    #[tokio::test]
    async fn test_rollup_summaries_single() {
        let llm = LlmHandler::mock("should not be called");
        let s = ContextSummarizer::new(llm, None);
        let input = vec!["only one summary".to_string()];
        let result = s.rollup_summaries(&input).await.unwrap();
        assert_eq!(result, "only one summary", "single input returned as-is");
    }

    #[tokio::test]
    async fn test_rollup_summaries_multiple_calls_llm() {
        // 多条摘要 → 调 LLM 合并
        let llm = LlmHandler::mock("combined summary");
        let s = ContextSummarizer::new(llm, None);
        let input = vec![
            "summary one".to_string(),
            "summary two".to_string(),
            "summary three".to_string(),
        ];
        let result = s.rollup_summaries(&input).await.unwrap();
        assert_eq!(result, "combined summary");
    }

    // ========== C1: extract_json_from_text 测试 ==========

    #[test]
    fn test_extract_json_direct() {
        let json = r#"{"key":"value"}"#;
        assert_eq!(extract_json_from_text(json), json);
    }

    #[test]
    fn test_extract_json_from_markdown_json_block() {
        let text = "```json\n{\"key\":\"value\"}\n```";
        assert_eq!(extract_json_from_text(text), r#"{"key":"value"}"#);
    }

    #[test]
    fn test_extract_json_from_markdown_block() {
        let text = "```\n{\"key\":\"value\"}\n```";
        assert_eq!(extract_json_from_text(text), r#"{"key":"value"}"#);
    }

    #[test]
    fn test_extract_json_with_surrounding_text() {
        let text = r#"Here is the result: {"key":"value"} done."#;
        assert_eq!(extract_json_from_text(text), r#"{"key":"value"}"#);
    }

    #[test]
    fn test_extract_json_no_braces_returns_original() {
        let text = "no json here";
        assert_eq!(extract_json_from_text(text), "no json here");
    }

    #[test]
    fn test_session_summary_out_deserialize() {
        let json =
            r#"{"summary":"test","stable_facts":[{"key":"k","value":"v","confidence":0.8}]}"#;
        let out: SessionSummaryOut = serde_json::from_str(json).unwrap();
        assert_eq!(out.summary, "test");
        assert_eq!(out.stable_facts.len(), 1);
        assert_eq!(out.stable_facts[0].key, "k");
    }

    #[test]
    fn test_session_summary_out_deserialize_no_facts() {
        let json = r#"{"summary":"test"}"#;
        let out: SessionSummaryOut = serde_json::from_str(json).unwrap();
        assert_eq!(out.summary, "test");
        assert!(out.stable_facts.is_empty());
    }
}
