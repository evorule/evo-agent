// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! DAP 代理端点：适配器清单检测 + WS 透明转发升级（运行和调试板块）。
//!
//! - `GET /api/dap/adapters`：白名单适配器清单（来自 `dap.adapters` 设置键；
//!   **缺省空=fail-closed**——部署方显式配置后才可 spawn 适配器进程）
//! - `GET /api/dap/ws?adapter=<名>&token=`：WS 升级，白名单命中即 spawn 适配器
//!   并双向透传（WS 文本帧 ↔ 适配器 stdio Content-Length 帧）；断连/适配器
//!   退出即收尾并 kill。
//!
//! 治理：spawn/exit 经治理事件流 record-only（`dap_session` 事件型）；鉴权与
//! 既有 WS 端点同款双层（auth 中间件第一层 + `?token=` fallback 第二层）。

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::api::agent_api::AgentApiState;
use crate::dap::{adapters_from_settings, AdapterSpec};
use crate::governance::TerminalAction;

/// WS 帧大小上限（与 LSP WS 门同款 4MB；DAP 消息远小于此，超限断开）
const WS_MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// WS 连接 query 参数
#[derive(Debug, serde::Deserialize)]
pub struct DapWsQuery {
    /// 适配器名（须命中 `dap.adapters` 白名单）
    pub adapter: String,
    /// 鉴权 token（WS 握手不能带 Authorization 头时的 fallback）
    #[serde(default)]
    pub token: Option<String>,
}

/// `GET /api/dap/adapters` — 白名单适配器清单
pub async fn adapters(State(state): State<AgentApiState>) -> impl IntoResponse {
    let (merged, _) = state.workbench_settings().merged();
    let list: Vec<Value> = adapters_from_settings(merged.get("dap.adapters"))
        .into_iter()
        .map(|(name, spec)| {
            json!({
                "name": name,
                "command": spec.command,
                "langs": spec.langs,
            })
        })
        .collect();
    Json(json!({ "adapters": list }))
}

/// `GET /api/dap/ws?adapter=<名>` — WS 升级（白名单校验在升级响应前完成）
pub async fn dap_ws(
    ws: WebSocketUpgrade,
    Query(q): Query<DapWsQuery>,
    State(state): State<AgentApiState>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let (merged, _) = state.workbench_settings().merged();
    let adapters = adapters_from_settings(merged.get("dap.adapters"));
    let Some(spec) = adapters
        .iter()
        .find(|(n, _)| *n == q.adapter)
        .map(|(_, s)| s.clone())
    else {
        warn!(adapter = %q.adapter, "dap ws rejected — adapter not whitelisted");
        return Err((
            StatusCode::NOT_FOUND,
            Json(
                json!({ "error": format!("adapter '{}' not in dap.adapters whitelist", q.adapter) }),
            ),
        ));
    };
    Ok(ws
        .max_message_size(WS_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| run_dap_socket(socket, state, spec)))
}

/// 单条 DAP WS 连接：桥 spawn + 双向泵；断连/适配器退出即 kill 收尾
async fn run_dap_socket(socket: WebSocket, state: AgentApiState, spec: AdapterSpec) {
    let (mut sink, mut stream) = socket.split();
    let (in_tx, in_rx) = mpsc::channel::<String>(128);
    let (out_tx, mut out_rx) = mpsc::channel::<String>(128);

    // 入向泵:WS 文本帧 → 桥(二进制帧 DAP over WS 不使用;忽略)
    let in_task = tokio::spawn(async move {
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(WsMessage::Text(text)) => {
                    if in_tx.send(text.to_string()).await.is_err() {
                        break; // 桥已结束
                    }
                }
                Ok(WsMessage::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    // 治理留痕:适配器 spawn(record-only;与 LSP 门 tracing 并行,面板可消费)
    crate::governance::hooks::dap_session(
        crate::governance::TerminalAction::Create,
        &format!("{}, cmd={}", spec.command, spec.args.join(" ")),
    );

    let workdir = state.workdir().to_path_buf();
    let adapter_name = q_adapter_name(&spec);
    let bridge = tokio::spawn(async move {
        crate::dap::run_bridge(spec, workdir, in_rx, out_tx).await;
    });

    // 出向泵:桥 → WS 文本帧
    while let Some(text) = out_rx.recv().await {
        if sink.send(WsMessage::Text(text.into())).await.is_err() {
            break; // 客户端已断开;桥将经入向通道关闭自行收尾
        }
    }

    // 收尾:kill 适配器(桥的 select 两臂任一结束都视为会话终止)+退出留痕
    bridge.abort();
    let _ = bridge.await;
    in_task.abort();
    let _ = in_task.await;
    crate::governance::hooks::dap_session(
        crate::governance::TerminalAction::Close,
        &format!("{adapter_name} session closed"),
    );
    debug!(adapter = %adapter_name, "dap ws connection cleaned up");
}

/// 白名单条目 → 展示名(留痕用;AdapterSpec 未回传名称,调用方持名)
fn q_adapter_name(spec: &AdapterSpec) -> String {
    spec.command.clone()
}
