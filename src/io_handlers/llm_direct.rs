// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! LLM 直连传输（轨 B，opt-in）—— `mode = "direct"` 时 agent 进程直连 provider
//!
//! ## 定位（与 relay 档的分工）
//!
//! relay 档（缺省）所有 LLM 出站经执行体中继，key 归边执行体侧（SG-1）。
//! direct 档为显式启用的直连通道：agent 进程持凭据（env 链/toml 槽位），
//! 直连 `[llm] api_base` 声明的 OpenAI 兼容端点——单机部署不再要求执行体
//! 侧配 key。mode 分派收在 [`LlmHandler`](super::llm_handler::LlmHandler)
//! 内部（IoDispatcher/runner/audited_llm 下游零感知）；本文件只承载
//! direct 传输本体，relay 路径代码物理不动。
//!
//! ## 请求形状
//!
//! 与 relay 档共用 [`to_openai_wire_messages`](super::llm_handler::to_openai_wire_messages)
//! 产出的请求体（model/messages/temperature/max_tokens/stream/tools/
//! tool_choice，OpenAI 兼容 wire 形状——执行体原样透传即 provider 形状，
//! 请求侧无新增转换）。出站 = POST `api_base`（全 URL）+
//! `Authorization: Bearer {api_key}`。
//!
//! ## 响应归一
//!
//! provider raw（OpenAI 兼容 chat.completions）→ 内部四键形状（与执行体
//! 归一输出同构，`LlmResponse` 可直接反序列化）：
//! - `choices[0].message.content` → `content`
//! - `choices[0].message.tool_calls[]` → `[{tool_name, args, id?}]`
//!   （`function.arguments` 字符串 parse，失败回落 `{}`；**provider 原生
//!   id 透传**——relay 档执行体四键不含 id，direct 档保真是本档能力差异）
//! - `choices[0].finish_reason` → `finish_reason`
//! - `usage` → `token_usage`（try-parse，`prompt_tokens_details.cached_tokens`
//!   展平，形状不符降级 null）
//! - `choices[0].message.reasoning_content` → `reasoning`（第五键，非空携带）
//! - MiniMax 业务错误面（HTTP 200 + `base_resp.status_code != 0`）显式转 Err
//! - 缺 `choices` / 非 JSON → Err（错误字符串形态与执行体路径一致）
//!
//! ## 重试与错误映射
//!
//! 与 relay 档同款策略（复用同一组判定/退避函数，零策略分叉）：
//! 429/5xx/网络错 → 指数退避重试；401/403/4xx → 不重试直接报。
//!
//! ## 流式
//!
//! OpenAI SSE chunk 逐帧增量解析（`data: {...}` / `[DONE]` 哨兵），产出
//! 与 relay 真流式同型的 `StreamChunk` 序列（ThinkingDelta/Delta/
//! ToolCallDelta/Done），下游（runner 增量渲染 + Done 聚合消费）零感知。

use std::collections::BTreeMap;

use futures_core::Stream;
use futures_util::StreamExt;
use serde_json::Value;
use tracing::{debug, warn};

use super::llm_handler::{
    backoff_duration, is_retryable_error, is_retryable_status, parse_retry_after, StreamChunk,
};
use crate::agent::translator::{LlmResponse, TokenUsage, ToolCall};

/// direct 档连接参数（agent 侧持有的直连凭据与端点）
///
/// 由 `LlmConfig::from_config` 在 `mode = "direct"` 时构造；relay 档
/// handler 不持有本结构（None），凭据不进 relay 路径任何代码。
#[derive(Debug, Clone)]
pub struct DirectLlmConfig {
    /// provider 凭据（启动期校验非空；只进内存与请求头，不落盘不进日志）
    pub api_key: String,
    /// provider 直连端点（OpenAI 兼容 chat/completions 全 URL）
    pub api_base: String,
}

/// direct 档单次出站参数（自 handler 字段透传，stream! 块内不持有 &self）
#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectRetryParams {
    pub max_retries: usize,
    pub base_backoff_secs: f64,
    pub max_backoff_secs: f64,
    pub connect_timeout_secs: u64,
    pub request_timeout_secs: u64,
}

