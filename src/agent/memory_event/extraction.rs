// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 032 EventExtractor —— 从对话/工具结果中提取 MemoryEvent
//!
//! ## 提取策略(Q13 方案 C:显式优先 + 关键词触发)
//!
//! | 提取方式 | 触发 | 确定性 | 用途 |
//! |---|---|---|---|
//! | **显式提取** | 用户说"记住这件事" | ✅ 完全确定 | 用户主动标记的重要事件 |
//! | **关键词触发** | 检测到"生日""分手"等关键词 | ✅ 触发确定 | 自动发现潜在重要事件 |
//! | **工具触发** | 工具结果(如天气 API) | ✅ 完全确定 | I/O 触发事件 |
//! | **LLM 辅助** | 提取触发后,LLM 填结构化字段 | ⚠️ 结构确定 | 只填 JSON 字段,不生成叙事 |
//!
//! ## LLM 辅助提取的约束
//!
//! - LLM 输出必须是 **JSON 结构**(event_type + entities + content + emotion)
//! - temperature=0(最大确定性)
//! - LLM **不参与存储**,只参与"从对话文本中识别结构化字段"
//! - 回放时 LLM 只做 NL 包装(把结构化事件翻译成叙述)
//!
//! ## 提取模型(Q18 方案 B:单独配置 extraction_model)
//!
//! - `extraction_model` 字段(可选,默认 fallback 到主模型)
//! - 可用便宜模型(如 GPT-4o-mini)做提取,降低成本

use serde::{Deserialize, Serialize};

use crate::io_handler::IoHandler;
use crate::io_handlers::LlmHandler;
use crate::json_convert::serde_to_tcb;

use super::event::{Emotion, EventSource, EventType, MemoryEvent};

/// 提取触发类型
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractionTrigger {
    /// 用户显式标记(说"记住这件事")
    Explicit,
    /// 关键词触发
    Keyword(String),
    /// 工具结果触发
    ToolResult,
}

/// 提取配置
#[derive(Debug, Clone)]
pub struct ExtractionConfig {
    /// 关键词列表(触发自动提取)
    pub keywords: Vec<String>,
    /// 显式触发短语列表
    pub explicit_phrases: Vec<String>,
    /// 提取模型名称(None = fallback 到主模型)
    pub extraction_model: Option<String>,
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            keywords: DEFAULT_KEYWORDS.iter().map(|s| s.to_string()).collect(),
            explicit_phrases: DEFAULT_EXPLICIT_PHRASES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            extraction_model: None,
        }
    }
}

/// 默认关键词列表(可被 agent.json 覆盖)
const DEFAULT_KEYWORDS: &[&str] = &[
    "生日",
    "分手",
    "毕业",
    "入职",
    "离职",
    "搬家",
    "结婚",
    "生子",
    "去世",
    "逝世",
    "生病",
    "住院",
    "手术",
    "和解",
    "表白",
    "道歉",
    "告别",
    "最后一次",
    "第一次",
    "里程碑",
    "成就",
];

/// 默认显式触发短语
const DEFAULT_EXPLICIT_PHRASES: &[&str] = &[
    "记住这件事",
    "记住这个",
    "记录下来",
    "帮我记住",
    "这个很重要",
    "记一下",
];

/// 032 EventExtractor —— 从对话/工具结果中提取 MemoryEvent
///
/// 提取流程:
/// 1. 检查显式触发(用户说"记住这件事")
/// 2. 检查关键词触发(消息包含"生日""分手"等)
/// 3. 如有触发,调 LLM 提取结构化字段(temperature=0)
/// 4. 返回 MemoryEvent(或 None)
pub struct EventExtractor {
    /// LLM handler(从主 handler clone,共享 API key)
    llm: LlmHandler,
    /// 提取配置
    config: ExtractionConfig,
}

impl EventExtractor {
    /// 创建提取器
    pub fn new(llm: LlmHandler, config: ExtractionConfig) -> Self {
        Self { llm, config }
    }

    /// 从默认配置创建
    pub fn with_defaults(llm: LlmHandler) -> Self {
        Self::new(llm, ExtractionConfig::default())
    }

    /// 设置提取模型
    pub fn with_extraction_model(mut self, model: &str) -> Self {
        self.config.extraction_model = Some(model.to_string());
        self
    }

    /// 检测触发类型(不调 LLM,纯文本匹配)
    ///
    /// 优先级:显式 > 关键词
    pub fn detect_trigger(&self, user_message: &str) -> Option<ExtractionTrigger> {
        // 1. 显式触发
        for phrase in &self.config.explicit_phrases {
            if user_message.contains(phrase) {
                return Some(ExtractionTrigger::Explicit);
            }
        }

        // 2. 关键词触发
        for keyword in &self.config.keywords {
            if user_message.contains(keyword) {
                return Some(ExtractionTrigger::Keyword(keyword.clone()));
            }
        }

        None
    }

