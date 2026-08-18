// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! LLM I/O Handler -- invokes external LLM API
//!
//! ## G3:重试机制
//!
//! `execute()` 内置指数退避重试,触发条件:
//! - HTTP 429 / 500 / 502 / 503 / 504
//! - 网络错误(timeout / connect / request)
//!
//! 不重试:400 / 401 / 403(客户端错误,重试无意义)。
//! 退避算法:`min(base * 2^attempt, max)`,叠加 ±20% 抖动。

use std::collections::BTreeMap;
use std::time::Duration;

use async_stream::stream;
use evorule_tcb::JsonValue;
use futures_core::Stream;
use futures_util::StreamExt;
use rand::Rng;
use serde_json;
use tracing::{debug, warn};

use crate::agent::translator::{LlmResponse, TokenUsage, ToolCall};
use crate::io_handler::{IoHandler, IoResult};
use crate::json_convert::serde_to_tcb;

/// 默认最大重试次数
const DEFAULT_MAX_RETRIES: usize = 3;
/// 默认初始退避(秒)
const DEFAULT_BASE_BACKOFF_SECS: f64 = 1.0;
/// 默认最大退避(秒)
const DEFAULT_MAX_BACKOFF_SECS: f64 = 30.0;

/// G1:LLM 流式输出的单个分片
///
/// `execute_stream` 依次产出:
/// ```text
/// Delta(text) * N  →  (ToolCallDelta * N)?  →  Done(LlmResponse)
/// ```
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// 文本增量(本次帧的 delta 文本,非累积)
    Delta(String),
    /// tool_call 增量(可能多次,需 caller 聚合)
    ToolCallDelta {
        /// tool_call 的索引(OpenAI SSE 按 index 分片)
        index: usize,
        /// 增量 JSON 片段(可能是部分 JSON,需拼接后再解析)
        fragment: String,
    },
    /// 流结束,携带完整 `LlmResponse`(聚合自所有 delta)
    Done(LlmResponse),
    /// 流内部非致命警告(如单帧解析失败,流继续)
    Warn(String),
}

/// G1:tool_call 聚合器(按 SSE index 累积)
#[derive(Debug, Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    /// arguments 分片累积(OpenAI 分多帧发)
    arguments: String,
}

impl ToolCallAccumulator {
    /// 把累积结果转为 `ToolCall`(translator.rs 的类型)
    ///
    /// arguments 是分片拼接的字符串,最终解析为 JSON Value。
    /// 如果拼接后不是合法 JSON,fallback 到空对象 `{}`。
    fn to_tool_call(&self) -> Option<ToolCall> {
        let name = self.name.clone()?;
        let args_str = if self.arguments.is_empty() {
            "{}".to_string()
        } else {
            self.arguments.clone()
        };
        let arguments: serde_json::Value =
            serde_json::from_str(&args_str).unwrap_or(serde_json::json!({}));
        Some(ToolCall { name, arguments })
    }
}

/// LLM I/O Handler -- invokes external LLM API
#[derive(Debug, Clone)]
pub struct LlmHandler {
    default_model: String,
    api_base: String,
    api_key: Option<String>,
    /// If set, skip HTTP and return a canned LlmResponse-format JSON.
    /// Used by tests to avoid hitting a real LLM API.
    mock_content: Option<String>,
    /// G3:最大重试次数(0 = 不重试,只发一次请求)
    max_retries: usize,
    /// G3:初始退避(秒)
    base_backoff_secs: f64,
    /// G3:最大退避(秒)
    max_backoff_secs: f64,
}

impl LlmHandler {
    /// Create new LLM handler
    pub fn new(default_model: &str, api_base: &str, api_key: Option<String>) -> Self {
        Self {
            default_model: default_model.to_string(),
            api_base: api_base.to_string(),
            api_key,
            mock_content: None,
            max_retries: DEFAULT_MAX_RETRIES,
            base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
            max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
        }
    }

