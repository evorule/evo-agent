// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! PTY WS 端点:工作台集成终端(人的终端)。
//!
//! - `GET /api/pty/ws?term={id}&cols=&rows=&token=` → WS 升级(与会话 WS/LSP WS
//!   同款三层鉴权:middleware `?token=` fallback + handler 内二次校验)
//! - 上行:**Binary**=键盘字节;**Text**=控制帧(白名单 `resize|pause|resume`)
//! - 下行:**Binary**=PTY 输出;**Text**=控制帧(`gov_event`/`exit`/`error`)
//! - 治理分级:会话创建/关闭推 `gov_event`(record-only 展示层);会话内零拦截
//! - 生命周期:并发上限 8;child 退出→`exit` 帧→回收;WS 断开→树级 kill
//!
//! 背压:前端写队列超水位发 `pause`——本端置 paused 停止转发(有界通道随即
//! 充满→读线程停读→内核管道反压 shell),`resume` 恢复;16ms 合帧在前端。

#![forbid(unsafe_code)]

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info, warn};

use crate::api::agent_api::AgentApiState;
use crate::pty::session::{PtyMsg, PtySessionHandle};
use crate::pty::{session, PtyControl};

/// 单帧大小上限(上行;防畸形帧内存攻击)
const WS_MAX_MESSAGE_BYTES: usize = 256 * 1024;

/// WS 连接 query 参数
#[derive(Debug, Deserialize)]
pub struct PtyWsQuery {
    /// 终端实例标识(前端 chips 键;透传回治理事件)
    pub term: Option<String>,
    /// 初始尺寸(前端 fit 实测;ConPTY 零尺寸坑)
    /// 初始列数(前端 fit 实测)
    pub cols: Option<u16>,
    /// 初始行数
    pub rows: Option<u16>,
    /// 鉴权 token(auth 启用时必填)
    pub token: Option<String>,
}