    /// 从对话消息中提取 MemoryEvent
    ///
    /// 流程:
    /// 1. 检测触发(显式/关键词)
    /// 2. 如有触发,调 LLM 提取结构化字段
    /// 3. 返回 MemoryEvent
    ///
    /// 参数:
    /// - `user_message`: 用户消息
    /// - `assistant_response`: 助手回复(可选,提供更多上下文)
    /// - `event_id`: 事件 ID(由调用方生成)
    /// - `timestamp`: 事件时间戳
    pub async fn extract_from_conversation(
        &self,
        user_message: &str,
        assistant_response: Option<&str>,
        event_id: &str,
        timestamp: u64,
    ) -> Result<Option<MemoryEvent>, String> {
        let trigger = match self.detect_trigger(user_message) {
            Some(t) => t,
            None => return Ok(None), // 无触发,不提取
        };

        // 调 LLM 提取结构化字段
        let extracted = self
            .extract_with_llm(user_message, assistant_response)
            .await?;

        // 构造 MemoryEvent
        let source = match &trigger {
            ExtractionTrigger::Explicit => EventSource::UserInput,
            ExtractionTrigger::Keyword(_) => EventSource::LlmExtraction,
            ExtractionTrigger::ToolResult => EventSource::ToolResult,
        };

        let confidence = match &trigger {
            ExtractionTrigger::Explicit => 1.0,
            ExtractionTrigger::Keyword(_) => 0.8,
            ExtractionTrigger::ToolResult => 1.0,
        };

        let mut event = MemoryEvent::new_root(event_id, extracted.event_type, timestamp, source)
            .with_confidence(confidence);

        event.content = extracted.content;
        if let Some(emotion) = extracted.emotion {
            event.emotion = Some(emotion);
        }
        if !extracted.entities.is_empty() {
            event.entities = extracted.entities;
        }
        if !extracted.tags.is_empty() {
            event.tags = extracted.tags;
        }

        Ok(Some(event))
    }

    /// 从工具结果中提取 MemoryEvent
    ///
    /// 工具结果触发的提取是确定性的(不需要 LLM),由调用方提供结构化字段。
    pub fn extract_from_tool_result(
        &self,
        event_id: &str,
        timestamp: u64,
        event_type: EventType,
        content: serde_json::Value,
        entities: Vec<super::entity::EntityRef>,
    ) -> MemoryEvent {
        let mut event =
            MemoryEvent::new_root(event_id, event_type, timestamp, EventSource::ToolResult)
                .with_content(content)
                .with_confidence(1.0);
        event.entities = entities;
        event
    }

    /// 调 LLM 提取结构化事件字段
    ///
    /// LLM 输出必须是 JSON 结构(event_type + entities + content + emotion),
    /// 不是自然语言叙事。temperature=0 保证最大确定性。
    async fn extract_with_llm(
        &self,
        user_message: &str,
        assistant_response: Option<&str>,
    ) -> Result<ExtractedEvent, String> {
        // 构建 prompt
        let mut conversation = format!("用户: {}", user_message);
        if let Some(resp) = assistant_response {
            conversation.push_str(&format!("\n助手: {}", resp));
        }

        let prompt = format!(
            "请从以下对话中提取结构化事件信息,输出 JSON 格式。\n\n\
             对话:\n{}\n\n\
             输出 JSON 格式:\n\
             {{\n\
               \"event_type\": {{\"kind\": \"Conversation\", \"subtype\": \"Farewell\"}},\n\
               \"entities\": [{{\"entity_id\": \"person_xxx\", \"role\": \"recipient\"}}],\n\
               \"content\": {{\"summary\": \"简要描述\", \"key_phrases\": [\"关键词1\"]}},\n\
               \"emotion\": {{\"valence\": 0.3, \"arousal\": 0.6, \"labels\": [\"love\"], \"subject\": \"User\"}},\n\
               \"tags\": [\"important\"]\n\
             }}\n\n\
             可用的 event_type kind: Conversation, Relationship, Milestone, Habit, Health, EmotionEvent, Location, Item, IOTrigger, SystemObservation, Custom\n\
             如果对话中没有值得记录的事件,返回: {{\"event_type\": {{\"kind\": \"Custom\", \"subtype\": \"none\"}}}}\n\
             只输出 JSON,不要输出其他内容。",
            conversation
        );

        let mut messages_vec: Vec<serde_json::Value> = Vec::with_capacity(2);
        messages_vec.push(serde_json::json!({
            "role": "system",
            "content": EXTRACTION_SYSTEM_PROMPT,
        }));
        messages_vec.push(serde_json::json!({
            "role": "user",
            "content": prompt,
        }));

        let mut params_map = serde_json::Map::new();
        if let Some(model) = &self.config.extraction_model {
            params_map.insert(
                "model".to_string(),
                serde_json::Value::String(model.clone()),
            );
        }
        params_map.insert(
            "temperature".to_string(),
            serde_json::json!(0.0), // temperature=0 保证最大确定性
        );
        params_map.insert("max_tokens".to_string(), serde_json::json!(512));
        params_map.insert(
            "messages".to_string(),
            serde_json::Value::Array(messages_vec),
        );
        let params_json = serde_json::Value::Object(params_map);
        let params = serde_to_tcb(&params_json);

        let result = self.llm.execute(&params).await?;

        // 解析 LLM 响应为 JSON
        let response: crate::agent::translator::LlmResponse =
            serde_json::from_str(&result.to_string())
                .map_err(|e| format!("parse LLM response: {}", e))?;

        // 提取 JSON(可能包裹在 markdown 代码块中)
        let json_str = extract_json_from_text(&response.content);
        let extracted: ExtractedEvent = serde_json::from_str(&json_str)
            .map_err(|e| format!("parse extracted event: {} (raw: {})", e, response.content))?;

        // 如果是 Custom("none"),表示无事件
        if let EventType::Custom(ref s) = extracted.event_type {
            if s == "none" {
                return Err("no event to extract".to_string());
            }
        }

        Ok(extracted)
    }

