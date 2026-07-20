// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! LLM I/O Handler -- invokes external LLM API

use serde_json;
use tier0_tcb::JsonValue;
use tracing::{debug, warn};

use crate::io_handler::{IoHandler, IoResult};
use crate::json_convert::serde_to_tcb;

/// LLM I/O Handler -- invokes external LLM API
#[derive(Debug, Clone)]
pub struct LlmHandler {
    default_model: String,
    api_base: String,
    api_key: Option<String>,
    /// If set, skip HTTP and return a canned LlmResponse-format JSON.
    /// Used by tests to avoid hitting a real LLM API.
    mock_content: Option<String>,
}

impl LlmHandler {
    /// Create new LLM handler
    pub fn new(default_model: &str, api_base: &str, api_key: Option<String>) -> Self {
        Self {
            default_model: default_model.to_string(),
            api_base: api_base.to_string(),
            api_key,
            mock_content: None,
        }
    }

    /// Create LLM handler with default config
    ///
    /// Priority: MiniMax > DeepSeek > OpenAI
    pub fn with_defaults() -> Self {
        if let Ok(api_key) = std::env::var("MINIMAX_API_KEY") {
            Self {
                default_model: std::env::var("MINIMAX_MODEL").unwrap_or_else(|_| "MiniMax-M2.5".to_string()),
                api_base: std::env::var("MINIMAX_API_BASE").unwrap_or_else(|_| "https://api.minimaxi.com/v1/text/chatcompletion_v2".to_string()),
                api_key: Some(api_key),
                mock_content: None,
            }
        } else if let Ok(api_key) = std::env::var("DEEPSEEK_API_KEY") {
            Self {
                default_model: std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-chat".to_string()),
                api_base: std::env::var("DEEPSEEK_API_BASE").unwrap_or_else(|_| "https://api.deepseek.com/v1/chat/completions".to_string()),
                api_key: Some(api_key),
                mock_content: None,
            }
        } else if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
            Self {
                default_model: std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string()),
                api_base: std::env::var("OPENAI_API_BASE").unwrap_or_else(|_| "https://api.openai.com/v1/chat/completions".to_string()),
                api_key: Some(api_key),
                mock_content: None,
            }
        } else {
            Self {
                default_model: "MiniMax-M2.5".to_string(),
                api_base: "https://api.minimaxi.com/v1/text/chatcompletion_v2".to_string(),
                api_key: None,
                mock_content: None,
            }
        }
    }

    /// Create a mock LLM handler for tests.
    ///
    /// Returns a deterministic `LlmResponse`-shaped JSON regardless of input.
    /// No HTTP call is made, no API key is needed.
    /// Wire into `AgentRunner` via `with_llm_handler` to test the full
    /// io_request -> io_response -> stable loop without a real LLM.
    pub fn mock(content: &str) -> Self {
        Self {
            default_model: "mock".to_string(),
            api_base: "mock://".to_string(),
            api_key: None,
            mock_content: Some(content.to_string()),
        }
    }

    /// Returns true if this handler is a mock (skips HTTP).
    pub fn is_mock(&self) -> bool {
        self.mock_content.is_some()
    }
}

#[async_trait::async_trait]
impl IoHandler for LlmHandler {
    /// Execute LLM API invocation
    async fn execute(&self, params: &JsonValue) -> IoResult {
        // Mock LLM short-circuit (used by tests).
        // Return a canned LlmResponse-shaped JSON so handle_call_external
        // can parse it without making a real HTTP call.
        if let Some(content) = &self.mock_content {
            let response = serde_json::json!({
                "content": content,
                "tool_calls": null,
                "finish_reason": "stop",
                "token_usage": null,
            });
            return Ok(serde_to_tcb(&response));
        }

        let params_str = params.to_string();
        let api_base = self.api_base.clone();
        let api_key = self.api_key.clone();
        let default_model = self.default_model.clone();

        let params_val: serde_json::Value =
            serde_json::from_str(&params_str).map_err(|e| format!("parse params: {}", e))?;

        let model = params_val
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(&default_model);

        let temperature = params_val
            .get("temperature")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.7);

        let max_tokens = params_val
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(4096);

