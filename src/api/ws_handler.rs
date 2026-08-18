// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G16:WebSocket 双向流 — 网络版 REPL
//!
//! 在 G15(本地 stdin REPL)的基础上,把同一套 `run_streaming` / `run_continuation`
//! 机制暴露到 WebSocket,支持多用户、跨进程的实时对话。
//!
//! ## 路由
//!
//! `GET /api/sessions/{id}/ws?agent_type=researcher&token=xxx`
//!
//! - `id`:session ID。`"new"` 表示首次连接(由服务端创建新 session),
//!   传具体 ID 则复用已有 session(continuation 模式)。
//! - `agent_type`:必填 query param,对应 `agents/<name>.json`,用于构造 AgentRunner。
//! - `token`:鉴权 token(当 auth 启用时必填,复用 G7 token 列表)。
//!
//! ## 消息协议(双向 JSON)
//!
//! **Client → Server**(snake_case `type`):
//! ```json
//! {"type":"message","content":"帮我查天气"}
//! {"type":"interrupt"}
//! {"type":"rewind","version":5}
//! ```
//!
//! **Server → Client**(PascalCase `type`,复用 G4 AgentEvent 序列化):
//! ```json
//! {"type":"SessionCreated","session_id":"42"}
//! {"type":"LlmDelta","text":"好的"}
//! {"type":"Done","success":true,"content":"...","steps":3,...}
//! {"type":"Error","error":"..."}
//! ```
//!
//! ## 生命周期
//!
//! 1. 客户端连接 → 服务端在升级前校验 `?token=`,失败返回 401(不升级)。
//! 2. 客户端发 `message` → 服务端构造 fresh AgentRunner(每轮新建,
//!    因 `run_streaming`/`run_continuation` 消费 `self`),
//!    首轮调 `run_streaming`(创建 session),后续调 `run_continuation`(复用 session)。
//! 3. 服务端把 AgentEvent 流逐个序列化为 JSON 文本帧推给客户端。
//! 4. 客户端可在任意时刻发 `interrupt` → 服务端 cancel 当前 runner 的 CancellationToken。
//! 5. 客户端发 `rewind` → 服务端调 evorule `rewind` API(需在无活跃轮次时)。
//! 6. 任一方关闭 WebSocket → 连接结束(session 保留在 evorule,可重连续用)。

#![forbid(unsafe_code)]

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::agent::{AgentError, AgentEvent, AgentRunner};
use crate::api::agent_api::AgentApiState;

// =============================================================================
// Query / 消息协议类型
// =============================================================================

/// WebSocket 连接的 query 参数
///
/// - `token`:鉴权 token(auth 启用时必填)
/// - `agent_type`:agent 类型(必填,对应 `agents/<name>.json`)
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    /// 鉴权 token(复用 G7 AuthConfig.validate)
    pub token: Option<String>,
    /// agent 类型,用于加载 AgentDefinition 并构造 AgentRunner
    pub agent_type: Option<String>,
}

/// Client → Server 消息(snake_case `type` tag)
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    /// 用户消息 — 触发一轮 agent 执行
    Message {
        /// 用户输入内容
        content: String,
    },
    /// 中断当前正在执行的轮次(cancel runner)
    Interrupt,
    /// 回滚到指定版本(需在无活跃轮次时调用)
    Rewind {
        /// 目标版本号
        version: u64,
    },
}

/// 服务端 → 客户端的内部事件(用于区分 AgentEvent 和轮次结束信号)
enum WsEvent {
    /// Agent 事件(或流级错误)
    Agent(Result<AgentEvent, AgentError>),
    /// 当前轮次结束(事件流耗尽)
    TurnEnd,
}

// =============================================================================
// ws_handler — HTTP 入口(鉴权 + 升级)
// =============================================================================