/// direct 档非流式调用（含同款重试循环）
///
/// `body` 为与 relay 档共用的 OpenAI 兼容请求体（调用侧已构造）。
/// 返回内部四键形状 JSON（可被 `LlmResponse` 反序列化）。
pub(crate) async fn execute(
    body: &Value,
    cfg: &DirectLlmConfig,
    retry: DirectRetryParams,
) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(retry.connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(retry.request_timeout_secs))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    for attempt in 0..=retry.max_retries {
        let result = client
            .post(&cfg.api_base)
            .bearer_auth(&cfg.api_key)
            .json(body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                if attempt > 0 {
                    debug!(attempt, "direct LLM API recovered after retry");
                }
                let json: Value = resp
                    .json()
                    .await
                    .map_err(|e| format!("LLM API response parse error: {}", e))?;
                return normalize_provider_response(json);
            }
            Ok(resp) if is_retryable_status(resp.status()) && attempt < retry.max_retries => {
                let status = resp.status();
                let backoff = parse_retry_after(&resp).unwrap_or_else(|| {
                    backoff_duration(attempt, retry.base_backoff_secs, retry.max_backoff_secs)
                });
                warn!(
                    attempt,
                    status = status.as_u16(),
                    backoff_ms = backoff.as_millis() as u64,
                    "direct LLM API retryable status, backing off"
                );
                let _ = resp.text().await;
                tokio::time::sleep(backoff).await;
                continue;
            }
            Ok(resp) => {
                let status = resp.status();
                let error_text = resp.text().await.unwrap_or_default();
                if attempt == retry.max_retries && is_retryable_status(status) {
                    warn!(
                        attempt,
                        status = status.as_u16(),
                        "direct LLM API retries exhausted"
                    );
                } else {
                    warn!(
                        status = status.as_u16(),
                        "direct LLM API non-retryable error"
                    );
                }
                return Err(format!("LLM API error ({}): {}", status, error_text));
            }
            Err(e) if is_retryable_error(&e) && attempt < retry.max_retries => {
                let backoff =
                    backoff_duration(attempt, retry.base_backoff_secs, retry.max_backoff_secs);
                warn!(
                    attempt,
                    error = %e,
                    backoff_ms = backoff.as_millis() as u64,
                    "direct LLM API network error, backing off"
                );
                tokio::time::sleep(backoff).await;
                continue;
            }
            Err(e) => {
                if attempt == retry.max_retries {
                    warn!(attempt, error = %e, "direct LLM API retries exhausted");
                } else {
                    warn!(error = %e, "direct LLM API non-retryable network error");
                }
                return Err(format!("LLM API request failed: {}", e));
            }
        }
    }
    Err("LLM API retry loop exhausted without return".to_string())
}

/// provider raw（OpenAI 兼容 chat.completions）→ 内部四键形状
///
/// 归一规则见模块头注；输出可直接 `serde_json::from_value::<LlmResponse>`。
/// MiniMax 直连形态（`base_resp` 业务错误面）与缺 `choices` 显式报错。
pub(crate) fn normalize_provider_response(json: Value) -> Result<Value, String> {
    // MiniMax 业务错误面:HTTP 200 + base_resp.status_code != 0(与执行体
    // completion_four_keys 同判据;其他 provider 无该键零影响)
    if let Some(base_resp) = json.get("base_resp") {
        let status_code = base_resp.get("status_code").and_then(Value::as_i64);
        let is_error = match status_code {
            Some(code) => code != 0,
            None => true, // 有 base_resp 无 status_code:形态异常,按错误处理
        };
        if is_error {
            let status_msg = base_resp
                .get("status_msg")
                .and_then(Value::as_str)
                .unwrap_or("(no status_msg)");
            return Err(format!(
                "LLM API business error (status_code={status_code:?}): {status_msg}"
            ));
        }
    }

    let choice = json
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or_else(|| {
            let snippet: String = json.to_string().chars().take(300).collect();
            format!("LLM API response missing choices: {snippet}")
        })?;

    let content = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // tool_calls:function.name/arguments → 内部统一形状;provider 原生 id
    // 透传(journal 消费端无 id 配对依赖,真 id 保真是 direct 档能力)
    let tool_calls = choice
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
        .map(|arr| {
            let normalized: Vec<Value> = arr
                .iter()
                .filter_map(|tc| {
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .or_else(|| tc.get("tool_name").and_then(Value::as_str))?;
                    let args = match tc.get("function").and_then(|f| f.get("arguments")) {
                        Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                            .unwrap_or(Value::Object(Default::default())),
                        Some(obj @ Value::Object(_)) => obj.clone(),
                        _ => Value::Object(Default::default()),
                    };
                    let mut call = serde_json::json!({"tool_name": name, "args": args});
                    if let Some(id) = tc.get("id").and_then(Value::as_str) {
                        call["id"] = Value::String(id.to_string());
                    }
                    Some(call)
                })
                .collect();
            Value::Array(normalized)
        })
        .filter(|arr| !arr.as_array().unwrap().is_empty());

    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string);

    let token_usage = parse_token_usage(json.get("usage"));

    let reasoning = choice
        .get("message")
        .and_then(|m| m.get("reasoning_content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut out = serde_json::json!({
        "content": content,
        "tool_calls": tool_calls,
        "finish_reason": finish_reason,
        "token_usage": token_usage,
    });
    if !reasoning.is_empty() {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("reasoning".to_string(), Value::String(reasoning));
        }
    }
    Ok(out)
}

