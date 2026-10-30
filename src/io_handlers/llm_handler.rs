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
//! (与 agent `LlmResponse` 同构;推理模型增补第五键 `reasoning`,
//! 仅非空携带)。
//!
//! ## 流式双路径
//!
//! `execute_stream` 真流式优先:执行体流式端点 `/llm/execute-stream`
//! (由 executor_url 派生)就绪时增量解析归一化帧(思考/正文/tool_call
//! 分片逐帧下行);端点不存在(旧执行体,404/405)自动回落整包路径并
//! 置进程级负缓存;env `EVORULE_LLM_STREAM_DISABLE=1` 显式停用。
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
use futures_util::StreamExt;
use rand::Rng;
use serde_json;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::agent::translator::{LlmResponse, TokenUsage};
use crate::io_handler::{IoHandler, IoResult};

/// 默认最大重试次数
const DEFAULT_MAX_RETRIES: usize = 3;
/// default connect timeout (secs) - unattended runs must not wait forever on a hung endpoint
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
/// default per-request total timeout (secs)
/// (2026-10-26: 300→1800 对齐执行体 LLM_EXECUTE_TIMEOUT_SECS 实值——推理模型
///  长回合单次流式调用实测可超 300s,超时被本地切断时报 error decoding
///  response body 而非显式 timeout,易误导排查)
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 1800;
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

/// 进程级负缓存(按流式端点 URL 记账):曾探得端点不存在(404/405)的
/// 端点,后续调用直走 legacy 整包路径,不再重复探测(旧执行体组合下
/// 省去每轮一次 404 往返)。按 URL 而非全局单旗标——多执行体部署下
/// 各端点独立记账,互不误伤。
static STREAM_ENDPOINTS_MISSING: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

fn mark_stream_endpoint_missing(stream_url: &str) {
    if let Ok(mut slot) = STREAM_ENDPOINTS_MISSING.lock() {
        slot.get_or_insert_with(Default::default)
            .insert(stream_url.to_string());
    }
}

/// 清负缓存条目(测试专用):mockito 随机端口在并行测试下可能复用刚释放
/// 的端口号,404 记账测试收尾须清理,防同端口后继测试被负缓存毒化走
/// legacy(撞 mockito 无匹配路由的 501)。生产执行体地址稳定,无需清理面。
#[cfg(test)]
pub(crate) fn clear_stream_endpoint_missing_for_test(stream_url: &str) {
    if let Ok(mut slot) = STREAM_ENDPOINTS_MISSING.lock() {
        if let Some(set) = slot.as_mut() {
            set.remove(stream_url);
        }
    }
}

fn stream_endpoint_missing(stream_url: &str) -> bool {
    match STREAM_ENDPOINTS_MISSING.lock() {
        Ok(slot) => slot.as_ref().is_some_and(|set| set.contains(stream_url)),
        Err(_) => false, // 锁中毒不放大故障,仅退化为重复探测
    }
}

/// env 显式停用检测:`EVORULE_LLM_STREAM_DISABLE=1` → true
/// (参数注入式,便于测试;调用点传 `std::env::var(..).ok()`)
fn stream_disabled_by_env(env_value: Option<String>) -> bool {
    env_value.as_deref() == Some("1")
}

/// 执行体端点 → 流式端点 URL:剥 `/llm/execute` 后缀拼 `/llm/execute-stream`
/// (同一插件进程的孪生端点,同令牌同执法面)。不以该后缀收尾(自托管改路径)
/// 返回 None——无流式端点可试,直走 legacy 整包路径。
fn stream_endpoint_of(executor_url: &str) -> Option<String> {
    executor_url
        .strip_suffix("/llm/execute")
        .map(|base| format!("{base}/llm/execute-stream"))
}

/// G1:LLM 流式输出的单个分片
///
/// `execute_stream` 依次产出(真流式路径):
/// ```text
/// ThinkingDelta * N  →  Delta(text) * N  →  (ToolCallDelta * N)?  →  Done(LlmResponse)
/// ```
/// legacy 整包回落路径为合成序列:ThinkingDelta(整块,思考非空时) →
/// Delta(content) → Done(LlmResponse)。
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// 思考增量(推理模型 reasoning_content,非累积;仅展示链路,不持久化)
    ThinkingDelta(String),
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
    /// [llm] max_tokens 配置兜底(None = 4096 硬兜底,既有行为零变化;
    /// 请求级 params.max_tokens > 本配置 > 4096)
    config_max_tokens: Option<u64>,
    /// direct 档配置(None = relay 档,出站行为与既有逐字节一致;Some =
    /// `mode="direct"` 显式启用,execute/execute_stream 分派直连传输)。
    /// 分派收在 handler 内部——IoDispatcher/runner/audited_llm 零感知。
    direct: Option<super::llm_direct::DirectLlmConfig>,
    /// 脚本式 mock 状态(测试装置):按调用次序吐预置结果并记录每次收到的
    /// 请求参数。None = 非脚本 mock,短路分支不触碰。
    mock_script: Option<std::sync::Arc<std::sync::Mutex<MockScriptState>>>,
}

