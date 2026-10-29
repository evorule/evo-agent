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
//! `GET /api/sessions/{id}/ws?agent_type=researcher&ticket=xxx`
//!
//! - `id`:session ID。`"new"` 表示首次连接(由服务端创建新 session),
//!   传具体 ID 则复用已有 session(continuation 模式)。
//! - `agent_type`:必填 query param,对应 `agents/<name>.json`,用于构造 AgentRunner。
//! - `ticket`:一次性短票(WS 握手不能带 Authorization 头,客户端先以 Bearer
//!   换票;auth 启用时经 `?ticket=` 通行,`?token=` 长期凭据恒 401)。
//!
//! ## 消息协议(双向 JSON)
//!
//! **Client → Server**(snake_case `type`):
//! ```json
//! {"type":"message","content":"帮我查天气"}
//! {"type":"interrupt"}
//! {"type":"rewind","version":5}
//! {"type":"resume"}
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
//! 1. 客户端连接 → 服务端在升级前由 auth 中间件校验(Authorization Bearer 或
//!    一次性 ticket),失败返回 401(不升级)。
//! 2. 客户端发 `message` → 服务端构造 fresh AgentRunner(每轮新建,
//!    因 `run_streaming`/`run_continuation` 消费 `self`),
//!    首轮调 `run_streaming`(创建 session),后续调 `run_continuation`(复用 session)。
//! 3. 服务端把 AgentEvent 流逐个序列化为 JSON 文本帧推给客户端。
//! 4. 客户端可在任意时刻发 `interrupt` → 服务端 cancel 当前 runner 的 CancellationToken。
//! 5. 客户端发 `rewind` → 服务端调 evorule `rewind` API(需在无活跃轮次时)。
//! 6. 客户端发 `resume` → 服务端对当前会话走崩溃恢复管线(`resume_crashed`,
//!    无新输入:加载历史+悬挂工具处置+LLM 自然续完中断轮;挂起审批不恢复)。
//! 7. 任一方关闭 WebSocket → 连接结束(session 保留在 evorule,可重连续用)。

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
use tracing::{debug, error, info, warn};

use crate::agent::translator::UserImageRef;
use crate::agent::{AgentError, AgentEvent, AgentRunner, MemoryManager};
use crate::api::agent_api::AgentApiState;

// =============================================================================
// Query / 消息协议类型
// =============================================================================

/// WebSocket 连接的 query 参数
///
/// - `agent_type`:agent 类型(必填,对应 `agents/<name>.json`)
/// - `workspace`:会话级工作区绝对路径(可选,C 批「会话级 workspace」)——
///   在场时本轮连接的所有轮次以该路径为能力边界根(读写同域,与 REST 面
///   `workspace` 请求字段同语义;声明优先,`--read-root` 不参与)。连接级
///   意志:改工作区须重连生效(新建会话/换线连接即携带)。
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    /// agent 类型,用于加载 AgentDefinition 并构造 AgentRunner
    pub agent_type: Option<String>,
    /// 会话级工作区(可选,绝对路径;缺省=serve workdir 行为零变化)
    pub workspace: Option<String>,
}

