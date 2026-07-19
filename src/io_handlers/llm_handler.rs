// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! LLM I/O Handler 鈥斺€?璋冪敤澶栭儴 LLM API

use serde_json;
use tier0_tcb::JsonValue;
use tracing::{debug, warn};

use crate::io_handler::{IoHandler, IoResult};
use crate::json_convert::serde_to_tcb;

/// LLM I/O Handler 鈥斺€?璋冪敤澶栭儴 LLM API
#[derive(Debug, Clone)]
pub struct LlmHandler {
    default_model: String,
    api_base: String,
    api_key: Option<String>,
}

impl LlmHandler {
    /// 鍒涘缓鏂扮殑 LLM Handler
    pub fn new(default_model: &str, api_base: &str, api_key: Option<String>) -> Self {
        Self {
            default_model: default_model.to_string(),
            api_base: api_base.to_string(),
            api_key,
        }
    }

    /// 浣跨敤榛樿閰嶇疆鍒涘缓 LLM Handler
    /// 
    /// 浼樺厛绾э細MiniMax > DeepSeek > OpenAI
    pub fn with_defaults() -> Self {
        if let Ok(api_key) = std::env::var("MINIMAX_API_KEY") {
            Self {
                default_model: std::env::var("MINIMAX_MODEL").unwrap_or_else(|_| "MiniMax-M2.5".to_string()),
                api_base: std::env::var("MINIMAX_API_BASE").unwrap_or_else(|_| "https://api.minimax.io/v1/text/chatcompletion_v2".to_string()),
                api_key: Some(api_key),
            }
        } else if let Ok(api_key) = std::env::var("DEEPSEEK_API_KEY") {
            Self {
                default_model: std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-chat".to_string()),
                api_base: std::env::var("DEEPSEEK_API_BASE").unwrap_or_else(|_| "https://api.deepseek.com/v1/chat/completions".to_string()),
                api_key: Some(api_key),
            }
        } else if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
            Self {
                default_model: std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string()),
                api_base: std::env::var("OPENAI_API_BASE").unwrap_or_else(|_| "https://api.openai.com/v1/chat/completions".to_string()),
                api_key: Some(api_key),
            }
        } else {
            Self {
                default_model: "MiniMax-M2.5".to_string(),
                api_base: "https://api.minimax.io/v1/text/chatcompletion_v2".to_string(),
                api_key: None,
            }
        }
    }
}

#[async_trait::async_trait]
impl IoHandler for LlmHandler {
    /// 鎵ц LLM API 璋冪敤
    async fn execute(&self, params: &JsonValue) -> IoResult {
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

        debug!(model = model, "鍑嗗璋冪敤 LLM API");

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

                    let _content = json
                        .get("choices")
                        .and_then(|c| c.as_array())
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("message"))
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();

                    let _finish_reason = json
                        .get("choices")
                        .and_then(|c| c.as_array())
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("finish_reason"))
                        .and_then(|r| r.as_str());

                    let response_val = serde_to_tcb(&json);
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
            "https://api.minimax.io/v1/text/chatcompletion_v2"
        );
    }
}
