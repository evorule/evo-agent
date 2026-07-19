// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent 杩愯鍣?鈥斺€?ReAct 寰幆鎵ц鏍稿績锛堜簨浠堕┍鍔ㄦ灦鏋勶級
//!
//! 瀹屾暣 Fact 闂幆娴佺▼锛?//! AgentRunner 鎻愪氦 Command 鈫?POST /api/sessions/{id}/command 鈫?evorule 浜х敓 IoRequest 鈫?//! SSE 鎺ㄩ€?io_request 浜嬩欢 鈫?AgentRunner 鎵ц澶栭儴璋冪敤 鈫?POST /api/sessions/{id}/io_response 鈫?//! evorule 浜х敓 IoResponse + StateTransition 鈫?SSE 鎺ㄩ€?stable 浜嬩欢 鈫?AgentRunner 杩斿洖缁撴灉

use std::collections::BTreeMap;
use std::time::Duration;

use async_stream::stream;
use futures_core::Stream;
use serde_json::Value;
use tier0_tcb::JsonValue;
use tracing::info;

use crate::agent::delegate::DelegateContext;
use crate::agent::memory::MemoryManager;
use crate::agent::translator::{LlmResponse, Message};
use crate::api::evorule_client::{EvoruleApiClient, EvoruleApiError};
use crate::io_handler::IoHandler;
use crate::io_handlers::{LlmHandler, ToolHandler};
use crate::json_convert::serde_to_tcb;

pub const DEFAULT_MAX_DELEGATE_DEPTH: usize = 3;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub agent_type: String,
    pub system_prompt: String,
    pub model: String,
    pub temperature: f32,
    pub max_steps: usize,
    pub step_timeout: Duration,
    pub tool_names: Vec<String>,
    pub llm_retry_count: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent_type: "default".to_string(),
            system_prompt: "浣犳槸涓€涓府鍔╂€у姪鎵".to_string(),
            model: "gpt-4o-mini".to_string(),
            temperature: 0.7,
            max_steps: 10,
            step_timeout: Duration::from_secs(60),
            tool_names: Vec::new(),
            llm_retry_count: 3,
        }
    }
}