/// G16:WebSocket 升级 handler
///
/// 路由:`GET /api/sessions/{id}/ws?agent_type=xxx&token=yyy`
///
/// 鉴权流程(双层):
/// 1. **auth_middleware**(layer 层)检查 `?token=` query param → 失败返回 401
///    (在 WebSocketUpgrade 提取之前执行,未授权请求不会触发 WS 升级)
/// 2. **handler 内**二次校验 `?token=`(defense in depth)→ 失败返回 401
/// 3. **handler 内**校验 `?agent_type=` → 缺失返回 400
/// 4. `ws.on_upgrade(handle_ws)` — 升级 WebSocket,进入双向循环
///
/// 注意:auth_middleware 已对 `?token=` fallback 生效(参见 `auth::extract_token`),
/// 因此 WebSocket 升级前鉴权由中间件保证。handler 内的二次校验是 defense in depth。
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AgentApiState>,
    Path(session_id): Path<String>,
    Query(query): Query<WsQuery>,
) -> Response {
    // 1. 二次鉴权(defense in depth — 中间件已做第一层校验)
    if state.auth_config().enabled() {
        let token = match &query.token {
            Some(t) => t.as_str(),
            None => {
                warn!("G16: WS upgrade rejected — missing ?token= query param");
                return StatusCode::UNAUTHORIZED.into_response();
            }
        };
        if !state.auth_config().validate(token) {
            warn!("G16: WS upgrade rejected — invalid token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }

    // 2. 必须指定 agent_type(构造 AgentRunner 需要)
    let agent_type = match &query.agent_type {
        Some(t) => t.clone(),
        None => {
            warn!("G16: WS upgrade rejected — missing ?agent_type= query param");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    info!(
        session_id = %session_id,
        agent_type = %agent_type,
        "G16: WebSocket upgrading"
    );

    // 3. 升级 WebSocket,进入 handle_ws 双向循环
    ws.on_upgrade(move |socket| handle_ws(socket, state, session_id, agent_type))
}

// =============================================================================
// handle_ws — 双向消息循环
// =============================================================================

/// WebSocket 双向消息循环
///
/// 使用 `tokio::select!` 并发处理:
/// - **客户端消息**(WS receiver):`message` / `interrupt` / `rewind`
/// - **Agent 事件**(通过 mpsc channel 转发):序列化为 JSON 推给客户端
///
/// 每轮 agent 执行在独立 spawn 的 task 中运行,事件通过 channel 转发给主循环。
/// 这样 `interrupt` 可以在 agent 执行期间被实时处理(cancel 当前轮次的 token)。
async fn handle_ws(
    socket: WebSocket,
    state: AgentApiState,
    initial_session_id: String,
    agent_type: String,
) {
    // 拆分 socket 为 sender(推事件给客户端)和 receiver(读客户端消息)
    let (mut sender, mut receiver) = socket.split();

    // 当前 session_id(None = 首次连接,等 SessionCreated 事件赋值)
    // 若路径传了具体 ID(非 "new"),直接作为 continuation 的 session
    let mut current_session: Option<String> =
        if initial_session_id == "new" || initial_session_id.is_empty() {
            None
        } else {
            Some(initial_session_id)
        };

    // Agent 事件 channel:spawned task → 主循环
    // event_tx 由主循环持有(用于 spawn 时 clone),不 drop,所以 recv() 只在
    // spawned task 结束且无更多 clone 时返回 None。用 WsEvent::TurnEnd 显式标记轮次结束。
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<WsEvent>(128);

    // 当前轮次的 cancel token(interrupt 时 cancel)
    let mut current_cancel: Option<CancellationToken> = None;
    // 是否有轮次正在执行(防止并发 message)
    let mut turn_active = false;

    info!(
        agent_type = %agent_type,
        session_id = ?current_session,
        "G16: WebSocket connected"
    );

    loop {
        tokio::select! {
            // ===== 读客户端消息(始终轮询) =====
            msg = receiver.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Message { content }) => {
                                if turn_active {
                                    let _ = send_ws_json(
                                        &mut sender,
                                        serde_json::json!({
                                            "type": "Error",
                                            "error": "a turn is already active; send interrupt first"
                                        }),
                                    ).await;
                                    continue;
                                }
                                // 构造 fresh AgentRunner(run_streaming/run_continuation 消费 self)
                                let runner = match construct_runner(&state, &agent_type) {
                                    Some(r) => r,
                                    None => {
                                        let _ = send_ws_json(
                                            &mut sender,
                                            serde_json::json!({
                                                "type": "Error",
                                                "error": format!("agent '{}' not found", agent_type)
                                            }),
                                        ).await;
                                        continue;
                                    }
                                };
                                // 提取 cancel token(interrupt 用)
                                let cancel = runner.cancel_token().clone();
                                current_cancel = Some(cancel);
                                turn_active = true;

                                // 首轮(无 session)→ run_streaming(创建 session)
                                // 后续(有 session)→ run_continuation(复用 session)
                                let event_stream = if let Some(sid) = current_session.clone() {
                                    info!(session_id = %sid, "G16: continuation turn");
                                    runner.run_continuation(sid, content)
                                } else {
                                    info!("G16: first turn (new session)");
                                    runner.run_streaming(content)
                                };

                                // spawn task:把事件流转发到 channel
                                let tx = event_tx.clone();
                                tokio::spawn(async move {
                                    let mut stream = event_stream;
                                    while let Some(ev) = stream.next().await {
                                        if tx.send(WsEvent::Agent(ev)).await.is_err() {
                                            // 主循环已退出(receiver drop),停止转发
                                            return;
                                        }
                                    }
                                    // 事件流结束 → 发 TurnEnd 信号
                                    let _ = tx.send(WsEvent::TurnEnd).await;
                                });
                            }
                            Ok(ClientMessage::Interrupt) => {
                                if let Some(cancel) = &current_cancel {
                                    cancel.cancel();
                                    info!("G16: interrupt sent to current turn");
                                    let _ = send_ws_json(
                                        &mut sender,
                                        serde_json::json!({
                                            "type": "Info",
                                            "message": "interrupt sent"
                                        }),
                                    ).await;
                                } else {
                                    let _ = send_ws_json(
                                        &mut sender,
                                        serde_json::json!({
                                            "type": "Info",
                                            "message": "no active turn to interrupt"
                                        }),
                                    ).await;
                                }
                            }
                            Ok(ClientMessage::Rewind { version }) => {
                                if turn_active {
                                    let _ = send_ws_json(
                                        &mut sender,
                                        serde_json::json!({
                                            "type": "Error",
                                            "error": "cannot rewind during active turn; send interrupt first"
                                        }),
                                    ).await;
                                } else if let Some(sid) = &current_session {
                                    info!(session_id = %sid, version = version, "G16: rewind");
                                    match state.evorule_client().rewind(sid, version).await {
                                        Ok(_) => {
                                            let _ = send_ws_json(
                                                &mut sender,
                                                serde_json::json!({
                                                    "type": "Info",
                                                    "message": format!("rewound to version {}", version)
                                                }),
                                            ).await;
                                        }
                                        Err(e) => {
                                            warn!(error = %e, "G16: rewind failed");
                                            let _ = send_ws_json(
                                                &mut sender,
                                                serde_json::json!({
                                                    "type": "Error",
                                                    "error": format!("rewind failed: {}", e)
                                                }),
                                            ).await;
                                        }
                                    }
                                } else {
                                    let _ = send_ws_json(
                                        &mut sender,
                                        serde_json::json!({
                                            "type": "Error",
                                            "error": "no session to rewind"
                                        }),
                                    ).await;
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, raw = %text, "G16: invalid client message");
                                let _ = send_ws_json(
                                    &mut sender,
                                    serde_json::json!({
                                        "type": "Error",
                                        "error": format!("invalid message: {}", e)
                                    }),
                                ).await;
                            }
                        }
                    }
                    Some(Ok(WsMessage::Binary(_))) => {
                        warn!("G16: received binary WS message, ignoring");
                    }
                    Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => {
                        // axum 自动响应 Ping,无需手动处理
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        info!("G16: client sent Close frame");
                        break;
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, "G16: WS receive error");
                        break;
                    }
                    None => {
                        info!("G16: WS receiver stream ended");
                        break;
                    }
                }
            }
            // ===== 转发 Agent 事件给客户端 =====
            event = event_rx.recv() => {
                match event {
                    Some(WsEvent::Agent(result)) => {
                        // 跟踪 session_id(首轮 run_streaming 会产出 SessionCreated)
                        if let Ok(AgentEvent::SessionCreated { session_id: sid }) = &result {
                            current_session = Some(sid.clone());
                            info!(session_id = %sid, "G16: session created");
                        }
                        // 序列化 + 推给客户端
                        let json = agent_event_to_json(result);
                        if send_ws_json(&mut sender, json).await.is_err() {
                            warn!("G16: failed to send WS frame, client may have disconnected");
                            break;
                        }
                    }
                    Some(WsEvent::TurnEnd) => {
                        // 当前轮次的事件流耗尽 → 清理状态
                        turn_active = false;
                        current_cancel = None;
                        info!("G16: turn ended");
                    }
                    None => {
                        // event_tx 全部 drop(不应发生 — 主循环持有 event_tx)
                        // 视为连接异常,退出
                        warn!("G16: event channel closed unexpectedly");
                        break;
                    }
                }
            }
        }
    }

    // 清理:cancel 任何仍在执行的轮次
    if let Some(cancel) = &current_cancel {
        cancel.cancel();
    }
    let _ = sender.close().await;
    info!("G16: WebSocket disconnected");
}

