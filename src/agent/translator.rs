// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! LLM response parser -- parses LLM-returned JSON into tool calls
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Message type
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum Message {
    #[serde(rename = "system")]
    /// System message
    System {
        /// Message content
        content: String,
    },
    #[serde(rename = "user")]
    /// User message
    User {
        /// Message content
        content: String,
    },
    #[serde(rename = "assistant")]
    /// Assistant message
    Assistant {
        /// Message content
        content: String,
        /// Tool call list
        tool_calls: Option<Vec<ToolCall>>,
    },
    #[serde(rename = "tool")]
    /// Tool message
    Tool {
        /// Tool return content
        content: String,
        /// Tool name
        tool_name: String,
    },
}

impl Message {
    /// Get message content
    pub fn content(&self) -> &str {
        match self {
            Message::System { content } => content,
            Message::User { content } => content,
            Message::Assistant { content, .. } => content,
            Message::Tool { content, .. } => content,
        }
    }
}

/// Tool call
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(rename = "tool_name")]
    /// Tool name
    pub name: String,
    #[serde(rename = "args")]
    /// Tool arguments
    pub arguments: Value,
}

/// LLM response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    /// Response content
    pub content: String,
    /// Tool call list
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Finish reason
    pub finish_reason: Option<String>,
    /// Token usage
    pub token_usage: Option<TokenUsage>,
}

/// Token usage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Prompt token count
    pub prompt_tokens: usize,
    /// Completion token count
    pub completion_tokens: usize,
    /// Total token count
    pub total_tokens: usize,
}

impl LlmResponse {
    /// Check whether contains tool calls
    pub fn is_tool_call(&self) -> bool {
        self.tool_calls.is_some() && !self.tool_calls.as_ref().unwrap().is_empty()
    }

    /// Check whether finished
    pub fn is_finished(&self) -> bool {
        matches!(
            self.finish_reason.as_deref(),
            Some("stop") | Some("end_turn")
        )
    }

    /// Extract response content
    pub fn extract_content(&self) -> String {
        self.content.clone()
    }
}

/// Parse LLM response
pub fn parse_llm_response(json: &str) -> Result<LlmResponse, serde_json::Error> {
    serde_json::from_str(json)
}

/// Extract tool call list
pub fn extract_tool_calls(json: &str) -> Result<Vec<ToolCall>, serde_json::Error> {
    let response: LlmResponse = serde_json::from_str(json)?;
    Ok(response.tool_calls.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_content() {
        let sys = Message::System {
            content: "sys".to_string(),
        };
        assert_eq!(sys.content(), "sys");

        let user = Message::User {
            content: "user".to_string(),
        };
        assert_eq!(user.content(), "user");

        let assist = Message::Assistant {
            content: "assist".to_string(),
            tool_calls: None,
        };
        assert_eq!(assist.content(), "assist");

        let tool = Message::Tool {
            content: "tool".to_string(),
            tool_name: "echo".to_string(),
        };
        assert_eq!(tool.content(), "tool");
    }

    #[test]
    fn test_llm_response_is_tool_call() {
        let resp = LlmResponse {
            content: "".to_string(),
            tool_calls: Some(vec![ToolCall {
                name: "search".to_string(),
                arguments: serde_json::json!({"query": "test"}),
            }]),
            finish_reason: None,
            token_usage: None,
        };
        assert!(resp.is_tool_call());

        let resp = LlmResponse {
            content: "hello".to_string(),
            tool_calls: None,
            finish_reason: None,
            token_usage: None,
        };
        assert!(!resp.is_tool_call());
    }

    #[test]
    fn test_llm_response_is_finished() {
        let resp = LlmResponse {
            content: "done".to_string(),
            tool_calls: None,
            finish_reason: Some("stop".to_string()),
            token_usage: None,
        };
        assert!(resp.is_finished());

        let resp = LlmResponse {
            content: "thinking".to_string(),
            tool_calls: None,
            finish_reason: Some("tool_call".to_string()),
            token_usage: None,
        };
        assert!(!resp.is_finished());

        let resp = LlmResponse {
            content: "".to_string(),
            tool_calls: None,
            finish_reason: None,
            token_usage: None,
        };
        assert!(!resp.is_finished());
    }

    #[test]
    fn test_parse_llm_response() {
        let json = r#"{
            "content": "hello",
            "tool_calls": [{"tool_name": "echo", "args": {"text": "hi"}}],
            "finish_reason": "stop",
            "token_usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        }"#;
        let resp = parse_llm_response(json).expect("parse");
        assert_eq!(resp.content, "hello");
        assert!(resp.is_tool_call());
        assert!(resp.is_finished());
        assert_eq!(resp.token_usage.unwrap().total_tokens, 15);
    }

    #[test]
    fn test_extract_tool_calls() {
        let json = r#"{
            "content": "",
            "tool_calls": [
                {"tool_name": "search", "args": {"query": "rust"}},
                {"tool_name": "write", "args": {"file": "test.txt"}}
            ],
            "finish_reason": null
        }"#;
        let calls = extract_tool_calls(json).expect("extract");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[1].name, "write");
    }

    #[test]
    fn test_extract_tool_calls_empty() {
        let json = r#"{
            "content": "no calls",
            "tool_calls": null,
            "finish_reason": "stop"
        }"#;
        let calls = extract_tool_calls(json).expect("extract");
        assert!(calls.is_empty());
    }
}