/// provider raw usage → `TokenUsage`（try-parse 同口径;cached_tokens 展平;
/// 形状不符 → None,防 usage 缺字段导致整响应失败）
fn parse_token_usage(usage: Option<&Value>) -> Option<Value> {
    let usage = usage.filter(|v| !v.is_null())?;
    let mut parsed: TokenUsage = serde_json::from_value(usage.clone()).ok()?;
    if parsed.cached_tokens.is_none() {
        parsed.cached_tokens = usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
    }
    serde_json::to_value(parsed).ok()
}

/// SSE tool_call 按 index 聚合状态（Done 终帧合成完整 tool_calls 用）
#[derive(Default)]
struct DirectToolAcc {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// OpenAI SSE 单帧(delta 帧形态)→ 增量产出与终帧记账
///
/// 帧形态:`data: {...}` 行提取(多行 join)→ 空帧/`[DONE]` 哨兵 →
/// JSON 解析失败 Warn 跳过(流继续,与 relay 同语义)→ delta 分派:
/// reasoning_content / content / tool_calls[index] 分片 / finish_reason
/// 与 usage 终值记账。MiniMax 流式业务错面(base_resp.status_code != 0)
/// 显式 Err。
fn provider_sse_frame_outcome(
    frame: &str,
    acc_content: &mut String,
    acc_reasoning: &mut String,
    acc_tools: &mut BTreeMap<usize, DirectToolAcc>,
    acc_finish: &mut Option<String>,
    acc_usage: &mut Option<Value>,
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

    let Ok(json) = serde_json::from_str::<Value>(&data) else {
        return (
            vec![StreamChunk::Warn(
                "direct SSE 帧解析失败(跳过,流继续)".to_string(),
            )],
            FrameEnd::Continue,
        );
    };

    // MiniMax 流式业务错误面:HTTP 200 内 base_resp.status_code != 0
    if let Some(base_resp) = json.get("base_resp") {
        let status_code = base_resp.get("status_code").and_then(Value::as_i64);
        let is_error = match status_code {
            Some(code) => code != 0,
            None => true,
        };
        if is_error {
            let status_msg = base_resp
                .get("status_msg")
                .and_then(Value::as_str)
                .unwrap_or("(no status_msg)");
            return (
                Vec::new(),
                FrameEnd::Fail(format!(
                    "LLM API business error (status_code={status_code:?}): {status_msg}"
                )),
            );
        }
    }

    let Some(choice) = json
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    else {
        // usage-only 终帧(stream_options.include_usage 形态:choices 空数组
        // + usage 记账)或其他无 choices 帧——仅记账不产出
        if let Some(u) = json.get("usage").filter(|v| !v.is_null()) {
            *acc_usage = Some(u.clone());
        }
        return (Vec::new(), FrameEnd::Continue);
    };

    let mut chunks = Vec::new();
    let delta = choice.get("delta");

    if let Some(r) = delta
        .and_then(|d| d.get("reasoning_content"))
        .and_then(Value::as_str)
    {
        if !r.is_empty() {
            acc_reasoning.push_str(r);
            chunks.push(StreamChunk::ThinkingDelta(r.to_string()));
        }
    }
    if let Some(c) = delta.and_then(|d| d.get("content")).and_then(Value::as_str) {
        if !c.is_empty() {
            acc_content.push_str(c);
            chunks.push(StreamChunk::Delta(c.to_string()));
        }
    }
    if let Some(calls) = delta
        .and_then(|d| d.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for tc in calls {
            let Some(idx) = tc.get("index").and_then(Value::as_u64) else {
                continue;
            };
            let slot = acc_tools.entry(idx as usize).or_default();
            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                slot.id = Some(id.to_string());
            }
            if let Some(f) = tc.get("function") {
                if let Some(n) = f.get("name").and_then(Value::as_str) {
                    if !n.is_empty() {
                        slot.name.push_str(n);
                    }
                }
                if let Some(frag) = f.get("arguments").and_then(Value::as_str) {
                    if !frag.is_empty() {
                        slot.arguments.push_str(frag);
                        chunks.push(StreamChunk::ToolCallDelta {
                            index: idx as usize,
                            fragment: frag.to_string(),
                        });
                    }
                }
            }
        }
    }
    if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
        if !fr.is_empty() {
            *acc_finish = Some(fr.to_string());
        }
    }
    if let Some(u) = json.get("usage").filter(|v| !v.is_null()) {
        *acc_usage = Some(u.clone());
    }

    (chunks, FrameEnd::Continue)
}