// =============================================================================
// 辅助函数
// =============================================================================

/// 构造一个 fresh AgentRunner(每轮新建)
///
/// `run_streaming` / `run_continuation` 消费 `self`,所以每轮需要重新构造。
/// 构造成本低(AgentRunner::new 只存 config + client handle)。
///
/// 返回 `None` = agent_type 加载失败(404 等价)。
fn construct_runner(state: &AgentApiState, agent_type: &str) -> Option<AgentRunner> {
    let def = state.definitions().load(agent_type).ok()?;
    let config = def.to_agent_config();
    let runner = AgentRunner::new(config, state.evorule_client().clone())
        // G8:注入 HttpApproval — candidate 工具返回 needs_approval 时,
        // runner 通过 oneshot channel 等 POST /approve(60s 超时自动拒绝)
        .with_approval_callback(Arc::new(crate::agent::approval::HttpApproval::new(
            state.pending_approvals().clone(),
        )))
        // G17:注入 metrics — runner 在 session/step/LLM/工具关键路径插桩
        .with_metrics(state.metrics().clone());
    Some(runner)
}

/// 把 serde_json::Value 序列化为 WS Text 帧并发送
///
/// 返回 `Err` = sender 已关闭(客户端断开),调用方应退出循环。
async fn send_ws_json(
    sender: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
    json: serde_json::Value,
) -> Result<(), axum::Error> {
    sender.send(WsMessage::Text(json.to_string().into())).await
}