#[derive(Debug)]
pub enum AgentError {
    LlmError(String),
    ToolError(String),
    Timeout(String),
    MaxStepsExceeded(usize),
    DelegateError(String),
    MemoryError(String),
    Internal(String),
    EvoruleError(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::LlmError(e) => write!(f, "LLM error: {}", e),
            AgentError::ToolError(e) => write!(f, "Tool error: {}", e),
            AgentError::Timeout(e) => write!(f, "Timeout: {}", e),
            AgentError::MaxStepsExceeded(s) => write!(f, "Max steps exceeded: {}", s),
            AgentError::DelegateError(e) => write!(f, "Delegate error: {}", e),
            AgentError::MemoryError(e) => write!(f, "Memory error: {}", e),
            AgentError::Internal(e) => write!(f, "Internal error: {}", e),
            AgentError::EvoruleError(e) => write!(f, "Evorule error: {}", e),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<EvoruleApiError> for AgentError {
    fn from(e: EvoruleApiError) -> Self {
        AgentError::EvoruleError(e.to_string())
    }
}

impl From<crate::agent::memory::MemoryError> for AgentError {
    fn from(e: crate::agent::memory::MemoryError) -> Self {
        AgentError::MemoryError(e.to_string())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentResult {
    pub success: bool,
    pub content: String,
    pub steps: usize,
    pub duration_ms: u64,
    pub tool_calls: Vec<String>,
    pub error: Option<String>,
}

impl AgentResult {
    pub fn success(
        content: String,
        steps: usize,
        duration_ms: u64,
        tool_calls: Vec<String>,
    ) -> Self {
        Self {
            success: true,
            content,
            steps,
            duration_ms,
            tool_calls,
            error: None,
        }
    }

    pub fn error(error: String, steps: usize, duration_ms: u64) -> Self {
        Self {
            success: false,
            content: String::new(),
            steps,
            duration_ms,
            tool_calls: Vec::new(),
            error: Some(error),
        }
    }
}

pub struct AgentRunner {
    config: AgentConfig,
    evorule_client: EvoruleApiClient,
    llm_handler: LlmHandler,
    tool_handler: ToolHandler,
    memory: Option<MemoryManager>,
    delegate_context: Option<DelegateContext>,
    session_id: Option<String>,
    join_cluster_id: Option<String>,
}

impl AgentRunner {
    pub fn new(config: AgentConfig, evorule_client: EvoruleApiClient) -> Self {
        Self {
            config,
            evorule_client,
            llm_handler: LlmHandler::with_defaults(),
            tool_handler: ToolHandler::new(),
            memory: None,
            delegate_context: None,
            session_id: None,
            join_cluster_id: None,
        }
    }

    /// 鏇挎崲榛樿鐨?LLM Handler(鐢ㄤ簬鐪熷疄鎺ュ叆鐗瑰畾 provider)
    pub fn with_llm_handler(mut self, llm_handler: LlmHandler) -> Self {
        self.llm_handler = llm_handler;
        self
    }

    pub fn with_tool_handler(mut self, tool_handler: ToolHandler) -> Self {
        self.tool_handler = tool_handler;
        self
    }

    pub fn with_memory(mut self, memory: MemoryManager) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn with_delegate_context(mut self, ctx: DelegateContext) -> Self {
        self.delegate_context = Some(ctx);
        self
    }

    pub fn with_join_cluster(mut self, cluster_id: &str) -> Self {
        self.join_cluster_id = Some(cluster_id.to_string());
        self
    }

    pub async fn run(&mut self, goal: &str) -> Result<AgentResult, AgentError> {
        let start_time = std::time::Instant::now();

        let system_prompt = if let Some(memory) = self.memory.as_mut() {
            memory.build_system_prompt(&self.config.system_prompt)
        } else {
            self.config.system_prompt.clone()
        };

        let session_id = self.evorule_client.create_session(None).await?;
        self.session_id = Some(session_id.clone());
        info!(%session_id, "Created evorule session");

        let _recalled_fact_ids = self.auto_recall(&session_id).await?;

        if let Some(cluster_id) = &self.join_cluster_id {
            self.evorule_client.join_cluster(&session_id, cluster_id).await?;
            info!(%session_id, cluster_id, "Joined cluster");
        }

        let command = self.build_call_external_command(&system_prompt, goal);
        self.evorule_client.submit_command(&session_id, &command).await?;
        info!(%session_id, "Submitted call_external command");

        let mut event_stream = self.evorule_client.subscribe_events(&session_id).await?;
        let mut step_count = 0;
        let mut tool_calls: Vec<String> = Vec::new();
        let mut messages: Vec<Message> = Vec::new();

        if !system_prompt.is_empty() {
            messages.push(Message::System { content: system_prompt });
        }
        messages.push(Message::User { content: goal.to_string() });

        while let Some(event) = event_stream.next().await {
            match event.event_type.as_str() {
                "io_request" => {
                    step_count += 1;
                    if step_count > self.config.max_steps {
                        let duration = start_time.elapsed().as_millis() as u64;
                        return Ok(AgentResult::error(
                            format!("Max steps exceeded: {}", self.config.max_steps),
                            step_count,
                            duration,
                        ));
                    }

                    info!(%session_id, step = step_count, "Received io_request event");
                    let result = self.handle_io_request(&event.payload, &mut messages, &mut tool_calls).await?;

                    if let Some(request_id) = event.payload.get("id").and_then(|v| v.as_u64()) {
                        self.evorule_client
                            .submit_io_response(&session_id, request_id, &result, None)
                            .await?;
                        info!(%session_id, request_id, "Submitted io_response");
                    }
                }
                "stable" => {
                    let duration = start_time.elapsed().as_millis() as u64;
                    let state = self.evorule_client.get_state(&session_id).await?;
                    let content = state["payload"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();

                    info!(%session_id, "Received stable event, execution complete");
                    return Ok(AgentResult::success(content, step_count, duration, tool_calls));
                }
                "phase_change" => {
                    let phase = event.payload.get("phase").and_then(|v| v.as_str()).unwrap_or("unknown");
                    info!(%session_id, %phase, "Phase changed");
                }
                "state_transition" => {
                    info!(%session_id, "State transition occurred");
                }
                "error" => {
                    let error_msg = event.payload.get("message").and_then(|v| v.as_str()).unwrap_or("unknown error");
                    let duration = start_time.elapsed().as_millis() as u64;
                    
                    if let Ok(rewind_result) = self.auto_rewind(&session_id).await {
                        info!(%session_id, "Auto-rewind successful, retrying from version {}", rewind_result);
                        continue;
                    }
                    
                    return Ok(AgentResult::error(error_msg.to_string(), step_count, duration));
                }
                "invariant_violation" => {
                    info!(%session_id, "Invariant violation detected");
                }
                _ => {
                    info!(%session_id, event_type = %event.event_type, "Unknown event type");
                }
            }
        }

        let duration = start_time.elapsed().as_millis() as u64;
        Ok(AgentResult::error("Event stream closed".to_string(), step_count, duration))
    }

    fn build_call_external_command(&self, system_prompt: &str, goal: &str) -> Value {
        serde_json::json!({
            "type": "call_external",
            "params": {
                "model": self.config.model,
                "temperature": self.config.temperature,
                "system_prompt": system_prompt,
                "goal": goal,
                "tool_names": self.config.tool_names,
            }
        })
    }

    async fn handle_io_request(
        &self,
        payload: &Value,
        messages: &mut Vec<Message>,
        tool_calls: &mut Vec<String>,
    ) -> Result<Value, AgentError> {
        let io_type = payload
            .get("io_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Internal("missing io_type in IoRequest".to_string()))?;

        let params = payload.get("params").cloned().unwrap_or(Value::Null);

        match io_type {
            "call_external" => self.handle_call_external(&params, messages).await,
            "call_service" => self.handle_call_service(&params, messages, tool_calls).await,
            _ => Err(AgentError::Internal(format!("unsupported io_type: {}", io_type))),
        }
    }

    async fn handle_call_external(&self, params: &Value, messages: &mut Vec<Message>) -> Result<Value, AgentError> {
        let model = params.get("model").and_then(|v| v.as_str()).unwrap_or(&self.config.model);
        let temperature = params.get("temperature").and_then(|v| v.as_f64()).unwrap_or(self.config.temperature as f64);

        let serde_messages = serde_json::to_value(&mut *messages)
            .map_err(|e| AgentError::Internal(format!("serialize messages: {}", e)))?;
        let tcb_messages = serde_to_tcb(&serde_messages);

        let mut call_params = BTreeMap::new();
        call_params.insert("model".to_string(), JsonValue::string(model.to_string()));
        call_params.insert("temperature".to_string(), JsonValue::string(temperature.to_string()));
        call_params.insert("messages".to_string(), tcb_messages);

        let llm_result = self.execute_external("call_external", &JsonValue::Object(call_params)).await?;

        let llm_response: LlmResponse = serde_json::from_str(&llm_result.to_string())
            .map_err(|e| AgentError::Internal(format!("parse LLM response: {}", e)))?;

        messages.push(Message::Assistant {
            content: llm_response.content.clone(),
            tool_calls: llm_response.tool_calls.clone(),
        });

        Ok(serde_json::json!({
            "content": llm_response.content,
            "tool_calls": llm_response.tool_calls,
            "is_finished": llm_response.is_finished(),
        }))
    }

    async fn handle_call_service(
        &self,
        params: &Value,
        messages: &mut Vec<Message>,
        tool_calls: &mut Vec<String>,
    ) -> Result<Value, AgentError> {
        let tool_name = params.get("tool_name").and_then(|v| v.as_str()).ok_or_else(
            || AgentError::Internal("missing tool_name in call_service params".to_string()),
        )?;

        let args = params.get("args").cloned().unwrap_or(Value::Null);
        let args_tcb = serde_to_tcb(&args);

        let mut call_params = BTreeMap::new();
        call_params.insert("tool_name".to_string(), JsonValue::string(tool_name.to_string()));
        call_params.insert("args".to_string(), args_tcb);

        let tool_result = self.execute_external("call_service", &JsonValue::Object(call_params)).await?;

        tool_calls.push(tool_name.to_string());
        messages.push(Message::Tool {
            content: tool_result.to_string(),
            tool_name: tool_name.to_string(),
        });

        Ok(serde_json::json!({
            "tool_name": tool_name,
            "result": tool_result.to_string(),
        }))
    }

    async fn execute_external(&self, io_type: &str, params: &JsonValue) -> Result<JsonValue, AgentError> {
        tokio::time::timeout(self.config.step_timeout, async {
            let mut instr = BTreeMap::new();
            instr.insert("type".to_string(), JsonValue::string(io_type.to_string()));
            instr.insert("params".to_string(), params.clone());

            let tcb_instr = JsonValue::Object(instr);
            self.execute_io_request(&tcb_instr).await
        })
        .await
        .map_err(|_| AgentError::Timeout(format!("{} timeout", io_type)))?
    }

    async fn execute_io_request(&self, request: &JsonValue) -> Result<JsonValue, AgentError> {
        match request.get("type").and_then(|v| v.as_str()) {
            Some("call_external") => {
                let params = request.get("params").cloned().unwrap_or(JsonValue::Null);
                self.execute_llm_request(&params).await
            }
            Some("call_service") => {
                let params = request.get("params").cloned().unwrap_or(JsonValue::Null);
                self.execute_tool_request(&params).await
            }
            Some(t) => Err(AgentError::Internal(format!("unsupported io type: {}", t))),
            None => Err(AgentError::Internal("missing io type".to_string())),
        }
    }

    async fn execute_llm_request(&self, params: &JsonValue) -> Result<JsonValue, AgentError> {
        // 鐪熷疄璋冪敤 LlmHandler(宸茬敤 reqwest 瀹炵幇鐪熷疄 HTTP API)
        // - 璇?MINIMAX_API_KEY / DEEPSEEK_API_KEY / OPENAI_API_KEY 鐜鍙橀噺
        // - 鏀寔 messages / tools / temperature / max_tokens
        // - 杩斿洖瀹屾暣鐨?OpenAI 鍏煎 JSON 鍝嶅簲
        self.llm_handler
            .execute(params)
            .await
            .map_err(|e| AgentError::LlmError(e))
    }

    async fn execute_tool_request(&self, params: &JsonValue) -> Result<JsonValue, AgentError> {
        // Real call through ToolHandler (60s timeout, tool_not_found detection).
        // Replaces the previous stub that returned a hardcoded "Simulated tool result".
        self.tool_handler
            .execute(params)
            .await
            .map_err(AgentError::ToolError)
    }

    async fn auto_recall(&self, session_id: &str) -> Result<Vec<u64>, AgentError> {
        let shared_facts = self.evorule_client.get_shared_facts(Some("shared.")).await?;

        if shared_facts.is_empty() {
            info!(%session_id, "No shared facts to recall");
            return Ok(Vec::new());
        }

        let mut recalled_ids = Vec::new();
        let mut recalled_content = String::new();

        for fact in &shared_facts {
            recalled_ids.push(fact.fact_id);
            recalled_content.push_str(&format!(
                "[Fact {}] {}: {}\n",
                fact.fact_id,
                fact.path,
                serde_json::to_string(&fact.value).unwrap_or_default()
            ));
        }

        info!(%session_id, fact_count = recalled_ids.len(), "Auto-recalled shared facts");

        if let Some(memory) = self.memory.as_ref() {
            let mut mem = memory.clone();
            mem.set("auto_recall_context", &recalled_content).await?;
        }

        self.evorule_client.record_used_at_startup(session_id, &recalled_ids).await?;
        info!(%session_id, "Recorded used_at_startup");

        Ok(recalled_ids)
    }

    async fn auto_rewind(&self, session_id: &str) -> Result<u64, AgentError> {
        let history = self.evorule_client.get_facts(session_id, None).await?;

        if history.len() < 2 {
            return Err(AgentError::Internal("Not enough history to rewind".to_string()));
        }

        let target_version = history[history.len() - 2].version;
        info!(%session_id, target_version, "Attempting auto-rewind");

        let rewind_result = self.evorule_client.rewind(session_id, target_version).await?;
        let rewind_version = rewind_result["version"].as_u64().unwrap_or(target_version);

        info!(%session_id, rewind_version, "Auto-rewind completed");
        Ok(rewind_version)
    }

    pub async fn join_cluster(&mut self, cluster_id: &str) -> Result<(), AgentError> {
        if let Some(session_id) = &self.session_id {
            self.evorule_client.join_cluster(session_id, cluster_id).await?;
            self.join_cluster_id = Some(cluster_id.to_string());
            info!(%session_id, cluster_id, "Joined cluster");
        }
        Ok(())
    }

    pub async fn leave_cluster(&mut self) -> Result<(), AgentError> {
        if let Some(session_id) = &self.session_id {
            self.evorule_client.leave_cluster(session_id).await?;
            self.join_cluster_id = None;
            info!(%session_id, "Left cluster");
        }
        Ok(())
    }

    pub async fn get_cluster_status(&self) -> Result<Value, AgentError> {
        if let Some(session_id) = &self.session_id {
            self.evorule_client.get_cluster_status(session_id).await.map_err(|e| e.into())
        } else {
            Err(AgentError::Internal("No active session".to_string()))
        }
    }

    pub async fn replay_session(&self, session_id: &str) -> Result<Vec<Value>, AgentError> {
        self.evorule_client.replay(session_id).await.map_err(|e| e.into())
    }

    pub async fn diff_session(&self, session_id: &str, version_a: u64, version_b: u64) -> Result<Value, AgentError> {
        self.evorule_client.diff(session_id, version_a, version_b).await.map_err(|e| e.into())
    }

    pub async fn compare_strategies(
        &self,
        session_a: &str,
        session_b: &str,
    ) -> Result<Value, AgentError> {
        let history_a = self.evorule_client.get_facts(session_a, None).await?;
        let history_b = self.evorule_client.get_facts(session_b, None).await?;

        let version_a = history_a.last().map(|f| f.version).unwrap_or(0);
        let version_b = history_b.last().map(|f| f.version).unwrap_or(0);

        let diff_a = self.evorule_client.diff(session_a, 0, version_a).await?;
        let diff_b = self.evorule_client.diff(session_b, 0, version_b).await?;

        let replay_a = self.evorule_client.replay(session_a).await?;
        let replay_b = self.evorule_client.replay(session_b).await?;

        Ok(serde_json::json!({
            "session_a": {
                "id": session_a,
                "final_version": version_a,
                "history_length": history_a.len(),
                "diff": diff_a,
                "replay": replay_a,
            },
            "session_b": {
                "id": session_b,
                "final_version": version_b,
                "history_length": history_b.len(),
                "diff": diff_b,
                "replay": replay_b,
            },
        }))
    }

    pub fn run_streaming(
        self,
        goal: String,
    ) -> impl Stream<Item = Result<String, AgentError>> + 'static {
        let config = self.config;
        let evorule_client = self.evorule_client;
        let memory = self.memory;

        stream! {
            let start_time = std::time::Instant::now();

            let system_prompt = if let Some(mem) = memory {
                mem.build_system_prompt(&config.system_prompt)
            } else {
                config.system_prompt.clone()
            };

            let session_id = match evorule_client.create_session(None).await {
                Ok(id) => id,
                Err(e) => {
                    yield Err(AgentError::EvoruleError(e.to_string()));
                    return;
                }
            };

            yield Ok(format!("Session created: {}", session_id));

            let command = serde_json::json!({
                "type": "call_external",
                "params": {
                    "model": config.model,
                    "temperature": config.temperature,
                    "system_prompt": &system_prompt,
                    "goal": &goal,
                    "tool_names": config.tool_names,
                }
            });

            if let Err(e) = evorule_client.submit_command(&session_id, &command).await {
                yield Err(AgentError::EvoruleError(e.to_string()));
                return;
            }

            yield Ok("Command submitted, waiting for events...".to_string());

            let mut event_stream = match evorule_client.subscribe_events(&session_id).await {
                Ok(s) => s,
                Err(e) => {
                    yield Err(AgentError::EvoruleError(e.to_string()));
                    return;
                }
            };

            let mut step_count = 0;

            while let Some(event) = event_stream.next().await {
                match event.event_type.as_str() {
                    "io_request" => {
                        step_count += 1;
                        yield Ok(format!("Step {}: Received io_request", step_count));

                        let io_type = event.payload.get("io_type").and_then(|v| v.as_str()).unwrap_or("unknown");
                        yield Ok(format!("Executing {}...", io_type));

                        let result = serde_json::json!({"content": "Simulated response"});
                        if let Some(request_id) = event.payload.get("id").and_then(|v| v.as_u64()) {
                            if let Err(e) = evorule_client.submit_io_response(&session_id, request_id, &result, None).await {
                                yield Err(AgentError::EvoruleError(e.to_string()));
                                return;
                            }
                            yield Ok(format!("Submitted io_response for request {}", request_id));
                        }
                    }
                    "stable" => {
                        let duration = start_time.elapsed().as_millis() as u64;
                        yield Ok(format!("Done in {}ms after {} steps", duration, step_count));
                        return;
                    }
                    "error" => {
                        let msg = event.payload.get("message").and_then(|v| v.as_str()).unwrap_or("unknown");
                        yield Err(AgentError::EvoruleError(msg.to_string()));
                        return;
                    }
                    _ => {
                        yield Ok(format!("Event: {}", event.event_type));
                    }
                }
            }
        }
    }
}

pub fn merge_delegate_tool(
    _tool_name: &str,
    args: &JsonValue,
    delegate_context: &DelegateContext,
) -> JsonValue {
    let mut merged = args.clone();
    if let JsonValue::Object(map) = &mut merged {
        map.insert(
            "delegate_depth".to_string(),
            JsonValue::integer(delegate_context.current_depth as i64),
        );
        map.insert(
            "parent_agent".to_string(),
            JsonValue::string(delegate_context.parent_agent_type.clone()),
        );
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:8080")
    }

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();
        assert_eq!(config.agent_type, "default");
        assert_eq!(config.model, "gpt-4o-mini");
        assert!((config.temperature - 0.7).abs() < 0.01);
        assert_eq!(config.max_steps, 10);
        assert_eq!(config.step_timeout, Duration::from_secs(60));
        assert!(config.tool_names.is_empty());
        assert_eq!(config.llm_retry_count, 3);
    }

    #[test]
    fn test_agent_result_success() {
        let result = AgentResult::success("hello".to_string(), 3, 100, vec!["tool1".to_string()]);
        assert!(result.success);
        assert_eq!(result.content, "hello");
        assert_eq!(result.steps, 3);
        assert_eq!(result.duration_ms, 100);
        assert_eq!(result.tool_calls, vec!["tool1"]);
        assert!(result.error.is_none());
    }

    #[test]
    fn test_agent_result_error() {
        let result = AgentResult::error("timeout".to_string(), 5, 5000);
        assert!(!result.success);
        assert!(result.content.is_empty());
        assert_eq!(result.steps, 5);
        assert_eq!(result.error, Some("timeout".to_string()));
    }

    #[test]
    fn test_agent_error_display() {
        let err = AgentError::MaxStepsExceeded(10);
        assert!(format!("{}", err).contains("10"));

        let err = AgentError::Timeout("test".to_string());
        assert!(format!("{}", err).contains("Timeout"));

        let err = AgentError::LlmError("API down".to_string());
        assert!(format!("{}", err).contains("LLM error"));

        let err = AgentError::EvoruleError("connection failed".to_string());
        assert!(format!("{}", err).contains("Evorule error"));
    }

    #[test]
    fn test_merge_delegate_tool() {
        let delegate_ctx = DelegateContext {
            current_depth: 2,
            parent_agent_type: "parent".to_string(),
            definitions: crate::agent::definition::AgentDefinitionManager::with_default_dir(),
            evorule_client: make_test_client(),
        };

        let args = serde_json::json!({"query": "test"});
        let tcb_args = serde_to_tcb(&args);
        let merged = merge_delegate_tool("search", &tcb_args, &delegate_ctx);

        assert_eq!(
            merged.get("delegate_depth").and_then(|v| v.as_i64()),
            Some(2)
        );
        assert_eq!(
            merged.get("parent_agent").and_then(|v| v.as_str()),
            Some("parent")
        );
        assert_eq!(merged.get("query").and_then(|v| v.as_str()), Some("test"));
    }

    #[test]
    fn test_build_call_external_command() {
        let config = AgentConfig::default();
        let client = make_test_client();
        let runner = AgentRunner::new(config, client);

        let command = runner.build_call_external_command("system prompt", "test goal");
        assert_eq!(command["type"], "call_external");
        assert_eq!(command["params"]["model"], "gpt-4o-mini");
        assert_eq!(command["params"]["goal"], "test goal");
        assert_eq!(command["params"]["system_prompt"], "system prompt");
    }

    #[test]
    fn test_agent_runner_new() {
        let config = AgentConfig::default();
        let client = make_test_client();
        let runner = AgentRunner::new(config, client);

        assert!(runner.session_id.is_none());
        assert!(runner.memory.is_none());
        assert!(runner.delegate_context.is_none());
    }

    #[tokio::test]
    async fn test_auto_recall_empty_shared_facts() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/shared/facts?prefix=shared.")
            .with_status(200)
            .with_body("[]")
            .create_async()
            .await;

        let config = AgentConfig::default();
        let runner = AgentRunner::new(config, client);

        let result = runner.auto_recall("test-session").await;

        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_auto_recall_api_error_fetching_facts() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/shared/facts?prefix=shared.")
            .with_status(500)
            .with_body("Internal server error")
            .create_async()
            .await;

        let config = AgentConfig::default();
        let runner = AgentRunner::new(config, client);

        let result = runner.auto_recall("test-session").await;

        assert!(result.is_err());
        assert!(matches!(result.err().unwrap(), AgentError::EvoruleError(_)));
    }

    #[tokio::test]
    async fn test_auto_recall_api_error_recording_used_at_startup() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/shared/facts?prefix=shared.")
            .with_status(200)
            .with_body(r#"[{"fact_id": 1, "path": "shared.test", "value": "test", "source_session_id": 100, "version": 1}]"#)
            .create_async()
            .await;

        server.mock("POST", "/api/sessions/test-session/used_at_startup")
            .with_status(500)
            .with_body("Internal server error")
            .create_async()
            .await;

        let config = AgentConfig::default();
        let runner = AgentRunner::new(config, client);

        let result = runner.auto_recall("test-session").await;

        assert!(result.is_err());
        assert!(matches!(result.err().unwrap(), AgentError::EvoruleError(_)));
    }

    #[tokio::test]
    async fn test_auto_recall_with_memory() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/shared/facts?prefix=shared.")
            .with_status(200)
            .with_body(r#"[{"fact_id": 1, "path": "shared.knowledge", "value": "important info", "source_session_id": 100, "version": 1}]"#)
            .create_async()
            .await;

        server.mock("POST", "/api/sessions/test-session/used_at_startup")
            .with_status(200)
            .with_body(r#"{"success": true}"#)
            .create_async()
            .await;

        server.mock("POST", "/api/sessions/test-session/payload")
            .with_status(200)
            .with_body(r#"{"success": true}"#)
            .create_async()
            .await;

        server.mock("GET", "/api/sessions/test-session/payload")
            .with_status(200)
            .with_body(r#"{"auto_recall_context": "[Fact 1] shared.knowledge: \"important info\"\n"}"#)
            .create_async()
            .await;

        let config = AgentConfig::default();
        let memory = crate::agent::memory::MemoryManager::new("test", client.clone()).with_session_id("test-session");
        let runner = AgentRunner::new(config, client).with_memory(memory);

        let result = runner.auto_recall("test-session").await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn test_auto_recall_with_multiple_facts() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/shared/facts?prefix=shared.")
            .with_status(200)
            .with_body(r#"[{"fact_id": 1, "path": "shared.guideline", "value": "safety rule", "source_session_id": 100, "version": 1}, {"fact_id": 2, "path": "shared.knowledge", "value": "domain knowledge", "source_session_id": 200, "version": 2}, {"fact_id": 3, "path": "shared.policy", "value": "company policy", "source_session_id": 300, "version": 3}]"#)
            .create_async()
            .await;

        server.mock("POST", "/api/sessions/test-session/used_at_startup")
            .with_status(200)
            .with_body(r#"{"success": true}"#)
            .create_async()
            .await;

        let config = AgentConfig::default();
        let runner = AgentRunner::new(config, client);

        let result = runner.auto_recall("test-session").await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_auto_rewind_not_enough_history() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(200)
            .with_body(r#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}]"#)
            .create_async()
            .await;

        let config = AgentConfig::default();
        let runner = AgentRunner::new(config, client);

        let result = runner.auto_rewind("test-session").await;

        assert!(result.is_err());
        assert!(matches!(result.err().unwrap(), AgentError::Internal(msg) if msg.contains("Not enough history")));
    }

    #[tokio::test]
    async fn test_auto_rewind_empty_history() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(200)
            .with_body("[]")
            .create_async()
            .await;

        let config = AgentConfig::default();
        let runner = AgentRunner::new(config, client);

        let result = runner.auto_rewind("test-session").await;

        assert!(result.is_err());
        assert!(matches!(result.err().unwrap(), AgentError::Internal(msg) if msg.contains("Not enough history")));
    }

    #[tokio::test]
    async fn test_auto_rewind_api_error_fetching_facts() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(500)
            .with_body("Internal server error")
            .create_async()
            .await;

        let config = AgentConfig::default();
        let runner = AgentRunner::new(config, client);

        let result = runner.auto_rewind("test-session").await;

        assert!(result.is_err());
        assert!(matches!(result.err().unwrap(), AgentError::EvoruleError(_)));
    }

    #[tokio::test]
    async fn test_auto_rewind_api_error_rewinding() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(200)
            .with_body(r#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 1, "id": 2, "path": "command", "value": "test", "type": "Command"}]"#)
            .create_async()
            .await;

        server.mock("GET", "/api/sessions/test-session/rewind/0")
            .with_status(500)
            .with_body("Internal server error")
            .create_async()
            .await;

        let config = AgentConfig::default();
        let runner = AgentRunner::new(config, client);

        let result = runner.auto_rewind("test-session").await;

        assert!(result.is_err());
        assert!(matches!(result.err().unwrap(), AgentError::EvoruleError(_)));
    }

    #[tokio::test]
    async fn test_auto_rewind_missing_version_in_response() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(200)
            .with_body(r#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 1, "id": 2, "path": "command", "value": "test", "type": "Command"}]"#)
            .create_async()
            .await;

        server.mock("GET", "/api/sessions/test-session/rewind/0")
            .with_status(200)
            .with_body(r#"{"payload": {}, "queue": []}"#)
            .create_async()
            .await;

        let config = AgentConfig::default();
        let runner = AgentRunner::new(config, client);

        let result = runner.auto_rewind("test-session").await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_auto_rewind_success() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(200)
            .with_body(r#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 1, "id": 2, "path": "command", "value": "test", "type": "Command"}, {"version": 2, "id": 3, "path": "error", "value": "error", "type": "Error"}]"#)
            .create_async()
            .await;

        server.mock("GET", "/api/sessions/test-session/rewind/1")
            .with_status(200)
            .with_body(r#"{"version": 1, "payload": {}, "queue": []}"#)
            .create_async()
            .await;

        let config = AgentConfig::default();
        let runner = AgentRunner::new(config, client);

        let result = runner.auto_rewind("test-session").await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_auto_rewind_non_sequential_versions() {
        let mut server = mockito::Server::new_async().await;
        let client = EvoruleApiClient::new(&server.url());

        server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(200)
            .with_body(r#"[{"version": 5, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 12, "id": 2, "path": "command", "value": "test", "type": "Command"}, {"version": 47, "id": 3, "path": "error", "value": "error", "type": "Error"}]"#)
            .create_async()
            .await;

        server.mock("GET", "/api/sessions/test-session/rewind/12")
            .with_status(200)
            .with_body(r#"{"version": 12, "payload": {}, "queue": []}"#)
            .create_async()
            .await;

        let config = AgentConfig::default();
        let runner = AgentRunner::new(config, client);

        let result = runner.auto_rewind("test-session").await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 12);
    }
}
