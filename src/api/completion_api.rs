// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! AI 补全/Inline Chat 端点(FIM 代理+改写代理)。
//!
//! 与对话 LLM 的关系:复用 `LlmConfig` 配置面(模型/key/端点),但请求形态独立
//! ——补全是**非流式短超时** FIM 调用,Inline Chat 是**改写全文**调用,均不经
//! 对话执行体(llm_handler 在途重构中,独立实现避免耦合;配置面同源零重复)。
//!
//! 治理边界:两端点挂 G16 token 中间件;服务端不落盘 prompt 正文(tracing
//! 脱敏);外发目标=既有 LLM 配置端点(无新第三方);.evoignore 排除在前端
//! prompt 组装层执行(serve 端不重复判定)。

#![forbid(unsafe_code)]

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::agent_api::AgentApiState;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fim_budget_clips_by_chars() {
        let prefix: String = "中".repeat(9000);
        let cut: String = {
            let n = prefix.chars().count();
            prefix.chars().skip(n - PREFIX_BUDGET_CHARS).collect()
        };
        assert_eq!(cut.chars().count(), PREFIX_BUDGET_CHARS);
    }

    #[test]
    fn inline_chat_request_parses_minimal() {
        let r: InlineChatRequest =
            serde_json::from_value(json!({ "instruction": "add doc", "code": "fn a() {}" }))
                .unwrap();
        assert_eq!(r.instruction, "add doc");
        assert!(r.path.is_none());
    }
}

/// 补全服务端超时(超时即丢弃,前端静默)
const COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(800);
/// Inline Chat 改写超时(改写全文较长)
const INLINE_CHAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
/// FIM 上下文预算:prefix/suffix 字符裁剪(约 2000/500 token 量级)
const PREFIX_BUDGET_CHARS: usize = 8000;
const SUFFIX_BUDGET_CHARS: usize = 2000;

/// 补全请求(前端预算裁剪后仍可超长,serve 再裁兜底)
#[derive(Debug, Deserialize)]
pub struct CompletionRequest {
    /// 光标前文本(已由前端按预算裁剪;超长再裁一次兜底)
    pub prefix: String,
    /// 光标后文本
    #[serde(default)]
    pub suffix: String,
    /// 语言 id(补全 prompt 上下文)
    #[serde(default)]
    pub lang: Option<String>,
}

/// Inline Chat 改写请求
#[derive(Debug, Deserialize)]
pub struct InlineChatRequest {
    /// 改写指令(用户输入)
    pub instruction: String,
    /// 待改写代码(选区;未选中=光标所在行段)
    pub code: String,
    /// 文件路径(仅供留痕摘要)
    #[serde(default)]
    pub path: Option<String>,
    /// 语言 id
    #[serde(default)]
    pub lang: Option<String>,
}

fn llm_not_configured() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "LLM 未配置" })),
    )
        .into_response()
}

/// OpenAI 兼容 chat/completions 调用(非流式;统一超时)。
/// 返回首条 choice 文本;HTTP/网络错误→Err(描述)。
async fn openai_compatible_call(
    api_base: &str,
    api_key: &str,
    model: &str,
    messages: &[serde_json::Value],
    stop: Option<Vec<String>>,
    timeout: std::time::Duration,
    max_tokens: u32,
) -> Result<String, String> {
    let url = format!("{}/chat/completions", api_base.trim_end_matches('/'));
    // 文档化(clippy):构造即用
    let client = reqwest::Client::new();
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": false,
        "max_tokens": max_tokens,
        "temperature": 0.2,
    });
    if let Some(stop) = stop {
        body["stop"] =
            serde_json::Value::Array(stop.into_iter().map(serde_json::Value::String).collect());
    }
    let resp = tokio::time::timeout(
        timeout,
        client.post(&url).bearer_auth(api_key).json(&body).send(),
    )
    .await
    .map_err(|_| "LLM 请求超时".to_string())?
    .map_err(|e| format!("LLM 请求失败: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let _ = resp.text().await.unwrap_or_default();
        // 日志脱敏:仅状态码与响应头长度,不记响应正文(可能含用户代码)
        return Err(format!("LLM HTTP {status}"));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("LLM 响应解析失败: {e}"))?;
    let text = v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    Ok(text)
}

/// `POST /api/completion` — FIM 补全(prefix/suffix → 补全文本;非流式,800ms 超时)
pub async fn completion(
    State(state): State<AgentApiState>,
    Json(req): Json<CompletionRequest>,
) -> Response {
    let config = state.llm_config().clone();
    if config.api_key.is_empty() {
        return llm_not_configured();
    }
    let prefix_len = req.prefix.chars().count();
    let prefix: String = if prefix_len > PREFIX_BUDGET_CHARS {
        req.prefix
            .chars()
            .skip(prefix_len - PREFIX_BUDGET_CHARS)
            .collect()
    } else {
        req.prefix.clone()
    };
    let suffix: String = req.suffix.chars().take(SUFFIX_BUDGET_CHARS).collect();

    // 补全形态:自然语言指令(设计偏离——PSM token 形态经 MiniMax-M2.5 真机
    // 实测返回空,对话型模型不认识 FIM token;chat 指令形态实测有效,详见
    // 补全设计档的实施期变更记录)。suffix 作为续写上下文给出,不直接拼接。
    let messages = vec![
        json!({
            "role": "system",
            "content": "你是代码补全引擎。给定光标前的代码与光标后的代码,只输出光标处应续写的代码片段:不要重复已有代码,不要解释,不要 markdown 代码围栏。通常 1-5 行。"
        }),
        json!({
            "role": "user",
            "content": format!(
                "光标前的代码:
{prefix}

光标后的代码:
{suffix}

只输出光标处应续写的代码片段。"
            )
        }),
    ];
    let text = match openai_compatible_call(
        &config.api_base,
        &config.api_key,
        &config.model,
        &messages,
        Some(vec![
            "<|fim_prefix|>".into(),
            "<|fim_suffix|>".into(),
            "<|fim_hole|>".into(),
            "\n```".into(),
        ]),
        COMPLETION_TIMEOUT,
        256,
    )
    .await
    {
        Ok(t) => t,
        Err(_) => {
            // 补全失败前端静默:返回空文本(HTTP 200,零打扰纪律)
            return Json(json!({ "text": "" })).into_response();
        }
    };
    Json(json!({ "text": text })).into_response()
}

/// `POST /api/inline-chat` — 改写代理(指令+代码 → 改后全文;45s 超时)
pub async fn inline_chat(
    State(state): State<AgentApiState>,
    Json(req): Json<InlineChatRequest>,
) -> Response {
    let config = state.llm_config().clone();
    if config.api_key.is_empty() {
        return llm_not_configured();
    }
    if req.instruction.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "指令为空" })),
        )
            .into_response();
    }
    let lang = req.lang.clone().unwrap_or_else(|| "text".into());
    let messages = vec![
        json!({
            "role": "system",
            "content": "你是代码改写助手。只返回改写后的完整代码,不加解释,保持原有缩进与风格。不要 markdown 代码围栏。"
        }),
        json!({
            "role": "user",
            "content": format!(
                "指令:{}\n\n语言:{}\n待改写代码:\n```{}\n{}\n```",
                req.instruction, lang, lang, req.code
            )
        }),
    ];
    let text = match openai_compatible_call(
        &config.api_base,
        &config.api_key,
        &config.model,
        &messages,
        None,
        INLINE_CHAT_TIMEOUT,
        2048,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response();
        }
    };
    Json(json!({ "text": text })).into_response()
}