/// 把 AgentEvent(或流级错误)转成 JSON Value
///
/// 使用 PascalCase `type` tag,与 G16 spec §16.3.2 一致:
/// `{"type":"SessionCreated","session_id":"42"}`
///
/// 注意:AgentEvent 和 AgentError 未 derive Serialize,需手动映射(同 `agent_event_to_sse`)。
fn agent_event_to_json(event: Result<AgentEvent, AgentError>) -> serde_json::Value {
    match event {
        Ok(AgentEvent::SessionCreated { session_id }) => serde_json::json!({
            "type": "SessionCreated",
            "session_id": session_id,
        }),
        Ok(AgentEvent::Step { step }) => serde_json::json!({
            "type": "Step",
            "step": step,
        }),
        Ok(AgentEvent::LlmDelta { text }) => serde_json::json!({
            "type": "LlmDelta",
            "text": text,
        }),
        Ok(AgentEvent::ToolCall { name, args }) => serde_json::json!({
            "type": "ToolCall",
            "name": name,
            "args": args,
        }),
        Ok(AgentEvent::ToolResult { name, result }) => serde_json::json!({
            "type": "ToolResult",
            "name": name,
            "result": result,
        }),
        Ok(AgentEvent::LlmDone {
            content,
            finish_reason,
        }) => serde_json::json!({
            "type": "LlmDone",
            "content": content,
            "finish_reason": finish_reason,
        }),
        Ok(AgentEvent::Done(result)) => {
            // AgentResult derive Serialize,但需注入 type tag
            let mut v = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "type".to_string(),
                    serde_json::Value::String("Done".to_string()),
                );
            }
            v
        }
        Ok(AgentEvent::Error(err)) => serde_json::json!({
            "type": "Error",
            "error": err.to_string(),
        }),
        Ok(AgentEvent::Info(msg)) => serde_json::json!({
            "type": "Info",
            "message": msg,
        }),
        Ok(AgentEvent::ApprovalRequired {
            tool_name,
            command,
            risk,
            alternative,
        }) => serde_json::json!({
            "type": "ApprovalRequired",
            "tool_name": tool_name,
            "command": command,
            "risk": risk,
            "alternative": alternative,
        }),
        Ok(AgentEvent::ApprovalResult {
            tool_name,
            approved,
        }) => serde_json::json!({
            "type": "ApprovalResult",
            "tool_name": tool_name,
            "approved": approved,
        }),
        Err(err) => serde_json::json!({
            "type": "Error",
            "error": err.to_string(),
        }),
    }
}

