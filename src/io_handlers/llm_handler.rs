// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! LLM I/O Handler -- 经执行体中继调用外部 LLM API
//!
//! ## SG-1 件1:执行体中继(key 归边)
//!
//! 本 handler 不再直连 provider:所有 LLM 出站统一 POST 到
//! ai-plugin 扩展型执行体端点(`executor_url`,默认
//! `http://127.0.0.1:9130/llm/execute`),请求体形状与原 provider
//! 请求一致(model/messages/temperature/max_tokens/stream/tools/tool_choice,
//! 由执行体原样透传)。API key 由执行体(server 侧)持有,agent 侧
//! 不携带任何凭据。响应为执行体归一后的四键形状
//! `{content, tool_calls:[{tool_name,args}], finish_reason, token_usage}`
//! (与 agent `LlmResponse` 同构;流式请求由执行体做 SSE 聚合后整包返回)。
//!
//! ## G3:重试机制
//!
//! `execute()` 内置指数退避重试,触发条件:
//! - HTTP 429 / 500 / 502 / 503 / 504
//! - 网络错误(timeout / connect / request)
//!
//! 不重试:400 / 401 / 403(客户端错误,重试无意义)。
//! 退避算法:`min(base * 2^attempt, max)`,叠加 ±20% 抖动。

use std::time::Duration;

use async_stream::stream;
use futures_core::Stream;
use rand::Rng;
use serde_json;
use serde_json::Value;
use tracing::{debug, warn};

use crate::agent::translator::{LlmResponse, TokenUsage};
use crate::io_handler::{IoHandler, IoResult};

/// 默认最大重试次数
const DEFAULT_MAX_RETRIES: usize = 3;
/// default connect timeout (secs) - unattended runs must not wait forever on a hung endpoint
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
/// default per-request total timeout (secs)
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 300;
/// 默认初始退避(秒)
const DEFAULT_BASE_BACKOFF_SECS: f64 = 1.0;
/// 默认最大退避(秒)
const DEFAULT_MAX_BACKOFF_SECS: f64 = 30.0;

/// 实际生效的生成参数(单一事实源,供链上留痕与请求体构造共用)
///
/// 入参 `None`(或链上未传)时按兜底值填充:
/// temperature 兜底 0.7、max_tokens 兜底 4096、stream 恒为 true
/// (本 handler 的请求统一走流式端点)。
/// 返回 `(temperature, max_tokens, stream)`。
pub fn effective_generation_params(
    temperature: Option<f64>,
    max_tokens: Option<u64>,
) -> (f64, u64, bool) {
    let t = temperature.unwrap_or(0.7);
    let m = max_tokens.unwrap_or(4096);
    (t, m, true)
}

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

/// LLM I/O Handler -- 经执行体中继调用外部 LLM API
#[derive(Debug, Clone)]
pub struct LlmHandler {
    default_model: String,
    /// 执行体端点(SG-1 件1 key 归边):所有 LLM 出站统一 POST 到此 URL
    executor_url: String,
    /// 执行体调用方令牌:Some = 出站 Authorization: Bearer 携带,
    /// ai-plugin 校验匹配即放行(本机其他进程无令牌即 401)
    executor_token: Option<String>,
    /// If set, skip HTTP and return a canned LlmResponse-format JSON.
    /// Used by tests to avoid hitting a real LLM API.
    mock_content: Option<String>,
    /// G3:最大重试次数(0 = 不重试,只发一次请求)
    max_retries: usize,
    /// G3:初始退避(秒)
    base_backoff_secs: f64,
    /// G3:最大退避(秒)
    max_backoff_secs: f64,
    /// 连接超时(秒)
    connect_timeout_secs: u64,
    /// 单请求总超时(秒)
    request_timeout_secs: u64,
}

