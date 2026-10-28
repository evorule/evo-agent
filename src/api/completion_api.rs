// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! AI 补全/Inline Chat 端点(FIM 代理+改写代理)。
//!
//! 出站路径与对话 LLM **同链归边**:统一 POST 扩展执行体(`/llm/execute`),
//! 由执行体完成模型准入白名单执法(网络入口,先于出站)、多 provider 路由与
//! 凭据持有——serve 侧零凭据,补全/改写不再自建直连 LLM 的旁路。请求形态仍
//! 独立:补全是**非流式短超时**调用,Inline Chat 是**改写全文**调用;模型取
//! 共享配置当前生效模型(随会话模型热切换联动)。
//!
//! 治理边界:两端点挂 token 中间件;服务端不落盘 prompt 正文(tracing 脱敏);
//! 外发目标=配置的扩展执行体(无新第三方);.evoignore 排除在前端 prompt
//! 组装层执行(serve 端不重复判定)。执行体不可达时的失败语义:补全=空文本
//! 零打扰(HTTP 200),改写=502 显式上抛——与对话链路同域可见。

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

    // ---------- 归边执行体出站路径(补全/改写与对话同链) ----------

    type Capture = std::sync::Arc<std::sync::Mutex<Option<(String, serde_json::Value)>>>;

    /// 起 mock 执行体(POST /llm/execute):捕获 Authorization+请求体,回四键应答
    async fn spawn_mock_executor(reply: serde_json::Value, captured: Capture) -> String {
        let app = axum::Router::new().route(
            "/llm/execute",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| {
                    let captured = captured.clone();
                    let reply = reply.clone();
                    async move {
                        let auth = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        *captured.lock().unwrap() = Some((auth, body));
                        Json(reply)
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/llm/execute")
    }

    /// 起可指定状态码与错误体的 mock 执行体(拒绝路径)
    async fn spawn_mock_executor_with_status(status: StatusCode, error: &str) -> String {
        let body = json!({ "error": error });
        let app = axum::Router::new().route(
            "/llm/execute",
            axum::routing::post(move || {
                let body = body.clone();
                async move { (status, Json(body)).into_response() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/llm/execute")
    }

    fn state_with_executor(executor_url: &str, token: Option<&str>) -> AgentApiState {
        let cfg = crate::config::LlmConfig {
            executor_url: executor_url.to_string(),
            executor_token: token.map(str::to_string),
            ..crate::config::LlmConfig::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        AgentApiState::new_with_metrics(
            crate::agent::AgentDefinitionManager::with_default_dir(),
            crate::api::evorule_client::EvoruleApiClient::new("http://localhost:8080"),
            std::sync::Arc::new(crate::api::metrics::Metrics::new().unwrap()),
            tmp.path().to_path_buf(),
            std::sync::Arc::new(crate::api::workspace_client::WorkspaceApiClient::new(
                "http://localhost:8080",
            )),
            std::sync::Arc::new(std::sync::RwLock::new(
                crate::io_handlers::tool_handler::ToolHandler::new(),
            )),
        )
        .with_llm_config(cfg)
    }

    #[tokio::test]
    async fn completion_routes_through_executor_without_local_key() {
        // 归边语义:api_key 恒空(SG-1 凭据归边)下补全照常可用——走执行体
        let captured: Capture = std::sync::Arc::new(std::sync::Mutex::new(None));
        let url = spawn_mock_executor(
            json!({ "content": "let x = 1;", "tool_calls": null, "finish_reason": "stop" }),
            captured.clone(),
        )
        .await;
        let state = state_with_executor(&url, Some("tok-1"));
        let resp = completion(
            State(state),
            Json(CompletionRequest {
                prefix: "fn ma".into(),
                suffix: String::new(),
                lang: Some("rust".into()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["text"], "let x = 1;");
        let (auth, body) = captured.lock().unwrap().clone().unwrap();
        assert_eq!(auth, "Bearer tok-1", "执行体调用方令牌随配置携带");
        assert_eq!(body["model"], "MiniMax-M2.5", "模型=共享配置当前生效模型");
        assert_eq!(body["stream"], serde_json::json!(false));
        assert_eq!(body["max_tokens"], serde_json::json!(256));
        assert!(body["stop"].is_array(), "FIM 停止序列随请求透传");
        assert!(body["messages"].as_array().unwrap().len() == 2);
    }

    #[tokio::test]
    async fn completion_silent_empty_when_executor_unreachable() {
        // 执行体不可达 → 空文本零打扰(HTTP 200,失败原因落日志不回传)
        let state = state_with_executor("http://127.0.0.1:1/llm/execute", None);
        let resp = completion(
            State(state),
            Json(CompletionRequest {
                prefix: "fn ma".into(),
                suffix: String::new(),
                lang: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["text"], "");
    }

    #[tokio::test]
    async fn inline_chat_routes_through_executor() {
        let captured: Capture = std::sync::Arc::new(std::sync::Mutex::new(None));
        let url = spawn_mock_executor(
            json!({ "content": "fn main() {}", "tool_calls": null, "finish_reason": "stop" }),
            captured.clone(),
        )
        .await;
        let state = state_with_executor(&url, None);
        let resp = inline_chat(
            State(state),
            Json(InlineChatRequest {
                instruction: "加注释".into(),
                code: "fn main(){}".into(),
                path: Some("a.rs".into()),
                lang: Some("rust".into()),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["text"], "fn main() {}");
        let (_, body) = captured.lock().unwrap().clone().unwrap();
        assert_eq!(body["max_tokens"], serde_json::json!(2048));
        assert!(body.get("stop").is_none(), "改写无停止序列");
    }

    #[tokio::test]
    async fn inline_chat_surfaces_executor_rejection() {
        // 白名单/路由拒绝来自执行体 502+error 体重:显式上抛给前端(不再旁路)
        let url =
            spawn_mock_executor_with_status(StatusCode::BAD_GATEWAY, "模型 m 不在准入白名单").await;
        let state = state_with_executor(&url, None);
        let resp = inline_chat(
            State(state),
            Json(InlineChatRequest {
                instruction: "改写".into(),
                code: "code".into(),
                path: None,
                lang: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let v = body_json(resp).await;
        assert!(v["error"].as_str().unwrap().contains("白名单"));
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}

/// 补全服务端超时(超时即丢弃,前端静默)。设计原值 800ms;实施期上调 10s——
/// 推理型模型(MiniMax M2.5 真机实测 2.8-3.4s/M3 1.6s,偶发抖动破 3s)
/// 恒超 800ms/3s 即恒空文本;去重缓存+前端 debounce 仍兜请求频次,
/// 迟到的建议由前端过期丢弃,不打扰输入
const COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(10_000);
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
        Json(json!({ "error": "LLM 未配置(扩展执行体地址为空)" })),
    )
        .into_response()
}

/// 扩展执行体调用(非流式;补全/改写与对话 LLM 同链归边)。
///
/// POST `executor_url`(全 URL 形态),body 透传(model/messages/stream/max_tokens/
/// temperature[/stop]);执行体完成白名单执法与多 provider 路由后回四键同构
/// `{content, tool_calls, finish_reason, token_usage}`,此处只取 content。
/// HTTP/网络错误→Err(描述,不含凭据);执行体拒绝(白名单/路由/provider 失败)
/// 以非 2xx+error 体重上抛,归因消息优先透传(错误体为执行体自诊断,不含
/// 用户代码;无 error 字段时只报状态码)。
async fn executor_call(
    executor_url: &str,
    executor_token: Option<&str>,
    model: &str,
    messages: &[serde_json::Value],
    stop: Option<Vec<String>>,
    timeout: std::time::Duration,
    max_tokens: u32,
) -> Result<String, String> {
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
    // 文档化(clippy):构造即用;调用方各自短超时,不复用全局长连接客户端
    let client = reqwest::Client::new();
    let mut req = client.post(executor_url).json(&body);
    if let Some(token) = executor_token.map(str::trim).filter(|t| !t.is_empty()) {
        req = req.bearer_auth(token);
    }
    let resp = tokio::time::timeout(timeout, req.send())
        .await
        .map_err(|_| "LLM 请求超时".to_string())?
        .map_err(|e| format!("执行体请求失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        // 执行体错误体=自诊断消息(白名单/路由/provider 归因,不含用户代码)
        // ——优先透传归因;无 error 字段则只按状态码归因(不记原始正文)
        let reason = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(reason);
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("执行体响应解析失败: {e}"))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(format!("执行体拒绝: {err}"));
    }
    Ok(v["content"].as_str().unwrap_or_default().to_string())
}

/// `POST /api/completion` — FIM 补全(prefix/suffix → 补全文本;非流式,10s 超时)
pub async fn completion(
    State(state): State<AgentApiState>,
    Json(req): Json<CompletionRequest>,
) -> Response {
    let config = state.llm_config().clone();
    if config.executor_url.trim().is_empty() {
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
    let text = match executor_call(
        &config.executor_url,
        config.executor_token.as_deref(),
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
        Err(e) => {
            // 补全失败前端静默:返回空文本(HTTP 200,零打扰纪律);
            // 失败原因落 tracing 日志面(不回传前端)
            tracing::warn!(error = %e, "completion: llm call failed, returning empty");
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
    if config.executor_url.trim().is_empty() {
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
    let text = match executor_call(
        &config.executor_url,
        config.executor_token.as_deref(),
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