        let prompt = params_val
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let mut body = serde_json::Map::new();
        body.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
        body.insert(
            "temperature".to_string(),
            serde_json::Value::Number(
                serde_json::Number::from_f64(temperature).unwrap_or(serde_json::Number::from(0)),
            ),
        );
        body.insert(
            "max_tokens".to_string(),
            serde_json::Value::Number(max_tokens.into()),
        );

        if params_val.get("messages").is_some() {
            body.insert(
                "messages".to_string(),
                params_val
                    .get("messages")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            );
        } else {
            body.insert(
                "messages".to_string(),
                serde_json::json!([{"role": "user", "content": prompt}]),
            );
        }

        if params_val.get("tools").is_some() {
            body.insert(
                "tools".to_string(),
                params_val
                    .get("tools")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            );
            body.insert(
                "tool_choice".to_string(),
                serde_json::Value::String("auto".to_string()),
            );
        }

        debug!(model = model, "ready to invoke LLM API");

        let client = reqwest::Client::new();
        let mut request = client
            .post(&api_base)
            .json(&serde_json::Value::Object(body));

        if let Some(api_key) = &api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }

        let response = request.send().await;

        match response {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let json: serde_json::Value = resp
                        .json()
                        .await
                        .map_err(|e| format!("LLM API response parse error: {}", e))?;

                    // 将 OpenAI 兼容的响应格式转换为 LlmResponse 结构:
                    //   choices[0].message.content   → content
                    //   choices[0].message.tool_calls → tool_calls
                    //   choices[0].finish_reason     → finish_reason
                    //   usage                        → token_usage
                    let choice = json
                        .get("choices")
                        .and_then(|c| c.as_array())
                        .and_then(|c| c.get(0));

                    let content = choice
                        .and_then(|c| c.get("message"))
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();

                    let tool_calls = choice
                        .and_then(|c| c.get("message"))
                        .and_then(|m| m.get("tool_calls"))
                        .cloned();

                    let finish_reason = choice
                        .and_then(|c| c.get("finish_reason"))
                        .and_then(|r| r.as_str())
                        .map(|s| s.to_string());

                    let token_usage = json.get("usage").cloned();

                    let response_json = serde_json::json!({
                        "content": content,
                        "tool_calls": tool_calls,
                        "finish_reason": finish_reason,
                        "token_usage": token_usage,
                    });

                    debug!(content_len = content.len(), finish_reason = ?finish_reason, "LLM API response parsed");
                    let response_val = serde_to_tcb(&response_json);
                    Ok(response_val)
                } else {
                    let error_text = resp.text().await.unwrap_or_default();
                    warn!(status = status.as_u16(), "LLM API request failed");
                    Err(format!("LLM API error ({}): {}", status, error_text))
                }
            }
            Err(e) => {
                warn!("LLM API request error: {}", e);
                Err(format!("LLM API request failed: {}", e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_handler_new() {
        let handler = LlmHandler::new("gpt-4", "https://api.example.com", Some("key".to_string()));
        assert_eq!(handler.default_model, "gpt-4");
        assert_eq!(handler.api_base, "https://api.example.com");
        assert!(handler.api_key.is_some());
    }

    #[test]
    fn test_llm_handler_with_defaults() {
        let handler = LlmHandler::with_defaults();
        assert_eq!(handler.default_model, "MiniMax-M2.5");
        assert_eq!(
            handler.api_base,
            "https://api.minimaxi.com/v1/text/chatcompletion_v2"
        );
        assert!(!handler.is_mock());
    }

    #[tokio::test]
    async fn test_llm_handler_mock_returns_canned_response() {
        use crate::io_handler::IoHandler;
        let handler = LlmHandler::mock("hello from mock");
        assert!(handler.is_mock());

        let params = tier0_tcb::JsonValue::empty_object();
        let result = handler.execute(&params).await.expect("mock execute");
        let s = result.to_string();
        // Must be parseable as LlmResponse (handle_call_external parses it this way)
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("mock response is valid JSON");
        assert_eq!(parsed["content"], "hello from mock");
        assert_eq!(parsed["finish_reason"], "stop");
        assert!(parsed["tool_calls"].is_null());
    }

    #[test]
    fn test_llm_handler_mock_does_not_require_api_key() {
        let handler = LlmHandler::mock("anything");
        assert!(handler.api_key.is_none());
        assert_eq!(handler.api_base, "mock://");
    }
}