    /// Create LLM handler with default config
    ///
    /// Priority: MiniMax > DeepSeek > OpenAI
    pub fn with_defaults() -> Self {
        if let Ok(api_key) = std::env::var("MINIMAX_API_KEY") {
            Self {
                default_model: std::env::var("MINIMAX_MODEL")
                    .unwrap_or_else(|_| "MiniMax-M2.5".to_string()),
                api_base: std::env::var("MINIMAX_API_BASE").unwrap_or_else(|_| {
                    "https://api.minimaxi.com/v1/text/chatcompletion_v2".to_string()
                }),
                api_key: Some(api_key),
                mock_content: None,
                max_retries: DEFAULT_MAX_RETRIES,
                base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
                max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
            }
        } else if let Ok(api_key) = std::env::var("DEEPSEEK_API_KEY") {
            Self {
                default_model: std::env::var("DEEPSEEK_MODEL")
                    .unwrap_or_else(|_| "deepseek-chat".to_string()),
                api_base: std::env::var("DEEPSEEK_API_BASE")
                    .unwrap_or_else(|_| "https://api.deepseek.com/v1/chat/completions".to_string()),
                api_key: Some(api_key),
                mock_content: None,
                max_retries: DEFAULT_MAX_RETRIES,
                base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
                max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
            }
        } else if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
            Self {
                default_model: std::env::var("OPENAI_MODEL")
                    .unwrap_or_else(|_| "gpt-4o-mini".to_string()),
                api_base: std::env::var("OPENAI_API_BASE")
                    .unwrap_or_else(|_| "https://api.openai.com/v1/chat/completions".to_string()),
                api_key: Some(api_key),
                mock_content: None,
                max_retries: DEFAULT_MAX_RETRIES,
                base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
                max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
            }
        } else {
            Self {
                default_model: "MiniMax-M2.5".to_string(),
                api_base: "https://api.minimaxi.com/v1/text/chatcompletion_v2".to_string(),
                api_key: None,
                mock_content: None,
                max_retries: DEFAULT_MAX_RETRIES,
                base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
                max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
            }
        }
    }

    /// G3:从 `LlmConfig` 构造 handler(由 CLI `cmd_run` 调用)
    ///
    /// 把 `config.llm.max_retries` 真正接到 handler 上,复活死代码。
    pub fn from_config(config: &crate::config::LlmConfig) -> Self {
        Self {
            default_model: config.model.clone(),
            api_base: config.api_base.clone(),
            api_key: if config.api_key.is_empty() {
                None
            } else {
                Some(config.api_key.clone())
            },
            mock_content: None,
            max_retries: config.max_retries,
            base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
            max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
        }
    }

    /// G3:自定义重试参数(builder 风格,测试时注入极小退避避免拖慢)
    pub fn with_retry_config(
        mut self,
        max_retries: usize,
        base_backoff_secs: f64,
        max_backoff_secs: f64,
    ) -> Self {
        self.max_retries = max_retries;
        self.base_backoff_secs = base_backoff_secs;
        self.max_backoff_secs = max_backoff_secs;
        self
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
            max_retries: 0, // mock 不重试
            base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
            max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
        }
    }

    /// Returns true if this handler is a mock (skips HTTP).
    pub fn is_mock(&self) -> bool {
        self.mock_content.is_some()
    }

    /// G3:构造请求体(从 IoRequest 参数提取 messages/tools 等)
    ///
    /// 拆出独立方法,便于 `execute` 和未来的 `execute_stream`(G1)复用。
    fn build_request_body(&self, params: &JsonValue) -> Result<serde_json::Value, String> {
        let params_str = params.to_string();
        let params_val: serde_json::Value =
            serde_json::from_str(&params_str).map_err(|e| format!("parse params: {}", e))?;

        let model = params_val
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_model);

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

        Ok(serde_json::Value::Object(body))
    }

    /// G3:发起单次 HTTP 请求(不含重试逻辑)
    ///
    /// 返回 `Ok(resp)` 表示请求成功送达(不代表业务成功,状态码可能 4xx/5xx),
    /// 返回 `Err(e)` 表示网络层错误。
    async fn do_http_call(
        &self,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let client = reqwest::Client::new();
        let mut request = client.post(&self.api_base).json(body);

        if let Some(api_key) = &self.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }

        request.send().await
    }

    /// G3:解析 2xx 响应为 LlmResponse 格式 JSON
    ///
    /// 把 OpenAI 兼容的响应格式转换为内部统一的 LlmResponse 结构:
    /// - `choices[0].message.content`   → content
    /// - `choices[0].message.tool_calls` → tool_calls
    /// - `choices[0].finish_reason`     → finish_reason
    /// - `usage`                        → token_usage
    async fn parse_success_response(&self, resp: reqwest::Response) -> Result<JsonValue, String> {
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("LLM API response parse error: {}", e))?;

        let choice = json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first());

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

        debug!(
            content_len = content.len(),
            finish_reason = ?finish_reason,
            "LLM API response parsed"
        );
        Ok(serde_to_tcb(&response_json))
    }

    /// G1:流式调用 LLM API(OpenAI 兼容 SSE)
    ///
    /// 返回一个 `Stream`,依次产出:
    /// ```text
    /// Delta(text) * N  →  (ToolCallDelta * N)?  →  Done(LlmResponse)
    /// ```
    ///
    /// 即使 LLM 中途返回 tool_calls,本方法也会在 `Done` 里返回
    /// 完整聚合的 `LlmResponse`,与 `execute()` 输出格式一致。
    ///
    /// ## 不重试
    ///
    /// SSE 流中断后不重试(会重复输出)。调用方收到 `Err` 后,
    /// 可 fallback 到非流式 `execute()`(带 G3 重试)。
    ///
    /// ## Mock
    ///
    /// 如果 `mock_content` 已设置,直接 yield `Delta` + `Done`,不发 HTTP。
    pub fn execute_stream(
        &self,
        params: &JsonValue,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<StreamChunk, String>> + Send>> {
        let body_result = self.build_request_body(params);
        let api_base = self.api_base.clone();
        let api_key = self.api_key.clone();
        let mock_content = self.mock_content.clone();

        Box::pin(stream! {
            // === Mock 短路 ===
            if let Some(content) = &mock_content {
                yield Ok(StreamChunk::Delta(content.clone()));
                yield Ok(StreamChunk::Done(LlmResponse {
                    content: content.clone(),
                    tool_calls: None,
                    finish_reason: Some("stop".to_string()),
                    token_usage: None,
                }));
                return;
            }

            // === 构造请求体(加 stream: true) ===
            let mut body = match body_result {
                Ok(b) => b,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            if let Some(obj) = body.as_object_mut() {
                obj.insert("stream".to_string(), serde_json::Value::Bool(true));
            }

            // === HTTP 请求 ===
            let client = reqwest::Client::new();
            let mut request = client.post(&api_base).json(&body);
            if let Some(key) = &api_key {
                request = request.header("Authorization", format!("Bearer {}", key));
            }

            let resp = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    yield Err(format!("LLM stream request failed: {}", e));
                    return;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                yield Err(format!("LLM API error ({}): {}", status, text));
                return;
            }

            // === SSE 解析 ===
            let mut byte_stream = resp.bytes_stream();
            let mut buf = String::new();
            let mut byte_buf: Vec<u8> = Vec::new(); // 跨 chunk 的未完成 UTF-8 字节
            let mut content_acc = String::new();
            let mut tool_calls_acc: BTreeMap<usize, ToolCallAccumulator> = BTreeMap::new();
            let mut finish_reason: Option<String> = None;
            let mut token_usage: Option<TokenUsage> = None;

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(format!("LLM stream interrupted: {}", e));
                        return;
                    }
                };

                // 追加到字节缓冲区,然后安全解码(处理跨 chunk 的多字节 UTF-8 字符)
                byte_buf.extend_from_slice(&chunk);
                match std::str::from_utf8(&byte_buf) {
                    Ok(s) => {
                        buf.push_str(s);
                        byte_buf.clear();
                    }
                    Err(e) => {
                        let safe_len = e.valid_up_to();
                        if safe_len > 0 {
                            let safe_part = std::str::from_utf8(&byte_buf[..safe_len]).unwrap();
                            buf.push_str(safe_part);
                            byte_buf = byte_buf[safe_len..].to_vec();
                        }
                        // safe_len == 0 表示字节不足一个完整字符,等待下一个 chunk
                    }
                }

                // 提取完整帧(以 \n\n 分隔)
                while let Some(pos) = buf.find("\n\n") {
                    let frame: String = buf.drain(..pos + 2).collect();
                    let frame_str = frame.trim_end_matches('\n').trim_end_matches('\r');

                    // 提取 data: 行
                    let data: String = frame_str
                        .lines()
                        .filter_map(|line| line.strip_prefix("data:").map(|s| s.trim_start()))
                        .collect::<Vec<_>>()
                        .join("\n");

                    if data.is_empty() {
                        continue;
                    }

                    // 检查 [DONE]
                    if data.trim() == "[DONE]" {
                        let tool_calls = build_tool_calls(&tool_calls_acc);
                        yield Ok(StreamChunk::Done(LlmResponse {
                            content: content_acc.clone(),
                            tool_calls,
                            finish_reason: finish_reason.take().or_else(|| Some("stop".to_string())),
                            token_usage: token_usage.take(),
                        }));
                        return;
                    }

                    // 解析 JSON
                    let json: serde_json::Value = match serde_json::from_str(&data) {
                        Ok(v) => v,
                        Err(e) => {
                            yield Ok(StreamChunk::Warn(format!(
                                "failed to parse SSE frame: {}",
                                e
                            )));
                            continue;
                        }
                    };

                    // 提取 choices[0]
                    let choice = json
                        .get("choices")
                        .and_then(|c| c.as_array())
                        .and_then(|c| c.first());

                    if let Some(choice) = choice {
                        let delta = choice.get("delta");

                        // Content delta
                        if let Some(content) = delta
                            .and_then(|d| d.get("content"))
                            .and_then(|c| c.as_str())
                        {
                            if !content.is_empty() {
                                content_acc.push_str(content);
                                yield Ok(StreamChunk::Delta(content.to_string()));
                            }
                        }

                        // Tool call deltas
                        if let Some(tcs) = delta
                            .and_then(|d| d.get("tool_calls"))
                            .and_then(|c| c.as_array())
                        {
                            for tc in tcs {
                                let index = tc
                                    .get("index")
                                    .and_then(|i| i.as_u64())
                                    .unwrap_or(0) as usize;
                                let entry = tool_calls_acc.entry(index).or_default();

                                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                    entry.id = Some(id.to_string());
                                }
                                if let Some(func) = tc.get("function") {
                                    if let Some(name) =
                                        func.get("name").and_then(|n| n.as_str())
                                    {
                                        entry.name = Some(name.to_string());
                                    }
                                    if let Some(args) =
                                        func.get("arguments").and_then(|a| a.as_str())
                                    {
                                        entry.arguments.push_str(args);
                                        yield Ok(StreamChunk::ToolCallDelta {
                                            index,
                                            fragment: args.to_string(),
                                        });
                                    }
                                }
                            }
                        }

                        // Finish reason
                        if let Some(fr) = choice
                            .get("finish_reason")
                            .and_then(|f| f.as_str())
                        {
                            finish_reason = Some(fr.to_string());
                        }
                    }

                    // Usage(部分 provider 在最后一帧带 usage)
                    if let Some(usage) = json.get("usage") {
                        // 尝试解析为 TokenUsage(字段不匹配时保留 None)
                        if let Ok(tu) = serde_json::from_value::<TokenUsage>(usage.clone()) {
                            token_usage = Some(tu);
                        }
                    }
                }
            }

            // 流自然结束(未收到 [DONE],部分 provider 不发)
            let tool_calls = build_tool_calls(&tool_calls_acc);
            yield Ok(StreamChunk::Done(LlmResponse {
                content: content_acc.clone(),
                tool_calls,
                finish_reason: finish_reason.take().or_else(|| Some("stop".to_string())),
                token_usage: token_usage.take(),
            }));
        })
    }
}