// =============================================================================
// 单元测试
// =============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // ===== ClientMessage 反序列化测试 =====

    #[test]
    fn test_client_message_deserialize() {
        let json = r#"{"type":"message","content":"hello"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            ClientMessage::Message { content } => assert_eq!(content, "hello"),
            _ => panic!("expected Message variant"),
        }
    }

    #[test]
    fn test_client_interrupt_deserialize() {
        let json = r#"{"type":"interrupt"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ClientMessage::Interrupt));
    }

    #[test]
    fn test_client_rewind_deserialize() {
        let json = r#"{"type":"rewind","version":5}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            ClientMessage::Rewind { version } => assert_eq!(version, 5),
            _ => panic!("expected Rewind variant"),
        }
    }

    #[test]
    fn test_client_message_invalid_type() {
        let json = r#"{"type":"unknown","content":"x"}"#;
        let result: Result<ClientMessage, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_client_message_missing_content() {
        let json = r#"{"type":"message"}"#;
        let result: Result<ClientMessage, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // ===== WsQuery 反序列化测试 =====

    #[test]
    fn test_ws_query_with_token_and_agent_type() {
        let query: WsQuery =
            serde_json::from_str(r#"{"token":"secret","agent_type":"researcher"}"#).unwrap();
        assert_eq!(query.token.as_deref(), Some("secret"));
        assert_eq!(query.agent_type.as_deref(), Some("researcher"));
    }

    #[test]
    fn test_ws_query_empty() {
        let query: WsQuery = serde_json::from_str(r#"{}"#).unwrap();
        assert!(query.token.is_none());
        assert!(query.agent_type.is_none());
    }

    // ===== agent_event_to_json 测试 =====

    #[test]
    fn test_event_to_json_session_created() {
        let v = agent_event_to_json(Ok(AgentEvent::SessionCreated {
            session_id: "s-42".to_string(),
        }));
        assert_eq!(v["type"], "SessionCreated");
        assert_eq!(v["session_id"], "s-42");
    }

    #[test]
    fn test_event_to_json_llm_delta() {
        let v = agent_event_to_json(Ok(AgentEvent::LlmDelta {
            text: "hello".to_string(),
        }));
        assert_eq!(v["type"], "LlmDelta");
        assert_eq!(v["text"], "hello");
    }

    #[test]
    fn test_event_to_json_step() {
        let v = agent_event_to_json(Ok(AgentEvent::Step { step: 7 }));
        assert_eq!(v["type"], "Step");
        assert_eq!(v["step"], 7);
    }

    #[test]
    fn test_event_to_json_tool_call_and_result() {
        let call = agent_event_to_json(Ok(AgentEvent::ToolCall {
            name: "search".to_string(),
            args: serde_json::json!({"q": "rust"}),
        }));
        assert_eq!(call["type"], "ToolCall");
        assert_eq!(call["name"], "search");
        assert_eq!(call["args"]["q"], "rust");

        let res = agent_event_to_json(Ok(AgentEvent::ToolResult {
            name: "search".to_string(),
            result: serde_json::json!({"hits": 3}),
        }));
        assert_eq!(res["type"], "ToolResult");
        assert_eq!(res["result"]["hits"], 3);
    }

    #[test]
    fn test_event_to_json_llm_done() {
        let v = agent_event_to_json(Ok(AgentEvent::LlmDone {
            content: "done".to_string(),
            finish_reason: Some("stop".to_string()),
        }));
        assert_eq!(v["type"], "LlmDone");
        assert_eq!(v["content"], "done");
        assert_eq!(v["finish_reason"], "stop");
    }

    #[test]
    fn test_event_to_json_done() {
        use crate::agent::AgentResult;
        let result = AgentResult::success("final".to_string(), 3, 100, vec!["search".to_string()]);
        let v = agent_event_to_json(Ok(AgentEvent::Done(result)));
        assert_eq!(v["type"], "Done");
        assert_eq!(v["success"], true);
        assert_eq!(v["content"], "final");
        assert_eq!(v["steps"], 3);
    }

    #[test]
    fn test_event_to_json_error_and_info() {
        let err_v = agent_event_to_json(Ok(AgentEvent::Error(AgentError::LlmError(
            "boom".to_string(),
        ))));
        assert_eq!(err_v["type"], "Error");
        assert!(err_v["error"].as_str().unwrap().contains("boom"));

        let info_v = agent_event_to_json(Ok(AgentEvent::Info("rewound".to_string())));
        assert_eq!(info_v["type"], "Info");
        assert_eq!(info_v["message"], "rewound");
    }

    #[test]
    fn test_event_to_json_approval_required() {
        let v = agent_event_to_json(Ok(AgentEvent::ApprovalRequired {
            tool_name: "shell_exec".to_string(),
            command: "rm -rf /tmp".to_string(),
            risk: "high".to_string(),
            alternative: "use trash".to_string(),
        }));
        assert_eq!(v["type"], "ApprovalRequired");
        assert_eq!(v["tool_name"], "shell_exec");
    }

    #[test]
    fn test_event_to_json_stream_level_error() {
        let v = agent_event_to_json(Err(AgentError::Internal("stream broke".to_string())));
        assert_eq!(v["type"], "Error");
        assert!(v["error"].as_str().unwrap().contains("stream broke"));
    }

    // ===== 鉴权 + 路由集成测试 =====

    use crate::api::agent_api::{router, router_with_auth};
    use crate::api::auth::AuthConfig;
    use crate::api::evorule_client::EvoruleApiClient;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn make_test_state() -> AgentApiState {
        AgentApiState::new(
            crate::agent::AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new("http://localhost:8080"),
        )
    }

    #[tokio::test]
    async fn test_ws_route_registered() {
        // 验证 /api/sessions/{id}/ws 路由已注册:
        // 发送非 WebSocket 请求 → WebSocketUpgrade 提取器拒绝(426),
        // 而非 404(路由未注册)。426 证明路由匹配成功。
        let state = make_test_state();
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/new/ws?agent_type=test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // 非 404 = 路由已注册(426 = WS 提取器拒绝非升级请求)
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "route should be registered"
        );
    }

    #[tokio::test]
    async fn test_ws_auth_rejects_missing_token() {
        // auth 启用 + 无 token → 401(auth_middleware 在 WS 提取前拦截)
        let state = make_test_state();
        let auth = AuthConfig::new(vec!["secret".to_string()], true);
        let app = router_with_auth(state, auth);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/new/ws?agent_type=researcher")
                    .header("Upgrade", "websocket")
                    .header("Connection", "upgrade")
                    .header("Sec-WebSocket-Version", "13")
                    .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_ws_auth_rejects_wrong_token() {
        // auth 启用 + 错误 token → 401
        let state = make_test_state();
        let auth = AuthConfig::new(vec!["secret".to_string()], true);
        let app = router_with_auth(state, auth);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/new/ws?agent_type=researcher&token=wrong")
                    .header("Upgrade", "websocket")
                    .header("Connection", "upgrade")
                    .header("Sec-WebSocket-Version", "13")
                    .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ===== WebSocket 集成测试(真实 TCP server + tokio-tungstenite) =====

    use tokio_tungstenite::tungstenite;

    /// 启动一个测试 axum server,返回 (端口, join_handle)
    async fn start_test_server(
        state: AgentApiState,
        auth: Option<AuthConfig>,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let app = match auth {
            Some(a) => router_with_auth(state, a),
            None => router(state),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn test_ws_tcp_missing_agent_type_rejected() {
        // 真实 TCP server:连接时缺少 agent_type → 400(WebSocket 升级前)
        let state = make_test_state();
        let (addr, _handle) = start_test_server(state, None).await;

        let url = format!("ws://{}/api/sessions/new/ws", addr);
        let result = tokio_tungstenite::connect_async(url).await;

        // 连接应失败(服务器返回 400,不升级 WebSocket)
        assert!(result.is_err(), "should not upgrade without agent_type");
        match result {
            Err(tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            }
            Err(e) => panic!("expected Http(400) error, got: {:?}", e),
            Ok(_) => panic!("expected error, got WS upgrade"),
        }
    }

    #[tokio::test]
    async fn test_ws_tcp_invalid_token_rejected() {
        // 真实 TCP server:auth 启用 + 错误 token → 401
        let state = make_test_state();
        let auth = AuthConfig::new(vec!["secret".to_string()], true);
        let (addr, _handle) = start_test_server(state, Some(auth)).await;

        let url = format!(
            "ws://{}/api/sessions/new/ws?agent_type=test&token=wrong",
            addr
        );
        let result = tokio_tungstenite::connect_async(url).await;

        assert!(result.is_err(), "should not upgrade with wrong token");
        match result {
            Err(tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            }
            Err(e) => panic!("expected Http(401) error, got: {:?}", e),
            Ok(_) => panic!("expected error, got WS upgrade"),
        }
    }

    #[tokio::test]
    async fn test_ws_tcp_valid_params_upgrade_succeeds() {
        // 真实 TCP server:auth 禁用 + 有 agent_type → WebSocket 升级成功(101)
        // 注意:升级成功后,发送 message 会触发 runner 调用 evorule API(localhost:8080),
        // 由于没有 evorule server,runner 会返回 Error 事件。
        // 这里只验证升级成功 + 能收到某种 JSON 帧(Error 帧)。
        let state = make_test_state();
        let (addr, _handle) = start_test_server(state, None).await;

        let url = format!("ws://{}/api/sessions/new/ws?agent_type=test", addr);
        let (mut ws_stream, resp) = tokio_tungstenite::connect_async(url)
            .await
            .expect("WS upgrade should succeed");

        // 101 Switching Protocols
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);

        // 发送一条 message,触发 runner 执行(会因 evorule 不可用而失败)
        let msg = serde_json::json!({"type":"message","content":"hello"});
        ws_stream
            .send(tokio_tungstenite::tungstenite::Message::Text(
                msg.to_string(),
            ))
            .await
            .unwrap();

        // 应收到至少一个 JSON 帧(Error 或 Info)
        let received =
            tokio::time::timeout(std::time::Duration::from_secs(10), ws_stream.next()).await;

        assert!(received.is_ok(), "should receive a frame within 10s");
        if let Ok(Some(Ok(frame))) = received {
            if let tungstenite::Message::Text(text) = frame {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                // 应包含 type 字段(Error / Info / SessionCreated 等)
                assert!(
                    v["type"].is_string(),
                    "frame should have 'type' field, got: {}",
                    text
                );
            } else {
                panic!("expected Text frame, got: {:?}", frame);
            }
        }

        // 关闭连接
        let _ = ws_stream.close(None).await;
    }

    #[tokio::test]
    async fn test_ws_tcp_interrupt_without_active_turn() {
        // 连接成功后,在无活跃轮次时发 interrupt → 应收到 Info("no active turn")
        let state = make_test_state();
        let (addr, _handle) = start_test_server(state, None).await;

        let url = format!("ws://{}/api/sessions/new/ws?agent_type=test", addr);
        let (mut ws_stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("WS upgrade should succeed");

        // 发送 interrupt(无活跃轮次)
        let msg = serde_json::json!({"type":"interrupt"});
        ws_stream
            .send(tokio_tungstenite::tungstenite::Message::Text(
                msg.to_string(),
            ))
            .await
            .unwrap();

        // 应收到 Info("no active turn to interrupt")
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), ws_stream.next())
            .await
            .expect("should receive a frame within 5s");

        let frame = received.unwrap().unwrap();
        if let tungstenite::Message::Text(text) = frame {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["type"], "Info");
            assert!(
                v["message"].as_str().unwrap().contains("no active turn"),
                "expected 'no active turn', got: {}",
                v["message"]
            );
        } else {
            panic!("expected Text frame, got: {:?}", frame);
        }

        let _ = ws_stream.close(None).await;
    }
}