/// `GET /api/pty/ws` — WS 升级
pub async fn pty_ws(
    ws: WebSocketUpgrade,
    State(state): State<AgentApiState>,
    Query(query): Query<PtyWsQuery>,
) -> Response {
    // 二次鉴权(defense in depth — 中间件已做第一层)
    if state.auth_config().enabled() {
        let ok = query
            .token
            .as_deref()
            .map(|t| state.auth_config().validate(t))
            .unwrap_or(false);
        if !ok {
            warn!("PTY WS upgrade rejected — invalid token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let term_id = query.term.clone().unwrap_or_else(|| "term-1".to_string());
    let cols = query.cols.unwrap_or(80).clamp(2, 500);
    let rows = query.rows.unwrap_or(24).clamp(2, 300);

    // 并发护栏(第 9 个拒绝)
    if !state.pty_registry().try_acquire() {
        return (
            StatusCode::CONFLICT,
            "terminal session limit reached (8)".to_string(),
        )
            .into_response();
    }

    ws.max_message_size(WS_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, term_id, cols, rows))
}

async fn handle_socket(
    socket: WebSocket,
    state: AgentApiState,
    term_id: String,
    cols: u16,
    rows: u16,
) {
    let (mut sink, mut stream) = socket.split();

    // shell 发现(设置覆盖 > 发现序)
    let (merged, _) = state.workbench_settings().merged();
    let shell_override = merged
        .get("terminal.shell")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let shell = session::discover_shell(&shell_override);
    let workdir = state.workdir().to_path_buf();

    let handle = match session::spawn_session(&shell, &workdir, cols, rows) {
        Ok(h) => h,
        Err(e) => {
            let text = json!({
                "type": "error",
                "hint": format!("终端启动失败({e}):请检查 terminal.shell 设置或 shell 安装"),
            })
            .to_string();
            let _ = sink.send(WsMessage::Text(text.into())).await;
            state.pty_registry().release();
            return;
        }
    };
    // 整体解构:各部件独立所有权(select 双分支不再互斥借用)
    let PtySessionHandle {
        msg_tx,
        mut output_rx,
        mut exit_rx,
        pid,
        shell_label,
    } = handle;
    info!(term = %term_id, shell = %shell_label, pid = pid, "PTY session created");
    // terminal_session_create 治理留痕(record-only，best-effort)
    crate::governance::hooks::terminal_session(
        &term_id,
        crate::governance::TerminalAction::Create,
        &format!(
            "{shell_label}, pid={}",
            pid.map_or_else(|| "n/a".to_string(), |p| p.to_string())
        ),
    );

    // 治理事件:会话创建(record-only,展示层留痕)
    let created = json!({
        "type": "gov_event",
        "event": {
            "action": "pty.created",
            "termId": term_id,
            "shell": shell_label,
            "cwd": workdir.to_string_lossy(),
            "approver": "human",
        },
    })
    .to_string();
    if sink.send(WsMessage::Text(created.into())).await.is_err() {
        cleanup(&state, &msg_tx, &term_id, "client-closed", None, &mut sink).await;
        return;
    }

    let mut paused = false;
    let mut exit_code: Option<i32> = None;

    let end_reason = loop {
        // 终态已定:推 exit 帧后收尾
        if let Some(code) = exit_code {
            let text = json!({"type": "exit", "code": code}).to_string();
            let _ = sink.send(WsMessage::Text(text.into())).await;
            break "exited";
        }
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(WsMessage::Binary(data))) => {
                    if msg_tx.send(PtyMsg::Input(data.to_vec())).is_err() {
                        break "writer-gone";
                    }
                }
                Some(Ok(WsMessage::Text(text))) => match crate::pty::parse_control_frame(&text) {
                    Some(PtyControl::Resize { cols, rows }) => {
                        if msg_tx.send(PtyMsg::Resize { cols, rows }).is_err() {
                            break "writer-gone";
                        }
                    }
                    Some(PtyControl::Pause) => paused = true,
                    Some(PtyControl::Resume) => paused = false,
                    None => debug!(term = %term_id, "dropping unknown pty control frame"),
                },
                Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break "client-closed",
                Some(Ok(_)) => {} // Ping/Pong 由 axum 处理
            },
            chunk = output_rx.recv(), if !paused => match chunk {
                Some(data) => {
                    if sink.send(WsMessage::Binary(data.into())).await.is_err() {
                        break "client-closed";
                    }
                }
                None => {
                    // 读线程结束(PTY 侧 EOF):小窗等退出码落定
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        exit_rx.recv(),
                    )
                    .await
                    {
                        Ok(Some(code)) => exit_code = Some(code),
                        _ => break "exited",
                    }
                }
            },
            code = exit_rx.recv() => {
                exit_code = Some(code.unwrap_or(-1));
            }
        }
    };

    cleanup(&state, &msg_tx, &term_id, end_reason, exit_code, &mut sink).await;
}

/// 收尾:树级 kill 兜底(幂等)→注册表释放→治理事件(客户端在则推送)。
/// reason 区分 正常退出(exited)/客户端断开(client-closed)/写侧消亡(writer-gone)。
async fn cleanup(
    state: &AgentApiState,
    msg_tx: &std::sync::mpsc::Sender<PtyMsg>,
    term_id: &str,
    reason: &str,
    exit_code: Option<i32>,
    sink: &mut (impl futures_util::Sink<WsMessage> + Unpin),
) {
    // 防孤儿:无论退出路径,树级 kill 兜底(幂等;已退进程 taskkill 报错忽略)
    let _ = msg_tx.send(PtyMsg::Kill);
    state.pty_registry().release();
    let closed = json!({
        "type": "gov_event",
        "event": {
            "action": "pty.closed",
            "termId": term_id,
            "reason": reason,
            "exitCode": exit_code,
        },
    })
    .to_string();
    let _ = sink.send(WsMessage::Text(closed.into())).await;
    info!(term = %term_id, reason, "PTY session cleaned up");
    // B22:terminal_session_close 治理留痕(record-only;best-effort)
    crate::governance::hooks::terminal_session(
        term_id,
        crate::governance::TerminalAction::Close,
        &format!(
            "{reason}, exit_code={}",
            exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "n/a".to_string())
        ),
    );
}