/// G1:把 tool_call 聚合器转为 `LlmResponse` 期望的 `Vec<ToolCall>`
fn build_tool_calls(acc: &BTreeMap<usize, ToolCallAccumulator>) -> Option<Vec<ToolCall>> {
    if acc.is_empty() {
        return None;
    }
    let result: Vec<ToolCall> = acc.values().filter_map(|tc| tc.to_tool_call()).collect();
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// G3:判断 HTTP 状态码是否值得重试
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
}

/// G3:判断网络层错误是否值得重试
fn is_retryable_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

/// G3:计算第 `attempt` 次重试的退避时间(指数 + ±20% 抖动)
///
/// - `attempt`:已失败的次数(0 = 第 1 次重试前的等待)
/// - `base`:初始退避(秒)
/// - `max`:退避上限(秒)
fn backoff_duration(attempt: usize, base: f64, max: f64) -> Duration {
    let exp = base * 2f64.powi(attempt as i32);
    let capped = exp.min(max);
    // jitter:±20%
    let mut rng = rand::thread_rng();
    let jitter = (rng.gen::<f64>() - 0.5) * 0.4 * capped;
    let final_secs = (capped + jitter).max(0.1);
    Duration::from_secs_f64(final_secs)
}

#[async_trait::async_trait]
impl IoHandler for LlmHandler {
    /// Execute LLM API invocation
    ///
    /// G3:内置指数退避重试。重试触发:429/5xx + 网络错误。
    async fn execute(&self, params: &JsonValue) -> IoResult {
        // Mock LLM short-circuit (used by tests).
        // 重试逻辑不适用 mock,直接返回。
        if let Some(content) = &self.mock_content {
            let response = serde_json::json!({
                "content": content,
                "tool_calls": null,
                "finish_reason": "stop",
                "token_usage": null,
            });
            return Ok(serde_to_tcb(&response));
        }

        let body = self.build_request_body(params)?;
        let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
        debug!(model = model, "ready to invoke LLM API");

        // G3:重试循环。attempt = 0 是首次请求,1..=max_retries 是重试。
        for attempt in 0..=self.max_retries {
            let result = self.do_http_call(&body).await;

            match result {
                // HTTP 请求送达 + 2xx
                Ok(resp) if resp.status().is_success() => {
                    if attempt > 0 {
                        debug!(attempt, "LLM API recovered after retry");
                    }
                    return self.parse_success_response(resp).await;
                }

                // HTTP 请求送达 + 可重试状态码 + 还有重试次数
                Ok(resp) if is_retryable_status(resp.status()) && attempt < self.max_retries => {
                    let status = resp.status();
                    // 优先读 Retry-After header(秒),没有则用指数退避
                    let backoff = parse_retry_after(&resp).unwrap_or_else(|| {
                        backoff_duration(attempt, self.base_backoff_secs, self.max_backoff_secs)
                    });
                    warn!(
                        attempt,
                        status = status.as_u16(),
                        backoff_ms = backoff.as_millis() as u64,
                        "LLM API retryable status, backing off"
                    );
                    // 消费 body 以释放连接(否则 reqwest 会泄漏 socket)
                    let _ = resp.text().await;
                    tokio::time::sleep(backoff).await;
                    continue;
                }

                // HTTP 请求送达 + 不可重试状态码 或 重试耗尽
                Ok(resp) => {
                    let status = resp.status();
                    let error_text = resp.text().await.unwrap_or_default();
                    if attempt == self.max_retries && is_retryable_status(status) {
                        warn!(
                            attempt,
                            status = status.as_u16(),
                            "LLM API retries exhausted"
                        );
                    } else {
                        warn!(status = status.as_u16(), "LLM API non-retryable error");
                    }
                    return Err(format!("LLM API error ({}): {}", status, error_text));
                }

                // 网络错误 + 可重试 + 还有重试次数
                Err(e) if is_retryable_error(&e) && attempt < self.max_retries => {
                    let backoff =
                        backoff_duration(attempt, self.base_backoff_secs, self.max_backoff_secs);
                    warn!(
                        attempt,
                        error = %e,
                        backoff_ms = backoff.as_millis() as u64,
                        "LLM API network error, backing off"
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }

                // 网络错误 + 不可重试 或 重试耗尽
                Err(e) => {
                    if attempt == self.max_retries {
                        warn!(attempt, error = %e, "LLM API retries exhausted");
                    } else {
                        warn!(error = %e, "LLM API non-retryable network error");
                    }
                    return Err(format!("LLM API request failed: {}", e));
                }
            }
        }

        // 理论不可达:循环要么 return,要么 continue;max_retries=0 时第一次迭代必 return
        Err("LLM API retry loop exhausted without return".to_string())
    }
}

/// G3:解析 `Retry-After` header
///
/// 支持两种格式:
/// - 整数秒:`Retry-After: 120`
/// - HTTP 日期:`Retry-After: Wed, 21 Oct 2026 07:28:00 GMT`(暂不支持,返回 None)
///
/// 上限 60 秒(避免 server 端配错导致长时间挂起)。
fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let header = resp.headers().get(reqwest::header::RETRY_AFTER)?;
    let s = header.to_str().ok()?;
    let secs: u64 = s.parse().ok()?;
    // 上限 60 秒,防止 server 配错
    let capped = secs.min(60);
    Some(Duration::from_secs(capped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handler::IoHandler;

    #[test]
    fn test_llm_handler_new() {
        let handler = LlmHandler::new("gpt-4", "https://api.example.com", Some("key".to_string()));
        assert_eq!(handler.default_model, "gpt-4");
        assert_eq!(handler.api_base, "https://api.example.com");
        assert!(handler.api_key.is_some());
        // G3:默认重试参数
        assert_eq!(handler.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(handler.base_backoff_secs, DEFAULT_BASE_BACKOFF_SECS);
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

    #[test]
    fn test_llm_handler_from_config_carries_max_retries() {
        // G3:验证 from_config 真正把 config.max_retries 传到 handler
        use crate::config::LlmConfig;
        let mut cfg = LlmConfig::default();
        cfg.max_retries = 5;
        cfg.api_key = "test-key".to_string();
        let handler = LlmHandler::from_config(&cfg);
        assert_eq!(handler.max_retries, 5);
        assert_eq!(handler.api_key, Some("test-key".to_string()));
        assert_eq!(handler.default_model, cfg.model);
    }

    #[test]
    fn test_llm_handler_with_retry_config_builder() {
        let handler = LlmHandler::new("m", "https://x", None).with_retry_config(10, 0.5, 5.0);
        assert_eq!(handler.max_retries, 10);
        assert_eq!(handler.base_backoff_secs, 0.5);
        assert_eq!(handler.max_backoff_secs, 5.0);
    }

    #[test]
    fn test_llm_handler_mock_has_zero_retries() {
        // mock 不重试,避免测试时出现意外 sleep
        let handler = LlmHandler::mock("hello");
        assert_eq!(handler.max_retries, 0);
    }

    #[tokio::test]
    async fn test_llm_handler_mock_returns_canned_response() {
        let handler = LlmHandler::mock("hello from mock");
        assert!(handler.is_mock());

        let params = evorule_tcb::JsonValue::empty_object();
        let result = handler.execute(&params).await.expect("mock execute");
        let s = result.to_string();
        // Must be parseable as LlmResponse (handle_call_external parses it this way)
        let parsed: serde_json::Value =
            serde_json::from_str(&s).expect("mock response is valid JSON");
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

    // ========== G3 重试机制单元测试 ==========

    #[test]
    fn test_is_retryable_status() {
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS)); // 429
        assert!(is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        )); // 500
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY)); // 502
        assert!(is_retryable_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        )); // 503
        assert!(is_retryable_status(reqwest::StatusCode::GATEWAY_TIMEOUT)); // 504