impl LlmHandler {
    /// Create new LLM handler
    ///
    /// `executor_url` 为 ai-plugin 扩展型执行体端点(不直连 provider,
    /// agent 侧不持 API key)。
    pub fn new(default_model: &str, executor_url: &str) -> Self {
        Self {
            default_model: default_model.to_string(),
            executor_url: executor_url.to_string(),
            executor_token: None,
            mock_content: None,
            max_retries: DEFAULT_MAX_RETRIES,
            base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
            max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }

    /// G3:覆盖最大重试次数（builder 风格；契约测试用 0 关闭重试避免退避延迟）
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// 主模型名(getter;G4 判据生成落账记录生成模型用)
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// 覆盖请求超时(秒；连接超时固定 DEFAULT_CONNECT_TIMEOUT_SECS)
    pub fn with_request_timeout(mut self, secs: u64) -> Self {
        self.request_timeout_secs = secs;
        self
    }

    /// Create LLM handler with default config
    ///
    /// SG-1 件1(key 归边):agent 侧不再读 `*_API_KEY` 凭据,出站统一
    /// 指向执行体端点——优先 env `EVORULE_LLM_EXECUTOR_URL`,回落
    /// `http://127.0.0.1:9130/llm/execute`(ai-plugin 默认监听)。
    /// 模型名仍可经 env `MINIMAX_MODEL` 覆盖,回落 `MiniMax-M2.5`
    /// (须在执行体白名单内,越界由执行体拒绝)。
    pub fn with_defaults() -> Self {
        Self {
            default_model: std::env::var("MINIMAX_MODEL")
                .unwrap_or_else(|_| "MiniMax-M2.5".to_string()),
            executor_url: std::env::var("EVORULE_LLM_EXECUTOR_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:9130/llm/execute".to_string()),
            executor_token: std::env::var("EVORULE_LLM_EXECUTOR_TOKEN").ok(),
            mock_content: None,
            max_retries: DEFAULT_MAX_RETRIES,
            base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
            max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }

    /// G3:从 `LlmConfig` 构造 handler(由 CLI `cmd_run` 调用)
    ///
    /// 把 `config.llm.max_retries` 真正接到 handler 上,复活死代码。
    /// `executor_url` 映射自 `config.llm.executor_url`;配置中的 `api_key`
    /// 字段不再消费(key 归边,凭据仅存于执行体侧)。
    pub fn from_config(config: &crate::config::LlmConfig) -> Self {
        Self {
            default_model: config.model.clone(),
            executor_url: config.executor_url.clone(),
            executor_token: config.executor_token.clone(),
            mock_content: None,
            max_retries: config.max_retries,
            base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
            max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
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

    /// 预算失配详情（供 runner 启动断言消费；
    /// Ok=匹配,Err=人读详情——公式同 worst_case_budget_secs 实算）。
    pub fn step_budget_mismatch(&self, step_timeout_secs: u64) -> Result<(), String> {
        let budget = self.worst_case_budget_secs();
        if step_timeout_secs >= budget {
            return Ok(());
        }
        Err(format!(
            "step_timeout ({}s) < LLM worst-case budget ({}s = request_timeout {}s × {} attempts \
             + backoff + 10s margin): LLM 端点慢时 step_timeout 先炸,报 call_external timeout \
             掩盖真因(step budget mismatch);调大 step_timeout_secs 或调小 llm.max_retries/request_timeout",
            step_timeout_secs, budget, self.request_timeout_secs, 1 + self.max_retries
        ))
    }

    /// 最坏耗时预算——**预算公式唯一真相源**。
    ///
    /// request_timeout × 尝试次数(1+max_retries) + 退避和上界 + 连接建立余量。
    /// 退避按无 jitter 上界（cap=max_backoff）求和：base·(2^n -1) 封顶
    /// max_backoff×n。连接余量取 10s（connect_timeout 与首字节外的排队）。
    ///
    /// 消费方：`AgentRunner::from_definition` 启动断言 step_timeout ≥ 本值
    /// （fail-fast，防「step_timeout 先炸掩盖 LLM 慢」的误归因）。
    pub fn worst_case_budget_secs(&self) -> u64 {
        let attempts = 1 + self.max_retries as u64;
        let mut backoff_sum = 0f64;
        for attempt in 0..self.max_retries {
            let exp = self.base_backoff_secs * 2f64.powi(attempt as i32);
            backoff_sum += exp.min(self.max_backoff_secs);
        }
        attempts * self.request_timeout_secs + backoff_sum.ceil() as u64 + 10
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
            executor_url: "mock://".to_string(),
            executor_token: None,
            mock_content: Some(content.to_string()),
            max_retries: 0, // mock 不重试
            base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
            max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }

    /// Returns true if this handler is a mock (skips HTTP).
    pub fn is_mock(&self) -> bool {
        self.mock_content.is_some()
    }

    /// G3:构造请求体(从 IoRequest 参数提取 messages/tools 等)
    ///
    /// 拆出独立方法,便于 `execute` 和未来的 `execute_stream`(G1)复用。
    fn build_request_body(&self, params: &Value) -> Result<serde_json::Value, String> {
        let params_str = params.to_string();
        let params_val: serde_json::Value =
            serde_json::from_str(&params_str).map_err(|e| format!("parse params: {}", e))?;

        let model = params_val
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_model);

        let (temperature, max_tokens, _stream) = effective_generation_params(
            params_val.get("temperature").and_then(|v| v.as_f64()),
            params_val.get("max_tokens").and_then(|v| v.as_u64()),
        );

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
                to_openai_wire_messages(
                    params_val
                        .get("messages")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                ),
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
    /// 返回 `Err(e)` 表示网络层错误。SG-1 件1:请求指向执行体端点,
    /// 不携带任何 Authorization(key 归边,凭据由执行体持有)。
    async fn do_http_call(
        &self,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        // connect/request dual timeout - unattended runs must not hang
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(self.connect_timeout_secs))
            .timeout(std::time::Duration::from_secs(self.request_timeout_secs))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let mut req = client.post(&self.executor_url).json(body);
        if let Some(tok) = &self.executor_token {
            req = req.header("Authorization", format!("Bearer {tok}"));
        }
        req.send().await
    }

    /// G3:解析 2xx 响应为 LlmResponse 格式 JSON
    ///
    /// SG-1 件1:执行体(`POST /llm/execute`)返回的是已归一的四键形状
    /// `{content, tool_calls:[{tool_name,args}], finish_reason, token_usage}`
    /// (与 agent `LlmResponse` 同构)。本方法做**宽松透传归一**:
    /// - 缺键/空值兜底(content→"",tool_calls→null)
    /// - `token_usage` 逐值 try-parse 为 `TokenUsage`,形状不符(如
    ///   provider raw usage 缺字段)时降级为 null——禁止因 usage 形状
    ///   偏差导致整个 LlmResponse 反序列化失败
    /// - 业务层错误(provider HTTP 200 + JSON 错误体)已由执行体显式
    ///   转为非 2xx,此处收到的 2xx 响应必为四键形状
    async fn parse_success_response(&self, resp: reqwest::Response) -> Result<Value, String> {
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("LLM API response parse error: {}", e))?;
        normalize_executor_response(json)
    }

    /// G1:流式调用 LLM API(经执行体中继)
    ///
    /// SG-1 件1:流式 SSE 解析与聚合已上移至执行体(`POST /llm/execute`
    /// 收到 `stream:true` 后对 provider SSE 做服务端聚合),agent 收到的
    /// 是聚合完成的四键 JSON 整包。本方法把整包合成为流式事件序列:
    ///
    /// ```text
    /// (content 非空时) Delta(content)  →  Done(LlmResponse)
    /// ```
    ///
    /// `Done` 携带完整聚合的 `LlmResponse`,与 `execute()` 输出格式一致。
    ///
    /// ## 不重试
    ///
    /// 流式请求失败不重试。调用方收到 `Err` 后,可 fallback 到非流式
    /// `execute()`(带 G3 重试)。
    ///
    /// ## Mock
    ///
    /// 如果 `mock_content` 已设置,直接 yield `Delta` + `Done`,不发 HTTP。
    pub fn execute_stream(
        &self,
        params: &Value,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<StreamChunk, String>> + Send>> {
        let body_result = self.build_request_body(params);
        let executor_url = self.executor_url.clone();
        let mock_content = self.mock_content.clone();
        let executor_token = self.executor_token.clone();
        // 超时值提升为局部量（stream! 块内不持有 &self）
        let connect_secs = self.connect_timeout_secs;
        let request_secs = self.request_timeout_secs;

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

            // === 构造请求体(加 stream: true——执行体按此走 SSE 聚合) ===
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

            // === HTTP 请求(执行体端点) ===
            // key 归边:仅携带执行体调用方令牌(SG-1 件1 配套),
            // provider 凭据不出执行体。
            // streaming path same dual timeout
            let client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(connect_secs))
                .timeout(std::time::Duration::from_secs(request_secs))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());

            let mut req = client.post(&executor_url).json(&body);
            if let Some(tok) = &executor_token {
                req = req.header("Authorization", format!("Bearer {tok}"));
            }
            let resp = match req.send().await {
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

            // === 整包解析(执行体聚合结果,四键 JSON) ===
            let json: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    yield Err(format!("LLM API response parse error: {}", e));
                    return;
                }
            };
            let normalized = match normalize_executor_response(json) {
                Ok(v) => v,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

            // === 整包合成:单 Delta(content 非空时)+ Done ===
            let content = normalized
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !content.is_empty() {
                yield Ok(StreamChunk::Delta(content.clone()));
            }
            let llm_response: LlmResponse = serde_json::from_value(normalized)
                .unwrap_or_else(|_| LlmResponse {
                    content,
                    tool_calls: None,
                    finish_reason: Some("stop".to_string()),
                    token_usage: None,
                });
            yield Ok(StreamChunk::Done(llm_response));
        })
    }
}