/// 单帧处理后的流向(direct 流式路径)
pub(crate) enum FrameEnd {
    /// 继续收帧
    Continue,
    /// 流收口([DONE] 哨兵)
    Stop,
    /// 以 Err 收口(fail-visible)
    Fail(String),
}

/// direct 档流式调用(OpenAI SSE 增量解析)
///
/// 产出与 relay 真流式同型 `StreamChunk` 序列;`[DONE]` 哨兵后合成完整
/// `LlmResponse` 终帧(聚合自全部增量:content/reasoning/tool_calls 按
/// index 聚合/finish_reason/usage)。流未含 `[DONE]` 即连接提前关闭 →
/// fail-visible Err(与 relay 未含 done 终帧同语义)。流式请求不重试
/// (调用方可回落非流式 `execute`,与 relay 调用方约定一致)。
pub(crate) fn execute_stream(
    body: Value,
    cfg: DirectLlmConfig,
    retry: DirectRetryParams,
) -> std::pin::Pin<Box<dyn Stream<Item = Result<StreamChunk, String>> + Send>> {
    Box::pin(async_stream::stream! {
        let mut body = body;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".to_string(), Value::Bool(true));
        }

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(retry.connect_timeout_secs))
            .timeout(std::time::Duration::from_secs(retry.request_timeout_secs))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let resp = match client
            .post(&cfg.api_base)
            .bearer_auth(&cfg.api_key)
            .json(&body)
            .send()
            .await
        {
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

        // SSE 增量解析(\n\n 切帧,残留缓冲收尾——与 relay 切帧语义对齐)
        let mut buffer = String::new();
        let mut acc_content = String::new();
        let mut acc_reasoning = String::new();
        let mut acc_tools: BTreeMap<usize, DirectToolAcc> = BTreeMap::new();
        let mut acc_finish: Option<String> = None;
        let mut acc_usage: Option<Value> = None;
        let mut saw_done = false;
        let mut sse = resp.bytes_stream();
        'frames: loop {
            while let Some(pos) = buffer.find("\n\n") {
                let frame: String = buffer.drain(..pos + 2).collect();
                let (chunks, end) = provider_sse_frame_outcome(
                    &frame,
                    &mut acc_content,
                    &mut acc_reasoning,
                    &mut acc_tools,
                    &mut acc_finish,
                    &mut acc_usage,
                );
                for chunk in chunks {
                    yield Ok(chunk);
                }
                match end {
                    FrameEnd::Continue => {}
                    FrameEnd::Stop => {
                        saw_done = true;
                        break 'frames;
                    }
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
        // 流收尾残留缓冲(末帧无尾随空行形态)
        let residual = std::mem::take(&mut buffer);
        if !residual.trim().is_empty() {
            let (chunks, end) = provider_sse_frame_outcome(
                &residual,
                &mut acc_content,
                &mut acc_reasoning,
                &mut acc_tools,
                &mut acc_finish,
                &mut acc_usage,
            );
            for chunk in chunks {
                yield Ok(chunk);
            }
            match end {
                FrameEnd::Continue => {}
                FrameEnd::Stop => saw_done = true,
                FrameEnd::Fail(msg) => {
                    yield Err(msg);
                    return;
                }
            }
        }
        if !saw_done {
            yield Err("direct 流式响应未含 [DONE] 终止符(连接提前关闭)".to_string());
            return;
        }

        // === [DONE] 后合成完整 LlmResponse 终帧(聚合自全部增量) ===
        let tool_calls = if acc_tools.is_empty() {
            None
        } else {
            Some(
                acc_tools
                    .into_values()
                    .map(|acc| {
                        let arguments: Value =
                            serde_json::from_str(acc.arguments.trim()).unwrap_or(Value::Object(Default::default()));
                        ToolCall {
                            name: acc.name,
                            arguments,
                            id: acc.id,
                        }
                    })
                    .collect::<Vec<ToolCall>>(),
            )
        };
        let token_usage = acc_usage.as_ref().and_then(|u| {
            let mut parsed: TokenUsage = serde_json::from_value(u.clone()).ok()?;
            if parsed.cached_tokens.is_none() {
                parsed.cached_tokens = u
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize);
            }
            Some(parsed)
        });
        yield Ok(StreamChunk::Done(LlmResponse {
            content: acc_content,
            tool_calls,
            finish_reason: acc_finish.or_else(|| Some("stop".to_string())),
            token_usage,
        }));
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------- normalize_provider_response 归一逐字段 ----------

    #[test]
    fn test_normalize_full_shape() {
        let raw = json!({
            "id": "chatcmpl-1",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "答案"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "prompt_tokens_details": {"cached_tokens": 4}
            }
        });
        let out = normalize_provider_response(raw).unwrap();
        assert_eq!(out["content"], "答案");
        assert_eq!(out["finish_reason"], "stop");
        assert_eq!(out["tool_calls"], Value::Null);
        assert_eq!(out["token_usage"]["prompt_tokens"], 10);
        assert_eq!(out["token_usage"]["cached_tokens"], 4);
        // 非思考模型:四键形状,无第五键
        assert!(out.get("reasoning").is_none());
    }

    #[test]
    fn test_normalize_tool_calls_with_native_id() {
        // OpenAI 原生 id 透传;arguments 字符串 parse
        let raw = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_abc123",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let out = normalize_provider_response(raw).unwrap();
        let calls = out["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["tool_name"], "get_weather");
        assert_eq!(calls[0]["args"]["city"], "北京");
        assert_eq!(calls[0]["id"], "call_abc123");
    }

    #[test]
    fn test_normalize_arguments_invalid_json_fallback_empty() {
        // arguments 非法 JSON → 归一为空对象(对齐执行体既有容错)
        let raw = json!({
            "choices": [{
                "message": {"tool_calls": [{
                    "id": "call_x", "type": "function",
                    "function": {"name": "f", "arguments": "{broken"}
                }]}
            }]
        });
        let out = normalize_provider_response(raw).unwrap();
        assert_eq!(out["tool_calls"][0]["args"], json!({}));
    }

    #[test]
    fn test_normalize_reasoning_carried_only_when_non_empty() {
        let raw = json!({
            "choices": [{
                "message": {"content": "答", "reasoning_content": "思考中"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
        });
        let out = normalize_provider_response(raw).unwrap();
        assert_eq!(out["reasoning"], "思考中");
        assert_eq!(out["token_usage"]["total_tokens"], 3);
    }

    #[test]
    fn test_normalize_minimax_business_error() {
        // HTTP 200 + base_resp.status_code != 0 → 显式 Err
        let raw = json!({
            "base_resp": {"status_code": 1004, "status_msg": "invalid api key"},
            "choices": []
        });
        let err = normalize_provider_response(raw).unwrap_err();
        assert!(err.contains("1004"));
        assert!(err.contains("invalid api key"));
    }

    #[test]
    fn test_normalize_missing_choices_error() {
        let raw = json!({"error": {"message": "boom"}});
        let err = normalize_provider_response(raw).unwrap_err();
        assert!(err.contains("missing choices"));
        assert!(err.contains("boom"));
    }

    #[test]
    fn test_normalize_usage_shape_mismatch_degrades_null() {
        // usage 形状不符 → 降级 null,不拖垮整响应
        let raw = json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
            "usage": {"unexpected_field": true}
        });
        let out = normalize_provider_response(raw).unwrap();
        assert_eq!(out["token_usage"], Value::Null);
        assert_eq!(out["content"], "ok");
    }

    // ---------- SSE 帧解析 ----------

    #[test]
    fn test_sse_frames_content_and_done() {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tools = BTreeMap::new();
        let mut finish = None;
        let mut usage = None;

        let (chunks, end) = provider_sse_frame_outcome(
            "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n",
            &mut content,
            &mut reasoning,
            &mut tools,
            &mut finish,
            &mut usage,
        );
        assert!(matches!(end, FrameEnd::Continue));
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0], StreamChunk::Delta(t) if t == "你"));

        let (chunks, end) = provider_sse_frame_outcome(
            "data: [DONE]\n\n",
            &mut content,
            &mut reasoning,
            &mut tools,
            &mut finish,
            &mut usage,
        );
        assert!(matches!(end, FrameEnd::Stop));
        assert!(chunks.is_empty());
        assert_eq!(content, "你");
    }

    #[test]
    fn test_sse_frames_tool_call_accumulation() {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tools = BTreeMap::new();
        let mut finish = None;
        let mut usage = None;

        // 分片 1:index+id+name+arguments 前半
        let (chunks, _) = provider_sse_frame_outcome(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\"\"}}]}}]}\n\n",
            &mut content, &mut reasoning, &mut tools, &mut finish, &mut usage,
        );
        assert_eq!(chunks.len(), 1);
        assert!(
            matches!(&chunks[0], StreamChunk::ToolCallDelta { index: 0, fragment } if fragment == "{\"a\"")
        );
        // 分片 2:arguments 后半
        let (chunks, _) = provider_sse_frame_outcome(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]}}]}\n\n",
            &mut content, &mut reasoning, &mut tools, &mut finish, &mut usage,
        );
        assert!(
            matches!(&chunks[0], StreamChunk::ToolCallDelta { index: 0, fragment } if fragment == ":1}")
        );
        // 终帧 finish_reason + usage
        provider_sse_frame_outcome(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,\"total_tokens\":5}}\n\n",
            &mut content, &mut reasoning, &mut tools, &mut finish, &mut usage,
        );
        assert_eq!(finish.as_deref(), Some("tool_calls"));
        assert!(usage.is_some());
        // 聚合:Done 帧合成完整 ToolCall(id 保真 + arguments 完整 parse)
        let acc = tools.remove(&0).unwrap();
        let arguments: Value = serde_json::from_str(&acc.arguments).unwrap();
        assert_eq!(arguments["a"], 1);
        assert_eq!(acc.id.as_deref(), Some("call_9"));
        assert_eq!(acc.name, "f");
    }

    #[test]
    fn test_sse_frame_reasoning_delta() {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tools = BTreeMap::new();
        let mut finish = None;
        let mut usage = None;
        let (chunks, end) = provider_sse_frame_outcome(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"思\"}}]}\n\n",
            &mut content,
            &mut reasoning,
            &mut tools,
            &mut finish,
            &mut usage,
        );
        assert!(matches!(end, FrameEnd::Continue));
        assert!(matches!(&chunks[0], StreamChunk::ThinkingDelta(t) if t == "思"));
        assert_eq!(reasoning, "思");
    }

    #[test]
    fn test_sse_frame_malformed_json_warn_skip() {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tools = BTreeMap::new();
        let mut finish = None;
        let mut usage = None;
        let (chunks, end) = provider_sse_frame_outcome(
            "data: {broken json}\n\n",
            &mut content,
            &mut reasoning,
            &mut tools,
            &mut finish,
            &mut usage,
        );
        assert!(matches!(end, FrameEnd::Continue));
        assert!(matches!(&chunks[0], StreamChunk::Warn(_)));
    }

    #[test]
    fn test_sse_frame_minimax_business_error_fails_visible() {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tools = BTreeMap::new();
        let mut finish = None;
        let mut usage = None;
        let (chunks, end) = provider_sse_frame_outcome(
            "data: {\"base_resp\":{\"status_code\":1027,\"status_msg\":\"out of quota\"},\"choices\":[]}\n\n",
            &mut content, &mut reasoning, &mut tools, &mut finish, &mut usage,
        );
        assert!(chunks.is_empty());
        match end {
            FrameEnd::Fail(msg) => {
                assert!(msg.contains("1027"));
                assert!(msg.contains("out of quota"));
            }
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn test_sse_usage_only_final_frame_recorded() {
        // include_usage 形态:choices 空数组 + usage 终帧,仅记账
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tools = BTreeMap::new();
        let mut finish = None;
        let mut usage = None;
        let (chunks, end) = provider_sse_frame_outcome(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":8,\"total_tokens\":15}}\n\n",
            &mut content, &mut reasoning, &mut tools, &mut finish, &mut usage,
        );
        assert!(matches!(end, FrameEnd::Continue));
        assert!(chunks.is_empty());
        assert!(usage.is_some());
    }

    // ---------- 集成(mock provider server,先例 = llm_handler mock 族) ----------

    use crate::io_handler::IoHandler;
    use crate::io_handlers::llm_handler::LlmHandler;

    fn direct_retry_params_for_test() -> DirectRetryParams {
        DirectRetryParams {
            max_retries: 3,
            base_backoff_secs: 0.001,
            max_backoff_secs: 0.01,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
        }
    }

    /// from_config 分派:mode=direct 时 handler.execute 出站直连 mock provider
    /// (请求带 Bearer 凭据;OpenAI raw 响应归一四键)
    #[tokio::test]
    async fn test_llm_handler_dispatches_direct_execute() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/")
            .match_header("authorization", "Bearer test-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"id":"c1","choices":[{"message":{"role":"assistant","content":"你好"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
            )
            .create_async()
            .await;

        let cfg = crate::config::LlmConfig {
            mode: "direct".to_string(),
            api_key: "test-key".to_string(),
            api_base: server.url(),
            ..Default::default()
        };
        assert_eq!(cfg.mode, "direct");
        let handler = LlmHandler::from_config(&cfg);
        let out = handler
            .execute(&serde_json::json!({"prompt": "hi"}))
            .await
            .unwrap();
        assert_eq!(out["content"], "你好");
        assert_eq!(out["finish_reason"], "stop");
        assert_eq!(out["token_usage"]["total_tokens"], 5);
        // 归一输出可反序列化为 LlmResponse(四键同构判据)
        let llm_resp: LlmResponse = serde_json::from_value(out).unwrap();
        assert_eq!(llm_resp.content, "你好");
    }

    /// direct 工具调用剧本:OpenAI 原生 tool_calls 归一 + 真 id 保真 +
    /// LlmResponse 反序列化(原生 id 端到端保真)
    #[tokio::test]
    async fn test_direct_tool_call_e2e_native_id() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"call_native_1","type":"function","function":{"name":"file_read","arguments":"{\"path\":\"/tmp/a.txt\"}"}}]},"finish_reason":"tool_calls"}],"usage":null}"#,
            )
            .create_async()
            .await;

        let cfg = DirectLlmConfig {
            api_key: "k".into(),
            api_base: server.url(),
        };
        let body = serde_json::json!({"model":"m","messages":[{"role":"user","content":"读文件"}]});
        let out = execute(&body, &cfg, direct_retry_params_for_test())
            .await
            .unwrap();
        let llm_resp: LlmResponse = serde_json::from_value(out).unwrap();
        let calls = llm_resp.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[0].arguments["path"], "/tmp/a.txt");
        assert_eq!(calls[0].id.as_deref(), Some("call_native_1"));
    }

    /// direct 重试剧本:503 → 200 同款指数退避(策略复用,零分叉)
    #[tokio::test]
    async fn test_direct_retry_on_5xx() {
        let mut server = mockito::Server::new_async().await;
        let _m1 = server
            .mock("POST", "/")
            .with_status(503)
            .with_body("server error")
            .create_async()
            .await;
        let _m2 = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#)
            .create_async()
            .await;

        let cfg = DirectLlmConfig {
            api_key: "k".into(),
            api_base: server.url(),
        };
        let body = serde_json::json!({"model":"m","messages":[{"role":"user","content":"hi"}]});
        let out = execute(&body, &cfg, direct_retry_params_for_test())
            .await
            .unwrap();
        assert_eq!(out["content"], "ok");
    }

    /// direct 负向:401 凭据错误不重试直接报(可重试状态码不含 401/403)
    #[tokio::test]
    async fn test_direct_401_no_retry() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/")
            .expect(1)
            .with_status(401)
            .with_body(r#"{"error":{"message":"invalid api key"}}"#)
            .create_async()
            .await;

        let cfg = DirectLlmConfig {
            api_key: "bad".into(),
            api_base: server.url(),
        };
        let body = serde_json::json!({"model":"m","messages":[{"role":"user","content":"hi"}]});
        let err = execute(&body, &cfg, direct_retry_params_for_test())
            .await
            .unwrap_err();
        assert!(err.contains("401"));
        m.assert(); // expect(1):恰好一次请求,未重试
    }

    /// direct 流式剧本:OpenAI SSE 增量帧 → StreamChunk 序列 + Done 聚合
    #[tokio::test]
    async fn test_direct_stream_sse_e2e() {
        let mut server = mockito::Server::new_async().await;
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"思\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"答\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let _m = server
            .mock("POST", "/")
            .match_header("authorization", "Bearer sk-stream")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse_body)
            .create_async()
            .await;

        let cfg = DirectLlmConfig {
            api_key: "sk-stream".into(),
            api_base: server.url(),
        };
        let body = serde_json::json!({"model":"m","messages":[{"role":"user","content":"hi"}]});
        use futures_util::StreamExt;
        let mut stream = execute_stream(body, cfg, direct_retry_params_for_test());
        let mut saw_thinking = false;
        let mut saw_delta = false;
        let mut done: Option<LlmResponse> = None;
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                StreamChunk::ThinkingDelta(t) => {
                    assert_eq!(t, "思");
                    saw_thinking = true;
                }
                StreamChunk::Delta(t) => {
                    assert_eq!(t, "答");
                    saw_delta = true;
                }
                StreamChunk::Done(r) => done = Some(r),
                other => panic!("unexpected chunk: {:?}", other),
            }
        }
        assert!(saw_thinking && saw_delta);
        let r = done.expect("Done 帧必达");
        assert_eq!(r.content, "答");
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
        assert_eq!(r.tool_calls, None);
        assert_eq!(r.token_usage.map(|u| u.total_tokens), Some(2));
    }

    /// 等价性双跑(本批灵魂判据):同一 goal 经 relay(mock 执行体)与
    /// direct(mock provider)各跑一次,归一输出逐字段同构——仅传输层
    /// 不同,四键形状一致
    #[tokio::test]
    async fn test_relay_direct_equivalence_same_goal() {
        // relay 侧:mock 执行体返回四键归一形状(执行体已归一)
        let mut relay_server = mockito::Server::new_async().await;
        let _relay_m = relay_server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"content":"你好","tool_calls":null,"finish_reason":"stop","token_usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
            )
            .create_async()
            .await;
        let relay_handler = LlmHandler::new("m", &relay_server.url());
        let goal = serde_json::json!({"messages":[{"role":"user","content":"hi"}]});
        let relay_out = relay_handler.execute(&goal).await.unwrap();

        // direct 侧:mock provider 返回 OpenAI raw(agent 侧归一)
        let mut direct_server = mockito::Server::new_async().await;
        let _direct_m = direct_server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"choices":[{"message":{"role":"assistant","content":"你好"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
            )
            .create_async()
            .await;
        let direct_cfg = DirectLlmConfig {
            api_key: "k".into(),
            api_base: direct_server.url(),
        };
        let body = serde_json::json!({"model":"m","messages":[{"role":"user","content":"hi"}]});
        let direct_out = execute(&body, &direct_cfg, direct_retry_params_for_test())
            .await
            .unwrap();

        // 同构判据:四键逐字段一致(等价性允许差异面 = 仅传输相关字段;
        // 本剧本两边都无 id/reasoning 键 → 输出 JSON 全等)
        assert_eq!(relay_out, direct_out);
    }
}