    /// 获取配置引用
    pub fn config(&self) -> &ExtractionConfig {
        &self.config
    }
}

/// LLM 提取的结构化事件
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExtractedEvent {
    event_type: EventType,
    #[serde(default)]
    entities: Vec<super::entity::EntityRef>,
    #[serde(default)]
    content: serde_json::Value,
    #[serde(default)]
    emotion: Option<Emotion>,
    #[serde(default)]
    tags: Vec<String>,
}

/// 从可能包含 markdown 代码块的文本中提取 JSON
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

/// 提取系统提示
const EXTRACTION_SYSTEM_PROMPT: &str = "\
你是一个事件提取助手。你的任务是从对话中识别值得记录的人生事件,并提取结构化字段。\n\
\n\
严格约束:\n\
1. 只提取对话中明确存在的信息,不能编造\n\
2. 输出必须是 JSON 格式,不要输出自然语言解释\n\
3. 情感维度从对话中推断,但标注为推断(不是事实)\n\
4. entity_id 用有意义的 ID,如 person_mom / pet_doudou\n\
5. 如果对话中没有值得记录的事件,返回 Custom(\"none\")";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_explicit_trigger() {
        let extractor = EventExtractor::with_defaults(LlmHandler::mock(""));

        assert_eq!(
            extractor.detect_trigger("帮我记住这件事"),
            Some(ExtractionTrigger::Explicit)
        );
        assert_eq!(
            extractor.detect_trigger("记住这件事,很重要"),
            Some(ExtractionTrigger::Explicit)
        );
        assert_eq!(
            extractor.detect_trigger("这个很重要,记一下"),
            Some(ExtractionTrigger::Explicit)
        );
    }

    #[test]
    fn test_detect_keyword_trigger() {
        let extractor = EventExtractor::with_defaults(LlmHandler::mock(""));

        assert_eq!(
            extractor.detect_trigger("今天是我生日"),
            Some(ExtractionTrigger::Keyword("生日".to_string()))
        );
        assert_eq!(
            extractor.detect_trigger("我们分手了"),
            Some(ExtractionTrigger::Keyword("分手".to_string()))
        );
    }

    #[test]
    fn test_detect_no_trigger() {
        let extractor = EventExtractor::with_defaults(LlmHandler::mock(""));

        assert_eq!(extractor.detect_trigger("今天天气不错"), None);
        assert_eq!(extractor.detect_trigger("你好"), None);
        assert_eq!(extractor.detect_trigger(""), None);
    }

    #[test]
    fn test_explicit_takes_priority_over_keyword() {
        let extractor = EventExtractor::with_defaults(LlmHandler::mock(""));

        // "记住" 是显式触发,"生日" 是关键词,显式优先
        let trigger = extractor.detect_trigger("记住这件事,今天是我生日");
        assert_eq!(trigger, Some(ExtractionTrigger::Explicit));
    }

    #[test]
    fn test_custom_keywords() {
        let mut config = ExtractionConfig::default();
        config.keywords = vec!["面试".to_string(), "offer".to_string()];
        let extractor = EventExtractor::new(LlmHandler::mock(""), config);

        assert_eq!(
            extractor.detect_trigger("今天去面试了"),
            Some(ExtractionTrigger::Keyword("面试".to_string()))
        );
        // 默认关键词不生效
        assert_eq!(extractor.detect_trigger("今天是我生日"), None);
    }

    #[tokio::test]
    async fn test_extract_from_conversation_no_trigger() {
        let extractor = EventExtractor::with_defaults(LlmHandler::mock(""));
        let result = extractor
            .extract_from_conversation("今天天气不错", None, "E001", 1000)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_extract_from_conversation_with_mock_llm() {
        let mock_response = r#"{"event_type":{"kind":"Milestone","subtype":"Birthday"},"entities":[{"entity_id":"person_me","role":"subject"}],"content":{"summary":"我的生日","age":30},"emotion":{"valence":0.8,"arousal":0.5,"labels":["joy"],"subject":"User"},"tags":["birthday"]}"#;
        let extractor = EventExtractor::with_defaults(LlmHandler::mock(mock_response));

        let result = extractor
            .extract_from_conversation("今天是我生日", None, "E001", 1000)
            .await
            .unwrap();

        assert!(result.is_some());
        let event = result.unwrap();
        assert_eq!(event.event_id, "E001");
        assert_eq!(
            event.event_type,
            EventType::Milestone(super::super::event::MilestoneSubtype::Birthday)
        );
        assert_eq!(event.source, EventSource::LlmExtraction);
        assert!((event.confidence - 0.8).abs() < 0.01);
        assert!(event.emotion.is_some());
        assert!(!event.tags.is_empty());
    }

    #[tokio::test]
    async fn test_extract_explicit_higher_confidence() {
        let mock_response = r#"{"event_type":{"kind":"Milestone","subtype":"Achievement"},"entities":[],"content":{"summary":"测试"},"emotion":null,"tags":[]}"#;
        let extractor = EventExtractor::with_defaults(LlmHandler::mock(mock_response));

        // 显式触发 → confidence = 1.0
        let result = extractor
            .extract_from_conversation("记住这件事,我完成了目标", None, "E001", 1000)
            .await
            .unwrap()
            .unwrap();
        assert!((result.confidence - 1.0).abs() < 0.01);
        assert_eq!(result.source, EventSource::UserInput);
    }

    #[tokio::test]
    async fn test_extract_llm_returns_none_event() {
        let mock_response = r#"{"event_type":{"kind":"Custom","subtype":"none"},"entities":[],"content":{},"emotion":null,"tags":[]}"#;
        let extractor = EventExtractor::with_defaults(LlmHandler::mock(mock_response));

        // LLM 返回 Custom("none"),表示无事件 → Err
        let result = extractor
            .extract_from_conversation("今天是我生日", None, "E001", 1000)
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_from_tool_result() {
        let extractor = EventExtractor::with_defaults(LlmHandler::mock(""));
        let event = extractor.extract_from_tool_result(
            "E001",
            1000,
            EventType::Custom("weather_alert".to_string()),
            serde_json::json!({"alert": "rain", "location": "Shanghai"}),
            vec![super::super::entity::EntityRef::new(
                "place_shanghai",
                "location",
            )],
        );

        assert_eq!(event.event_id, "E001");
        assert_eq!(event.source, EventSource::ToolResult);
        assert!((event.confidence - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_extract_json_from_text_plain() {
        let json = r#"{"key": "value"}"#;
        assert_eq!(extract_json_from_text(json), json);
    }

    #[test]
    fn test_extract_json_from_markdown_code_block() {
        let text = "```json\n{\"key\": \"value\"}\n```";
        assert_eq!(extract_json_from_text(text), r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_json_from_generic_code_block() {
        let text = "```\n{\"key\": \"value\"}\n```";
        assert_eq!(extract_json_from_text(text), r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_json_from_text_with_prefix() {
        let text = "Here is the result:\n{\"key\": \"value\"}\nDone.";
        assert_eq!(extract_json_from_text(text), r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extraction_config_default() {
        let config = ExtractionConfig::default();
        assert!(!config.keywords.is_empty());
        assert!(!config.explicit_phrases.is_empty());
        assert!(config.extraction_model.is_none());
        // 默认关键词包含"生日"
        assert!(config.keywords.contains(&"生日".to_string()));
        // 默认显式短语包含"记住这件事"
        assert!(config.explicit_phrases.contains(&"记住这件事".to_string()));
    }

    #[tokio::test]
    async fn test_extract_with_extraction_model() {
        let mock_response = r#"{"event_type":{"kind":"EmotionEvent"},"entities":[],"content":{"summary":"test"},"emotion":null,"tags":[]}"#;
        let extractor = EventExtractor::with_defaults(LlmHandler::mock(mock_response))
            .with_extraction_model("gpt-4o-mini");

        assert_eq!(
            extractor.config().extraction_model.as_deref(),
            Some("gpt-4o-mini")
        );
    }
}