/// SG-1 件1:执行体四键响应的宽松透传归一(自由函数,`execute` 与
/// `execute_stream` 共用)。
///
/// - `content`:缺键/非字符串 → ""
/// - `tool_calls`:缺键/null → None;数组项缺 `tool_name` 跳过;
///   `args` 缺失 fallback `{}`
/// - `finish_reason`:缺键/null → None;字符串原样保留
/// - `token_usage`:try-parse 为 `TokenUsage`,形状不符 → None
///   (与旧流式路径 usage 解析语义逐值同构,防止 provider usage
///   缺字段导致整个 LlmResponse 反序列化失败)
fn normalize_executor_response(json: Value) -> Result<Value, String> {
    let content = json
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let tool_calls = json
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            let normalized: Vec<serde_json::Value> = arr
                .iter()
                .filter_map(|tc| {
                    let name = tc.get("tool_name").and_then(|n| n.as_str())?;
                    let args = tc.get("args").cloned().unwrap_or(serde_json::json!({}));
                    Some(serde_json::json!({"tool_name": name, "args": args}))
                })
                .collect();
            serde_json::Value::Array(normalized)
        })
        .filter(|arr| !arr.as_array().unwrap().is_empty());

    let finish_reason = json
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let token_usage = json
        .get("token_usage")
        .filter(|v| !v.is_null())
        .and_then(|v| serde_json::from_value::<TokenUsage>(v.clone()).ok());

    debug!(
        content_len = content.len(),
        finish_reason = ?finish_reason,
        tool_calls = ?tool_calls,
        "LLM executor response normalized"
    );
    Ok(serde_json::json!({
        "content": content,
        "tool_calls": tool_calls,
        "finish_reason": finish_reason,
        "token_usage": token_usage,
    }))
}