        // 不可重试
        assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST)); // 400
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED)); // 401
        assert!(!is_retryable_status(reqwest::StatusCode::FORBIDDEN)); // 403
        assert!(!is_retryable_status(reqwest::StatusCode::NOT_FOUND)); // 404
        assert!(!is_retryable_status(reqwest::StatusCode::OK)); // 200
    }

    #[test]
    fn test_backoff_duration_monotonic_and_capped() {
        // 验证:退避值不超过 max * 1.2(cap + ±20% 抖动),不低于 0.1(下限)
        let base = 1.0;
        let max = 10.0;
        for attempt in 0..10 {
            let d = backoff_duration(attempt, base, max);
            let secs = d.as_secs_f64();
            // 算法:capped = min(base * 2^attempt, max);jitter = ±20% * capped
            let exp = base * 2f64.powi(attempt as i32);
            let capped = exp.min(max);
            let lower = (capped * 0.8).max(0.1);
            let upper = capped * 1.2;
            assert!(
                secs >= lower * 0.95 && secs <= upper * 1.05,
                "attempt={}: secs={} not in [{}, {}] (exp={}, capped={})",
                attempt,
                secs,
                lower,
                upper,
                exp,
                capped
            );
        }
        // 高 attempt 必被 cap 到 max 附近
        let d = backoff_duration(20, base, max);
        let secs = d.as_secs_f64();
        assert!(
            secs >= 8.0 && secs <= 12.0,
            "attempt=20: secs={} should be capped near max=10 (±20%)",
            secs
        );
    }

    /// G3 集成测试:mock 服务器前 2 次 503,第 3 次 200,验证重试后成功
    #[tokio::test]
    async fn test_retry_succeeds_after_transient_503() {
        let mut server = mockito::Server::new_async().await;
        // 前 2 次返回 503,第 3 次返回 200
        let _m1 = server
            .mock("POST", "/")
            .with_status(503)
            .with_body("server error")
            .create_async()
            .await;
        let _m2 = server
            .mock("POST", "/")
            .with_status(503)
            .with_body("server error")
            .create_async()
            .await;
        let _m3 = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{}}"#,
            )
            .create_async()
            .await;

        // 极小退避,避免测试拖慢
        let handler = LlmHandler::new("m", &server.url(), Some("k".to_string()))
            .with_retry_config(5, 0.001, 0.01);

        let params = evorule_tcb::JsonValue::empty_object();
        let result = handler
            .execute(&params)
            .await
            .expect("should succeed after retries");
        let s = result.to_string();
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["content"], "ok");
    }

    /// G3 集成测试:400 不重试,立即失败
    #[tokio::test]
    async fn test_no_retry_on_400() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(400)
            .with_body(r#"{"error":"bad request"}"#)
            .expect(1) // 只调用一次,不重试
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url(), Some("k".to_string()))
            .with_retry_config(5, 0.001, 0.01);

        let params = evorule_tcb::JsonValue::empty_object();
        let result = handler.execute(&params).await;
        assert!(result.is_err(), "400 should fail immediately");
        let err = result.unwrap_err();
        assert!(
            err.contains("400"),
            "error should mention status 400: {}",
            err
        );
        mock.assert_async().await;
    }

    /// G3 集成测试:始终 429,重试 max_retries 次后失败
    #[tokio::test]
    async fn test_retry_exhausted_on_persistent_429() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(429)
            .with_body("rate limited")
            // max_retries=2 → 总共 3 次请求(1 次首试 + 2 次重试)
            .expect(3)
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url(), Some("k".to_string()))
            .with_retry_config(2, 0.001, 0.01);

        let params = evorule_tcb::JsonValue::empty_object();
        let result = handler.execute(&params).await;
        assert!(result.is_err(), "persistent 429 should fail");
        let err = result.unwrap_err();
        assert!(err.contains("429"), "error should mention 429: {}", err);
        mock.assert_async().await;
    }

    /// G3 集成测试:max_retries=0 时只发一次请求,不重试
    #[tokio::test]
    async fn test_no_retry_when_max_retries_zero() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(503)
            .with_body("error")
            .expect(1) // 只一次
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url(), Some("k".to_string()))
            .with_retry_config(0, 0.001, 0.01);

        let params = evorule_tcb::JsonValue::empty_object();
        let result = handler.execute(&params).await;
        assert!(result.is_err());
        mock.assert_async().await;
    }

    /// G3 集成测试:Retry-After header 被尊重(用极小值避免拖慢)
    #[tokio::test]
    async fn test_respects_retry_after_header() {
        let mut server = mockito::Server::new_async().await;
        let _m1 = server
            .mock("POST", "/")
            .with_status(429)
            .with_header("Retry-After", "0") // 0 秒,测试用
            .with_body("rate limited")
            .create_async()
            .await;
        let _m2 = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{}}"#,
            )
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url(), Some("k".to_string()))
            .with_retry_config(3, 100.0, 200.0); // 故意设大,验证 Retry-After 优先

        let params = evorule_tcb::JsonValue::empty_object();
        // 如果不读 Retry-After,会 sleep 100s 导致测试超时
        let result = tokio::time::timeout(Duration::from_secs(5), handler.execute(&params))
            .await
            .expect("should not timeout — Retry-After=0 must override large backoff");
        assert!(result.is_ok());
    }

    // ========== G1 流式响应测试 ==========

    /// G1:mock handler 的 execute_stream 应产出 Delta + Done
    #[tokio::test]
    async fn test_stream_mock_yields_delta_and_done() {
        let handler = LlmHandler::mock("hello world");
        let params = evorule_tcb::JsonValue::empty_object();
        let mut stream = handler.execute_stream(&params);

        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.expect("no error expected"));
        }

        // 应该有 2 个分片:Delta + Done
        assert_eq!(chunks.len(), 2, "expected Delta + Done, got {:?}", chunks);

        // 第一个是 Delta("hello world")
        match &chunks[0] {
            StreamChunk::Delta(text) => assert_eq!(text, "hello world"),
            other => panic!("expected Delta, got {:?}", other),
        }

        // 第二个是 Done(LlmResponse)
        match &chunks[1] {
            StreamChunk::Done(resp) => {
                assert_eq!(resp.content, "hello world");
                assert!(resp.tool_calls.is_none());
                assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
            }
            other => panic!("expected Done, got {:?}", other),
        }
    }

    /// G1:mockito SSE 服务器,验证多帧 delta 正确解析
    #[tokio::test]
    async fn test_stream_sse_multiple_delta_chunks() {
        let mut server = mockito::Server::new_async().await;
        // OpenAI 兼容 SSE 格式
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"!\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13}}\n\n",
            "data: [DONE]\n\n",
        );
        let _mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse_body)
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url(), Some("k".to_string()));
        let params = evorule_tcb::JsonValue::empty_object();
        let mut stream = handler.execute_stream(&params);

        let mut deltas = Vec::new();
        let mut done_response: Option<LlmResponse> = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamChunk::Delta(text)) => deltas.push(text),
                Ok(StreamChunk::Done(resp)) => {
                    done_response = Some(resp);
                }
                Ok(StreamChunk::Warn(msg)) => panic!("unexpected Warn: {}", msg),
                Ok(StreamChunk::ToolCallDelta { .. }) => {}
                Err(e) => panic!("stream error: {}", e),
            }
        }

        // 验证 delta 顺序
        assert_eq!(deltas, vec!["Hello", " world", "!"]);

        // 验证 Done 的聚合内容
        let resp = done_response.expect("should have Done");
        assert_eq!(resp.content, "Hello world!");
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        let usage = resp.token_usage.expect("should have usage");
        assert_eq!(usage.total_tokens, 13);
    }

    /// G1:tool_calls 跨帧聚合
    #[tokio::test]
    async fn test_stream_sse_tool_calls_aggregation() {
        let mut server = mockito::Server::new_async().await;
        // OpenAI tool_calls 分片格式
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"search\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"q\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"rust\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let _mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse_body)
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url(), Some("k".to_string()));
        let params = evorule_tcb::JsonValue::empty_object();
        let mut stream = handler.execute_stream(&params);

        let mut tool_call_fragments: Vec<String> = Vec::new();
        let mut done_response: Option<LlmResponse> = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamChunk::ToolCallDelta { fragment, .. }) => {
                    tool_call_fragments.push(fragment);
                }
                Ok(StreamChunk::Done(resp)) => {
                    done_response = Some(resp);
                }
                Ok(_) => {}
                Err(e) => panic!("stream error: {}", e),
            }
        }

        // 验证 tool_call 分片
        assert_eq!(
            tool_call_fragments,
            vec!["", "{\"q\":", "\"rust\"}"],
            "fragments should be in order"
        );

        // 验证 Done 中聚合的 tool_calls
        let resp = done_response.expect("should have Done");
        let tool_calls = resp.tool_calls.expect("should have tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "search");
        // arguments 应该是 {"q":"rust"}
        assert_eq!(resp.content, ""); // content 为 null → 空字符串
        assert_eq!(resp.finish_reason.as_deref(), Some("tool_calls"));
    }

    /// G1:HTTP 500 时第一项是 Err
    #[tokio::test]
    async fn test_stream_http_error_yields_err_first() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/")
            .with_status(500)
            .with_body("internal server error")
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url(), Some("k".to_string()));
        let params = evorule_tcb::JsonValue::empty_object();
        let mut stream = handler.execute_stream(&params);

        let first = stream.next().await;
        assert!(first.is_some(), "stream should yield at least one item");
        match first.unwrap() {
            Err(e) => assert!(e.contains("500"), "error should mention 500: {}", e),
            other => panic!("expected Err, got {:?}", other),
        }

        // 流应该结束(不再有第二项)
        let second = stream.next().await;
        assert!(second.is_none(), "stream should end after error");
    }

    /// G1:流自然结束(无 [DONE] 终止符,部分 provider 不发)
    #[tokio::test]
    async fn test_stream_natural_end_without_done_marker() {
        let mut server = mockito::Server::new_async().await;
        // 没有 data: [DONE] 帧,流自然结束
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" there\"},\"finish_reason\":\"stop\"}]}\n\n",
        );
        let _mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse_body)
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url(), Some("k".to_string()));
        let params = evorule_tcb::JsonValue::empty_object();
        let mut stream = handler.execute_stream(&params);

        let mut deltas = Vec::new();
        let mut done_count = 0;
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamChunk::Delta(text)) => deltas.push(text),
                Ok(StreamChunk::Done(_)) => done_count += 1,
                Ok(other) => panic!("unexpected: {:?}", other),
                Err(e) => panic!("stream error: {}", e),
            }
        }

        assert_eq!(deltas, vec!["hi", " there"]);
        assert_eq!(done_count, 1, "should yield exactly one Done");
    }

    /// G1:请求体包含 stream: true
    #[tokio::test]
    async fn test_stream_request_body_includes_stream_true() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body("data: [DONE]\n\n")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "stream": true
            })))
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url(), Some("k".to_string()));
        let params = evorule_tcb::JsonValue::empty_object();
        let mut stream = handler.execute_stream(&params);

        // 消费完整个流
        while let Some(_item) = stream.next().await {}

        mock.assert_async().await;
    }
}