/// Client → Server 消息(snake_case `type` tag)
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    /// 用户消息 — 触发一轮 agent 执行
    Message {
        /// 用户输入内容
        content: String,
        /// S4-b:附件图片引用(上传回执 `{path,mime_type}` 原样回传;缺省无图
        /// 零行为变化,服务端沙箱校验+魔数复检,越界 fail-soft 丢弃)
        #[serde(default)]
        images: Vec<UserImageRef>,
        /// 交互批 计划模式:Some("plan")=本回合工具面收窄为只读族+
        /// plan_submit(LLM 提交计划即收束回合);缺省/其他值=零变化
        /// (对话模式的软引导仍走 wire 提示块,不进此字段)
        #[serde(default)]
        mode: Option<String>,
    },
    /// 中断当前正在执行的轮次(cancel runner)
    Interrupt,
    /// 回滚到指定版本(需在无活跃轮次时调用)
    Rewind {
        /// 目标版本号
        version: u64,
    },
    /// 恢复崩溃会话(C3 前端恢复接线)——无新输入,服务端加载历史+悬挂工具
    /// 处置后 LLM 自然续完中断轮(与 react 面 resume_session_id 同一管线;
    /// 挂起的人工审批不恢复,由前端提示条明示)
    Resume,
    /// B22 s4_publish 治理事件通知(前端 markFinalized 首次定稿产物时发;
    /// serve 侧入 gov 账本,摘要级,路径即指针——不加新端点走既有 WS 面)
    S4Publish {
        /// 定稿产物路径
        path: String,
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
/// 路由:`GET /api/sessions/{id}/ws?agent_type=xxx&ticket=yyy`
///
/// 鉴权流程(单点执法):
/// 1. **auth_middleware**(layer 层)校验 Authorization Bearer 或 `?ticket=`
///    一次性短票 → 失败返回 401(在 WebSocketUpgrade 提取之前执行,未授权
///    请求不会触发 WS 升级)。ticket 消费式校验(TTL 30s,单次有效),升级
///    前语义不变
/// 2. **handler 内**校验 `?agent_type=` → 缺失返回 400
/// 3. `ws.on_upgrade(handle_ws)` — 升级 WebSocket,进入双向循环
///
/// 注意:`?token=` 长期凭据不再被接受(恒 401,回归锁见 auth 测试);WS 握手
/// 不能带自定义 Authorization 头,客户端以 Bearer 换一次性 ticket 后经
/// `?ticket=` 通行。
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AgentApiState>,
    Path(session_id): Path<String>,
    Query(query): Query<WsQuery>,
) -> Response {
    // 1. 必须指定 agent_type(构造 AgentRunner 需要)
    let agent_type = match &query.agent_type {
        Some(t) => t.clone(),
        None => {
            warn!("G16: WS upgrade rejected — missing ?agent_type= query param");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // 2. 会话级工作区(可选):fail-visible——非绝对路径在升级期即拒(400),
    // 不让坏值漏进轮次内才报错。校验通过后交给 construct_runner 逐轮应用。
    if let Some(ws) = &query.workspace {
        if !std::path::Path::new(ws).is_absolute() {
            warn!(workspace = %ws, "G16: WS upgrade rejected — workspace must be an absolute path");
            return StatusCode::BAD_REQUEST.into_response();
        }
    }

    info!(
        session_id = %session_id,
        agent_type = %agent_type,
        "G16: WebSocket upgrading"
    );

    // 3. 升级 WebSocket,进入 handle_ws 双向循环
    ws.on_upgrade(move |socket| handle_ws(socket, state, session_id, agent_type, query.workspace))
}

// =============================================================================
// handle_ws — 双向消息循环
// =============================================================================

/// WebSocket 双向消息循环
///
/// 使用 `tokio::select!` 并发处理:
/// - **客户端消息**(WS receiver):`message` / `interrupt` / `rewind` / `resume`
/// - **Agent 事件**(通过 mpsc channel 转发):序列化为 JSON 推给客户端
///
/// 每轮 agent 执行在独立 spawn 的 task 中运行,事件通过 channel 转发给主循环。
/// 这样 `interrupt` 可以在 agent 执行期间被实时处理(cancel 当前轮次的 token)。
async fn handle_ws(
    socket: WebSocket,
    state: AgentApiState,
    initial_session_id: String,
    agent_type: String,
    workspace: Option<String>,
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

    // 文件系统事件广播(连接级订阅;连接建立后发生的变更可见,不补发历史)
    let mut fs_rx = state.fs_events().subscribe();

    // 当前轮次的 cancel token(interrupt 时 cancel)
    let mut current_cancel: Option<CancellationToken> = None;
    // 是否有轮次正在执行(防止并发 message)
    let mut turn_active = false;
    // 会话索引:最近一条用户消息(标题来源;SessionCreated 时随索引落一行)
    let mut pending_title: Option<String> = None;
    // 会话索引:会话是否真实建立过(SessionCreated 已见)
    // — 防幽灵条目:localStorage/URL 残留不存在的 session id 时,
    //   run_continuation 会失败,失败 turn 不得写入索引
    let mut session_established = false;
    // 会话索引:当前轮是否成功(Done success / 流级错误);每轮开始时重置
    let mut turn_succeeded = false;

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
                                   Ok(ClientMessage::Message { content, images, mode }) => {
                                       if turn_active {
                                           if let Err(e) = send_ws_json(
                                               &mut sender,
                                               serde_json::json!({
                                                   "type": "Error",
                                                   "error": "a turn is already active; send interrupt first"
                                               }),
                                           ).await {
                                               debug!(error = %e, "ws send Error(turn_active) failed; client likely disconnected");
                                           }
                                           continue;
                                       }
                                       // 构造 fresh AgentRunner(run_streaming/run_continuation 消费 self)
                                       // 交互批 计划模式:mode=plan 时本回合工具闸收窄(回合级,
                                       // 随 fresh runner 天然单回合)
                                       let plan_mode = mode.as_deref() == Some("plan");
                                       let runner = match construct_runner(&state, &agent_type, workspace.as_deref()).await {
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
                                       }
                                       .with_plan_mode(plan_mode);
                                       // 提取 cancel token(interrupt 用)
                                       let cancel = runner.cancel_token().clone();
                                       current_cancel = Some(cancel);
                                       turn_active = true;
                                       // 会话索引:记录标题来源(首轮用户消息)
                                       pending_title = Some(content.clone());
                                       // 会话索引:新轮次重置成功标志
                                       turn_succeeded = false;

                                       // 首轮(无 session)→ run_streaming(创建 session)
                                       // 后续(有 session)→ run_continuation(复用 session)
                                       // S4-b:images 旁路透传(空切片零行为变化)
                                       let event_stream = if let Some(sid) = current_session.clone() {
                                           info!(session_id = %sid, "G16: continuation turn");
                                           runner.run_continuation(sid, content, images)
                                       } else {
                                           info!("G16: first turn (new session)");
                                           runner.run_streaming(content, images)
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
                                           if let Err(e) = tx.send(WsEvent::TurnEnd).await {
                                               debug!(error = %e, "TurnEnd send failed; main ws loop already exited");
                                           }
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
                                               Ok(reply) => {
                                                   // 透传服务端回执的实际回退版本(缺省回落到请求版本)
                                                   let actual = reply["actual_version"]
                                                       .as_u64()
                                                       .unwrap_or(version);
                                                   let _ = send_ws_json(
                                                       &mut sender,
                                                       serde_json::json!({
                                                           "type": "Info",
                                                           "message": format!("rewound to version {}", actual),
                                                           "requested_version": version,
                                                           "actual_version": actual,
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
                                   Ok(ClientMessage::Resume) => {
                                       if turn_active {
                                           if let Err(e) = send_ws_json(
                                               &mut sender,
                                               serde_json::json!({
                                                   "type": "Error",
                                                   "error": "a turn is already active; send interrupt first"
                                               }),
                                           ).await {
                                               debug!(error = %e, "ws send Error(turn_active) failed; client likely disconnected");
                                           }
                                           continue;
                                       }
                                       // 崩溃恢复(无新输入):与 react 面 resume_session_id
                                       // 同一 resume_crashed 管线。会话索引口径与 continuation
                                       // 一致——恢复路径不发 SessionCreated(既有会话),不手置
                                       // session_established,Done success 才落索引:恢复失败
                                       //(如引擎侧无状态)不产生幽灵条目;索引合并 title 取首个
                                       // 非空,恢复轮无新标题不抹既有标题。
                                       match current_session.clone() {
                                           Some(sid) => {
                                               let runner = match construct_runner(&state, &agent_type, workspace.as_deref()).await {
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
                                               let cancel = runner.cancel_token().clone();
                                               current_cancel = Some(cancel);
                                               turn_active = true;
                                               turn_succeeded = false;
                                               info!(session_id = %sid, "G16: crash-resume turn");
                                               let event_stream = runner.resume_crashed(sid);
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
                                                   if let Err(e) = tx.send(WsEvent::TurnEnd).await {
                                                       debug!(error = %e, "TurnEnd send failed; main ws loop already exited");
                                                   }
                                               });
                                           }
                                           None => {
                                               let _ = send_ws_json(
                                                   &mut sender,
                                                   serde_json::json!({
                                                       "type": "Error",
                                                       "error": "no session to resume; connect with a session id first"
                                                   }),
                                               ).await;
                                           }
                                       }
                                   }
                                   Ok(ClientMessage::S4Publish { path }) => {
                                       // B22:定稿通知入 gov 账本(best-effort,不回帧不打断
                                       // WS 流;gov 写入走 serve 内部通道不经审批卡——
        // 写入动作本身即 Fact,§四.3)
                                       // session 未建立("new" 连接尚未 SessionCreated)时
                                       // 无主会话可归属,按 best-effort 丢弃
                                       if let Some(sid) = current_session.as_deref() {
                                           crate::governance::hooks::s4_publish(sid, &path);
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
                               match &result {
                                   Ok(AgentEvent::SessionCreated { session_id: sid, .. }) => {
                                       current_session = Some(sid.clone());
                                       session_established = true;
                                       info!(session_id = %sid, "G16: session created");
                                       // 会话索引:新会话落一行(fail-soft,仅展示辅助)
                                       state.session_index().record(
                                           &crate::api::session_index::SessionIndexEntry {
                                               session_id: sid.clone(),
                                               agent_type: agent_type.clone(),
                                               created_at: crate::api::session_index::unix_now(),
                                               last_active: crate::api::session_index::unix_now(),
                                               title: pending_title
                                                   .as_deref()
                                                   .unwrap_or("")
                                                   .chars()
                                                   .take(60)
                                                   .collect(),
                                           },
                                       );
                                   }
                                   Ok(AgentEvent::Done(r)) => turn_succeeded = r.success,
                                   // 流级错误 / Error 事件 = 本轮失败(不落索引)
                                   Ok(AgentEvent::Error(_)) | Err(_) => turn_succeeded = false,
                                   _ => {}
                               }
                               // 序列化 + 推给客户端
                               let mut json = agent_event_to_json(result.clone());
                               // 每轮权威校正(2026-10-03): Done 回执附服务端 Fact 版本
                               // (actual_version,与 rewind 回执同口径)——console 侧每轮据此
                               // 同步本地指针,版本漂移窗口清零。查询失败回落=不带字段
                               // (客户端容错保留旧指针,fail-soft)。
                               if json["type"] == "Done" {
                                   if let Some(sid) = &current_session {
                                       match crate::agent::runner::session_version(
                                           state.evorule_client(),
                                           sid,
                                       )
                                       .await
                                       {
                                           Ok(ver) => {
                                               json["actual_version"] = serde_json::json!(ver);
                                           }
                                           Err(e) => {
                                               debug!(error = %e, session_id = %sid, "session_version query failed; Done frame without actual_version");
                                           }
                                       }
                                   }
                               }
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
                               // 会话索引:续用会话(重连恢复/多轮)无 SessionCreated 事件,
                               // 以 TurnEnd 补记录刷新 last_active(读时去重合并)。
                               // 仅当会话真实建立过或本轮成功时落记录 — 防幽灵条目:
                               // localStorage 残留不存在的 session id 时 run_continuation
                               // 失败,失败 turn 不写入索引
                               if let Some(sid) = &current_session {
                                   if session_established || turn_succeeded {
                                       state.session_index().record(
                                           &crate::api::session_index::SessionIndexEntry {
                                               session_id: sid.clone(),
                                               agent_type: agent_type.clone(),
                                               created_at: crate::api::session_index::unix_now(),
                                               last_active: crate::api::session_index::unix_now(),
                                               title: String::new(),
                                           },
                                       );
                                       // 同批落本地消息快照(与 transcript 端点同一
                                       // load_transcript 投影路径,保证同源同构)。fail-soft:
                                       // 快照是展示辅助(服务会话 TTL 回收后的历史可见),
                                       // 失败只 warn 不阻断会话流程。
                                       let ns = crate::api::agent_api::resolve_memory_namespace(
                                           &state,
                                           &agent_type,
                                       );
                                       match crate::api::snapshots::capture_from_engine(
                                           state.evorule_client(),
                                           state.snapshots(),
                                           sid,
                                           &agent_type,
                                           &ns,
                                       )
                                       .await
                                       {
                                           Ok(p) => debug!(path = %p.display(), "snapshot saved"),
                                           Err(e) => warn!(
                                               error = %e,
                                               session_id = %sid,
                                               "snapshot save failed (fail-soft)"
                                           ),
                                       }
                                   }
                               }
                           }
                           None => {
                               // event_tx 全部 drop(不应发生 — 主循环持有 event_tx)
                               // 视为连接异常,退出
                               warn!("G16: event channel closed unexpectedly");
                               break;
                           }
                       }
                   }
                   // ===== 转发文件系统事件批量给客户端(工作台文件树实时刷新) =====
                   fs_batch = fs_rx.recv() => {
                       match fs_batch {
                           Ok(batch) => {
                               let json = serde_json::json!({ "type": "fs_events", "events": &*batch });
                               if send_ws_json(&mut sender, json).await.is_err() {
                                   warn!("G16: failed to send fs_events frame, client may have disconnected");
                                   break;
                               }
                           }
                           Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                               // 设计取舍:不补发——跳过落后量,前端手动刷新兜底
                               debug!(skipped = n, "G16: fs_events lagged; skipping missed batches");
                           }
                           Err(_) => {
                               // hub 关闭 = serve 正在关闭;正常退出连接
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

/// G8 审批决策端三态路由（2026-09-26 设计/2026-10-15 实施）
///
/// - `Some("auto_policy")`:PolicyApproval 判定式（无人值守，现状语义不动）
/// - `Some("policy")`:PolicyReview 评审式——条件文件 `.evo/approval-policy.toml`
///   serve 侧独占读取，加载失败 fail-visible 降级 HttpApproval（显式留痕，
///   绝不静默按无条件放行处理）；评审调用经 AuditedLlm sidecar（评审 prompt
///   与判定全文入审计链），fallback 为 HttpApproval 60s 人工审批窗
/// - 其余（manual/缺省）:HttpApproval（现状行为零变化）
///
/// 返回 `(决策端, Runaway 护栏策略)`:policy 模式加载成功时同时产出
/// [`RunawayPolicy`]（护栏注入面——计数阈值+复述条件清单复用
/// 同一份加载结果）；其余模式护栏为 None（不启用，行为零变化）。
///
/// crate 内共享：WS 面（construct_runner）与 HTTP 面（run_agent /
/// run_agent_stream）同口径调用，避免三态路由在 serve 各执行面分叉。
/// `approval.exemptionOverride` 设置读取(用户显式关闭红线豁免强制;
/// serve WS/HTTP 两面共读,缺省 false=红线强制生效)
pub(crate) fn approval_exemption_override(state: &AgentApiState) -> bool {
    state
        .workbench_settings()
        .merged()
        .0
        .get("approval.exemptionOverride")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 审批档位解析单源:定义级 `approval_mode` 优先;未指定时回落设置键
/// `approval.mode.shellExec`(每工具组档位=设置键的接线——shell_exec 为
/// candidate 主面,其设置档位充作会话缺省档位);两级均未指定 → None
/// (manual 缺省)。未知设置值不计(回落 None,门禁语义保守)
pub(crate) fn resolve_approval_mode(
    state: &AgentApiState,
    def_mode: Option<&str>,
) -> Option<String> {
    if let Some(m) = def_mode {
        return Some(m.to_string());
    }
    let (merged, _) = state.workbench_settings().merged();
    let v = merged
        .get("approval.mode.shellExec")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    match v {
        "manual" | "auto_policy" | "policy" => Some(v.to_string()),
        _ => None,
    }
}

pub(crate) fn build_approval_callback(
    state: &AgentApiState,
    mode: Option<&str>,
) -> (
    Arc<dyn crate::agent::approval::ApprovalCallback>,
    Option<crate::agent::runner::RunawayPolicy>,
) {
    use crate::agent::approval_policy::{ApprovalPolicyFile, AuditedReviewInvoker, PolicyReview};
    if mode == Some("auto_policy") {
        // 红线豁免前置闸:豁免清单适用于所有档位——命中且未显式关闭时
        // 回落人工审批卡(无人值守场景 60s 窗超时自动拒,保守安全)
        let override_flag = approval_exemption_override(state);
        let guarded = crate::agent::approval::RedLineGuardApproval::new(
            Arc::new(crate::agent::approval::PolicyApproval),
            Arc::new(crate::governance::GovAuditedApproval::new(Arc::new(
                crate::agent::approval::HttpApproval::new(state.pending_approvals().clone()),
            ))),
            override_flag,
        );
        return (Arc::new(guarded), None);
    }
    if mode == Some("policy") {
        let policy_path = state.workdir().join(".evo").join("approval-policy.toml");
        match ApprovalPolicyFile::load(&policy_path) {
            Ok(policy) => {
                let override_flag = approval_exemption_override(state);
                let audited = crate::agent::audited_llm::AuditedLlm::new(
                    state.evorule_client().clone(),
                    crate::io_handlers::LlmHandler::from_config(&state.llm_config()),
                );
                let runaway = crate::agent::runner::RunawayPolicy::from_policy_file(&policy);
                return (
                    Arc::new(PolicyReview::new(
                        policy,
                        Arc::new(AuditedReviewInvoker::new(audited)),
                        // B22:降级人工窗接审批卡留痕装饰器(仅此路会产生人工审批卡)
                        Arc::new(crate::governance::GovAuditedApproval::new(Arc::new(
                            crate::agent::approval::HttpApproval::new(
                                state.pending_approvals().clone(),
                            ),
                        ))),
                        override_flag,
                    )),
                    Some(runaway),
                );
            }
            Err(e) => {
                error!(
                    path = %policy_path.display(),
                    error = %e,
                    "approval-policy.toml load failed; policy mode degrades to manual approval"
                );
            }
        }
    }
    // B22:manual 档审批卡接治理留痕装饰器(approval_requested/resolved 入
    // gov 账本;auto_policy 判定式无人工卡不接——高频排除)
    (
        Arc::new(crate::governance::GovAuditedApproval::new(Arc::new(
            crate::agent::approval::HttpApproval::new(state.pending_approvals().clone()),
        ))),
        None,
    )
}

/// 构造一个 fresh AgentRunner(每轮新建)
///
/// `run_streaming` / `run_continuation` 消费 `self`,所以每轮需要重新构造。
/// 构造成本低(AgentRunner::new 只存 config + client handle)。
///
/// 记忆:定义启用了 memory(含 serve 侧对 general 的缺省注入)时构建
/// `MemoryManager`,使 continuation 轮次能回注会话历史。同步为
/// best-effort:拉取失败只记 warn 不阻断(历史加载走会话状态接口,
/// 不依赖本地同步结果)。
///
/// 返回 `None` = agent_type 加载失败(404 等价)或会话级工作区非法(升级期
/// 已校验绝对路径,此处为纵深防御,正常不可达)。
async fn construct_runner(
    state: &AgentApiState,
    agent_type: &str,
    workspace: Option<&str>,
) -> Option<AgentRunner> {
    let mut def = state.definitions().load(agent_type).ok()?;
    // 与 HTTP 端点同口径:general 定义未配置 memory 时注入最小持久记忆档
    if agent_type == "general"
        && (def.memory.memory_type == "none" || def.memory.memory_type.is_empty())
    {
        def.memory.memory_type = "persistent".to_string();
        def.memory.namespace = "general".to_string();
    }
    // C 批「会话级 workspace」:连接携带 ?workspace= → 请求级注入能力边界
    // (与 REST 面 run_agent 同口径:声明态整份覆盖,读写同域 rooted 于
    // workspace;声明优先,--read-root 不参与)。失败 fail-visible 拒构造。
    if let Some(ws) = workspace {
        if let Err(e) = crate::api::serve_tools::apply_workspace_boundary(&mut def, ws) {
            error!(
                agent_type = %agent_type,
                workspace = %ws,
                error = %e,
                "ws session workspace invalid; rejecting runner construction"
            );
            return None;
        }
    }
    // 与 HTTP 端点(run_agent / run_agent_stream)同口径:按 def.tools 白名单从
    // state.toolkit(union 30 工具)过滤出该 agent 可用工具,挂 runner 的 tool_handler。
    // 缺这步时 runner 的 tool_handler 为空 —— LLM 请求无工具契约(模型只能盲猜
    // 或输出供应商原生 XML),也调不通 call_service 工具。
    let (merged_settings, _) = state.workbench_settings().merged();
    // B22-C:事件轨道开关 serve 面安装(first-wins 幂等,改键后重启生效;
    // CLI 不 init 缺省开。HTTP agent_api 路径红线零触碰,其面缺省开——
    // 开关生效面注记见施工日志)
    crate::agent::event_ledger::init_event_ledger_enabled(
        merged_settings
            .get("agentTools.eventLedger")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
    );
    // B22 顺手修:A 项 GovLedger 单例 serve 面安装——PR1/PR2 交付了骨架与
    // 事件源 hooks,但 init_ledger 此前无生产调用点,gov 留痕实际 no-op;
    // 此处补接线(client 同源;重复 init 幂等,断言见 ledger.rs 测试)
    crate::governance::init_ledger(state.evorule_client().clone());
    let mut filtered = crate::api::serve_tools::build_filtered_toolkit_with_switches(
        &state.toolkit(),
        &def.tools,
        &merged_settings,
    );
    // 开关裁剪后同步收紧 def.tools(同 HTTP 端点口径:白名单声明=执行器=LLM 契约
    // 三者一致;WS 路径虽无 from_definition 校验,能力边界/L2 判定消费 def.tools
    // 也应基于过滤后的真实工具面)
    crate::api::serve_tools::restrict_tools_to_surface(&mut def.tools, &filtered);
    // M5-a:能力边界接线(同 HTTP 端点口径:声明重绑工具面 + 生效边界注入 runner)
    // B 批「读宽写窄」:读域根经 state 注入(--read-root),合成边界携带双域自报
    let capability_boundary = crate::api::serve_tools::wire_capability_boundary_with_read_root(
        &mut filtered,
        &def,
        state.workdir(),
        state.read_root(),
    );
    // skills 装配(C 形态):两源合并 —— definition 显式声明(最高优先,声明时刻=
    // 人工把关) + 两级目录注册项(经 REST 注册审批闸口 active + blake3 哈希钉死,
    // 失配自动降级)。声明解析失败/目录级失败/账本损坏 = 拒构造(fail-visible,
    // H 族门禁不可静默降级);单 skill 不合格由扫描器跳过 + warn,不炸会话创建。
    let declared_skills = match crate::api::serve_tools::resolve_declared_skills(&def) {
        Ok(declared) => declared.unwrap_or_default(),
        Err(e) => {
            error!(
                agent_type = %agent_type,
                error = %e,
                "skills declaration invalid; rejecting runner construction"
            );
            return None;
        }
    };
    let merged_skills =
        match crate::api::skill_api::merged_manifest_for_session(state, declared_skills) {
            Ok((manifest, _registry_path)) => manifest,
            Err(e) => {
                error!(
                    agent_type = %agent_type,
                    error = %e,
                    "skills scan/merge failed; rejecting runner construction"
                );
                return None;
            }
        };
    let resolved_skills =
        match crate::api::serve_tools::wire_skills(&mut filtered, Some(merged_skills)) {
            Ok(skills) => skills,
            Err(e) => {
                error!(
                    agent_type = %agent_type,
                    error = %e,
                    "skills wiring failed; rejecting runner construction"
                );
                return None;
            }
        };
    // S2 治理门禁段(S2 前移批):三段合并构造,经 runner 配置进组装层
    // (v2 序=权威紧跟 S1 身份段之后;不再改写 def.system_prompt——K-17②)
    let governance_segment =
        crate::api::serve_tools::build_governance_segment(state.evorule_client(), &def.tools).await;
    let config = def.to_agent_config();
    // 用户 hooks 装载(交互批 第二批):{workdir}/.evo-agent/hooks.json;
    // 缺失=None 零变化;解析失败=拒绝构造(fail-visible,与 skills 同型
    // ——坏的 hooks 文件静默跳过会让用户误以为在防)。每回合 fresh
    // runner=构造期装载,文件变更下回合生效
    let user_hooks = match crate::agent::user_hooks::WiredHooks::load_from_workdir(state.workdir())
    {
        Ok(v) => v.map(std::sync::Arc::new),
        Err(e) => {
            error!(
                error = %e,
                "user hooks load failed; rejecting runner construction"
            );
            return None;
        }
    };
    // G8 审批决策端 + Runaway 护栏:同一次 mode 判别/文件加载
    // 双产出(决策端挂 ApprovalCallback,护栏策略挂 with_runaway_policy)
    // 档位解析单源:定义级优先,未指定回落设置键 approval.mode.shellExec
    let resolved_mode = resolve_approval_mode(state, def.approval_mode.as_deref());
    let (approval_callback, runaway_policy) =
        build_approval_callback(state, resolved_mode.as_deref());
    let mut runner = AgentRunner::new(config, state.evorule_client().clone())
        // 用户 hooks(交互批 第二批)
        .with_user_hooks(user_hooks)
        // 39 号项 A:项目记忆段(AGENTS.md 层级,随 serve workdir 解析)
        .with_agents_md_segment(
            crate::agent::runner::load_agents_md_segment(state.workdir()).clone(),
        )
        // M5-a:注入生效能力边界(声明重绑工具面 + 会话边界段/边界事实)
        .with_capability_boundary(capability_boundary)
        // 挂 filtered toolkit(同 HTTP 端点口径,见 run_agent)
        .with_tool_handler(filtered)
        // G8:注入审批决策端(人工审查开合)——定义级 approval_mode 三态路由:
        // auto_policy=PolicyApproval 判定式(无人值守,理由必产);
        // policy=PolicyReview 评审式(独立 LLM 评审对照条件清单逐条判定,
        // 判定不可信一律降级人工;条件文件加载失败 fail-visible 降 HttpApproval);
        // 其余(manual/缺省)=HttpApproval 60s 人工审批窗(现状行为)
        .with_approval_callback(approval_callback)
        // 注入 Runaway 护栏(policy 档;None=不启用行为零变化)
        // ——200 次连续自动操作上限+修订④复述注入,跨重启计数回填
        .with_runaway_policy(runaway_policy)
        // 批K 段7:人在环问答端注入(与审批三态同口径):
        // auto_policy=无人值守语义 → CliAskUser 立即返回无人态(不模拟回答、不干等);
        // manual/policy/缺省 → HttpAskUser,pending 项经 POST /agents/{t}/answer 送达
        .with_ask_user_callback(match resolved_mode.as_deref() {
            Some("auto_policy") => {
                Arc::new(crate::builtin_tools::ask_user::CliAskUser { auto: true })
                    as Arc<dyn crate::builtin_tools::ask_user::AskUserCallback>
            }
            _ => Arc::new(crate::builtin_tools::ask_user::HttpAskUser {
                pending: state.pending_questions().clone(),
            }) as Arc<dyn crate::builtin_tools::ask_user::AskUserCallback>,
        })
        // G17:注入 metrics — runner 在 session/step/LLM/工具关键路径插桩
        .with_metrics(state.metrics().clone())
        // B21 PR-1:注入 journal 目录 — 会话事件流落盘(会话唯一真相源;
        // run_streaming_inner 内按会话创建 {sid}.jsonl)
        .with_journal_dir(state.workdir().join("data").join("sessions"))
        // checkpoint 仓库跟随会话工作区(与写工具族 apply_workspace_boundary
        // 同根统一解析);未声明会话工作区时回落 agent 工作区根(--agent-root
        // 优先,缺省=workdir;与 undo 工具 store/toolkit 构建期绑定同 root,
        // 不落 runtime)
        .with_workspace_checkpoint(&crate::api::serve_tools::session_workspace_root(
            workspace.map(std::path::Path::new),
            state.agent_workspace_root(),
        ))
        // 查账工具族接线（PR-11a）：以本会话态重绑 filtered 内查账工具实例
        .wire_accounting(state.workdir())
        // B21 D3:主动压缩策略(longSession.compaction.* 设置映射;与 HTTP
        // run/stream 端点同口径——此前 WS 主聊天面未接线,设置开了也不生效)
        .with_compaction_policy(crate::agent::runner::CompactionPolicy::from_settings(
            &merged_settings,
        ))
        // B2:注入 skills manifest(read_skill 已注册进 filtered,清单喂 S4b 槽位)
        .with_skills(resolved_skills)
        // S2 治理门禁段(v2 前移批:治理段进组装层独立槽位,不再内嵌基底)
        .with_governance_segment(governance_segment.clone())
        // serve 面 delegate 执行能力接线(B25 批顺手修,CLI run 主路径同款镜像;
        // 与 agent_api run/stream 端点同口径——此前 WS 会话 LLM 不可见 delegate)
        .with_delegate_context(
            crate::agent::delegate::DelegateContext::new(
                def.agent_type.as_str(),
                state.definitions().clone(),
                state.evorule_client().clone(),
            )
            .with_journal_dir(state.workdir().join("data").join("sessions"))
            // 项目记忆段下放(与主 runner 同源)
            .with_agents_md_segment(crate::agent::runner::load_agents_md_segment(
                state.workdir(),
            ))
            // 子代理工具契约:union toolkit 快照下放(delegate 内部按子定义
            // def.tools 白名单过滤——此前子代理零工具契约=纯文本执行器)
            .with_toolkit((*state.toolkit()).clone(), state.workdir())
            // 治理段同源下放(子代理 S2 槽位与主 runner 同源,纪律无豁免面)
            .with_governance_segment(governance_segment)
            // 委托窗口 token 归账(差值随 SpawnRecord 落 journal)
            .with_token_counter(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0))),
        );
    // PR-H4 验收修复:WS 流式路径此前从未消费 def.context_window_tokens——
    // max_context_tokens 停在 AgentRunner::new 字段默认 8192,context_window
    // 亦为 None(裁剪管路整体缺席;实测 session_spawn 链预算误判 32768=4×8192,
    // 而定义声明窗口 204800)。与 from_definition 同口径接线(单一实现
    // wire_definition_context_window),链预算基数与裁剪管路随之归真。
    runner.wire_definition_context_window(def.context_window_tokens);
    // 记忆启用时构建 MemoryManager(TTL / 持久化模式按定义透传)
    if def.memory.memory_type != "none" && !def.memory.memory_type.is_empty() {
        let mut mem = MemoryManager::new(&def.memory.namespace, state.evorule_client().clone());
        if let Some(ttl) = def.memory.ttl_secs {
            mem = mem.with_ttl_secs(ttl);
        }
        // F-618 阶段1:LexStore 检索缓存注入(与 runner 构造路径同款;缺省
        // 未声明=None=全量拉取路径零影响)。缺此注入时 serve 流即使定义
        // 声明 lex_store 也不生效(缓存与 lex_cache_stats 观测同源失活)
        if let Some(db) = &def.memory.lex_store {
            match crate::agent::lexstore::LexStore::open(std::path::Path::new(db)) {
                Ok(store) => mem.set_lex_store(std::sync::Arc::new(store)),
                Err(e) => warn!(
                    db = %db,
                    error = %e,
                    "LexStore open failed; recall falls back to full fetch"
                ),
            }
        }
        if let Err(e) = mem.sync_from_evorule().await {
            warn!(
                agent_type = %agent_type,
                error = %e,
                "memory sync failed; continuing without local sync"
            );
        }
        runner = runner
            .with_message_persist_mode(def.memory.message_persist.to_mode().unwrap_or_default())
            .with_memory(mem);
        // B22 PR5:E2E 实测补装配——事件轨道与证据链 store 在 WS 面此前只在
        // from_definition(CLI)构造,serve 面四触发点实际 no-op。与 from_definition
        // 同口径补齐(store 共享 namespace+client;ledger 同源;EL-/E- 前缀互斥)
        let event_store = crate::agent::memory_event::MemoryEventStore::new(
            &def.memory.namespace,
            state.evorule_client().clone(),
        );
        let ledger = crate::agent::event_ledger::EventLedger::new(
            event_store.clone(),
            &def.memory.namespace,
            state.evorule_client().clone(),
        );
        runner = runner
            .with_memory_event_store(event_store)
            .with_event_ledger(std::sync::Arc::new(ledger));
        // sediment 管线装配(单一实现,与 from_definition 同口径)——
        // serve 流面此前零装配,sediment 以 Default 跑(namespace="default"
        // 错域/llm_model_id="unknown"),知识提取/巩固/摘要全跳。收编方法
        // 覆盖 extractor/recipe 穿线/sediment_config/自省工具/验收命令/
        // summarizer+auditor 全链;装配失败 fail-visible 降 None(与 def
        // 加载失败同型,上层回 Error 帧)。注:本调用位于 memory_type!=
        // "none" 分支内——"none" 型 agent 无 memory,sediment 全链本就
        // no-op,与 CLI 面差异仅限矛盾配置场景(声明自省工具+memory none)
        match runner.assemble_sediment_pipeline(&def) {
            Ok(r) => runner = r,
            Err(e) => {
                error!(
                    agent_type = %agent_type,
                    error = %e,
                    "sediment pipeline assembly failed"
                );
                return None;
            }
        }
    }
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
/// 本函数=结构化事件帧契约的单一事实源:serve WS 面与 CLI `run --json` NDJSON 面
/// 共用此映射,两帧面逐帧同构(同构断言测试锁定)。
pub fn agent_event_to_json(event: Result<AgentEvent, AgentError>) -> serde_json::Value {
    match event {
        Ok(AgentEvent::SessionCreated {
            session_id,
            memory_enabled,
            constitution,
        }) => serde_json::json!({
            "type": "SessionCreated",
            "session_id": session_id,
            "memory_enabled": memory_enabled,
            "constitution": constitution,
        }),
        Ok(AgentEvent::Step { step }) => serde_json::json!({
            "type": "Step",
            "step": step,
        }),
        Ok(AgentEvent::LlmDelta { text }) => serde_json::json!({
            "type": "LlmDelta",
            "text": text,
        }),
        Ok(AgentEvent::LlmThinkingDelta { text }) => serde_json::json!({
            "type": "LlmThinkingDelta",
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
        Ok(AgentEvent::PlanSubmitted { plan_id, plan }) => serde_json::json!({
            "type": "PlanSubmitted",
            "plan_id": plan_id,
            "plan": plan,
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
            proposal_id,
        }) => serde_json::json!({
            "type": "ApprovalRequired",
            "tool_name": tool_name,
            "command": command,
            "risk": risk,
            "alternative": alternative,
            "proposal_id": proposal_id,
        }),
        Ok(AgentEvent::ApprovalResult {
            tool_name,
            approved,
            approver,
            auto_rejected,
        }) => serde_json::json!({
            "type": "ApprovalResult",
            "tool_name": tool_name,
            "approved": approved,
            "approver": approver,
            "auto_rejected": auto_rejected,
        }),
        // 批K 段7:人在环问答帧 —— 前端弹问答卡,经 POST /agents/{t}/answer 回传
        Ok(AgentEvent::AskUser {
            question_id,
            question,
            options,
            multi_select,
            timeout_secs,
        }) => serde_json::json!({
            "type": "AskUser",
            "question_id": question_id,
            "question": question,
            "options": options,
            "multi_select": multi_select,
            "timeout_secs": timeout_secs,
        }),
        // 批K 段7:问答结果(已回答/超时) —— 前端据此收卡置态
        Ok(AgentEvent::AskUserResult {
            question_id,
            answered,
            timed_out,
        }) => serde_json::json!({
            "type": "AskUserResult",
            "question_id": question_id,
            "answered": answered,
            "timed_out": timed_out,
        }),
        // 主动压缩观测帧(前端一次性提示;与 journal compaction_performed 同源)
        Ok(AgentEvent::CompactionPerformed {
            before_chars,
            after_chars,
            summary_generated,
        }) => serde_json::json!({
            "type": "CompactionPerformed",
            "before_chars": before_chars,
            "after_chars": after_chars,
            "summary_generated": summary_generated,
        }),
        Err(err) => serde_json::json!({
            "type": "Error",
            "error": err.to_string(),
        }),
    }
}

/// 把 AgentEvent(或流级错误)序列化为一行 JSON(NDJSON)或缩进 JSON
///
/// CLI `run --json` 的输出形态:compact=一行一事件(管道/jq 消费);
/// pretty=缩进多行(人读检视)。帧内容与 [`agent_event_to_json`](serve WS 面)完全一致。
pub fn agent_event_json_line(event: Result<AgentEvent, AgentError>, pretty: bool) -> String {
    let value = agent_event_to_json(event);
    if pretty {
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
    } else {
        value.to_string()
    }
}

// =============================================================================
// 单元测试
// =============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // ===== 审批档位解析单源测试(定义级优先/设置键回落/未知值保守) =====

    fn resolver_state(dir: &std::path::Path) -> AgentApiState {
        AgentApiState::new_with_metrics(
            crate::agent::AgentDefinitionManager::with_default_dir(),
            crate::api::evorule_client::EvoruleApiClient::new("http://localhost:0"),
            std::sync::Arc::new(crate::api::metrics::Metrics::new().unwrap()),
            dir.to_path_buf(),
            std::sync::Arc::new(crate::api::workspace_client::WorkspaceApiClient::new(
                "http://localhost:0",
            )),
            std::sync::Arc::new(std::sync::RwLock::new(
                crate::io_handlers::tool_handler::ToolHandler::new(),
            )),
        )
    }

    #[test]
    fn resolve_approval_mode_definition_wins_over_setting() {
        let dir = tempfile::tempdir().unwrap();
        let state = resolver_state(dir.path());
        // 定义级显式指定:无视设置键
        assert_eq!(
            resolve_approval_mode(&state, Some("policy")),
            Some("policy".to_string())
        );
    }

    #[test]
    fn resolve_approval_mode_falls_back_to_setting_key() {
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("data").join("workbench_settings.json");
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(
            &settings_path,
            r#"{"approval.mode.shellExec": "auto_policy"}"#,
        )
        .unwrap();
        let state = resolver_state(dir.path());
        assert_eq!(
            resolve_approval_mode(&state, None),
            Some("auto_policy".to_string())
        );
    }

    #[test]
    fn resolve_approval_mode_default_manual_and_unknown_conservative() {
        let dir = tempfile::tempdir().unwrap();
        let state = resolver_state(dir.path());
        // 出厂默认 manual:显式 manual 态(行为与缺省 None 的 manual 臂一致)
        assert_eq!(
            resolve_approval_mode(&state, None),
            Some("manual".to_string())
        );
        // 用户手写未知值:不计,回落 None(manual 缺省,门禁保守)
        let settings_path = dir.path().join("data").join("workbench_settings.json");
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(&settings_path, r#"{"approval.mode.shellExec": "yolo"}"#).unwrap();
        assert_eq!(resolve_approval_mode(&state, None), None);
    }

    // ===== ClientMessage 反序列化测试 =====

    #[test]
    fn test_client_message_deserialize() {
        let json = r#"{"type":"message","content":"hello"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            // S4-b:images 缺省反序列化为空(无图消息零行为变化)
            ClientMessage::Message {
                content,
                images,
                mode,
            } => {
                assert_eq!(content, "hello");
                assert!(images.is_empty());
                assert!(mode.is_none());
            }
            _ => panic!("expected Message variant"),
        }
    }

    /// S4-b:images 旁路字段随消息反序列化(上传回执原样回传形态)
    #[test]
    fn test_client_message_deserialize_with_images() {
        let json = r#"{"type":"message","content":"look","images":[
            {"path":".evo-agent/uploads/20261020-a.png","mime_type":"image/png"}]}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            ClientMessage::Message {
                content, images, ..
            } => {
                assert_eq!(content, "look");
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].path, ".evo-agent/uploads/20261020-a.png");
                assert_eq!(images[0].mime_type, "image/png");
            }
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
    fn test_client_resume_deserialize() {
        let json = r#"{"type":"resume"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ClientMessage::Resume));
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
    fn test_ws_query_with_agent_type() {
        // 未知字段(如旧版 `?token=`)被 serde 忽略——旧客户端 URL 不炸,只被
        // 中间件 401(回归锁见 auth::test_middleware_query_token_is_rejected)
        let query: WsQuery =
            serde_json::from_str(r#"{"token":"secret","agent_type":"researcher"}"#).unwrap();
        assert_eq!(query.agent_type.as_deref(), Some("researcher"));
    }

    #[test]
    fn test_ws_query_empty() {
        let query: WsQuery = serde_json::from_str(r#"{}"#).unwrap();
        assert!(query.agent_type.is_none());
    }

    // ===== agent_event_to_json 测试 =====

    #[test]
    fn test_event_to_json_session_created() {
        let v = agent_event_to_json(Ok(AgentEvent::SessionCreated {
            session_id: "s-42".to_string(),
            memory_enabled: true,
            constitution: Some("pass@2".to_string()),
        }));
        assert_eq!(v["type"], "SessionCreated");
        assert_eq!(v["session_id"], "s-42");
        assert_eq!(v["memory_enabled"], true);
        assert_eq!(v["constitution"], "pass@2", "审查结论入站外面");
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
    fn test_event_to_json_llm_thinking_delta() {
        let v = agent_event_to_json(Ok(AgentEvent::LlmThinkingDelta {
            text: "思考中".to_string(),
        }));
        assert_eq!(v["type"], "LlmThinkingDelta");
        assert_eq!(v["text"], "思考中");
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
            proposal_id: "ap-ws-1".to_string(),
        }));
        assert_eq!(v["type"], "ApprovalRequired");
        assert_eq!(v["tool_name"], "shell_exec");
        assert_eq!(v["proposal_id"], "ap-ws-1");
    }

    #[test]
    fn test_event_to_json_stream_level_error() {
        let v = agent_event_to_json(Err(AgentError::Internal("stream broke".to_string())));
        assert_eq!(v["type"], "Error");
        assert!(v["error"].as_str().unwrap().contains("stream broke"));
    }

    // ===== CLI JSON 事件面同构断言(NDJSON 行形态与 WS 帧逐帧一致) =====

    /// 固定事件样本:覆盖全部帧变体(含流级错误);每次调用产出新鲜实例
    fn sample_frame_events() -> Vec<Result<AgentEvent, AgentError>> {
        use crate::agent::AgentResult;
        vec![
            Ok(AgentEvent::SessionCreated {
                session_id: "s-iso".to_string(),
                memory_enabled: true,
                constitution: Some("pass@2".to_string()),
            }),
            Ok(AgentEvent::Step { step: 1 }),
            Ok(AgentEvent::LlmDelta {
                text: "he\nllo".to_string(),
            }),
            Ok(AgentEvent::LlmThinkingDelta {
                text: "想".to_string(),
            }),
            Ok(AgentEvent::ToolCall {
                name: "search".to_string(),
                args: serde_json::json!({"q": "iso"}),
            }),
            Ok(AgentEvent::ToolResult {
                name: "search".to_string(),
                result: serde_json::json!({"hits": 1}),
            }),
            Ok(AgentEvent::LlmDone {
                content: "ok".to_string(),
                finish_reason: Some("stop".to_string()),
            }),
            Ok(AgentEvent::PlanSubmitted {
                plan_id: "p-1".to_string(),
                plan: serde_json::json!({"steps": ["a", "b"]}),
            }),
            Ok(AgentEvent::Info("rewind".to_string())),
            Ok(AgentEvent::ApprovalRequired {
                tool_name: "shell_exec".to_string(),
                command: "echo hi".to_string(),
                risk: "low".to_string(),
                alternative: String::new(),
                proposal_id: "ap-iso".to_string(),
            }),
            Ok(AgentEvent::ApprovalResult {
                tool_name: "shell_exec".to_string(),
                approved: true,
                approver: "user".to_string(),
                auto_rejected: false,
            }),
            Ok(AgentEvent::AskUser {
                question_id: "q-1".to_string(),
                question: "pick".to_string(),
                options: vec!["a".to_string(), "b".to_string()],
                multi_select: false,
                timeout_secs: 30,
            }),
            Ok(AgentEvent::AskUserResult {
                question_id: "q-1".to_string(),
                answered: true,
                timed_out: false,
            }),
            Ok(AgentEvent::Error(AgentError::LlmError("mid".to_string()))),
            Ok(AgentEvent::Done(AgentResult::success(
                "final".to_string(),
                2,
                50,
                vec!["search".to_string()],
            ))),
            Err(AgentError::Internal("stream broke".to_string())),
            Ok(AgentEvent::CompactionPerformed {
                before_chars: 5000,
                after_chars: 1200,
                summary_generated: true,
            }),
        ]
    }

    #[test]
    fn json_line_matches_ws_frame_for_every_variant() {
        // 同构锚:同一事件,WS 帧映射与 CLI NDJSON 行形态(compact+pretty)一致
        for ev in sample_frame_events() {
            let ws_frame = agent_event_to_json(ev.clone());
            let line = agent_event_json_line(ev.clone(), false);
            let parsed: serde_json::Value =
                serde_json::from_str(&line).expect("ndjson line parses");
            assert_eq!(parsed, ws_frame);

            let pretty: serde_json::Value =
                serde_json::from_str(&agent_event_json_line(ev, true)).unwrap();
            assert_eq!(pretty, ws_frame);
        }
    }

    #[test]
    fn json_line_compact_is_single_line_ndjson() {
        // NDJSON 契约:compact 形态一行一事件,载荷内换行被 JSON 转义不破行
        let ev = Ok(AgentEvent::LlmDelta {
            text: "line1\nline2".to_string(),
        });
        let line = agent_event_json_line(ev, false);
        assert_eq!(line.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["text"], "line1\nline2");
    }

    #[test]
    fn json_lines_sequence_matches_ws_frames_frame_by_frame() {
        // 事件序列合成 NDJSON 流,按行解析后与 WS 面逐帧对照
        let events = sample_frame_events();
        let ws: Vec<serde_json::Value> = events
            .iter()
            .map(|e| agent_event_to_json(e.clone()))
            .collect();
        let stream = events
            .iter()
            .map(|e| agent_event_json_line(e.clone(), false))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: Vec<serde_json::Value> = stream
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed.len(), ws.len());
        for (p, w) in parsed.iter().zip(ws.iter()) {
            assert_eq!(p, w);
        }
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
    async fn test_ws_auth_rejects_missing_credentials() {
        // auth 启用 + 无 Bearer/无 ticket → 401(auth_middleware 在 WS 提取前拦截)
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
    async fn test_ws_auth_rejects_query_token() {
        // auth 启用 + query 长期凭据 `?token=` → 恒 401(query 凭据已废止,
        // 刻意不识别;客户端改走 Bearer 换一次性 ticket)
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

    #[tokio::test]
    async fn test_ws_tcp_valid_ticket_upgrade_succeeds() {
        // 真实 TCP server:auth 启用 + Bearer 换取的一次性 ticket → `?ticket=`
        // 通行中间件,升级成功(101);同票二次消费将 401(见 auth::TicketStore 测试)
        let state = make_test_state();
        let auth = AuthConfig::new(vec!["secret".to_string()], true);
        let (ticket, _ttl) = auth.issue_ticket();
        let (addr, _handle) = start_test_server(state, Some(auth)).await;

        let url = format!(
            "ws://{}/api/sessions/new/ws?agent_type=test&ticket={ticket}",
            addr
        );
        let result = tokio_tungstenite::connect_async(url).await;

        assert!(result.is_ok(), "should upgrade with valid one-time ticket");
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
        // 真实 TCP server:auth 启用 + query 长期凭据 `?token=` → 401(已废止)
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