/// 脚本式 mock 状态(测试装置):`steps` 按次序消费(Err = 流式错误串,
/// Ok = 正常响应内容),耗尽后请求一律报错——防测试静默放行多余调用;
/// `observed_params` 记录每次 execute_stream 收到的完整请求参数,
/// 供测试断言「第二次调用的 messages 已被裁剪」等请求侧形态。
#[derive(Debug, Default)]
pub struct MockScriptState {
    steps: std::collections::VecDeque<Result<String, String>>,
    observed_params: Vec<Value>,
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
            config_max_tokens: None,
            direct: None,
            mock_script: None,
        }
    }

    /// G3:覆盖最大重试次数（builder 风格；契约测试用 0 关闭重试避免退避延迟）
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// [llm] max_tokens 配置兜底(builder 风格;None = 4096 硬兜底,
    /// 请求级 params.max_tokens 仍压过本值)
    pub fn with_config_max_tokens(mut self, v: Option<u64>) -> Self {
        self.config_max_tokens = v;
        self
    }

    /// 测试观测口:配置兜底 max_tokens 当前值(None = 4096 硬兜底生效)。
    /// 构造点接线防回归测试消费(主回合/子代理 runner 的 handler 必须
    /// 携带统一解析产物,缺省 with_defaults 落 4096 会被推理模型思考吃满)。
    #[cfg(test)]
    pub(crate) fn config_max_tokens_for_test(&self) -> Option<u64> {
        self.config_max_tokens
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
            config_max_tokens: None,
            direct: None,
            mock_script: None,
        }
    }

    /// G3:从 `LlmConfig` 构造 handler(由 CLI `cmd_run` 调用)
    ///
    /// 把 `config.llm.max_retries` 真正接到 handler 上,复活死代码。
    /// `executor_url` 映射自 `config.llm.executor_url`;relay 档(缺省)
    /// 配置中的 `api_key` 字段不消费(key 归边,凭据仅存于执行体侧)。
    /// `mode="direct"` 时构造直连配置(api_key/api_base 生效,启动期
    /// 校验已保证非空——校验入口在 config validate,fail-fast 在前)。
    pub fn from_config(config: &crate::config::LlmConfig) -> Self {
        let mut handler = Self {
            default_model: config.model.clone(),
            executor_url: config.executor_url.clone(),
            executor_token: config.executor_token.clone(),
            mock_content: None,
            max_retries: config.max_retries,
            base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
            max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            config_max_tokens: config.max_tokens,
            direct: if config.mode == "direct" {
                Some(super::llm_direct::DirectLlmConfig {
                    api_key: config.api_key.clone(),
                    api_base: config.api_base.clone(),
                })
            } else {
                None
            },
            mock_script: None,
        };
        // [llm] timeout_secs 显式声明即生效（此前被静默忽略）；
        // 0（未显式配置）保持内建缺省——接通前后未配置用户行为不变。
        if config.timeout_secs > 0 {
            handler.request_timeout_secs = config.timeout_secs;
        }
        handler
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
        // step_timeout=0（不限步超时）无预算失配可言——判定跳过。
        if step_timeout_secs == 0 {
            return Ok(());
        }
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

    /// direct 档重试参数包(两处分派点共用;数值全部取自 handler 既有
    /// 字段——direct 复用 relay 同款策略,零常量分叉)
    fn direct_retry_params(&self) -> super::llm_direct::DirectRetryParams {
        super::llm_direct::DirectRetryParams {
            max_retries: self.max_retries,
            base_backoff_secs: self.base_backoff_secs,
            max_backoff_secs: self.max_backoff_secs,
            connect_timeout_secs: self.connect_timeout_secs,
            request_timeout_secs: self.request_timeout_secs,
        }
    }

    /// 引擎 pending 预算对齐断言（供 runner 启动断言消费；Ok=匹配,Err=人读详情）。
    ///
    /// LLM 最坏耗时超过引擎 pending I/O 预算（io_error_timeout）时,单步内的
    /// 慢 LLM 调用会令引擎侧 pending 先被回收——agent 侧提交成为 unknown/stale。
    /// agent 侧预防层(提交前放弃+续跑兜底)接得住后果,本断言给出可见证据与
    /// 调参指引。engine_io_timeout_secs=0(预防层禁用)由调用方守卫,不进本函数。
    pub fn engine_budget_mismatch(&self, engine_io_timeout_secs: u64) -> Result<(), String> {
        if engine_io_timeout_secs == 0 {
            return Ok(());
        }
        let budget = self.worst_case_budget_secs();
        if engine_io_timeout_secs >= budget {
            return Ok(());
        }
        Err(format!(
            "engine io_error_timeout ({}s) < LLM worst-case budget ({}s = request_timeout {}s × {} attempts \
             + backoff + 10s margin): 慢 LLM 调用会超出引擎 pending 预算,agent 提交前护栏将放弃提交并依赖续跑兜底; \
             调大引擎侧 io_error_timeout 或调小 llm.max_retries/request_timeout 可消除该失配",
            engine_io_timeout_secs, budget, self.request_timeout_secs, 1 + self.max_retries
        ))
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
            config_max_tokens: None,
            direct: None,
            mock_script: None,
        }
    }

    /// 脚本式 mock(测试装置):按调用次序吐预置结果——`Err(错误串)` 模拟
    /// 流式调用失败(如请求体超限拒绝),`Ok(内容)` 模拟正常响应;耗尽后
    /// 再来的请求一律报 "mock script exhausted"(防测试静默放行多余调用)。
    /// 每次调用收到的请求参数经 `mock_observed_params` 可回读。
    pub fn mock_scripted(steps: Vec<Result<String, String>>) -> Self {
        Self {
            default_model: "mock".to_string(),
            executor_url: "mock://".to_string(),
            executor_token: None,
            mock_content: None,
            max_retries: 0,
            base_backoff_secs: DEFAULT_BASE_BACKOFF_SECS,
            max_backoff_secs: DEFAULT_MAX_BACKOFF_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            config_max_tokens: None,
            direct: None,
            mock_script: Some(std::sync::Arc::new(std::sync::Mutex::new(
                MockScriptState {
                    steps: steps.into(),
                    observed_params: Vec::new(),
                },
            ))),
        }
    }

    /// 回读脚本式 mock 记录的历次请求参数(快照;仅脚本 mock 有记录)。
    pub fn mock_observed_params(&self) -> Vec<Value> {
        self.mock_script
            .as_ref()
            .map(|s| s.lock().unwrap().observed_params.clone())
            .unwrap_or_default()
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
            params_val
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .or(self.config_max_tokens),
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
    /// 双路径设计(真流式优先,旧执行体自动回落):
    /// - **真流式路径**:执行体流式端点(`/llm/execute-stream`,由
    ///   `executor_url` 剥 `/llm/execute` 后缀派生)就绪时,agent 侧对
    ///   响应 `bytes_stream` 增量解析,归一化帧逐个下行——思考/正文/
    ///   tool_call 分片全程不经整包缓冲。env `EVORULE_LLM_STREAM_DISABLE=1`
    ///   显式停用;进程级负缓存(曾探得 404/405)后不再重复探测。
    /// - **legacy 整包路径**(回落/停用/URL 不可派生):执行体聚合后
    ///   整包返回,本方法合成流式事件序列(思考整块 → 正文 → Done)。
    ///
    /// 真流式帧契约(执行体下行):`{"type":"reasoning","text"}` /
    /// `{"type":"content","text"}` / `{"type":"tool_call_delta","index",
    /// "name"?,"fragment"}` / `{"type":"done",...五键聚合}` /
    /// `{"type":"error","error"}` + `data: [DONE]` 哨兵。error 帧与流
    /// 读取失败以 Err 下行(fail-visible);安全网:全程零 reasoning 帧
    /// 而 done 帧带非空思考 → 补发单帧 ThinkingDelta(防 provider 仅在
    /// 终帧给思考块)。
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

        // direct 档分派(在 relay stream 构造前短路——relay 双路径代码
        // 物理不被触碰):请求构造共用,传输/增量解析走 llm_direct
        if let Some(direct_cfg) = &self.direct {
            return match body_result {
                Ok(body) => super::llm_direct::execute_stream(
                    body,
                    direct_cfg.clone(),
                    self.direct_retry_params(),
                ),
                Err(e) => Box::pin(async_stream::stream! {
                    yield Err::<StreamChunk, String>(e);
                }),
            };
        }

        let executor_url = self.executor_url.clone();
        let mock_content = self.mock_content.clone();
        let mock_script = self.mock_script.clone();
        let executor_token = self.executor_token.clone();
        // 超时值提升为局部量（stream! 块内不持有 &self）
        let connect_secs = self.connect_timeout_secs;
        let request_secs = self.request_timeout_secs;
        // 脚本 mock 记录请求参数用(stream! 块 'static 约束,参数须 owned 进块)
        let script_params = params.clone();

        Box::pin(stream! {
            // === 脚本式 mock(测试装置):按脚本次序吐错误或正常响应 ===
            if let Some(script) = &mock_script {
                // lock 收敛在同步块(guard 不跨 yield,保 stream Send)
                let step = {
                    let mut st = script.lock().unwrap();
                    st.observed_params.push(script_params.clone());
                    st.steps.pop_front()
                };
                match step {
                    Some(Err(e)) => {
                        yield Err::<StreamChunk, String>(e);
                        return;
                    }
                    Some(Ok(content)) => {
                        yield Ok(StreamChunk::Delta(content.clone()));
                        yield Ok(StreamChunk::Done(LlmResponse {
                            content,
                            tool_calls: None,
                            finish_reason: Some("stop".to_string()),
                            token_usage: None,
                        }));
                        return;
                    }
                    None => {
                        yield Err::<StreamChunk, String>("mock script exhausted".to_string());
                        return;
                    }
                }
            }

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

            // === 构造请求体(加 stream: true——执行体按此走流式) ===
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

            // === HTTP 客户端(双超时;key 归边:仅携带执行体调用方令牌,
            // provider 凭据不出执行体) ===
            let client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(connect_secs))
                .timeout(std::time::Duration::from_secs(request_secs))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());
            let build_request = |url: &str| {
                let mut req = client.post(url).json(&body);
                if let Some(tok) = &executor_token {
                    req = req.header("Authorization", format!("Bearer {tok}"));
                }
                req
            };

            // === 路径选择:env 显式停用 / 负缓存已记账 / URL 不可派生 → legacy ===
            let stream_disabled = stream_disabled_by_env(std::env::var("EVORULE_LLM_STREAM_DISABLE").ok());
            let stream_url = if stream_disabled {
                None
            } else {
                stream_endpoint_of(&executor_url)
                    .filter(|url| !stream_endpoint_missing(url))
            };

            if let Some(stream_url) = stream_url {
                match build_request(&stream_url).send().await {
                    Err(e) => {
                        yield Err(format!("LLM stream request failed: {}", e));
                        return;
                    }
                    // 端点不存在(旧执行体)→ 负缓存置位 + 回落 legacy(留痕可观测)
                    Ok(resp)
                        if matches!(
                            resp.status(),
                            reqwest::StatusCode::NOT_FOUND
                                | reqwest::StatusCode::METHOD_NOT_ALLOWED
                        ) =>
                    {
                        mark_stream_endpoint_missing(&stream_url);
                        info!(
                            executor_url = %executor_url,
                            status = %resp.status(),
                            "执行体流式端点不可用,回落整包路径(负缓存记账,后续不再探测该端点)"
                        );
                    }
                    Ok(resp) if !resp.status().is_success() => {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        yield Err(format!("LLM API error ({}): {}", status, text));
                        return;
                    }
                    Ok(resp) => {
                        // === 真流式:bytes_stream 增量解析,逐帧下行(不经整包缓冲) ===
                        let mut buffer = String::new();
                        let mut saw_reasoning = false;
                        let mut saw_done = false;
                        let mut sse = resp.bytes_stream();
                        'frames: loop {
                            // 先消费缓冲内完整帧(\n\n 切帧),再读下一字节块
                            while let Some(pos) = buffer.find("\n\n") {
                                let frame: String = buffer.drain(..pos + 2).collect();
                                let (chunks, end) =
                                    executor_sse_frame_outcome(&frame, &mut saw_reasoning, &mut saw_done);
                                for chunk in chunks {
                                    yield Ok(chunk);
                                }
                                match end {
                                    FrameEnd::Continue => {}
                                    FrameEnd::Stop => break 'frames,
                                    FrameEnd::Fail(msg) => {
                                        yield Err(msg);
                                        return;
                                    }
                                }
                            }
                            match sse.next().await {
                                Some(Ok(chunk)) => {
                                    buffer.push_str(&String::from_utf8_lossy(&chunk));
                                }
                                Some(Err(e)) => {
                                    yield Err(format!("LLM stream read failed: {}", e));
                                    return;
                                }
                                None => break 'frames,
                            }
                        }
                        // 流收尾残留缓冲(末帧无尾随空行形态):与执行体切帧语义对齐
                        let residual = std::mem::take(&mut buffer);
                        if !residual.trim().is_empty() {
                            let (chunks, end) = executor_sse_frame_outcome(
                                &residual,
                                &mut saw_reasoning,
                                &mut saw_done,
                            );
                            for chunk in chunks {
                                yield Ok(chunk);
                            }
                            if let FrameEnd::Fail(msg) = end {
                                yield Err(msg);
                                return;
                            }
                        }
                        if !saw_done {
                            yield Err("执行体流式响应未含 done 终帧(连接提前关闭)".to_string());
                            return;
                        }
                        return;
                    }
                }
            }

            // === legacy 整包路径(执行体聚合结果,四/五键 JSON) ===
            let resp = match build_request(&executor_url).send().await {
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

            // === 整包合成:ThinkingDelta(思考非空,整块) →
            // Delta(content 非空) → Done ===
            let reasoning = normalized
                .get("reasoning")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !reasoning.is_empty() {
                yield Ok(StreamChunk::ThinkingDelta(reasoning));
            }
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

/// 单帧处理后的流向(真流式路径)
enum FrameEnd {
    /// 继续收帧
    Continue,
    /// 流收口(done 帧已下行 / [DONE] 哨兵)
    Stop,
    /// 以 Err 收口(fail-visible;调用方 yield Err 后终止)
    Fail(String),
}

/// 执行体流式端点单帧解析 → 下行帧集 + 流向。
///
/// 帧形态:`data:` 行提取(多行 join,与执行体切帧语义对齐)→ 空行跳过 →
/// `[DONE]` 哨兵收口 → JSON 解析失败 warn 跳过(流继续)→ 按帧 `type`
/// 分派到 ThinkingDelta/Delta/ToolCallDelta/done(五键归一+安全网)/error。
fn executor_sse_frame_outcome(
    frame: &str,
    saw_reasoning: &mut bool,
    saw_done: &mut bool,
) -> (Vec<StreamChunk>, FrameEnd) {
    let data: String = frame
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return (Vec::new(), FrameEnd::Continue);
    }
    if data.trim() == "[DONE]" {
        return (Vec::new(), FrameEnd::Stop);
    }
    let frame_json: Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(e) => {
            return (
                vec![StreamChunk::Warn(format!(
                    "SSE 帧解析失败(跳过,流继续): {e}"
                ))],
                FrameEnd::Continue,
            );
        }
    };
    match frame_json.get("type").and_then(Value::as_str) {
        Some("reasoning") => {
            let text = frame_json.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                (Vec::new(), FrameEnd::Continue)
            } else {
                *saw_reasoning = true;
                (
                    vec![StreamChunk::ThinkingDelta(text.to_string())],
                    FrameEnd::Continue,
                )
            }
        }
        Some("content") => {
            let text = frame_json.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                (Vec::new(), FrameEnd::Continue)
            } else {
                (
                    vec![StreamChunk::Delta(text.to_string())],
                    FrameEnd::Continue,
                )
            }
        }
        Some("tool_call_delta") => {
            let index = frame_json.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let fragment = frame_json
                .get("fragment")
                .and_then(Value::as_str)
                .unwrap_or("");
            (
                vec![StreamChunk::ToolCallDelta {
                    index,
                    fragment: fragment.to_string(),
                }],
                FrameEnd::Continue,
            )
        }
        Some("done") => {
            // 安全网:全程零 reasoning 帧 → 终帧思考块补发(防 provider 仅在
            // 终帧给思考块);随后五键归一 → Done 收口
            let reasoning = frame_json
                .get("reasoning")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut chunks = Vec::new();
            if !*saw_reasoning && !reasoning.is_empty() {
                chunks.push(StreamChunk::ThinkingDelta(reasoning.to_string()));
            }
            let normalized = match normalize_executor_response(frame_json) {
                Ok(v) => v,
                Err(e) => return (Vec::new(), FrameEnd::Fail(e)),
            };
            let content = normalized
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let llm_response: LlmResponse =
                serde_json::from_value(normalized).unwrap_or_else(|_| LlmResponse {
                    content,
                    tool_calls: None,
                    finish_reason: Some("stop".to_string()),
                    token_usage: None,
                });
            *saw_done = true;
            chunks.push(StreamChunk::Done(llm_response));
            (chunks, FrameEnd::Stop)
        }
        Some("error") => {
            let msg = frame_json
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("(执行体未附错误说明)");
            (Vec::new(), FrameEnd::Fail(msg.to_string()))
        }
        other => (
            vec![StreamChunk::Warn(format!(
                "SSE 帧类型未知(跳过): {other:?}"
            ))],
            FrameEnd::Continue,
        ),
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
/// - `reasoning`(推理模型增补第五键):缺键/非字符串 → "";归一输出
///   仅非空携带——非思考模型输出与原四键形状逐字节一致。`LlmResponse`
///   结构不读该键(思考内容只走增量帧,本键仅作 execute_stream 消费面)
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
                    // tool_call 真 id 保真透传：provider 原生 id 原样保留，供 wire
                    // 重组按 id 直配（按名 FIFO 降级为 legacy fallback）。缺省不
                    // 携带——无 id 时归一输出与旧四键形状逐字节一致
                    let mut call = serde_json::json!({"tool_name": name, "args": args});
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        call["id"] = serde_json::Value::String(id.to_string());
                    }
                    Some(call)
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
        .and_then(|v| {
            let mut usage: TokenUsage = serde_json::from_value(v.clone()).ok()?;
            // 执行体透传的是 provider raw usage(缓存命中输入 token 嵌在
            // prompt_tokens_details.cached_tokens)——归一时展平入
            // TokenUsage.cached_tokens,随 io_response/审计链/指标面可见
            if usage.cached_tokens.is_none() {
                usage.cached_tokens = v
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize);
            }
            Some(usage)
        });

    let reasoning = json
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    debug!(
        content_len = content.len(),
        reasoning_len = reasoning.len(),
        finish_reason = ?finish_reason,
        tool_calls = ?tool_calls,
        "LLM executor response normalized"
    );
    let mut out = serde_json::json!({
        "content": content,
        "tool_calls": tool_calls,
        "finish_reason": finish_reason,
        "token_usage": token_usage,
    });
    // 第五键 reasoning:仅非空携带——非思考模型输出与原四键形状逐字节一致
    if !reasoning.is_empty() {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("reasoning".to_string(), Value::String(reasoning));
        }
    }
    Ok(out)
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
                            // 真 id 直配优先：归一层保留的 provider 原生 id（或上游
                            // 已转换历史的既有 id）原样上行；无真 id 时确定性合成
                            // （`call_{index}_{name}`，既有行为，legacy fallback）
                            let id = call
                                .get("id")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| format!("call_{}_{}", i, name));
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
///
/// pub(crate):direct 档(llm_direct.rs)复用同款策略,重试判定零分叉
pub(crate) fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
}

