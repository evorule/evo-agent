// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Evorule HTTP API 瀹㈡埛绔?鈥斺€?灏佽 evorule 鐨勬墍鏈?API 璋冪敤

use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::pin::Pin;
use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EvoruleApiError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),
    #[error("API returned error: {status} {message}")]
    ApiError { status: u16, message: String },
    #[error("Invalid response format")]
    InvalidResponse,
    #[error("Session not found")]
    SessionNotFound,
    #[error("Invalid version: {0}")]
    InvalidVersion(String),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct EvoruleApiClient {
    base_url: String,
    client: Client,
}

impl EvoruleApiClient {
    pub fn new(base_url: &str) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: base_url.to_string(),
            client,
        }
    }

    pub async fn create_session(&self, initial_content: Option<&Value>) -> Result<String, EvoruleApiError> {
        let url = format!("{}/api/sessions", self.base_url);

        let body = if let Some(content) = initial_content {
            serde_json::json!({ "initial_content": content })
        } else {
            serde_json::json!({})
        };

        let resp = self.client.post(&url).json(&body).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let session_id = result["session_id"]
            .as_u64()
            .ok_or(EvoruleApiError::InvalidResponse)?
            .to_string();

        Ok(session_id)
    }

    pub async fn create_session_fork(
        &self,
        parent_id: &str,
        version: Option<u64>,
    ) -> Result<String, EvoruleApiError> {
        let url = if let Some(v) = version {
            format!("{}/api/sessions/fork/{}?version={}", self.base_url, parent_id, v)
        } else {
            format!("{}/api/sessions/fork/{}", self.base_url, parent_id)
        };

        let resp = self.client.post(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let session_id = result["session_id"]
            .as_u64()
            .ok_or(EvoruleApiError::InvalidResponse)?
            .to_string();

        Ok(session_id)
    }

    pub async fn submit_command(&self, session_id: &str, command: &Value) -> Result<(), EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/command", self.base_url, session_id);

        // 鏈嶅姟绔?CommandRequest 瑕佹眰 {"instruction": {...}} 鍖呰
        let body = serde_json::json!({ "instruction": command });
        let resp = self.client.post(&url).json(&body).send().await?;
        self.check_response(&resp).await?;

        Ok(())
    }

    pub async fn submit_io_response(
        &self,
        session_id: &str,
        request_id: u64,
        result: &Value,
        error: Option<&str>,
    ) -> Result<(), EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/io_response", self.base_url, session_id);

        let body = serde_json::json!({
            "request_id": request_id,
            "result": result,
            "error": error,
        });

        let resp = self.client.post(&url).json(&body).send().await?;
        self.check_response(&resp).await?;

        Ok(())
    }

    pub async fn update_payload(&self, session_id: &str, path: &str, value: &Value) -> Result<(), EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/payload", self.base_url, session_id);

        let body = serde_json::json!({
            "path": path,
            "value": value,
        });

        let resp = self.client.post(&url).json(&body).send().await?;
        self.check_response(&resp).await?;

        Ok(())
    }

    pub async fn get_state(&self, session_id: &str) -> Result<Value, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/state", self.base_url, session_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn get_facts(&self, session_id: &str, prefix: Option<&str>) -> Result<Vec<FactEntry>, EvoruleApiError> {
        let url = if let Some(p) = prefix {
            format!("{}/api/sessions/{}/facts?prefix={}", self.base_url, session_id, p)
        } else {
            format!("{}/api/sessions/{}/facts", self.base_url, session_id)
        };

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Vec<FactEntry> = resp.json().await?;
        Ok(result)
    }

    pub async fn get_shared_facts(&self, prefix: Option<&str>) -> Result<Vec<SharedFactEntry>, EvoruleApiError> {
        let url = if let Some(p) = prefix {
            format!("{}/api/shared/facts?prefix={}", self.base_url, p)
        } else {
            format!("{}/api/shared/facts", self.base_url)
        };

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Vec<SharedFactEntry> = resp.json().await?;
        Ok(result)
    }

    pub async fn get_shared_fact_source(&self, fact_id: u64) -> Result<SharedFactEntry, EvoruleApiError> {
        let url = format!("{}/api/shared/facts/{}/source", self.base_url, fact_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: SharedFactEntry = resp.json().await?;
        Ok(result)
    }

    pub async fn record_used_at_startup(&self, session_id: &str, fact_ids: &[u64]) -> Result<(), EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/used_at_startup", self.base_url, session_id);

        let body = serde_json::json!({
            "fact_ids": fact_ids,
        });

        let resp = self.client.post(&url).json(&body).send().await?;
        self.check_response(&resp).await?;

        Ok(())
    }

    pub async fn get_used_at_startup(&self, session_id: &str) -> Result<Vec<u64>, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/used_at_startup", self.base_url, session_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let fact_ids: Vec<u64> = result["fact_ids"]
            .as_array()
            .ok_or(EvoruleApiError::InvalidResponse)?
            .iter()
            .filter_map(|v| v.as_u64())
            .collect();

        Ok(fact_ids)
    }

    pub async fn get_sessions_using_fact(&self, fact_id: u64) -> Result<Vec<u64>, EvoruleApiError> {
        let url = format!("{}/api/shared/facts/{}/used_by", self.base_url, fact_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let sessions: Vec<u64> = result["sessions"]
            .as_array()
            .ok_or(EvoruleApiError::InvalidResponse)?
            .iter()
            .filter_map(|v| v.as_u64())
            .collect();

        Ok(sessions)
    }

    pub async fn get_audit_report(&self, session_id: &str) -> Result<Value, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/audit", self.base_url, session_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn verify_audit(&self, session_id: &str) -> Result<bool, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/audit/verify", self.base_url, session_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let valid = result["valid"].as_bool().ok_or(EvoruleApiError::InvalidResponse)?;

        Ok(valid)
    }

    pub async fn get_causal_chain(&self, session_id: &str, fact_id: u64) -> Result<Vec<Value>, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/audit/causal/{}", self.base_url, session_id, fact_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Vec<Value> = resp.json().await?;
        Ok(result)
    }

    pub async fn rewind(&self, session_id: &str, version: u64) -> Result<Value, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/rewind/{}", self.base_url, session_id, version);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn replay(&self, session_id: &str) -> Result<Vec<Value>, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/replay", self.base_url, session_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Vec<Value> = resp.json().await?;
        Ok(result)
    }

    pub async fn diff(&self, session_id: &str, a: u64, b: u64) -> Result<Value, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/diff?a={}&b={}", self.base_url, session_id, a, b);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn join_cluster(&self, session_id: &str, target_id: &str) -> Result<(), EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/join", self.base_url, session_id);

        // 鏈嶅姟绔?JoinRequest 瑕佹眰 JSON body锛歿"target_id": ..., "direction": ...}
        // target_id 鍦?evo-agent 涓槸 String锛岄渶瑙ｆ瀽涓?u64锛沝irection 榛樿鍙屽悜锛堜笉浼狅級
        let target_id_u64: u64 = target_id
            .parse()
            .map_err(|_| EvoruleApiError::InvalidVersion(format!("invalid target_id: {}", target_id)))?;
        let body = serde_json::json!({ "target_id": target_id_u64 });
        let resp = self.client.post(&url).json(&body).send().await?;
        self.check_response(&resp).await?;

        Ok(())
    }

    pub async fn leave_cluster(&self, session_id: &str) -> Result<(), EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/leave", self.base_url, session_id);

        let resp = self.client.post(&url).send().await?;
        self.check_response(&resp).await?;

        Ok(())
    }

    pub async fn get_cluster_status(&self, session_id: &str) -> Result<Value, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/cluster", self.base_url, session_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn subscribe_events(&self, session_id: &str) -> Result<EvoruleEventStream, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/events", self.base_url, session_id);

        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(EvoruleApiError::ApiError {
                status: resp.status().as_u16(),
                message: resp.text().await.unwrap_or_default(),
            });
        }

        Ok(EvoruleEventStream {
            buffer: Vec::new(),
            stream: Box::pin(resp.bytes_stream()),
        })
    }

    pub async fn get_debug_phase(&self, session_id: &str) -> Result<Value, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/debug/phase", self.base_url, session_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn get_debug_queue(&self, session_id: &str) -> Result<Value, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/debug/queue", self.base_url, session_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn get_debug_pending_io(&self, session_id: &str) -> Result<Value, EvoruleApiError> {
        let url = format!("{}/api/sessions/{}/debug/pending_io", self.base_url, session_id);

        let resp = self.client.get(&url).send().await?;
        self.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    async fn check_response(&self, resp: &reqwest::Response) -> Result<(), EvoruleApiError> {
        if !resp.status().is_success() {
            let status = resp.status().as_u16();

            if status == 404 {
                return Err(EvoruleApiError::SessionNotFound);
            }

            return Err(EvoruleApiError::ApiError {
                status,
                message: String::new(),
            });
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct FactEntry {
    pub version: u64,
    pub id: u64,
    pub path: String,
    pub value: Value,
    #[serde(rename = "type")]
    pub fact_type: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SharedFactEntry {
    pub fact_id: u64,
    pub path: String,
    pub value: Value,
    pub source_session_id: u64,
    pub version: u64,
}

pub struct EvoruleEventStream {
    buffer: Vec<u8>,
    stream: Pin<Box<dyn futures_core::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
}

#[derive(Debug, Deserialize)]
pub struct EvoruleEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
}

impl EvoruleEventStream {
    pub async fn next(&mut self) -> Option<EvoruleEvent> {
        let mut data = String::new();

        loop {
            while let Some(line_end) = self.buffer.windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&self.buffer[..line_end]).to_string();
                self.buffer.drain(..line_end + 2);

                if line.starts_with("data: ") {
                    let json_part = line.strip_prefix("data: ").unwrap_or(&line);
                    data.push_str(json_part);
                } else if line.is_empty() && !data.is_empty() {
                    match serde_json::from_str::<EvoruleEvent>(&data) {
                        Ok(ev) => return Some(ev),
                        Err(e) => {
                            tracing::warn!("Failed to parse event: {}", e);
                            data.clear();
                        }
                    }
                }
            }

            match self.stream.next().await {
                Some(Ok(chunk)) => {
                    self.buffer.extend_from_slice(&chunk);
                }
                Some(Err(e)) => {
                    tracing::warn!("SSE stream error: {}", e);
                    return None;
                }
                None => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:8080")
    }

    #[test]
    fn test_client_new() {
        let client = make_test_client();
        assert_eq!(client.base_url, "http://localhost:8080");
    }

    #[test]
    fn test_build_call_external_command() {
        let command = serde_json::json!({
            "type": "call_external",
            "params": {
                "model": "gpt-4o-mini",
                "temperature": 0.7,
                "system_prompt": "You are a helpful assistant",
                "goal": "Test task",
                "tool_names": ["search", "calculator"],
            }
        });

        assert_eq!(command["type"], "call_external");
        assert_eq!(command["params"]["model"], "gpt-4o-mini");
        assert_eq!(command["params"]["goal"], "Test task");
    }

    #[test]
    fn test_evorule_event_deserialize() {
        let json = r#"{"type":"io_request","payload":{"io_type":"call_external","id":1,"params":{"model":"test"}}}"#;
        let event: EvoruleEvent = serde_json::from_str(json).unwrap();

        assert_eq!(event.event_type, "io_request");
        assert_eq!(event.payload["io_type"], "call_external");
        assert_eq!(event.payload["id"], 1);
    }

    #[test]
    fn test_evorule_event_deserialize_stable() {
        let json = r#"{"type":"stable","payload":"task completed"}"#;
        let event: EvoruleEvent = serde_json::from_str(json).unwrap();

        assert_eq!(event.event_type, "stable");
        assert_eq!(event.payload, "task completed");
    }

    #[tokio::test]
    async fn test_fact_closed_loop_workflow() {
        let _client = make_test_client();

        let goal = "What is the weather today?";
        let system_prompt = "You are a weather assistant";

        let command = serde_json::json!({
            "type": "call_external",
            "params": {
                "model": "gpt-4o-mini",
                "temperature": 0.7,
                "system_prompt": system_prompt,
                "goal": goal,
                "tool_names": ["weather_api"],
            }
        });

        assert_eq!(command["type"], "call_external");
        assert_eq!(command["params"]["goal"], goal);
        assert_eq!(command["params"]["system_prompt"], system_prompt);

        let io_response = serde_json::json!({
            "content": "The weather is sunny today",
            "tool_calls": null,
            "is_finished": true,
        });

        assert_eq!(io_response["content"], "The weather is sunny today");
        assert!(io_response["is_finished"].as_bool().unwrap());
    }

    #[test]
    fn test_io_response_payload_structure() {
        let request_id: u64 = 123;
        let content = "Tool execution result";
        let used_facts = vec!["fact1".to_string(), "fact2".to_string()];

        let io_response_body = serde_json::json!({
            "request_id": request_id,
            "content": content,
            "used_facts": used_facts,
        });

        assert_eq!(io_response_body["request_id"], request_id);
        assert_eq!(io_response_body["content"], content);
        assert_eq!(io_response_body["used_facts"].as_array().unwrap().len(), 2);
    }
}