/// 把内部消息历史转换为 OpenAI wire 形状（出站边界转换）:
/// - Assistant.tool_calls: 内部 `{tool_name, args}` → `{id, type:"function", function:{name, arguments}}`
///   （已是 function 形状的项原样透传，兼容上游已转换的历史）
/// - Tool 消息: `tool_name` → `tool_call_id`（按名 FIFO 配对最近一个 assistant 产生的未消费 id，
///   无可配对时回退 `call_{tool_name}`），并移除非标准的 `tool_name` 键
/// - Tool 消息（B1 多模态上行）: 旁路 `images` 非空时 content 升级为多 part 数组
///   `[{type:"text"},{type:"image_url",...}]`，并移除非标准 `images` 键；
///   空/缺失时 content 保持纯字符串（与既有形态一致）
/// - User 消息（S4-b 用户轮多模态上行）: 旁路 `images` 非空时镜像 Tool 分支升级为
///   多 part 数组；text part 取 content 指纹 blob 的 `text` 字段（runner 侧 JSON 化
///   留痕），解析失败（如窗口预算注记破坏 JSON）回落原始字符串仍可见；
///   空/缺失时 content 保持纯字符串（无图消息 byte 级一致）
///
/// 其余消息与字段原样保留。id 确定性生成（`call_{index}_{name}`），同一 assistant 消息内
/// 多个同名 tool_call 按 index 区分。
fn to_openai_wire_messages(messages: serde_json::Value) -> serde_json::Value {
    let Some(arr) = messages.as_array() else {
        return messages;
    };
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(arr.len());
    // (tool_name, id) 待配对队列：assistant 产生、随后的 tool 消息按名 FIFO 消费
    let mut pending: Vec<(String, String)> = Vec::new();
    for msg in arr {
        let Some(obj) = msg.as_object() else {
            out.push(msg.clone());
            continue;
        };
        let role = obj.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let mut converted = obj.clone();
        match role {
            "assistant" => {
                pending.clear();
                if let Some(calls) = obj.get("tool_calls").and_then(|v| v.as_array()) {
                    let wire: Vec<serde_json::Value> = calls
                        .iter()
                        .enumerate()
                        .map(|(i, call)| {
                            if call.get("function").is_some() {
                                return call.clone();
                            }
                            let name = call
                                .get("tool_name")
                                .or_else(|| call.get("name"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let arguments = match call.get("args") {
                                Some(v) => v.to_string(),
                                None => "{}".to_string(),
                            };
                            let id = format!("call_{}_{}", i, name);
                            pending.push((name.clone(), id.clone()));
                            serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": arguments}
                            })
                        })
                        .collect();
                    converted.insert("tool_calls".to_string(), serde_json::Value::Array(wire));
                }
            }
            "tool" => {
                if let Some(name) = obj.get("tool_name").and_then(|v| v.as_str()) {
                    let id = pending
                        .iter()
                        .position(|(n, _)| n == name)
                        .map(|pos| pending.remove(pos).1)
                        .unwrap_or_else(|| format!("call_{}", name));
                    converted.insert("tool_call_id".to_string(), serde_json::Value::String(id));
                    converted.remove("tool_name");
                }
                // B1 多模态上行:旁路 images 非空时 content 升级为多 part 数组
                // [{type:"text"},{type:"image_url",image_url:{url:"data:<mime>;base64,<data>"}}];
                // 空/缺失时保持纯字符串 content(与既有 wire 形态 byte 级一致)。
                // 组装后删除非标准 images 键(与 tool_name 同口径)。
                if let Some(imgs) = obj.get("images").and_then(|v| v.as_array()) {
                    if !imgs.is_empty() {
                        let mut parts: Vec<serde_json::Value> = Vec::with_capacity(imgs.len() + 1);
                        let text = converted
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        parts.push(serde_json::json!({"type": "text", "text": text}));
                        for img in imgs {
                            let mime = img
                                .get("mime_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("image/png");
                            let data = img
                                .get("data_base64")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if data.is_empty() {
                                continue;
                            }
                            parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {"url": format!("data:{mime};base64,{data}")}
                            }));
                        }
                        converted.insert("content".to_string(), serde_json::Value::Array(parts));
                    }
                    converted.remove("images");
                }
            }
            "user" => {
                // S4-b 用户轮多模态上行:旁路 images 非空时镜像 tool 分支升级为
                // 多 part 数组;text part 取指纹 blob 的 text 字段,解析失败回落
                // 原始字符串(窗口预算丢弃注记破坏 JSON 形态时仍可见)。空/缺失
                // 时保持纯字符串 content(无图消息 wire 形态 byte 级一致),
                // images 键组装后移除(与 tool_name/images 同口径)。
                if let Some(imgs) = obj.get("images").and_then(|v| v.as_array()) {
                    if !imgs.is_empty() {
                        let raw = converted
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let text = serde_json::from_str::<Value>(raw)
                            .ok()
                            .and_then(|v| {
                                v.get("text")
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string())
                            })
                            .unwrap_or_else(|| raw.to_string());
                        let mut parts: Vec<serde_json::Value> = Vec::with_capacity(imgs.len() + 1);
                        parts.push(serde_json::json!({"type": "text", "text": text}));
                        for img in imgs {
                            let mime = img
                                .get("mime_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("image/png");
                            let data = img
                                .get("data_base64")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if data.is_empty() {
                                continue;
                            }
                            parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {"url": format!("data:{mime};base64,{data}")}
                            }));
                        }
                        converted.insert("content".to_string(), serde_json::Value::Array(parts));
                    }
                    converted.remove("images");
                }
            }
            _ => {}
        }
        out.push(serde_json::Value::Object(converted));
    }
    serde_json::Value::Array(out)
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
    async fn execute(&self, params: &Value) -> IoResult {
        // Mock LLM short-circuit (used by tests).
        // 重试逻辑不适用 mock,直接返回。
        if let Some(content) = &self.mock_content {
            let response = serde_json::json!({
                "content": content,
                "tool_calls": null,
                "finish_reason": "stop",
                "token_usage": null,
            });
            return Ok(response.clone());
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
    use futures_util::StreamExt;

    /// 最坏预算公式锁定
    #[test]
    fn test_worst_case_budget_formula_e7() {
        // 默认口径: 300s×4 attempts + backoff(1+2+4=7,cap 30 不触) + 10 = 1217
        let h = LlmHandler::new("m", "https://x");
        assert_eq!(h.worst_case_budget_secs(), 300 * 4 + 7 + 10);
        // cap 生效口径: base=20 → 20+30+30=80
        let h = h.with_retry_config(3, 20.0, 30.0);
        assert_eq!(h.worst_case_budget_secs(), 300 * 4 + 80 + 10);
        // 零重试口径: 单次请求 + 10
        let h = h.with_retry_config(0, 1.0, 30.0);
        assert_eq!(h.worst_case_budget_secs(), 300 + 10);
    }

    /// 失配判定（600=失配口径必红,1300=修复口径必绿）
    #[test]
    fn test_step_budget_mismatch_e7() {
        let h = LlmHandler::new("m", "https://x"); // budget=1225
        assert!(h.step_budget_mismatch(600).is_err());
        assert!(h
            .step_budget_mismatch(600)
            .unwrap_err()
            .contains("step budget mismatch"));
        assert!(h.step_budget_mismatch(1300).is_ok());
    }

    /// 实际生效参数:显式值原样透传,兜底值 0.7/4096,stream 恒 true
    #[test]
    fn test_effective_generation_params_defaults_and_passthrough() {
        // 缺省 → 兜底
        assert_eq!(effective_generation_params(None, None), (0.7, 4096, true));
        // 显式值 → 原样保留
        assert_eq!(
            effective_generation_params(Some(0.2), Some(1024)),
            (0.2, 1024, true)
        );
        // 混合:只传 temperature
        assert_eq!(
            effective_generation_params(Some(1.5), None),
            (1.5, 4096, true)
        );
        // stream 恒为 true(本 handler 统一走流式)
        let (_, _, stream) = effective_generation_params(Some(0.0), Some(1));
        assert!(stream);
    }

    /// 出站转换:内部 {tool_name, args} → OpenAI function 形状;tool 消息按名配对 id
    #[test]
    fn test_wire_messages_converts_internal_tool_calls() {
        let messages = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "",
             "tool_calls": [{"tool_name": "echo", "args": {"text": "hi"}}]},
            {"role": "tool", "content": "ok", "tool_name": "echo"}
        ]);
        let wire = to_openai_wire_messages(messages);
        let arr = wire.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        // user 原样
        assert_eq!(arr[0], serde_json::json!({"role": "user", "content": "hi"}));
        // assistant tool_calls → function 形状
        let call = &arr[1]["tool_calls"][0];
        assert_eq!(call["id"], "call_0_echo");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "echo");
        assert_eq!(call["function"]["arguments"], r#"{"text":"hi"}"#);
        assert!(call.get("tool_name").is_none());
        // tool 消息 → tool_call_id 配对,tool_name 移除
        assert_eq!(arr[2]["tool_call_id"], "call_0_echo");
        assert_eq!(arr[2]["content"], "ok");
        assert!(arr[2].get("tool_name").is_none());
    }

    /// 出站转换:已是 function 形状的项与 system/user 消息原样透传
    #[test]
    fn test_wire_messages_passthrough_wire_shape() {
        let messages = serde_json::json!([
            {"role": "system", "content": "sys"},
            {"role": "assistant", "content": "",
             "tool_calls": [{"id": "call_x", "type": "function",
                             "function": {"name": "f", "arguments": "{}"}}]},
            {"role": "tool", "content": "r", "tool_call_id": "call_x"}
        ]);
        let wire = to_openai_wire_messages(messages);
        let arr = wire.as_array().unwrap();
        assert_eq!(arr[1]["tool_calls"][0]["id"], "call_x");
        assert_eq!(arr[1]["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(arr[2]["tool_call_id"], "call_x");
    }

    /// 出站转换:tool 消息无前置 assistant tool_call 可配对时回退 call_{tool_name}
    #[test]
    fn test_wire_messages_tool_fallback_id() {
        let messages = serde_json::json!([
            {"role": "tool", "content": "r", "tool_name": "orphan"}
        ]);
        let wire = to_openai_wire_messages(messages);
        assert_eq!(wire[0]["tool_call_id"], "call_orphan");
    }

    /// B1 多模态上行:images 空/缺失 → 纯字符串 content 原样(与既有形态一致),
    /// 非 images 键形态不触碰
    #[test]
    fn test_wire_messages_images_empty_keeps_string_content() {
        let messages = serde_json::json!([
            {"role": "tool", "content": "ok", "tool_name": "echo", "images": []},
            {"role": "tool", "content": "ok2", "tool_name": "echo2"}
        ]);
        let wire = to_openai_wire_messages(messages);
        let arr = wire.as_array().unwrap();
        assert_eq!(arr[0]["content"], "ok");
        assert_eq!(arr[0]["tool_call_id"], "call_echo");
        assert_eq!(arr[1]["content"], "ok2");
    }

    /// B1 多模态上行:images 非空 → content 升级多 part 数组
    /// [text, image_url...],images 键移除;data 为空的项跳过
    #[test]
    fn test_wire_messages_images_become_content_parts() {
        let messages = serde_json::json!([
            {"role": "assistant", "content": "",
             "tool_calls": [{"tool_name": "image_load", "args": {"path": "a.png"}}]},
            {"role": "tool", "content": "fp", "tool_name": "image_load",
             "images": [
                 {"mime_type": "image/png", "data_base64": "QUJD"},
                 {"mime_type": "image/png", "data_base64": ""}
             ]}
        ]);
        let wire = to_openai_wire_messages(messages);
        let arr = wire.as_array().unwrap();
        let tool = &arr[1];
        assert_eq!(tool["tool_call_id"], "call_0_image_load");
        let parts = tool["content"].as_array().expect("content must be array");
        assert_eq!(parts.len(), 2, "text part + one non-empty image part");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "fp");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
        assert!(tool.get("images").is_none(), "sideband key must be removed");
    }

    /// S4-b 用户轮多模态上行:无图 user 消息(无 images 键)content 保持
    /// 纯字符串——与既有 wire 形态 byte 级一致(回滚保证)
    #[test]
    fn test_wire_user_message_without_images_untouched() {
        let messages = serde_json::json!([
            {"role": "user", "content": "hello world"}
        ]);
        let wire = to_openai_wire_messages(messages);
        let arr = wire.as_array().unwrap();
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"], "hello world");
        assert!(arr[0].get("images").is_none());
    }

    /// S4-b 用户轮多模态上行:images 非空 → content 升级多 part 数组,text part
    /// 取指纹 blob 的 text 字段(而非整个 blob),images 键移除
    #[test]
    fn test_wire_user_message_images_upgrade_parts_from_blob_text() {
        let blob = serde_json::json!({
            "text": "这张图里有什么?",
            "images_extracted": [
                {"fingerprint": "abc123", "bytes": 3, "mime_type": "image/png"}
            ]
        })
        .to_string();
        let messages = serde_json::json!([
            {"role": "user", "content": blob,
             "images": [{"mime_type": "image/jpeg", "data_base64": "QUJD"}]}
        ]);
        let wire = to_openai_wire_messages(messages);
        let arr = wire.as_array().unwrap();
        let user = &arr[0];
        let parts = user["content"].as_array().expect("content must be array");
        assert_eq!(parts.len(), 2, "text part + one image part");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(
            parts[0]["text"], "这张图里有什么?",
            "text part must be the blob's text field, not the whole blob"
        );
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/jpeg;base64,QUJD");
        assert!(user.get("images").is_none(), "sideband key must be removed");
    }

    /// S4-b:text 解析回落——content 非合法指纹 blob(如窗口预算注记破坏 JSON)
    /// 时回落原始字符串,text part 仍可见
    #[test]
    fn test_wire_user_message_blob_parse_fallback() {
        let messages = serde_json::json!([
            {"role": "user",
             "content": "{\"text\":\"hi\",\"images_extracted\":[]}\n[image omitted: window image budget, 1 dropped]",
             "images": [{"mime_type": "image/png", "data_base64": "QUJD"}]}
        ]);
        let wire = to_openai_wire_messages(messages);
        let arr = wire.as_array().unwrap();
        let parts = arr[0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        let text = parts[0]["text"].as_str().unwrap();
        assert!(
            text.contains("[image omitted: window image budget, 1 dropped]"),
            "fallback text must be the raw string with the omission note, got: {text}"
        );
    }

    #[test]
    fn test_llm_handler_new() {
        let handler = LlmHandler::new("gpt-4", "https://api.example.com");
        assert_eq!(handler.default_model, "gpt-4");
        assert_eq!(handler.executor_url, "https://api.example.com");
        // G3:默认重试参数
        assert_eq!(handler.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(handler.base_backoff_secs, DEFAULT_BASE_BACKOFF_SECS);
    }

    #[test]
    fn test_llm_handler_with_defaults() {
        let handler = LlmHandler::with_defaults();
        assert_eq!(handler.default_model, "MiniMax-M2.5");
        // SG-1 件1:出站统一指向执行体端点(key 归边,无 provider 直连)
        assert_eq!(handler.executor_url, "http://127.0.0.1:9130/llm/execute");
        assert!(!handler.is_mock());
    }

    #[test]
    fn test_llm_handler_from_config_carries_max_retries() {
        // G3:验证 from_config 真正把 config.max_retries 传到 handler
        use crate::config::LlmConfig;
        let cfg = LlmConfig {
            max_retries: 5,
            executor_url: "http://127.0.0.1:19999/llm/execute".to_string(),
            ..LlmConfig::default()
        };
        let handler = LlmHandler::from_config(&cfg);
        assert_eq!(handler.max_retries, 5);
        assert_eq!(handler.executor_url, "http://127.0.0.1:19999/llm/execute");
        assert_eq!(handler.default_model, cfg.model);
    }

    #[test]
    fn test_llm_handler_with_retry_config_builder() {
        let handler = LlmHandler::new("m", "https://x").with_retry_config(10, 0.5, 5.0);
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

        let params = Value::Object(Default::default());
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
    fn test_llm_handler_mock_skips_http() {
        let handler = LlmHandler::mock("anything");
        assert_eq!(handler.executor_url, "mock://");
        assert!(handler.is_mock());
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
            (8.0..=12.0).contains(&secs),
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
                // 执行体四键归一形状(SG-1 件1)
                r#"{"content":"ok","tool_calls":null,"finish_reason":"stop","token_usage":null}"#,
            )
            .create_async()
            .await;

        // 极小退避,避免测试拖慢
        let handler = LlmHandler::new("m", &server.url()).with_retry_config(5, 0.001, 0.01);

        let params = Value::Object(Default::default());
        let result = handler
            .execute(&params)
            .await
            .expect("should succeed after retries");
        let s = result.to_string();
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["content"], "ok");
    }

    /// 四键透传:执行体已归一的 {tool_name,args} 形状经 normalize 保真,
    /// 且 execute() 输出可被 handle_call_external 反序列化为 LlmResponse。
    /// (SG-1 件1:OpenAI wire→内部形状的归一已上移执行体,agent 只做
    /// 宽松透传与 token_usage try-parse)
    #[tokio::test]
    async fn test_execute_passes_through_normalized_tool_calls() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "content":"",
                    "tool_calls":[
                        {"tool_name":"file_read","args":{"path":"/tmp/a.txt"}}
                    ],
                    "finish_reason":"tool_calls",
                    "token_usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                }"#,
            )
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url()).with_retry_config(0, 0.001, 0.01);
        let params = Value::Object(Default::default());
        let result = handler
            .execute(&params)
            .await
            .expect("execute should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&result.to_string()).unwrap();

        let tcs = parsed["tool_calls"]
            .as_array()
            .expect("tool_calls should be array");
        assert_eq!(tcs.len(), 1, "one tool call expected");
        assert_eq!(
            tcs[0]["tool_name"], "file_read",
            "name must move to tool_name"
        );
        assert_eq!(
            tcs[0]["args"]["path"], "/tmp/a.txt",
            "arguments JSON string must be parsed into args object"
        );
        // 反序列化为 LlmResponse(即 handle_call_external 的消费方式)必须成功
        let resp: Result<crate::agent::translator::LlmResponse, _> =
            serde_json::from_str(&result.to_string());
        assert!(
            resp.is_ok(),
            "execute() output must deserialize into LlmResponse: {:?}",
            resp.err()
        );
        let resp = resp.unwrap();
        let tc = resp.tool_calls.unwrap();
        assert_eq!(tc[0].name, "file_read");
        assert_eq!(tc[0].arguments["path"], "/tmp/a.txt");
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

        let handler = LlmHandler::new("m", &server.url()).with_retry_config(5, 0.001, 0.01);

        let params = Value::Object(Default::default());
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

        let handler = LlmHandler::new("m", &server.url()).with_retry_config(2, 0.001, 0.01);

        let params = Value::Object(Default::default());
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

        let handler = LlmHandler::new("m", &server.url()).with_retry_config(0, 0.001, 0.01);

        let params = Value::Object(Default::default());
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
                r#"{"content":"ok","tool_calls":null,"finish_reason":"stop","token_usage":null}"#,
            )
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url()).with_retry_config(3, 100.0, 200.0); // 故意设大,验证 Retry-After 优先

        let params = Value::Object(Default::default());
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
        let params = Value::Object(Default::default());
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

    /// G1(SG-1 件1):执行体聚合整包 JSON → 单 Delta + Done
    #[tokio::test]
    async fn test_stream_executor_json_single_delta_and_done() {
        let mut server = mockito::Server::new_async().await;
        // 执行体对 stream:true 请求做 SSE 聚合后返回四键 JSON 整包
        let _mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"content":"Hello world!","tool_calls":null,"finish_reason":"stop",
                    "token_usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#,
            )
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url());
        let params = Value::Object(Default::default());
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
                // 整包路径不再产出 tool_call 增量(聚合已上移执行体)
                Ok(StreamChunk::ToolCallDelta { .. }) => {
                    panic!("ToolCallDelta must not be produced by whole-payload path")
                }
                Err(e) => panic!("stream error: {}", e),
            }
        }

        // 整包合成:content 非空 → 恰好一个 Delta(全量,非增量)
        assert_eq!(deltas, vec!["Hello world!"]);

        let resp = done_response.expect("should have Done");
        assert_eq!(resp.content, "Hello world!");
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        let usage = resp.token_usage.expect("should have usage");
        assert_eq!(usage.total_tokens, 13);
    }

    /// G1(SG-1 件1):执行体整包携带归一 tool_calls → Done 承载全量
    #[tokio::test]
    async fn test_stream_executor_json_with_tool_calls() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"content":"","tool_calls":[{"tool_name":"search","args":{"q":"rust"}}],
                    "finish_reason":"tool_calls","token_usage":null}"#,
            )
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url());
        let params = Value::Object(Default::default());
        let mut stream = handler.execute_stream(&params);

        let mut delta_count = 0;
        let mut done_response: Option<LlmResponse> = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamChunk::Delta(_)) => delta_count += 1,
                Ok(StreamChunk::Done(resp)) => {
                    done_response = Some(resp);
                }
                Ok(_) => {}
                Err(e) => panic!("stream error: {}", e),
            }
        }

        // content 为空 → 不产出 Delta,仅 Done
        assert_eq!(delta_count, 0, "empty content must not yield Delta");

        let resp = done_response.expect("should have Done");
        let tool_calls = resp.tool_calls.expect("should have tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "search");
        assert_eq!(tool_calls[0].arguments["q"], "rust");
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

        let handler = LlmHandler::new("m", &server.url());
        let params = Value::Object(Default::default());
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

    /// G1:请求体包含 stream: true(执行体按此走 SSE 聚合)
    #[tokio::test]
    async fn test_stream_request_body_includes_stream_true() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"content":"","tool_calls":null,"finish_reason":"stop","token_usage":null}"#,
            )
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "stream": true
            })))
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url());
        let params = Value::Object(Default::default());
        let mut stream = handler.execute_stream(&params);

        // 消费完整个流
        while let Some(_item) = stream.next().await {}

        mock.assert_async().await;
    }
}