/// G3:判断网络层错误是否值得重试(direct 档复用,同上)
pub(crate) fn is_retryable_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

/// G3:计算第 `attempt` 次重试的退避时间(指数 + ±20% 抖动)
///
/// - `attempt`:已失败的次数(0 = 第 1 次重试前的等待)
/// - `base`:初始退避(秒)
/// - `max`:退避上限(秒)
///
/// (direct 档复用,同上)
pub(crate) fn backoff_duration(attempt: usize, base: f64, max: f64) -> Duration {
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

        // direct 档分派(mode="direct" 显式启用):请求构造共用(wire 形状
        // = provider 形状),传输/归一走 llm_direct;relay 路径物理不动
        // (review 性实现——本 if 不在位时下方 relay 代码逐字节原样)。
        if let Some(direct_cfg) = &self.direct {
            return super::llm_direct::execute(&body, direct_cfg, self.direct_retry_params()).await;
        }

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
/// (direct 档复用,同上)
pub(crate) fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
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
        // 默认口径: request_timeout×4 attempts + backoff(1+2+4=7,cap 30 不触) + 10
        // (从缺省常量推导,防止 DEFAULT_REQUEST_TIMEOUT_SECS 调整时本测试硬编码漂移)
        let h = LlmHandler::new("m", "https://x");
        assert_eq!(
            h.worst_case_budget_secs(),
            DEFAULT_REQUEST_TIMEOUT_SECS * 4 + 7 + 10
        );
        // cap 生效口径: base=20 → 20+30+30=80
        let h = h.with_retry_config(3, 20.0, 30.0);
        assert_eq!(
            h.worst_case_budget_secs(),
            DEFAULT_REQUEST_TIMEOUT_SECS * 4 + 80 + 10
        );
        // 零重试口径: 单次请求 + 10
        let h = h.with_retry_config(0, 1.0, 30.0);
        assert_eq!(
            h.worst_case_budget_secs(),
            DEFAULT_REQUEST_TIMEOUT_SECS + 10
        );
    }

    /// 失配判定(半预算=失配口径必红,超预算=修复口径必绿;预算取自同实例实算)
    #[test]
    fn test_step_budget_mismatch_e7() {
        let h = LlmHandler::new("m", "https://x");
        let budget = h.worst_case_budget_secs();
        assert!(h.step_budget_mismatch(budget / 2).is_err());
        assert!(h
            .step_budget_mismatch(budget / 2)
            .unwrap_err()
            .contains("step budget mismatch"));
        assert!(h.step_budget_mismatch(budget + 100).is_ok());
    }

    /// step_timeout=0（不限步超时）跳过失配判定——0 值无预算失配可言。
    #[test]
    fn test_step_budget_mismatch_zero_is_ok() {
        let h = LlmHandler::new("m", "https://x");
        assert!(h.step_budget_mismatch(0).is_ok());
    }

    /// 引擎预算对齐断言:0=禁用守卫;失配命中(worst_case 远大于典型 60s
    /// 引擎预算);预算充足时放行。
    #[test]
    fn test_engine_budget_mismatch_dimensions() {
        let h = LlmHandler::new("m", "https://x");
        // 0=禁用,调用方守卫语义兜底
        assert!(h.engine_budget_mismatch(0).is_ok());
        // 典型部署:worst_case(1800+ 秒级) > 引擎 60s → 失配命中,文案含指引
        let err = h.engine_budget_mismatch(60).expect_err("mismatch expected");
        assert!(err.contains("engine io_error_timeout"), "msg: {err}");
        assert!(err.contains("续跑兜底"), "msg: {err}");
        // 预算充足:worst_case 内放行
        assert!(h.engine_budget_mismatch(h.worst_case_budget_secs()).is_ok());
    }

    /// from_config 消费 [llm] timeout_secs（死配置接通——
    /// 显式声明从被静默忽略转为生效）。
    #[test]
    fn test_from_config_consumes_timeout_secs() {
        let cfg = crate::config::LlmConfig {
            timeout_secs: 11,
            ..Default::default()
        };
        let h = LlmHandler::from_config(&cfg);
        assert_eq!(h.request_timeout_secs, 11);
    }

    /// timeout_secs=0（未显式配置）用内建缺省——接通前后
    /// 未配置用户行为不变（字段死配置时代即 1800 生效）。
    #[test]
    fn test_from_config_zero_uses_builtin_default() {
        let cfg = crate::config::LlmConfig::default();
        let h = LlmHandler::from_config(&cfg);
        assert_eq!(h.request_timeout_secs, DEFAULT_REQUEST_TIMEOUT_SECS);
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

    /// tool_call 真 id 保真:执行体响应归一时 id 键原样透传;无 id 项不合成
    /// id 键(与旧形状逐字节一致)
    #[test]
    fn test_normalize_preserves_tool_call_id() {
        let resp = serde_json::json!({
            "content": "",
            "tool_calls": [
                {"tool_name": "echo", "args": {"text": "hi"}, "id": "call_real_1"},
                {"tool_name": "bare", "args": {}}
            ],
            "finish_reason": "tool_call"
        });
        let out = normalize_executor_response(resp).expect("normalize");
        let calls = out["tool_calls"].as_array().expect("calls");
        assert_eq!(calls[0]["id"], "call_real_1");
        assert!(calls[1].get("id").is_none(), "无 id 项不得合成 id 键");
    }

    /// wire 重组真 id 直配:assistant 携带真 id → 出站用真 id,tool 消息按名
    /// FIFO 消费到的也是真 id;无 id 时回退 call_{i}_{name} 合成不变
    #[test]
    fn test_wire_messages_real_tool_call_id_direct_pairing() {
        let messages = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "",
             "tool_calls": [
                 {"tool_name": "a", "args": {}, "id": "call_real_a"},
                 {"tool_name": "b", "args": {}, "id": "call_real_b"}
             ]},
            {"role": "tool", "content": "ra", "tool_name": "a"},
            {"role": "tool", "content": "rb", "tool_name": "b"}
        ]);
        let wire = to_openai_wire_messages(messages);
        let arr = wire.as_array().unwrap();
        assert_eq!(arr[1]["tool_calls"][0]["id"], "call_real_a");
        assert_eq!(arr[1]["tool_calls"][1]["id"], "call_real_b");
        assert_eq!(arr[2]["tool_call_id"], "call_real_a");
        assert_eq!(arr[3]["tool_call_id"], "call_real_b");

        // 无 id 项:合成形态不变(既有回归锁镜像)
        let legacy = serde_json::json!([
            {"role": "assistant", "content": "",
             "tool_calls": [{"tool_name": "echo", "args": {"text": "hi"}}]},
            {"role": "tool", "content": "ok", "tool_name": "echo"}
        ]);
        let wire = to_openai_wire_messages(legacy);
        let arr = wire.as_array().unwrap();
        assert_eq!(arr[0]["tool_calls"][0]["id"], "call_0_echo");
        assert_eq!(arr[1]["tool_call_id"], "call_0_echo");
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
                Ok(StreamChunk::ThinkingDelta(_)) => {
                    panic!("ThinkingDelta must not be produced without reasoning content")
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

    // ========== 真流式路径(执行体 /llm/execute-stream 孪生端点) ==========

    /// 真流式:归一化帧序列逐帧下行(思考/正文/tool_call 分片/done 五键聚合),
    /// saw_reasoning=true 时 done 帧思考不重发;saw_done 语义锁定
    #[tokio::test]
    async fn test_stream_true_streaming_frame_sequence() {
        let mut server = mockito::Server::new_async().await;
        let sse = [
            r#"data: {"type":"reasoning","text":"思考A"}"#,
            r#"data: {"type":"content","text":"he"}"#,
            r#"data: {"type":"content","text":"llo"}"#,
            r#"data: {"type":"tool_call_delta","index":0,"name":"f","fragment":"{\"a\""}"#,
            r#"data: {"type":"tool_call_delta","index":0,"fragment":":1}"}"#,
            r#"data: {"type":"done","content":"hello","reasoning":"思考A","tool_calls":[{"tool_name":"f","args":{"a":1}}],"finish_reason":"tool_calls","token_usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
            "data: [DONE]",
        ]
        .join("\n\n");
        let _mock = server
            .mock("POST", "/llm/execute-stream")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse)
            .create_async()
            .await;

        let executor_url = format!("{}/llm/execute", server.url());
        let handler = LlmHandler::new("m", &executor_url);
        let params = Value::Object(Default::default());
        let mut stream = handler.execute_stream(&params);

        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.expect("no error expected"));
        }

        assert_eq!(
            chunks.len(),
            6,
            "思考增量已在场 → done 帧思考不重发: {:?}",
            chunks
        );
        match &chunks[0] {
            StreamChunk::ThinkingDelta(t) => assert_eq!(t, "思考A"),
            other => panic!("expected ThinkingDelta, got {:?}", other),
        }
        match &chunks[1] {
            StreamChunk::Delta(t) => assert_eq!(t, "he"),
            other => panic!("expected Delta, got {:?}", other),
        }
        match &chunks[2] {
            StreamChunk::Delta(t) => assert_eq!(t, "llo"),
            other => panic!("expected Delta, got {:?}", other),
        }
        match &chunks[3] {
            StreamChunk::ToolCallDelta { index, fragment } => {
                assert_eq!(*index, 0);
                assert_eq!(fragment, "{\"a\"");
            }
            other => panic!("expected ToolCallDelta, got {:?}", other),
        }
        match &chunks[4] {
            StreamChunk::ToolCallDelta { index, fragment } => {
                assert_eq!(*index, 0);
                assert_eq!(fragment, ":1}");
            }
            other => panic!("expected ToolCallDelta, got {:?}", other),
        }
        match &chunks[5] {
            StreamChunk::Done(resp) => {
                assert_eq!(resp.content, "hello");
                let tcs = resp.tool_calls.as_ref().expect("should have tool_calls");
                assert_eq!(tcs.len(), 1);
                assert_eq!(tcs[0].name, "f");
                assert_eq!(tcs[0].arguments["a"], 1);
                assert_eq!(resp.finish_reason.as_deref(), Some("tool_calls"));
                assert_eq!(resp.token_usage.as_ref().expect("usage").total_tokens, 7);
            }
            other => panic!("expected Done, got {:?}", other),
        }
    }

    /// 真流式:error 帧以 Err 收口(fail-visible),先前增量照常下行
    #[tokio::test]
    async fn test_stream_true_streaming_error_frame_fails_visible() {
        let mut server = mockito::Server::new_async().await;
        let sse = [
            r#"data: {"type":"content","text":"partial"}"#,
            r#"data: {"type":"error","error":"LLM API business error (status_code=1004): invalid"}"#,
        ]
        .join("\n\n");
        let _mock = server
            .mock("POST", "/llm/execute-stream")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse)
            .create_async()
            .await;

        let executor_url = format!("{}/llm/execute", server.url());
        let handler = LlmHandler::new("m", &executor_url);
        let params = Value::Object(Default::default());
        let mut stream = handler.execute_stream(&params);

        match stream.next().await.unwrap() {
            Ok(StreamChunk::Delta(t)) => assert_eq!(t, "partial"),
            other => panic!("expected Delta, got {:?}", other),
        }
        match stream.next().await.unwrap() {
            Err(e) => assert!(
                e.contains("business error"),
                "error frame message must surface: {}",
                e
            ),
            other => panic!("expected Err, got {:?}", other),
        }
        assert!(stream.next().await.is_none(), "Err 后流应终止");
    }

    /// 真流式安全网:全程零 reasoning 帧 → done 帧思考块补发单帧
    /// ThinkingDelta(紧邻 Done 之前,防 provider 仅在终帧给思考块)。
    /// 注意真流式逐帧下行:先到的 content 帧物理上先行,补发思考帧
    /// 位置在已下行增量之后、Done 之前(安全网目标是思考不丢,非重排)
    #[tokio::test]
    async fn test_stream_safety_net_reasoning_from_done_frame() {
        let mut server = mockito::Server::new_async().await;
        let sse = [
            r#"data: {"type":"content","text":"答"}"#,
            r#"data: {"type":"done","content":"答","reasoning":"思","tool_calls":null,"finish_reason":"stop","token_usage":null}"#,
        ]
        .join("\n\n");
        let _mock = server
            .mock("POST", "/llm/execute-stream")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse)
            .create_async()
            .await;

        let executor_url = format!("{}/llm/execute", server.url());
        let handler = LlmHandler::new("m", &executor_url);
        let params = Value::Object(Default::default());
        let mut stream = handler.execute_stream(&params);

        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.expect("no error expected"));
        }
        assert_eq!(chunks.len(), 3, "Delta + 补发思考帧 + Done: {:?}", chunks);
        match &chunks[0] {
            StreamChunk::Delta(t) => assert_eq!(t, "答"),
            other => panic!("expected Delta first (逐帧下行), got {:?}", other),
        }
        match &chunks[1] {
            StreamChunk::ThinkingDelta(t) => assert_eq!(t, "思"),
            other => panic!(
                "safety-net ThinkingDelta must precede Done, got {:?}",
                other
            ),
        }
        assert!(matches!(&chunks[2], StreamChunk::Done(_)));
    }

    /// 404 回落:流式端点不存在 → legacy 整包路径成功;负缓存记账 →
    /// 第二次调用不再探测(404 mock expect(1) 为证)
    #[tokio::test]
    async fn test_stream_404_fallback_and_negative_cache() {
        let mut server = mockito::Server::new_async().await;
        let m404 = server
            .mock("POST", "/llm/execute-stream")
            .with_status(404)
            .with_body("not found")
            .expect(1)
            .create_async()
            .await;
        let mexec = server
            .mock("POST", "/llm/execute")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"content":"legacy","tool_calls":null,"finish_reason":"stop","token_usage":null}"#,
            )
            .expect(2)
            .create_async()
            .await;

        let executor_url = format!("{}/llm/execute", server.url());
        let handler = LlmHandler::new("m", &executor_url);
        let params = Value::Object(Default::default());

        // 第一轮:探测 404 → 回落 legacy
        let mut first = handler.execute_stream(&params);
        let mut chunks1 = Vec::new();
        while let Some(item) = first.next().await {
            chunks1.push(item.expect("fallback path must succeed"));
        }
        assert_eq!(chunks1.len(), 2, "Delta(legacy) + Done: {:?}", chunks1);

        // 第二轮:负缓存记账 → 不再探测,直走 legacy
        let mut second = handler.execute_stream(&params);
        let mut chunks2 = Vec::new();
        while let Some(item) = second.next().await {
            chunks2.push(item.expect("second call must succeed"));
        }
        assert_eq!(chunks2.len(), 2);

        drop(first);
        drop(second);
        m404.assert_async().await;
        mexec.assert_async().await;
        // 收尾清负缓存:server 仍在监听(未 drop,端口未释放)时清理,并行
        // 后继测试不会持同端口——彻底消除 mockito 端口复用 × 负缓存串台
        let stream_url = format!("{}/llm/execute-stream", server.url());
        clear_stream_endpoint_missing_for_test(&stream_url);
    }

    /// legacy 整包五键增强:reasoning 非空 → ThinkingDelta(整块) 先于 Delta
    /// (URL 不带 /llm/execute 后缀 → 无流式端点可试,直走 legacy)
    #[tokio::test]
    async fn test_stream_legacy_five_keys_yields_thinking_block_first() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"content":"hi","reasoning":"思考整块","tool_calls":null,
                    "finish_reason":"stop","token_usage":null}"#,
            )
            .create_async()
            .await;

        let handler = LlmHandler::new("m", &server.url());
        let params = Value::Object(Default::default());
        let mut stream = handler.execute_stream(&params);

        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.expect("no error expected"));
        }
        assert_eq!(
            chunks.len(),
            3,
            "ThinkingDelta + Delta + Done: {:?}",
            chunks
        );
        match &chunks[0] {
            StreamChunk::ThinkingDelta(t) => assert_eq!(t, "思考整块"),
            other => panic!("expected ThinkingDelta first, got {:?}", other),
        }
        match &chunks[1] {
            StreamChunk::Delta(t) => assert_eq!(t, "hi"),
            other => panic!("expected Delta, got {:?}", other),
        }
        assert!(matches!(&chunks[2], StreamChunk::Done(_)));
    }

    /// 路径选择纯函数:env 开关语义 / URL 派生 / 负缓存记账(按 URL 独立)
    #[test]
    fn test_stream_path_selection_helpers() {
        // env 开关:仅字面 "1" 停用(测试经参数注入,不变异进程环境)
        assert!(stream_disabled_by_env(Some("1".to_string())));
        assert!(!stream_disabled_by_env(Some("0".to_string())));
        assert!(!stream_disabled_by_env(Some("".to_string())));
        assert!(!stream_disabled_by_env(None));
        // URL 派生:仅 /llm/execute 后缀可派生孪生端点
        assert_eq!(
            stream_endpoint_of("http://127.0.0.1:9130/llm/execute").as_deref(),
            Some("http://127.0.0.1:9130/llm/execute-stream")
        );
        assert_eq!(stream_endpoint_of("http://other/execute"), None);
        // 负缓存:按 URL 独立记账,互不误伤
        mark_stream_endpoint_missing("http://negcache-test/llm/execute-stream");
        assert!(stream_endpoint_missing(
            "http://negcache-test/llm/execute-stream"
        ));
        assert!(!stream_endpoint_missing(
            "http://other-host/llm/execute-stream"
        ));
    }

    /// 归一第五键:reasoning 非空携带;非思考模型输出与原四键形状一致
    #[test]
    fn test_normalize_executor_response_reasoning_key() {
        let out = normalize_executor_response(serde_json::json!({
            "content": "hi", "reasoning": "思考", "tool_calls": null,
            "finish_reason": "stop", "token_usage": null
        }))
        .unwrap();
        assert_eq!(out["reasoning"], "思考");
        assert_eq!(out["content"], "hi");

        let out = normalize_executor_response(serde_json::json!({
            "content": "hi", "tool_calls": null,
            "finish_reason": "stop", "token_usage": null
        }))
        .unwrap();
        assert!(
            out.get("reasoning").is_none(),
            "四键形状不得多出空 reasoning 键"
        );
        assert_eq!(out["content"], "hi");
    }

    /// 缓存命中观测展平:执行体透传的 provider raw 嵌套形状
    /// prompt_tokens_details.cached_tokens → TokenUsage.cached_tokens;
    /// 未回执/旧三键形状/嵌套对象缺键 → None(可空字段零破坏)
    #[test]
    fn test_normalize_executor_response_cached_tokens_flattened() {
        let out = normalize_executor_response(serde_json::json!({
            "content": "ok", "tool_calls": null, "finish_reason": "stop",
            "token_usage": {
                "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110,
                "prompt_tokens_details": { "cached_tokens": 80 }
            }
        }))
        .unwrap();
        let usage: TokenUsage = serde_json::from_value(out["token_usage"].clone()).unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.cached_tokens, Some(80), "嵌套回执必须展平可见");

        let out = normalize_executor_response(serde_json::json!({
            "content": "ok", "tool_calls": null, "finish_reason": "stop",
            "token_usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))
        .unwrap();
        let usage: TokenUsage = serde_json::from_value(out["token_usage"].clone()).unwrap();
        assert_eq!(
            usage.cached_tokens, None,
            "旧三键形状兼容,缓存字段缺省 None"
        );

        let out = normalize_executor_response(serde_json::json!({
            "content": "ok", "tool_calls": null, "finish_reason": "stop",
            "token_usage": {
                "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2,
                "prompt_tokens_details": {}
            }
        }))
        .unwrap();
        let usage: TokenUsage = serde_json::from_value(out["token_usage"].clone()).unwrap();
        assert_eq!(
            usage.cached_tokens, None,
            "嵌套对象缺 cached_tokens 亦 None"
        );
    }
}

#[cfg(test)]
mod sse_frame_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// o14 流式覆盖增强：executor_sse_frame_outcome 纯函数面边界锁。
    /// 覆盖八分支（content/reasoning/tool_call_delta/done/error/未知类型/
    /// [DONE] 哨兵/坏 JSON）+ 多行 join/空帧/安全网补发三边界。
    fn frame(data: &str) -> String {
        format!("data: {data}\n\n")
    }

    #[test]
    fn test_sse_content_frame_yields_delta() {
        let mut saw_reasoning = false;
        let mut saw_done = false;
        let (chunks, end) = executor_sse_frame_outcome(
            &frame(r#"{"type":"content","text":"你好"}"#),
            &mut saw_reasoning,
            &mut saw_done,
        );
        assert!(matches!(end, FrameEnd::Continue));
        assert_eq!(chunks.len(), 1, "content 帧产单 Delta");
        match &chunks[0] {
            StreamChunk::Delta(t) => assert_eq!(t, "你好"),
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    #[test]
    fn test_sse_multiline_data_joined() {
        let mut saw_reasoning = false;
        let mut saw_done = false;
        let raw = "data: {\"type\":\"content\",\ndata: \"text\":\"ab\"}\n\n";
        let (chunks, end) = executor_sse_frame_outcome(raw, &mut saw_reasoning, &mut saw_done);
        assert!(matches!(end, FrameEnd::Continue));
        match &chunks[0] {
            StreamChunk::Delta(t) => assert_eq!(t, "ab", "多 data: 行 join 后解析"),
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    #[test]
    fn test_sse_blank_and_non_data_lines_skipped() {
        let mut saw_reasoning = false;
        let mut saw_done = false;
        for raw in ["\n\n", "event: message\n\n", ": keep-alive comment\n\n"] {
            let (chunks, end) = executor_sse_frame_outcome(raw, &mut saw_reasoning, &mut saw_done);
            assert!(
                matches!(end, FrameEnd::Continue),
                "无 data 行应继续: {raw:?}"
            );
            assert!(chunks.is_empty(), "无 data 行零 chunks: {raw:?}");
        }
    }

    #[test]
    fn test_sse_done_sentinel_stops_without_chunks() {
        let mut saw_reasoning = false;
        let mut saw_done = false;
        let (chunks, end) =
            executor_sse_frame_outcome(&frame("[DONE]"), &mut saw_reasoning, &mut saw_done);
        assert!(matches!(end, FrameEnd::Stop));
        assert!(chunks.is_empty(), "[DONE] 哨兵零 chunks");
    }

    #[test]
    fn test_sse_malformed_json_warns_and_continues() {
        let mut saw_reasoning = false;
        let mut saw_done = false;
        let (chunks, end) =
            executor_sse_frame_outcome(&frame("{not-json"), &mut saw_reasoning, &mut saw_done);
        assert!(matches!(end, FrameEnd::Continue), "坏 JSON 流继续不收口");
        assert_eq!(chunks.len(), 1);
        assert!(matches!(chunks[0], StreamChunk::Warn(_)), "坏 JSON → Warn");
    }

    #[test]
    fn test_sse_tool_call_delta_index_fragment() {
        let mut saw_reasoning = false;
        let mut saw_done = false;
        let (chunks, end) = executor_sse_frame_outcome(
            &frame(r#"{"type":"tool_call_delta","index":2,"fragment":"{\"a\":"}"#),
            &mut saw_reasoning,
            &mut saw_done,
        );
        assert!(matches!(end, FrameEnd::Continue));
        match &chunks[0] {
            StreamChunk::ToolCallDelta { index, fragment } => {
                assert_eq!(*index, 2, "OpenAI SSE 按 index 分片");
                assert_eq!(fragment, "{\"a\":");
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }
    }

    #[test]
    fn test_sse_reasoning_frame_sets_flag() {
        let mut saw_reasoning = false;
        let mut saw_done = false;
        let (chunks, end) = executor_sse_frame_outcome(
            &frame(r#"{"type":"reasoning","text":"think…"}"#),
            &mut saw_reasoning,
            &mut saw_done,
        );
        assert!(matches!(end, FrameEnd::Continue));
        assert!(saw_reasoning, "reasoning 帧置位 saw_reasoning");
        assert!(matches!(chunks[0], StreamChunk::ThinkingDelta(_)));
    }

    #[test]
    fn test_sse_done_frame_no_backfill_when_reasoning_seen() {
        let mut saw_reasoning = true; // 途中已有 reasoning 帧
        let mut saw_done = false;
        let (chunks, end) = executor_sse_frame_outcome(
            &frame(r#"{"type":"done","reasoning":"late…","content":"ans","finish_reason":"stop"}"#),
            &mut saw_reasoning,
            &mut saw_done,
        );
        assert!(matches!(end, FrameEnd::Stop));
        assert!(saw_done);
        assert_eq!(chunks.len(), 1, "saw_reasoning 已置位 → 终帧思考块不补发");
        assert!(matches!(chunks[0], StreamChunk::Done(_)));
    }

    #[test]
    fn test_sse_done_frame_backfills_reasoning_safety_net() {
        let mut saw_reasoning = false; // 全程零 reasoning 帧
        let mut saw_done = false;
        let (chunks, end) = executor_sse_frame_outcome(
            &frame(
                r#"{"type":"done","reasoning":"only-at-end","content":"ans","finish_reason":"stop"}"#,
            ),
            &mut saw_reasoning,
            &mut saw_done,
        );
        assert!(matches!(end, FrameEnd::Stop));
        assert_eq!(chunks.len(), 2, "安全网：补发 ThinkingDelta + Done");
        assert!(
            matches!(chunks[0], StreamChunk::ThinkingDelta(ref t) if t == "only-at-end"),
            "终帧思考块补发在前"
        );
        match &chunks[1] {
            StreamChunk::Done(r) => assert_eq!(r.content, "ans"),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn test_sse_error_frame_fails_with_message() {
        let mut saw_reasoning = false;
        let mut saw_done = false;
        let (chunks, end) = executor_sse_frame_outcome(
            &frame(r#"{"type":"error","error":"upstream 500"}"#),
            &mut saw_reasoning,
            &mut saw_done,
        );
        assert!(chunks.is_empty());
        let FrameEnd::Fail(msg) = end else {
            panic!("error 帧应 Fail 收口")
        };
        assert_eq!(msg, "upstream 500");
        // 未知类型分支同测（Warn 跳过）
        let (chunks2, end2) = executor_sse_frame_outcome(
            &frame(r#"{"type":"mystery"}"#),
            &mut saw_reasoning,
            &mut saw_done,
        );
        assert!(matches!(end2, FrameEnd::Continue));
        assert!(matches!(chunks2[0], StreamChunk::Warn(_)));
    }
}

#[cfg(test)]
mod config_max_tokens_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// [llm] max_tokens 配置兜底三态:None=4096 硬兜底(既有行为零变化)/
    /// 配置值生效/请求级显式值压过配置
    #[test]
    fn config_max_tokens_precedence() {
        let base = LlmHandler::new("m", "https://x");
        let body = base
            .build_request_body(&serde_json::json!({"prompt": "p"}))
            .unwrap();
        assert_eq!(
            body["max_tokens"],
            serde_json::json!(4096),
            "None 兜底必须保持 4096(既有行为零变化)"
        );

        let configured = base.with_config_max_tokens(Some(32768));
        let body = configured
            .build_request_body(&serde_json::json!({"prompt": "p"}))
            .unwrap();
        assert_eq!(body["max_tokens"], serde_json::json!(32768));

        let body = configured
            .build_request_body(&serde_json::json!({"prompt": "p", "max_tokens": 1024}))
            .unwrap();
        assert_eq!(
            body["max_tokens"],
            serde_json::json!(1024),
            "请求级显式值压过配置兜底"
        );
    }
}
